// SPDX-License-Identifier: BSD-2-Clause
//! The Sky1's I2C devices on a model bus: the RX8900 (`rx8803`) and an
//! RTS5453 (`rts5453`), as firmware describes them on the board
//! (`RX008900` at 0x32, `CIXH200D` at 0x30), against register models of
//! both chips, attached by the real drivers of the aarch64 table.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use bus::i2c::{I2c, I2cTarget, Msg, Op};
use bus::provider::ProviderError;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Line, LineDetail, LineKind, StartCx,
    StopCx, StopReason, TestId, TestIdent,
};
use devtree::matching::Rank;
use ktest::ktest;

use super::cx;

/// Where the devices are in the model namespace.
const RTC_PATH: &str = "\\_SB_.TI2C.RTC0";
const PD_PATH: &str = "\\_SB_.TI2C.PD00";

/// The two chips' registers: the RX8900's sixteen at 0x32 (a write's
/// first byte sets the pointer), and the RTS5453's status at 0x5f and
/// acknowledgements at 0x50 at 0x30.
struct Chips(sync::SpinLock<Regs>);

struct Regs {
    rtc: [u8; 16],
    ptr: usize,
    pd_status: [u8; 6],
    pd_acks: usize,
}

static CHIPS: Chips = Chips(sync::SpinLock::new(Regs {
    // 2001-01-02 03:04:05, a Tuesday; control as the chip resets it.
    rtc: [
        0x05,
        0x04,
        0x03,
        1 << 2,
        0x02,
        0x01,
        0x01,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0x40,
    ],
    ptr: 0,
    // A USB 3 partner, reversed, giving the board power: unit 0 on the
    // board.
    pd_status: [5, 0x73, 0, 0x02, 0, 0],
    pd_acks: 0,
}));

struct Bus;

impl I2c for Bus {
    fn transfer(&self, _: &sched::Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        let mut r = CHIPS.0.lock();
        match msgs.first().map(|m| m.address) {
            Some(0x32) => {
                for m in msgs.iter_mut() {
                    match &mut m.op {
                        Op::Write(b) => {
                            if let Some((&p, rest)) = b.split_first() {
                                r.ptr = usize::from(p);
                                for &v in rest {
                                    let i = r.ptr % 16;
                                    r.rtc[i] = v;
                                    r.ptr += 1;
                                }
                            }
                        }
                        Op::Read(b) => {
                            for v in b.iter_mut() {
                                *v = r.rtc[r.ptr % 16];
                                r.ptr += 1;
                            }
                        }
                    }
                }
                Ok(())
            }
            Some(0x30) => match msgs {
                [
                    Msg {
                        op: Op::Write(c), ..
                    },
                    Msg {
                        op: Op::Read(b), ..
                    },
                ] if **c == [0x5f] => {
                    b.copy_from_slice(&r.pd_status);
                    Ok(())
                }
                [
                    Msg {
                        op: Op::Write(b), ..
                    },
                ] if b.first() == Some(&0x50) => {
                    r.pd_acks += 1;
                    Ok(())
                }
                _ => Err(ProviderError::Unsupported),
            },
            _ => Err(ProviderError::NoAck),
        }
    }

    fn speed(&self) -> u32 {
        400_000
    }
}

fn acpi_ident(hid: &str, uid: &str) -> devtree::ids::AcpiIdent {
    devtree::ids::AcpiIdent {
        hid: Some(hid.into()),
        uid: Some(uid.into()),
        ..devtree::ids::AcpiIdent::default()
    }
}

/// A bus standing in for `acpi0`: the controller (model 41), and the two
/// devices whose `I2cSerialBusV2` names it.
pub(crate) struct SkyBus;

impl Driver for SkyBus {
    const NAME: &'static str = "skyi2cbus";
    const IDS: Ids = Ids::Test(&[TestId {
        model: 40,
        rank: Rank::DEFAULT,
    }]);
    fn describe(_: &Ident) -> String {
        "Sky1 I2C test firmware bus".into()
    }
    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }
    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        use devtree::graph::{EdgeKind::Required, Origin::ResourceSource};
        let ctl = cx.add_child(Ident::Test(TestIdent { model: 41 }), None, vec![]);
        for (hid, uid, path, addr) in [
            ("RX008900", "0", RTC_PATH, 0x32u16),
            ("CIXH200D", "1", PD_PATH, 0x30),
        ] {
            let dev = cx.add_child(Ident::Acpi(acpi_ident(hid, uid), path.into()), None, vec![]);
            cx.add_dependency(dev, ctl, Required, ResourceSource);
            cx.set_lines(
                dev,
                vec![Line {
                    kind: LineKind::I2c,
                    name: None,
                    supplier: ctl,
                    id: u32::from(addr),
                    detail: LineDetail::I2c(I2cTarget {
                        address: addr,
                        ten_bit: false,
                        speed: 400_000,
                    }),
                }],
            );
        }
        Ok(())
    }
    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The controller: the model bus, and a taker standing in for
/// `acpi_iicbus` (no namespace to walk here), which moves both devices
/// under it as I2C devices at their addresses.
pub(crate) struct SkyCtl;

impl Driver for SkyCtl {
    const NAME: &'static str = "skyi2cctl";
    const IDS: Ids = Ids::Test(&[TestId {
        model: 41,
        rank: Rank::DEFAULT,
    }]);
    fn describe(_: &Ident) -> String {
        "Sky1 I2C test controller".into()
    }
    fn attach(_: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        Ok(Self)
    }
    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_i2c(Arc::new(Bus));
        for (path, addr) in [(RTC_PATH, 0x32u16), (PD_PATH, 0x30)] {
            let make = move |old: &Ident| match old {
                Ident::Acpi(i, p) => Some(Ident::Iic(bus::IicIdent {
                    addr,
                    acpi: Some((i.clone(), p.clone())),
                })),
                _ => None,
            };
            cx.take_child(path, "skyi2cbus", &make);
        }
        Ok(())
    }
    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

pub(crate) const SKY_BUS: DriverDecl = DriverDecl::of::<SkyBus>();
pub(crate) const SKY_CTL: DriverDecl = DriverDecl::of::<SkyCtl>();

/// A node under `root` whose name starts with `class` and a digit.
fn find_class(
    m: &bus::Manager,
    s: &sched::Sleepable,
    root: bus::DeviceId,
    class: &str,
) -> Option<(bus::DeviceId, String)> {
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        for c in m.children(p, s) {
            if let Some(n) = m.nameunit(c, s)
                && n.strip_prefix(class)
                    .is_some_and(|u| u.starts_with(|ch: char| ch.is_ascii_digit()))
            {
                return Some((c, n));
            }
            todo.push(c);
        }
    }
    None
}

ktest! {
    /// Both drivers attach to their models as FreeBSD's do to the chips;
    /// the RX8900 registers as a one-second clock after the EFI clock,
    /// reads its time, and takes the system time when the clocks are
    /// written; the RTS5453 reads its status into its sysctls; detach
    /// unregisters the clock and stops the poll.
    fn rx8900_and_rts5453_on_an_i2c_bus() {
        let s = cx();
        let m = bus::manager();
        let tb = m.add_child(&s, m.root(), Ident::Test(TestIdent { model: 40 }), None, vec![], &[]);
        m.wait_idle(&s);
        let (rtc, rtc_name) = find_class(m, &s, tb, "rx8803").expect("rx8803 attached");
        assert_eq!(m.state(rtc, &s), Some(bus::NodeState::Attached));
        assert_eq!(m.description(rtc, &s).as_deref(), Some("Epson RX8900 Real Time Clock"));
        let ident = m.ident(rtc, &s).expect("ident");
        let (mut loc, mut pnp) = (String::new(), String::new());
        ident.location(&mut loc);
        ident.pnpinfo(&mut pnp);
        assert_eq!(loc, alloc::format!("addr=0x32 handle={RTC_PATH}"));
        assert_eq!(pnp, "_HID=RX008900 _UID=0 _CID=none");
        // Registered after the EFI clock, at the same resolution.
        let clocks = clock::registered();
        kprintln!("clocks: {clocks:?}");
        let at = clocks.iter().position(|c| c.0 == rtc_name).expect("registered");
        assert_eq!(clocks[at].1, 1_000_000);
        if let Some(efi) = clocks.iter().position(|c| c.0.starts_with("efirtc")) {
            assert!(efi < at, "{clocks:?}");
        }
        let read: Vec<_> = clock::read_all(&s).into_iter().filter(|c| c.0 == rtc_name).collect();
        let t = read[0].1.expect("the model's time");
        assert_eq!((t.year, t.month, t.day, t.hour, t.minute, t.second), (2001, 1, 2, 3, 4, 5));
        // Writing the clocks sets the model to the system time, one
        // nanosecond into a second, and starts it again.
        clock::resettodr();
        let start = crate::time::now_ns();
        while CHIPS.0.lock().rtc[6] == 0x01 && crate::time::now_ns() - start < 3_000_000_000 {
            crate::time::sleep_ns(&s, 10_000_000);
        }
        clock::drain(&s, 2_000_000_000);
        let now = crate::time::realtime_ns().expect("time of day") / 1_000_000_000;
        let read: Vec<_> = clock::read_all(&s).into_iter().filter(|c| c.0 == rtc_name).collect();
        let secs = read[0].1.expect("the model's new time").unix_seconds().expect("valid");
        assert!(secs.abs_diff(now) <= 2, "the model reads {secs}, the system {now}");
        assert_eq!(CHIPS.0.lock().rtc[15] & 1, 0, "the clock runs again");
        // The PD controller: its status in the sysctls, and the board's
        // line logged at the first poll.
        let (pd, pd_name) = find_class(m, &s, tb, "rts5453").expect("rts5453 attached");
        assert_eq!(
            m.description(pd, &s).as_deref(),
            Some("Realtek RTS5453 USB Type-C PD controller")
        );
        let unit = pd_name.trim_start_matches("rts5453");
        let leaf = |l: &str| super::dev_text(&s, &alloc::format!("dev.rts5453.{unit}.{l}"));
        let start = crate::time::now_ns();
        while leaf("status").as_deref() == Some("unknown")
            && crate::time::now_ns() - start < 2_000_000_000
        {
            crate::time::sleep_ns(&s, 10_000_000);
        }
        assert_eq!(leaf("status").as_deref(), Some("73 00 02 00 00"));
        assert_eq!(leaf("mode").as_deref(), Some("USB"));
        assert_eq!(leaf("orientation").as_deref(), Some("reverse"));
        assert_eq!(leaf("data_role").as_deref(), Some("host"));
        assert_eq!(leaf("power_role").as_deref(), Some("sink"));
        assert_eq!(leaf("hpd").as_deref(), Some("none"));
        // A change is acknowledged at the next poll.
        CHIPS.0.lock().pd_status = [5, 0, 0, 0, 0, 0];
        let start = crate::time::now_ns();
        while CHIPS.0.lock().pd_acks == 0 && crate::time::now_ns() - start < 3_000_000_000 {
            crate::time::sleep_ns(&s, 10_000_000);
        }
        assert_eq!(CHIPS.0.lock().pd_acks, 1);
        assert_eq!(leaf("mode").as_deref(), Some("none"));
        // Detach: the clock is gone from the list, the poll thread joined.
        m.shutdown_under(tb, &s);
        assert!(!clock::registered().iter().any(|c| c.0 == rtc_name));
        assert_eq!(m.state(pd, &s), None);
        let _ = m.take_events();
    }
}
