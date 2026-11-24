// SPDX-License-Identifier: BSD-2-Clause
//! `hdacc`: one codec on a controller's link, and `hdaa`: its audio
//! function group, whose widget graph it reads.
//!
//! The devices the graph becomes (`pcmN`, the mixer) wait for the sound
//! engine.
//!
//! Behavior reference: sys/dev/sound/pci/hda/hdacc.c and hdaa.c (15.1).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use hda::{Codec, Function, verb};
use sched::Sleepable;

use crate::controller::Link;
use crate::names;

/// What `hdacc` hands the driver of a function group.
#[derive(Debug)]
pub(crate) struct FunctionGroup {
    link: Arc<Link>,
    nid: u8,
}

/// A codec seen through a [`Link`], for `hda`'s graph reader.
struct Cmd<'a> {
    link: &'a Link,
    s: &'a Sleepable,
}

impl Codec for Cmd<'_> {
    fn command(&mut self, v: u32) -> Option<u32> {
        self.link.command(self.s, v)
    }
}

/// `hdaccN`.
#[derive(Debug)]
pub(crate) struct Hdacc {
    link: Arc<Link>,
    groups: Vec<(u8, u8)>,
}

impl Driver for Hdacc {
    const NAME: &'static str = "hdacc";
    const IDS: Ids = Ids::Nexus(&["hdacc"]);

    fn describe(_: &Ident) -> String {
        "HDA CODEC".into()
    }

    fn describe_with(_: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        match bus_data.and_then(|d| d.downcast_ref::<Link>()) {
            Some(l) => alloc::format!("{} HDA CODEC", names::codec(l.vendor, l.device)),
            None => "HDA CODEC".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let link = cx.bus_data_arc::<Link>().ok_or(AttachError::Decline)?;
        let s = cx.sleepable();
        // The root node's subordinates are the function groups.
        let sub = link
            .command(s, verb::get_parameter(0, 0, verb::PARAM_SUB_NODE_COUNT))
            .filter(|&r| r != verb::INVALID)
            .ok_or(AttachError::Device("the codec does not answer"))?;
        let (start, total) = (
            u16::from(verb::sub_node_start(sub)),
            u16::from(verb::sub_node_total(sub)),
        );
        let mut groups = Vec::new();
        for nid in start..(start + total).min(256) {
            let Ok(nid) = u8::try_from(nid) else { break };
            let kind = link
                .command(s, verb::get_parameter(0, nid, verb::PARAM_FCT_GRP_TYPE))
                .map_or(0, verb::fct_grp_type);
            groups.push((nid, kind));
        }
        Ok(Self { link, groups })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        for &(nid, kind) in &self.groups {
            // Only the audio group has a driver; FreeBSD says so for
            // the others and powers them down.
            if kind != verb::FCT_GRP_AUDIO {
                continue;
            }
            let fg = FunctionGroup {
                link: Arc::clone(&self.link),
                nid,
            };
            let ident = Ident::Channel {
                class: "hdaa",
                channel: u32::from(nid),
            };
            cx.add_child(ident, Some(Arc::new(fg) as _), Vec::<Resource>::new());
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// `hdaaN`.
#[derive(Debug)]
pub(crate) struct Hdaa {
    /// The function group as read, kept for the devices built from it.
    _function: Function,
}

impl Driver for Hdaa {
    const NAME: &'static str = "hdaa";
    const IDS: Ids = Ids::Nexus(&["hdaa"]);

    fn describe(_: &Ident) -> String {
        "Audio Function Group".into()
    }

    fn describe_with(_: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        match bus_data.and_then(|d| d.downcast_ref::<FunctionGroup>()) {
            Some(fg) => alloc::format!(
                "{} Audio Function Group",
                names::codec(fg.link.vendor, fg.link.device)
            ),
            None => "Audio Function Group".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let fg = cx
            .bus_data_arc::<FunctionGroup>()
            .ok_or(AttachError::Decline)?;
        let s = cx.sleepable();
        // Power the group up (`hdaa_powerup`), then read its widgets.
        let _ = fg
            .link
            .command(s, verb::set_power_state(0, fg.nid, verb::POWER_D0));
        let mut cmd = Cmd { link: &fg.link, s };
        let function = hda::read_function(&mut cmd, 0, fg.nid)
            .map_err(|_| AttachError::Device("the function group could not be read"))?;
        Ok(Self {
            _function: function,
        })
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const HDACC: DriverDecl = DriverDecl::of::<Hdacc>();
/// See [`HDACC`].
pub const HDAA: DriverDecl = DriverDecl::of::<Hdaa>();
