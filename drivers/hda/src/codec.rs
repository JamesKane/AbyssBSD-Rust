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

/// What the audio function group keeps: the codec's link and the graph.
#[derive(Debug)]
struct Group {
    link: Arc<Link>,
    function: Function,
}

/// The groups attached, for [`tone_test`].
static GROUPS: sync::SpinLock<Vec<alloc::sync::Weak<Group>>> = sync::SpinLock::new(Vec::new());

/// Plays the test tone for `seconds` on the first audio function group
/// that is attached (the kernel's audio test), `None` if there is none.
pub fn tone_test(s: &Sleepable, seconds: u32) -> Option<crate::tone::Report> {
    let g = GROUPS.lock().iter().find_map(alloc::sync::Weak::upgrade)?;
    let log = |a: core::fmt::Arguments<'_>| bus::manager().log(format_args!("hdaa: {a}"));
    Some(crate::tone::play(s, &g.link, &g.function, seconds, &log))
}

/// `hdaaN`.
#[derive(Debug)]
pub(crate) struct Hdaa {
    group: Arc<Group>,
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
        let group = Arc::new(Group {
            link: Arc::clone(&fg.link),
            function,
        });
        GROUPS.lock().push(Arc::downgrade(&group));
        Ok(Self { group })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let unit = cx.unit();
        let group = Arc::clone(&self.group);
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            let g = Arc::clone(&group);
            let _ = t.add_writable(
                node,
                sysctl::Number::Auto,
                "tone",
                "Write a number of seconds (1 to 20) to play a 1 kHz test tone through the codec's best output pin (on a kernel thread; see the log)",
                0,
                sysctl::Value::Int(alloc::boxed::Box::new(|| 0)),
                sysctl::int_writer(move |seconds| {
                    let g = Arc::clone(&g);
                    let seconds = u32::try_from(seconds).unwrap_or(0);
                    if seconds == 0 {
                        return Ok(());
                    }
                    sched::spawn("hda-tone", sched::Priority::KERNEL, move |s| {
                        let name = alloc::format!("hdaa{unit}");
                        let log = |a: core::fmt::Arguments<'_>| {
                            bus::manager().log(format_args!("{name}: {a}"));
                        };
                        let r = crate::tone::play(s, &g.link, &g.function, seconds, &log);
                        log(format_args!("tone: finished: {r:?}"));
                    })
                    .map(|_| ())
                    .map_err(|_| abi::errno::Errno::EAGAIN)
                }),
            );
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const HDACC: DriverDecl = DriverDecl::of::<Hdacc>();
/// See [`HDACC`].
pub const HDAA: DriverDecl = DriverDecl::of::<Hdaa>();
