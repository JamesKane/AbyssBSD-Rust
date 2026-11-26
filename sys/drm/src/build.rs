// SPDX-License-Identifier: BSD-2-Clause
//! Building a device's display from its objects.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use drm_fourcc::{Fourcc, Modifier};
use kms_check::ObjId;
use sync::SpinLock;

use crate::kms::*;

/// Collects a display's objects, then [`build`](KmsBuilder::build)s
/// the device. Indices returned are the objects' places in their lists
/// and are what `possible_crtcs` masks and encoder lists name.
pub struct KmsBuilder {
    limits: KmsLimits,
    crtcs: Vec<Arc<dyn CrtcHw>>,
    planes: Vec<(Arc<dyn PlaneHw>, PlaneKind, u32)>,
    encoders: Vec<(Arc<dyn EncoderHw>, u32, u32)>,
    connectors: Vec<(Arc<dyn ConnectorHw>, Vec<usize>)>,
}

impl core::fmt::Debug for KmsBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "KmsBuilder({} display controllers, {} planes, {} encoders, {} connectors)",
            self.crtcs.len(),
            self.planes.len(),
            self.encoders.len(),
            self.connectors.len()
        )
    }
}

impl KmsBuilder {
    #[must_use]
    pub fn new(limits: KmsLimits) -> Self {
        Self {
            limits,
            crtcs: Vec::new(),
            planes: Vec::new(),
            encoders: Vec::new(),
            connectors: Vec::new(),
        }
    }

    /// Adds a display controller; returns its index.
    pub fn crtc(&mut self, hw: Arc<dyn CrtcHw>) -> usize {
        self.crtcs.push(hw);
        self.crtcs.len() - 1
    }

    /// Adds a plane that can scan out on the display controllers in `possible_crtcs`.
    pub fn plane(&mut self, hw: Arc<dyn PlaneHw>, kind: PlaneKind, possible_crtcs: u32) -> usize {
        self.planes.push((hw, kind, possible_crtcs));
        self.planes.len() - 1
    }

    /// Adds an encoder reaching `possible_crtcs`, cloning with the
    /// encoders in `possible_clones`.
    pub fn encoder(
        &mut self,
        hw: Arc<dyn EncoderHw>,
        possible_crtcs: u32,
        possible_clones: u32,
    ) -> usize {
        self.encoders.push((hw, possible_crtcs, possible_clones));
        self.encoders.len() - 1
    }

    /// Adds a connector reachable through the encoders `encoders`.
    pub fn connector(&mut self, hw: Arc<dyn ConnectorHw>, encoders: &[usize]) -> usize {
        self.connectors.push((hw, encoders.to_vec()));
        self.connectors.len() - 1
    }

    /// The device.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a graph that points outside itself or has no display controller;
    /// `ENOMEM`.
    pub fn build(self) -> Result<Kms, Errno> {
        let ncrtc = self.crtcs.len();
        let nplanes = self.planes.len();
        if ncrtc == 0 || ncrtc > 32 {
            return Err(Errno::EINVAL);
        }
        let all = u32::try_from((1u64 << ncrtc) - 1).map_err(|_| Errno::EINVAL)?;
        if self.planes.iter().any(|(_, _, m)| m & !all != 0)
            || self.encoders.iter().any(|(_, m, _)| m & !all != 0)
            || self
                .connectors
                .iter()
                .any(|(_, e)| e.iter().any(|i| *i >= self.encoders.len()))
        {
            return Err(Errno::EINVAL);
        }

        let mut props: Vec<Prop> = Vec::new();
        let mut add = |name: &'static str, kind: PropKind, immutable: bool, atomic: bool| {
            let id = PropId(u32::try_from(props.len() + 1).unwrap_or(u32::MAX));
            props.push(Prop {
                id,
                name,
                kind,
                immutable,
                atomic,
            });
            id
        };
        const PLANE_TYPES: &[(&str, u64)] = &[("Overlay", 0), ("Primary", 1), ("Cursor", 2)];
        const DPMS: &[(&str, u64)] = &[("On", 0), ("Standby", 1), ("Suspend", 2), ("Off", 3)];
        let u32max = u64::from(u32::MAX);
        let i32r = PropKind::SignedRange {
            min: i64::from(i32::MIN),
            max: i64::from(i32::MAX),
        };
        let std = Std {
            plane_type: add("type", PropKind::Enum(PLANE_TYPES), true, false),
            fb_id: add("FB_ID", PropKind::Object(DRM_MODE_OBJECT_FB), false, true),
            plane_crtc_id: add(
                "CRTC_ID",
                PropKind::Object(DRM_MODE_OBJECT_CRTC),
                false,
                true,
            ),
            src: [
                add(
                    "SRC_X",
                    PropKind::Range {
                        min: 0,
                        max: u32max,
                    },
                    false,
                    true,
                ),
                add(
                    "SRC_Y",
                    PropKind::Range {
                        min: 0,
                        max: u32max,
                    },
                    false,
                    true,
                ),
                add(
                    "SRC_W",
                    PropKind::Range {
                        min: 0,
                        max: u32max,
                    },
                    false,
                    true,
                ),
                add(
                    "SRC_H",
                    PropKind::Range {
                        min: 0,
                        max: u32max,
                    },
                    false,
                    true,
                ),
            ],
            dst: [
                add("CRTC_X", i32r, false, true),
                add("CRTC_Y", i32r, false, true),
                add(
                    "CRTC_W",
                    PropKind::Range {
                        min: 0,
                        max: i64::from(i32::MAX).cast_unsigned(),
                    },
                    false,
                    true,
                ),
                add(
                    "CRTC_H",
                    PropKind::Range {
                        min: 0,
                        max: i64::from(i32::MAX).cast_unsigned(),
                    },
                    false,
                    true,
                ),
            ],
            in_formats: add("IN_FORMATS", PropKind::Blob, true, false),
            in_fence_fd: add("IN_FENCE_FD", i32r, false, true),
            active: add("ACTIVE", PropKind::Range { min: 0, max: 1 }, false, true),
            mode_id: add("MODE_ID", PropKind::Blob, false, true),
            dpms: add("DPMS", PropKind::Enum(DPMS), false, false),
            edid: add("EDID", PropKind::Blob, true, false),
            conn_crtc_id: add(
                "CRTC_ID",
                PropKind::Object(DRM_MODE_OBJECT_CRTC),
                false,
                true,
            ),
        };
        let std_plane = [
            std.plane_type,
            std.fb_id,
            std.plane_crtc_id,
            std.src[0],
            std.src[1],
            std.src[2],
            std.src[3],
            std.dst[0],
            std.dst[1],
            std.dst[2],
            std.dst[3],
            std.in_formats,
            std.in_fence_fd,
        ];
        let std_crtc = [std.active, std.mode_id];
        let std_conn = [std.dpms, std.edid, std.conn_crtc_id];

        let mut next = 1u32;
        let mut take = || {
            let id = ObjId(next);
            next += 1;
            id
        };

        let mut state = State::default();
        let mut crtcs = Vec::new();
        for hw in self.crtcs {
            let mut ids = std_crtc.to_vec();
            let mut extra = Vec::new();
            for s in hw.properties() {
                ids.push(add(s.name, s.kind, s.immutable, false));
                extra.push(s.default);
            }
            state.crtcs.push(CrtcSt {
                extra,
                ..CrtcSt::default()
            });
            crtcs.push(CrtcObj {
                id: take(),
                hw,
                props: ids,
            });
        }
        let mut planes = Vec::new();
        for (hw, kind, possible_crtcs) in self.planes {
            let mut ids = std_plane.to_vec();
            let mut extra = Vec::new();
            for s in hw.properties() {
                ids.push(add(s.name, s.kind, s.immutable, false));
                extra.push(s.default);
            }
            state.planes.push(PlaneSt {
                extra,
                ..PlaneSt::default()
            });
            planes.push(PlaneObj {
                id: take(),
                hw,
                kind,
                possible_crtcs,
                props: ids,
            });
        }
        let mut encoders = Vec::new();
        for (hw, possible_crtcs, possible_clones) in self.encoders {
            encoders.push(EncoderObj {
                id: take(),
                hw,
                possible_crtcs,
                possible_clones,
            });
        }
        let mut connectors = Vec::new();
        for (hw, encs) in self.connectors {
            let mut ids = std_conn.to_vec();
            let mut extra = Vec::new();
            for s in hw.properties() {
                ids.push(add(s.name, s.kind, s.immutable, false));
                extra.push(s.default);
            }
            state.conns.push(ConnSt {
                extra,
                ..ConnSt::default()
            });
            connectors.push(ConnectorObj {
                id: take(),
                hw,
                encoders: encs,
                props: ids,
            });
        }

        // Each plane's format list is a blob the device owns.
        let mut blobs = BTreeMap::new();
        let mut in_formats = Vec::new();
        for p in &planes {
            let data = in_formats_blob(p.hw.formats(), p.hw.modifiers());
            let id = take();
            blobs.insert(id, Arc::new(Blob { id, data, owner: 0 }));
            in_formats.push(id);
        }

        Ok(Kms {
            limits: self.limits,
            crtcs,
            planes,
            encoders,
            connectors,
            props,
            std,
            state: SpinLock::new(state),
            blobs: SpinLock::new(blobs),
            fbs: SpinLock::new(BTreeMap::new()),
            in_formats,
            rt: (0..ncrtc)
                .map(|_| SpinLock::new(CrtcRt::default()))
                .collect(),
            shown: SpinLock::new(alloc::vec![None; nplanes]),
            done: sched::WaitQueue::new(),
            commit_lock: SpinLock::new(()),
            commit_id: core::sync::atomic::AtomicU64::new(1),
            unit: core::sync::atomic::AtomicU32::new(0),
            next_id: SpinLock::new(next),
        })
    }
}

/// An `IN_FORMATS` blob: the header, the format list, and one entry per
/// modifier with a bitmask of the formats (from a window of 64) it
/// applies to. Every modifier applies to every format here.
#[must_use]
pub fn in_formats_blob(formats: &[Fourcc], modifiers: &[Modifier]) -> Vec<u8> {
    const HEADER: usize = 24;
    let formats_len = formats.len() * 4;
    let mods_at = (HEADER + formats_len).next_multiple_of(8);
    // One entry per (modifier, window of 64 formats).
    let windows = formats.len().div_ceil(64);
    let entries = modifiers.len() * windows;
    let header = DrmFormatModifierBlob {
        version: FORMAT_BLOB_CURRENT,
        flags: 0,
        count_formats: u32::try_from(formats.len()).unwrap_or(u32::MAX),
        formats_offset: u32::try_from(HEADER).unwrap_or(0),
        count_modifiers: u32::try_from(entries).unwrap_or(u32::MAX),
        modifiers_offset: u32::try_from(mods_at).unwrap_or(0),
    };
    let mut b = alloc::vec![0u8; mods_at + entries * 24];
    header.encode(&mut b);
    for (i, f) in formats.iter().enumerate() {
        b[HEADER + i * 4..HEADER + i * 4 + 4].copy_from_slice(&f.0.to_ne_bytes());
    }
    let mut at = mods_at;
    for m in modifiers {
        for w in 0..windows {
            let n = (formats.len() - w * 64).min(64);
            let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            DrmFormatModifier {
                formats: mask,
                offset: u32::try_from(w * 64).unwrap_or(0),
                pad: 0,
                modifier: m.0,
            }
            .encode(&mut b[at..]);
            at += 24;
        }
    }
    b
}
