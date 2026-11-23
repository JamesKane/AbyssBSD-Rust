// SPDX-License-Identifier: BSD-2-Clause
//! The kernel mode setting object model: display controllers (CRTCs, in
//! Direct Rendering Manager's name for them), planes, encoders and connectors, with typed
//! properties, blobs and framebuffers.
//!
//! Display hardware is several drivers (GRAPHICS.md §7.2): each publishes
//! a narrow trait for its object, and the aggregate that owns the card
//! builds the graph with [`KmsBuilder`]. The core owns the state, checks
//! it (`lib/kms-check`) and tells the objects to commit.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use drm_fourcc::{Fourcc, MOD_LINEAR, Modifier};
use kms_check::{DstRect, Mode, ObjId, SrcRect};
use sync::SpinLock;

use crate::gem::Gem;

/// A property's number in a device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropId(pub u32);

/// What a property holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropKind {
    /// Unsigned, `min..=max`.
    Range { min: u64, max: u64 },
    /// Signed, `min..=max` (a value is its two's complement).
    SignedRange { min: i64, max: i64 },
    /// One of the named values.
    Enum(&'static [(&'static str, u64)]),
    /// Any combination of the named bits (the value is bit numbers' mask).
    Bitmask(&'static [(&'static str, u64)]),
    /// The ID of a blob.
    Blob,
    /// The ID of an object of this type (`DRM_MODE_OBJECT_*`).
    Object(u32),
}

/// A property a driver adds to its object, beyond the standard ones.
#[derive(Clone, Copy, Debug)]
pub struct PropSpec {
    pub name: &'static str,
    pub kind: PropKind,
    pub default: u64,
    /// Settable only by the driver.
    pub immutable: bool,
}

/// A property as the device registers it.
#[derive(Clone, Debug)]
pub struct Prop {
    pub id: PropId,
    pub name: &'static str,
    pub kind: PropKind,
    pub immutable: bool,
    /// Only the atomic ioctl sets it (`DRM_MODE_PROP_ATOMIC`).
    pub atomic: bool,
}

impl Prop {
    /// The `flags` field `GETPROPERTY` reports.
    #[must_use]
    pub fn flags(&self) -> u32 {
        let mut f = match self.kind {
            PropKind::Range { .. } => DRM_MODE_PROP_RANGE,
            PropKind::SignedRange { .. } => DRM_MODE_PROP_SIGNED_RANGE,
            PropKind::Enum(_) => DRM_MODE_PROP_ENUM,
            PropKind::Bitmask(_) => DRM_MODE_PROP_BITMASK,
            PropKind::Blob => DRM_MODE_PROP_BLOB,
            PropKind::Object(_) => DRM_MODE_PROP_OBJECT,
        };
        if self.immutable {
            f |= DRM_MODE_PROP_IMMUTABLE;
        }
        if self.atomic {
            f |= DRM_MODE_PROP_ATOMIC;
        }
        f
    }
}

/// `DRM_PLANE_TYPE_*`: the `type` property's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PlaneKind {
    Overlay = 0,
    Primary = 1,
    Cursor = 2,
}

/// How a connector stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Disconnected,
    Unknown,
}

impl Connection {
    pub(crate) fn wire_value(self) -> u32 {
        match self {
            Self::Connected => DRM_MODE_CONNECTED,
            Self::Disconnected => DRM_MODE_DISCONNECTED,
            Self::Unknown => DRM_MODE_UNKNOWNCONNECTION,
        }
    }
}

/// A CRTC's hardware: one display pipeline (scanout engine and timing).
pub trait CrtcHw: Send + Sync {
    /// Properties it adds to the standard ones.
    fn properties(&self) -> &[PropSpec] {
        &[]
    }
    /// The size of the gamma LUT, 0 for none.
    fn gamma_size(&self) -> u32 {
        0
    }
}

/// A plane's hardware: one layer.
pub trait PlaneHw: Send + Sync {
    /// Pixel formats it scans out.
    fn formats(&self) -> &[Fourcc];
    /// Modifiers it accepts, for every format.
    fn modifiers(&self) -> &[Modifier] {
        &[MOD_LINEAR]
    }
    /// Scale limits (source over destination, 16.16), `None` for 1:1.
    fn scale(&self) -> Option<(u32, u32)> {
        None
    }
    /// Properties it adds to the standard ones.
    fn properties(&self) -> &[PropSpec] {
        &[]
    }
}

/// An encoder's hardware: a transmitter between a CRTC and a connector.
pub trait EncoderHw: Send + Sync {
    /// `DRM_MODE_ENCODER_*`.
    fn kind(&self) -> u32;
}

/// A connector's hardware: a port, and what is plugged into it.
pub trait ConnectorHw: Send + Sync {
    /// `DRM_MODE_CONNECTOR_*`.
    fn kind(&self) -> u32;
    /// Whether a display is there (probes the hardware).
    fn detect(&self) -> Connection;
    /// The modes the display accepts, the preferred first.
    fn modes(&self) -> Vec<Mode>;
    /// Its EDID, if it has one.
    fn edid(&self) -> Option<Vec<u8>> {
        None
    }
    /// The display's size in millimetres, 0 if unknown.
    fn size_mm(&self) -> (u32, u32) {
        (0, 0)
    }
    /// Properties it adds to the standard ones.
    fn properties(&self) -> &[PropSpec] {
        &[]
    }
}

/// The device's size limits for framebuffers.
#[derive(Clone, Copy, Debug)]
pub struct KmsLimits {
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// What a plane's state is (committed, or pending in a commit).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaneSt {
    pub crtc: Option<ObjId>,
    pub fb: Option<ObjId>,
    pub src: SrcRect,
    pub dst: DstRect,
    /// Values of the driver's own properties, in [`PropSpec`] order.
    pub extra: Vec<u64>,
}

/// A CRTC's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrtcSt {
    pub active: bool,
    pub mode: Option<Mode>,
    /// The blob the mode came from, 0 for none.
    pub mode_blob: Option<ObjId>,
    pub extra: Vec<u64>,
}

/// A connector's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnSt {
    pub crtc: Option<ObjId>,
    pub dpms: u64,
    pub extra: Vec<u64>,
}

/// A device's whole display state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    pub crtcs: Vec<CrtcSt>,
    pub planes: Vec<PlaneSt>,
    pub conns: Vec<ConnSt>,
}

pub(crate) struct CrtcObj {
    pub id: ObjId,
    pub hw: Arc<dyn CrtcHw>,
    /// Standard properties, then the driver's.
    pub props: Vec<PropId>,
}

pub(crate) struct PlaneObj {
    pub id: ObjId,
    pub hw: Arc<dyn PlaneHw>,
    pub kind: PlaneKind,
    pub possible_crtcs: u32,
    pub props: Vec<PropId>,
}

pub(crate) struct EncoderObj {
    pub id: ObjId,
    pub hw: Arc<dyn EncoderHw>,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

pub(crate) struct ConnectorObj {
    pub id: ObjId,
    pub hw: Arc<dyn ConnectorHw>,
    /// Encoder indices it can use.
    pub encoders: Vec<usize>,
    pub props: Vec<PropId>,
}

/// A framebuffer: planes of buffers, in a format.
#[derive(Debug)]
pub struct Fb {
    pub id: ObjId,
    pub width: u32,
    pub height: u32,
    pub format: Fourcc,
    pub modifier: Modifier,
    pub flags: u32,
    pub gems: [Option<Arc<Gem>>; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    /// The file that made it (it goes when the file closes).
    pub owner: u64,
}

/// A property blob.
#[derive(Debug)]
pub struct Blob {
    pub id: ObjId,
    pub data: Vec<u8>,
    /// The file that made it, 0 for the device's own.
    pub owner: u64,
}

/// The standard property names and their places in an object's list.
pub(crate) struct Std {
    pub plane_type: PropId,
    pub fb_id: PropId,
    pub plane_crtc_id: PropId,
    pub src: [PropId; 4],
    pub dst: [PropId; 4],
    pub in_formats: PropId,
    pub active: PropId,
    pub mode_id: PropId,
    pub dpms: PropId,
    pub edid: PropId,
    pub conn_crtc_id: PropId,
}

/// The display side of a card.
pub struct Kms {
    pub(crate) limits: KmsLimits,
    pub(crate) crtcs: Vec<CrtcObj>,
    pub(crate) planes: Vec<PlaneObj>,
    pub(crate) encoders: Vec<EncoderObj>,
    pub(crate) connectors: Vec<ConnectorObj>,
    pub(crate) props: Vec<Prop>,
    pub(crate) std: Std,
    pub(crate) state: SpinLock<State>,
    pub(crate) blobs: SpinLock<BTreeMap<ObjId, Arc<Blob>>>,
    pub(crate) fbs: SpinLock<BTreeMap<ObjId, Arc<Fb>>>,
    /// Each plane's `IN_FORMATS` blob.
    pub(crate) in_formats: Vec<ObjId>,
    pub(crate) next_id: SpinLock<u32>,
}

impl core::fmt::Debug for Kms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Kms({} crtcs, {} planes, {} connectors)",
            self.crtcs.len(),
            self.planes.len(),
            self.connectors.len()
        )
    }
}

/// The most blobs and framebuffers the device holds, a guard against a
/// client that never frees.
const MAX_OBJECTS: usize = 4096;
/// The biggest blob (a mode list or an EDID is far smaller).
pub const MAX_BLOB: usize = 1 << 20;

impl Kms {
    fn alloc_id(&self) -> ObjId {
        let mut n = self.next_id.lock();
        let id = *n;
        *n += 1;
        ObjId(id)
    }

    pub(crate) fn prop(&self, id: PropId) -> Option<&Prop> {
        self.props.get(usize::try_from(id.0).ok()?.checked_sub(1)?)
    }

    pub(crate) fn crtc_index(&self, id: ObjId) -> Option<usize> {
        self.crtcs.iter().position(|c| c.id == id)
    }

    pub(crate) fn plane_index(&self, id: ObjId) -> Option<usize> {
        self.planes.iter().position(|p| p.id == id)
    }

    pub(crate) fn connector_index(&self, id: ObjId) -> Option<usize> {
        self.connectors.iter().position(|c| c.id == id)
    }

    pub(crate) fn encoder_index(&self, id: ObjId) -> Option<usize> {
        self.encoders.iter().position(|e| e.id == id)
    }

    /// The framebuffer `id`.
    #[must_use]
    pub fn fb(&self, id: ObjId) -> Option<Arc<Fb>> {
        self.fbs.lock().get(&id).cloned()
    }

    /// The blob `id`.
    #[must_use]
    pub fn blob(&self, id: ObjId) -> Option<Arc<Blob>> {
        self.blobs.lock().get(&id).cloned()
    }

    /// A copy of the committed state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state.lock().clone()
    }

    /// Makes a blob of `data`, owned by `owner`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for an empty or huge blob; `ENOMEM`; `ENOSPC` when the
    /// device holds too many.
    pub fn create_blob(&self, data: &[u8], owner: u64) -> Result<ObjId, Errno> {
        if data.is_empty() || data.len() > MAX_BLOB {
            return Err(Errno::EINVAL);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(data.len()).map_err(|_| Errno::ENOMEM)?;
        v.extend_from_slice(data);
        let id = self.alloc_id();
        let mut blobs = self.blobs.lock();
        if blobs.len() >= MAX_OBJECTS {
            return Err(Errno::ENOSPC);
        }
        blobs.insert(id, Arc::new(Blob { id, data: v, owner }));
        Ok(id)
    }

    /// Destroys blob `id`, which `owner` made.
    ///
    /// # Errors
    ///
    /// `ENOENT` if there is none; `EINVAL` if another file's or the
    /// device's.
    pub fn destroy_blob(&self, id: ObjId, owner: u64) -> Result<(), Errno> {
        let mut blobs = self.blobs.lock();
        match blobs.get(&id) {
            None => Err(Errno::ENOENT),
            Some(b) if b.owner != owner || owner == 0 => Err(Errno::EINVAL),
            Some(_) => {
                blobs.remove(&id);
                Ok(())
            }
        }
    }

    /// Registers a framebuffer.
    ///
    /// # Errors
    ///
    /// `ENOSPC` when the device holds too many.
    pub fn add_fb(&self, mut fb: Fb) -> Result<ObjId, Errno> {
        let id = self.alloc_id();
        fb.id = id;
        let mut fbs = self.fbs.lock();
        if fbs.len() >= MAX_OBJECTS {
            return Err(Errno::ENOSPC);
        }
        fbs.insert(id, Arc::new(fb));
        Ok(id)
    }

    /// Removes framebuffer `id`, which `owner` made. A plane showing it
    /// keeps its own reference until the next commit.
    ///
    /// # Errors
    ///
    /// `ENOENT` if there is none; `EINVAL` if another file's.
    pub fn remove_fb(&self, id: ObjId, owner: u64) -> Result<(), Errno> {
        let mut fbs = self.fbs.lock();
        match fbs.get(&id) {
            None => Err(Errno::ENOENT),
            Some(f) if f.owner != owner => Err(Errno::EINVAL),
            Some(_) => {
                fbs.remove(&id);
                Ok(())
            }
        }
    }

    /// A closing file takes its framebuffers and blobs with it.
    pub(crate) fn release_file(&self, owner: u64) {
        let gone: Vec<_> = {
            let mut fbs = self.fbs.lock();
            let ids: Vec<ObjId> = fbs
                .iter()
                .filter(|(_, f)| f.owner == owner)
                .map(|(i, _)| *i)
                .collect();
            ids.into_iter().filter_map(|i| fbs.remove(&i)).collect()
        };
        drop(gone);
        self.blobs.lock().retain(|_, b| b.owner != owner);
    }

    /// The current value of property `prop` on object `obj` (a CRTC,
    /// plane or connector), for the property ioctls.
    pub(crate) fn value(&self, obj: ObjId, prop: PropId) -> Option<u64> {
        let st = self.state.lock();
        let s = &self.std;
        if let Some(i) = self.plane_index(obj) {
            let p = &st.planes[i];
            let id = |o: Option<ObjId>| u64::from(o.map_or(0, |o| o.0));
            return Some(match prop {
                x if x == s.plane_type => self.planes[i].kind as u64,
                x if x == s.fb_id => id(p.fb),
                x if x == s.plane_crtc_id => id(p.crtc),
                x if x == s.src[0] => u64::from(p.src.x),
                x if x == s.src[1] => u64::from(p.src.y),
                x if x == s.src[2] => u64::from(p.src.w),
                x if x == s.src[3] => u64::from(p.src.h),
                // Signed values travel as two's complement in 64 bits.
                x if x == s.dst[0] => i64::from(p.dst.x).cast_unsigned(),
                x if x == s.dst[1] => i64::from(p.dst.y).cast_unsigned(),
                x if x == s.dst[2] => u64::from(p.dst.w),
                x if x == s.dst[3] => u64::from(p.dst.h),
                x if x == s.in_formats => u64::from(self.in_formats[i].0),
                _ => return self.extra_value(&self.planes[i].props, &p.extra, prop),
            });
        }
        if let Some(i) = self.crtc_index(obj) {
            let c = &st.crtcs[i];
            return Some(match prop {
                x if x == s.active => u64::from(c.active),
                x if x == s.mode_id => u64::from(c.mode_blob.map_or(0, |b| b.0)),
                _ => return self.extra_value(&self.crtcs[i].props, &c.extra, prop),
            });
        }
        if let Some(i) = self.connector_index(obj) {
            let c = &st.conns[i];
            return Some(match prop {
                x if x == s.conn_crtc_id => u64::from(c.crtc.map_or(0, |o| o.0)),
                x if x == s.dpms => c.dpms,
                x if x == s.edid => 0,
                _ => return self.extra_value(&self.connectors[i].props, &c.extra, prop),
            });
        }
        None
    }

    /// A driver property's value: its place in the object's own list.
    fn extra_value(&self, props: &[PropId], extra: &[u64], prop: PropId) -> Option<u64> {
        let pos = props.iter().position(|p| *p == prop)?;
        // The standard ones come first; the driver's follow.
        let std_count = props.len() - extra.len();
        extra.get(pos.checked_sub(std_count)?).copied()
    }
}
