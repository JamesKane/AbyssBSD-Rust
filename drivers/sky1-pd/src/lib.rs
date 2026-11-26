// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_pd`: the CIX Sky1's SCMI power domains, served by Trusted Firmware over SMC (Sky1 SoC
//! specification 01 §1.3). There is no ACPI device for the agent: the platform description
//! (`platform/cix-sky1`) names it, with its 128-byte shared memory in memory the UEFI map
//! reserves, and `acpi0` adds it as a child. Consumers the platform description gives a line
//! to (the GPU, domain 21) vote on a domain through `AttachCx::power_domain`.
//!
//! The driver issues no firmware call itself (DRIVER-MODEL.md §8.7): the kernel gives it a
//! [`Conduit`] that makes the one call it is granted, SiP function `0xC2000001`.
//!
//! At start it reports the protocol version, the number of domains, and the state of the GPU's
//! domain and of each display processor's, which the firmware leaves on for the display it
//! shows: a domain reading on there shows the channel works before anything is switched.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod scmi;
#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use bus::provider::{PerfLevel, PowerDomain, ProviderError};
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx};
use regs::Bytes;
use sched::Sleepable;
use sync::{Once, SpinLock};

use crate::scmi::{Channel, Doorbell, Shmem};

/// The firmware call the kernel makes for the driver: SiP function `0xC2000001` with the
/// shared memory's page number and offset (Sky1 SoC specification 01 §1.3.2), returning `x0`.
pub trait Conduit: Sync {
    fn scmi_smc(&self, page: u64, offset: u64) -> i64;
}

static CONDUIT: Once<&'static dyn Conduit> = Once::new();

/// Gives the driver the kernel's conduit, once, before drivers start.
pub fn set_conduit(c: &'static dyn Conduit) {
    let _ = CONDUIT.set(c);
}

/// The SMC agent's domains the start report reads: the GPU, and the five display processors.
const REPORTED: &[(u32, &str)] = &[
    (21, "gpu"),
    (6, "dpu0"),
    (7, "dpu1"),
    (8, "dpu2"),
    (9, "dpu3"),
    (10, "dpu4"),
];

struct Window(MmioGrant<Bytes>);

impl Shmem for &Window {
    fn read32(&self, off: usize) -> u32 {
        self.0.read32(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write32(off, v);
    }
}

struct Ring {
    conduit: &'static dyn Conduit,
    phys: u64,
}

impl Doorbell for &Ring {
    fn ring(&self) -> i64 {
        self.conduit.scmi_smc(self.phys >> 12, self.phys & 0xFFF)
    }
}

struct Agent {
    window: Window,
    ring: Ring,
    /// One message at a time; holds the next token.
    token: SpinLock<u16>,
    domains: u32,
    revoked: AtomicBool,
}

impl Agent {
    fn with<T>(&self, f: impl FnOnce(&mut Channel<&Window, &Ring>) -> T) -> T {
        let mut token = self.token.lock();
        let mut ch = Channel::new(&self.window, &self.ring);
        ch.set_token(*token);
        let out = f(&mut ch);
        *token = ch.token();
        out
    }
}

impl PowerDomain for Agent {
    fn levels(&self, id: u32) -> Result<u32, ProviderError> {
        if id >= self.domains {
            return Err(ProviderError::NoSuchLine);
        }
        Ok(2)
    }

    fn set_level(&self, _: &Sleepable, id: u32, level: PerfLevel) -> Result<(), ProviderError> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(ProviderError::NoSuchLine);
        }
        if id >= self.domains {
            return Err(ProviderError::NoSuchLine);
        }
        let state = match level {
            PerfLevel::OFF => scmi::STATE_OFF,
            PerfLevel::ON => scmi::STATE_ON,
            _ => return Err(ProviderError::OutOfRange),
        };
        self.with(|ch| ch.set_state(id, state)).map_err(|e| {
            bus::manager().log(format_args!("sky1_pd0: domain {id} to {state:#x}: {e:?}"));
            match e {
                scmi::Error::Status(code) => ProviderError::Firmware(code),
                _ => ProviderError::Firmware(-1),
            }
        })
    }
}

/// A `sky1_pdN` instance.
pub struct Sky1Pd {
    agent: Arc<Agent>,
}

impl core::fmt::Debug for Sky1Pd {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sky1Pd")
            .field("domains", &self.agent.domains)
            .finish_non_exhaustive()
    }
}

fn state_name(s: u32) -> &'static str {
    match s {
        scmi::STATE_ON => "on",
        scmi::STATE_OFF => "off",
        _ => "other",
    }
}

impl Driver for Sky1Pd {
    const NAME: &'static str = "sky1_pd";
    const IDS: Ids = Ids::Nexus(&["sky1_pd"]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 SCMI power domains (Trusted Firmware)".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let conduit = *CONDUIT
            .get()
            .ok_or(AttachError::Device("no firmware conduit"))?;
        let window = cx.mmio_bytes(0)?;
        if window.len() < 128 {
            return Err(AttachError::Device("shared memory smaller than 128 bytes"));
        }
        let phys = window.phys();
        let unit = cx.unit();
        let mut agent = Agent {
            window: Window(window),
            ring: Ring { conduit, phys },
            token: SpinLock::new(0),
            domains: 0,
            revoked: AtomicBool::new(false),
        };
        let log = |args: core::fmt::Arguments<'_>| {
            bus::manager().log(format_args!("sky1_pd{unit}: {args}"));
        };
        let (version, domains) = agent
            .with(|ch| Ok::<_, scmi::Error>((ch.version()?, ch.domains()?)))
            .map_err(|e| {
                log(format_args!("the firmware does not answer: {e:?}"));
                AttachError::Device("SCMI power-domain protocol does not answer")
            })?;
        agent.domains = domains;
        log(format_args!(
            "SCMI power domain protocol version {version:#x}, {domains} domains"
        ));
        let mut states = String::new();
        for &(id, name) in REPORTED {
            let s = agent.with(|ch| ch.state(id));
            let text = match s {
                Ok(s) => state_name(s),
                Err(_) => "unreadable",
            };
            if !states.is_empty() {
                states.push_str(", ");
            }
            states.push_str(name);
            states.push(' ');
            states.push_str(text);
        }
        log(format_args!("{states}"));
        Ok(Self {
            agent: Arc::new(agent),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_power_domains(Arc::clone(&self.agent) as Arc<dyn PowerDomain>);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: bus::StopReason) {
        self.agent.revoked.store(true, Ordering::Release);
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Pd>();
