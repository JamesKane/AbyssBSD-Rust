// SPDX-License-Identifier: BSD-2-Clause
//! The status decoding against what FreeBSD recorded on the Orange Pi 6
//! Plus, and the poll against a model of the chip's two registers.

extern crate std;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

use bus::i2c::{I2c, I2cSource, I2cTarget, Msg, Op};
use bus::provider::ProviderError;

use super::*;

const SYSCTL: &str = include_str!("../../../tests/corpus/opi6p/freebsd/sysctl-a.txt");

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    #[allow(unsafe_code)]
    unsafe {
        Sleepable::assume()
    }
}

/// Bytes 1 to 5 as `status` prints them, with the length byte first.
fn parse(raw: &str) -> Status {
    let mut st = [5u8; STATUS_LEN];
    for (i, b) in raw.split(' ').enumerate() {
        st[i + 1] = u8::from_str_radix(b, 16).unwrap();
    }
    Status(st)
}

/// Each unit's decoded fields, as FreeBSD's `sysctl -a` has them, from its
/// `status`.
#[test]
fn decodes_as_freebsd_recorded() {
    for unit in [0, 1] {
        let get = |leaf: &str| {
            let key = alloc::format!("dev.rts5453.{unit}.{leaf}: ");
            SYSCTL
                .lines()
                .find_map(|l| l.strip_prefix(key.as_str()))
                .unwrap_or_else(|| panic!("{key}"))
        };
        let st = parse(get("status"));
        assert_eq!(st.raw(), get("status"));
        assert_eq!(st.mode(), get("mode"), "unit {unit}");
        assert_eq!(st.orientation(), get("orientation"), "unit {unit}");
        assert_eq!(st.data_role(), get("data_role"), "unit {unit}");
        assert_eq!(st.power_role(), get("power_role"), "unit {unit}");
        assert_eq!(st.hpd(), get("hpd"), "unit {unit}");
        assert_eq!(get("poll_ms"), "1000");
    }
    // The lines FreeBSD's dmesg has for them.
    assert_eq!(
        parse("73 00 02 00 00").line(),
        "USB, reverse, data host, power sink"
    );
    assert_eq!(
        parse("00 00 00 00 00").line(),
        "none, none, data none, power none"
    );
    // DisplayPort: USB 3 and DP is pin assignment D, DP alone 4 lanes.
    let dp = parse("20 81 01 00 00");
    assert_eq!(
        (dp.mode(), dp.orientation(), dp.hpd(), dp.power_role()),
        ("USB+DP", "normal", "high", "source")
    );
    assert_eq!(
        dp.line(),
        "USB+DP, normal, data host, power source, HPD high"
    );
    assert_eq!(
        parse("00 01 00 00 00").line(),
        "DP, normal, data none, power none, HPD low"
    );
    assert_eq!(parse("90 10 00 00 00").mode(), "debug accessory");
    assert_eq!(parse("90 00 00 00 00").data_role(), "device");
}

/// The chip: a status (with its length byte) at 0x5f, and the
/// acknowledgements written to 0x50 logged.
#[derive(Default)]
struct Chip {
    status: StdMutex<[u8; STATUS_LEN]>,
    acks: StdMutex<Vec<Vec<u8>>>,
    fail: AtomicBool,
}

impl I2c for Chip {
    fn transfer(&self, _: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        if self.fail.load(Ordering::Relaxed) || msgs.iter().any(|m| m.address != 0x30) {
            return Err(ProviderError::NoAck);
        }
        match msgs {
            [
                Msg {
                    op: Op::Write(cmd), ..
                },
                Msg {
                    op: Op::Read(buf), ..
                },
            ] if **cmd == [DATA_STATUS] => {
                buf.copy_from_slice(&*self.status.lock().unwrap());
                Ok(())
            }
            [
                Msg {
                    op: Op::Write(b), ..
                },
            ] if b.first() == Some(&DATA_CTRL) => {
                self.acks.lock().unwrap().push(b[1..].to_vec());
                Ok(())
            }
            _ => Err(ProviderError::Unsupported),
        }
    }

    fn speed(&self) -> u32 {
        100_000
    }
}

static LOG: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

fn log(args: core::fmt::Arguments<'_>) {
    LOG.lock().unwrap().push(alloc::format!("{args}"));
}

#[test]
fn polls_log_changes_and_acknowledge_them() {
    let chip = Arc::new(Chip::default());
    *chip.status.lock().unwrap() = [5, 0, 0, 0, 0, 0];
    let dev = I2cSource::new(Arc::clone(&chip) as Arc<dyn I2c>).device(I2cTarget {
        address: 0x30,
        ten_bit: false,
        speed: 100_000,
    });
    let sc = Softc::new("rts54530".into(), dev, log);
    let s = s();
    assert_eq!(sc.field(Status::raw), "unknown", "before the first poll");
    // The first poll logs, and acknowledges nothing.
    sc.poll(&s);
    assert_eq!(sc.field(|s| s.mode().into()), "none");
    assert!(chip.acks.lock().unwrap().is_empty());
    // Nothing changed: nothing logged.
    sc.poll(&s);
    // A USB 3 partner, plugged reversed, giving power.
    *chip.status.lock().unwrap() = [5, 0x73, 0, 0x02, 0, 0];
    sc.poll(&s);
    assert_eq!(sc.field(Status::raw), "73 00 02 00 00");
    assert_eq!(*chip.acks.lock().unwrap(), [vec![2, INT_ACK, 0]]);
    // A change in the length byte alone is no change.
    *chip.status.lock().unwrap() = [6, 0x73, 0, 0x02, 0, 0];
    sc.poll(&s);
    // A DP sink's HPD IRQ is acknowledged with the interrupt.
    *chip.status.lock().unwrap() = [5, 0x20, 0x01 | HPD_STATE | HPD_IRQ, 0x01, 0, 0];
    sc.poll(&s);
    assert_eq!(chip.acks.lock().unwrap()[1], [2, INT_ACK, HPD_IRQ_ACK]);
    // A failed read is logged once, and the last status kept.
    chip.fail.store(true, Ordering::Relaxed);
    sc.poll(&s);
    sc.poll(&s);
    assert_eq!(sc.field(|s| s.hpd().into()), "high");
    chip.fail.store(false, Ordering::Relaxed);
    sc.poll(&s);
    let lines: Vec<String> = core::mem::take(&mut *LOG.lock().unwrap());
    assert_eq!(
        lines,
        [
            "rts54530: none, none, data none, power none",
            "rts54530: USB, reverse, data host, power sink",
            "rts54530: USB+DP, normal, data host, power source, HPD high",
            // `IIC_ENOACK`.
            "rts54530: cannot read its status: 2",
        ]
    );
    assert_eq!(chip.acks.lock().unwrap().len(), 2);
}
