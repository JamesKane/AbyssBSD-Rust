// SPDX-License-Identifier: MIT
// Copyright 2018 Advanced Micro Devices, Inc.
// Copyright 2018-2024 Advanced Micro Devices, Inc. All rights reserved.
// Copyright (C) 2018  Advanced Micro Devices, Inc.
// Copyright 2008 Advanced Micro Devices, Inc.
// Copyright 2008 Red Hat Inc.
// Copyright 2009 Jerome Glisse.
// Copyright 2025 Advanced Micro Devices, Inc.
// Copyright (C) 2020  Advanced Micro Devices, Inc.
// Copyright (C) 2022  Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! The AMD side of `drmn` (the graphics device node of FreeBSD's `drm-kmod`:
//! see the crate's module comment): what an AMD GPU's registers say at attach (plain
//! reads of two fixed registers), and, on request through sysctls, its IP
//! discovery table and the display state the firmware left.
//!
//! Nothing here writes to the GPU at attach. Reading the discovery table
//! writes the two VRAM index registers (and puts them back), so it is a
//! sysctl trigger, not attach work, and it runs under one lock.
//!
//! Layouts: the clean-room specification `amd-rdna2` (sections 01 and 08).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};

use amdgpu::discovery::{Discovery, hw};
use amdgpu::{display, regs};
use sync::SpinLock;
use vgapci::Aperture;

/// The GPU's register window, for `amdgpu`'s sequences.
struct Bar5<'a>(&'a Aperture);

impl regs::Window for Bar5<'_> {
    fn read32(&self, offset: usize) -> u32 {
        self.0.read32(offset).unwrap_or(u32::MAX)
    }
    fn write32(&self, offset: usize, value: u32) {
        self.0.write32(offset, value);
    }
}

/// What attach read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) device: u16,
    pub(crate) revision: u8,
    /// VRAM in MiB, as the memory size register says.
    pub(crate) vram_mib: u32,
    /// Whether the platform security processor has finished its boot work.
    pub(crate) psp_ready: bool,
}

/// An AMD GPU being driven from its sysctls.
pub(crate) struct Amd {
    name: String,
    gpu: Weak<Aperture>,
    /// Held across every index/data sequence and the table cache.
    table: SpinLock<Option<Discovery>>,
}

impl core::fmt::Debug for Amd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Amd").finish_non_exhaustive()
    }
}

/// Reads the identity (two plain reads); `None` if the device does not
/// answer.
pub(crate) fn identify(gpu: &Aperture) -> Option<Identity> {
    let w = Bar5(gpu);
    let memsize = regs::Window::read32(&w, regs::RCC_CONFIG_MEMSIZE);
    if memsize == u32::MAX {
        return None;
    }
    let ident = gpu.pci().function.header.ident;
    Some(Identity {
        device: ident.device,
        revision: ident.revision,
        vram_mib: memsize,
        psp_ready: regs::Window::read32(&w, regs::MP0_C2PMSG_33) >> 31 & 1 != 0,
    })
}

impl Amd {
    pub(crate) fn new(name: String, gpu: Weak<Aperture>) -> Self {
        Self {
            name,
            gpu,
            table: SpinLock::new(None),
        }
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("{}: {args}", self.name));
    }

    fn gpu(&self) -> Result<Arc<Aperture>, abi::errno::Errno> {
        self.gpu.upgrade().ok_or(abi::errno::Errno::ENXIO)
    }

    /// Reads the discovery table through the index registers and logs what
    /// is in it.
    pub(crate) fn discover(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        self.log(format_args!("reading the IP discovery table"));
        let found = {
            // One lock across the whole index/data sequence.
            let mut cache = self.table.lock();
            let r = regs::read_discovery(&Bar5(&gpu));
            if let Ok(d) = &r {
                *cache = Some(d.clone());
            }
            r
        };
        let d = match found {
            Ok(d) => d,
            Err(e) => {
                self.log(format_args!("discovery table not read: {e:?}"));
                return Err(Errno::EIO);
            }
        };
        self.log(format_args!(
            "discovery table version {}.{}, {} die(s), {} block(s), {} harvested",
            d.version.0,
            d.version.1,
            d.dies.len(),
            d.dies.first().map_or(0, |x| x.ips.len()),
            d.harvest.len(),
        ));
        if let Some(die) = d.dies.first() {
            for ip in &die.ips {
                self.log(format_args!(
                    "  block {} instance {}: version {}.{}.{}, {} segment(s): {:x?}",
                    ip.hw_id,
                    ip.instance,
                    ip.major,
                    ip.minor,
                    ip.revision,
                    ip.bases.len(),
                    ip.bases,
                ));
            }
        }
        if let Some(gc) = &d.gc {
            self.log(format_args!(
                "  GC info {}.{}: {} shader engine(s), {} shader array(s) each, {} compute \
                 unit(s) per array",
                gc.version.0,
                gc.version.1,
                gc.shader_engines,
                gc.shader_arrays_per_se,
                gc.cus_per_sa(),
            ));
        }
        Ok(())
    }

    /// Reads the display registers the firmware left, if the table says it
    /// is the chip the list is for.
    pub(crate) fn display(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let d = {
            let cache = self.table.lock();
            cache.clone()
        };
        let Some(d) = d else {
            self.log(format_args!(
                "display registers: read the discovery table first (dev.drmn sysctl `discover`)"
            ));
            return Err(Errno::EINVAL);
        };
        match display::read_all(&Bar5(&gpu), &d) {
            Ok(readings) => {
                self.log(format_args!(
                    "display registers as the firmware left them ({} reads):",
                    readings.len()
                ));
                for r in &readings {
                    self.log(format_args!(
                        "  {} @{:#07x} = {:#010x}",
                        r.name, r.offset, r.value
                    ));
                }
                Ok(())
            }
            Err(skip) => {
                self.log(format_args!("display registers not read: {skip:?}"));
                Err(Errno::ENOTSUP)
            }
        }
    }

    /// Reads a few words of VRAM through the aperture (BAR0), to learn
    /// whether it shows the same bytes as the firmware's framebuffer: the
    /// first words (where the firmware's picture starts), words just past
    /// the first 8 MiB, and the table's last 64 KiB.
    pub(crate) fn vram_probe(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let Some(len) = gpu.vram_len() else {
            self.log(format_args!("VRAM aperture (BAR0) is not mapped"));
            return Err(Errno::ENXIO);
        };
        self.log(format_args!("VRAM aperture (BAR0): {len:#x} bytes"));
        for (what, at) in [
            ("first words", 0usize),
            ("a line in", 4 * 1920 * 4),
            ("just past 8 MiB", 8 << 20),
            ("the middle of the aperture", len / 2),
        ] {
            let mut words = [0u32; 8];
            for (i, w) in words.iter_mut().enumerate() {
                *w = gpu.vram_read32(at + 4 * i).unwrap_or(u32::MAX);
            }
            self.log(format_args!("  {what} (offset {at:#x}): {words:08x?}"));
        }
        Ok(())
    }

    /// A one-line summary of the cached table, for a sysctl.
    pub(crate) fn summary(&self) -> String {
        match &*self.table.lock() {
            None => "not read".into(),
            Some(d) => {
                let v = |id| {
                    d.ip(id, 0).map_or_else(
                        || "-".into(),
                        |i| format!("{}.{}.{}", i.major, i.minor, i.revision),
                    )
                };
                format!(
                    "GC {} SDMA {} DMU {} PSP {} SMU {} IH {}",
                    v(hw::GC),
                    v(hw::SDMA0),
                    v(hw::DMU),
                    v(hw::MP0),
                    v(hw::MP1),
                    v(hw::OSSSYS),
                )
            }
        }
    }
}

/// The sysctls for `a`, under `node`.
pub(crate) fn publish(t: &mut sysctl::Tree, node: sysctl::NodeId, a: &Arc<Amd>) {
    let r = Arc::clone(a);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "discovered",
        "The IP block versions, once `discover` has run",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.summary()))),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "discover",
        "Write 1 to read the IP discovery table and log it",
        move || r.discover(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "vram",
        "Write 1 to read a few words of VRAM through BAR0 and log them (read-only)",
        move || r.vram_probe(),
    );
    let r = Arc::clone(a);
    crate::control::trigger(
        t,
        node,
        "display",
        "Write 1 to read the display registers and log them (read-only)",
        move || r.display(),
    );
}
