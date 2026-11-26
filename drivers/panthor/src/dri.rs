// SPDX-License-Identifier: BSD-2-Clause
//! Where the graphics library finds which device a node belongs to, as FreeBSD's drm-kmod
//! publishes it on this board (`tests/corpus/opi6p/freebsd/sysctl-a.txt`): `hw.dri.<unit>.busid`
//! (`platform:<name>` for a device that is not on PCI) and `.modesetting`; and per node,
//! `dev.drm.<minor>.busid`, which libdrm's FreeBSD platform-device support reads to tell which
//! card and render node are one device's.

use alloc::boxed::Box;
use alloc::format;

/// Publishes `busid` for the card of `unit` (minor `unit`, and `128 + unit` if it renders).
pub(crate) fn publish(
    s: &sched::Sleepable,
    unit: u32,
    render: bool,
    busid: &'static str,
    modesetting: bool,
) {
    bus::manager().sysctl(s, &mut |t| {
        let root = t.root();
        let node = |t: &mut sysctl::Tree, parent, name: &str, desc: &str| {
            t.child(parent, name)
                .or_else(|| t.add_node(parent, sysctl::Number::Auto, name, desc).ok())
        };
        if let Some(dri) =
            node(t, root, "hw", "").and_then(|hw| node(t, hw, "dri", "Direct rendering devices"))
            && let Ok(n) = t.add_node(
                dri,
                sysctl::Number::Auto,
                &format!("{unit}"),
                "A graphics card",
            )
        {
            let _ = t.add_value(
                n,
                sysctl::Number::Auto,
                "busid",
                "The device's bus address",
                sysctl::Value::Str(Box::new(move |out| out.push_str(busid))),
            );
            let m = i32::from(modesetting);
            let _ = t.add_value(
                n,
                sysctl::Number::Auto,
                "modesetting",
                "Whether the card sets display modes",
                sysctl::Value::Int(Box::new(move || m)),
            );
        }
        if let Some(drm) =
            node(t, root, "dev", "").and_then(|dev| node(t, dev, "drm", "Direct rendering nodes"))
        {
            let minors = [Some(unit), render.then_some(128 + unit)];
            for minor in minors.into_iter().flatten() {
                if let Ok(n) = t.add_node(drm, sysctl::Number::Auto, &format!("{minor}"), "A node")
                {
                    let _ = t.add_value(
                        n,
                        sysctl::Number::Auto,
                        "busid",
                        "Its device's bus address",
                        sysctl::Value::Str(Box::new(move |out| out.push_str(busid))),
                    );
                }
            }
        }
    });
}
