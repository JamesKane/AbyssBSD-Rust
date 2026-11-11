// SPDX-License-Identifier: BSD-2-Clause
//! The driver against a model of the RX8900's registers: BCD time, the
//! weekday one-hot, the flag register's voltage flags, the control
//! register's `RESET`.

extern crate std;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

use bus::i2c::{I2c, I2cSource, I2cTarget, Msg, Op};
use bus::provider::ProviderError;

use super::*;

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    #[allow(unsafe_code)]
    unsafe {
        Sleepable::assume()
    }
}

/// A logged transfer: the register, and the bytes written or the length
/// read.
type Seen = (u8, Result<Vec<u8>, usize>);

/// The chip at 0x32: sixteen registers; a write's first byte sets the
/// register pointer, a read reads on from it. Each transfer is logged as
/// (register, written bytes) or (register, read length).
#[derive(Default)]
struct Model {
    regs: StdMutex<([u8; 16], usize)>,
    log: StdMutex<Vec<Seen>>,
    /// Refuse everything, as an absent chip.
    absent: bool,
}

impl I2c for Model {
    fn transfer(&self, _: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        if self.absent || msgs.iter().any(|m| m.address != 0x32) {
            return Err(ProviderError::NoAck);
        }
        let mut r = self.regs.lock().unwrap();
        for m in msgs.iter_mut() {
            match &mut m.op {
                Op::Write(b) => {
                    let Some((&p, rest)) = b.split_first() else {
                        continue;
                    };
                    r.1 = usize::from(p);
                    if !rest.is_empty() {
                        self.log.lock().unwrap().push((p, Ok(rest.to_vec())));
                    }
                    for &v in rest {
                        let i = r.1 % 16;
                        r.0[i] = v;
                        r.1 += 1;
                    }
                }
                Op::Read(b) => {
                    let at = u8::try_from(r.1 % 16).unwrap();
                    self.log.lock().unwrap().push((at, Err(b.len())));
                    for v in b.iter_mut() {
                        *v = r.0[r.1 % 16];
                        r.1 += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn speed(&self) -> u32 {
        400_000
    }
}

fn chip(regs: [u8; 16]) -> (Arc<Model>, Rx8803Clock) {
    let m = Arc::new(Model {
        regs: StdMutex::new((regs, 0)),
        ..Model::default()
    });
    let dev = I2cSource::new(Arc::clone(&m) as Arc<dyn I2c>).device(I2cTarget {
        address: 0x32,
        ten_bit: false,
        speed: 400_000,
    });
    (m, Rx8803Clock::new("rx88030".into(), dev))
}

/// 2026-11-10 12:34:56, a Tuesday, with junk in the bits the driver
/// masks; control as the RX8900 resets it (`CSEL0`).
fn regs() -> [u8; 16] {
    let mut r = [0u8; 16];
    r[..7].copy_from_slice(&[
        0x80 | 0x56,
        0x80 | 0x34,
        0xc0 | 0x12,
        1 << 2,
        0xc0 | 0x10,
        0xe0 | 0x11,
        0x26,
    ]);
    r[usize::from(CTRL)] = 0x40;
    r
}

#[test]
fn reads_the_time() {
    let (m, c) = chip(regs());
    let t = c.gettime(&s()).unwrap();
    assert_eq!(
        (
            t.year,
            t.month,
            t.day,
            t.hour,
            t.minute,
            t.second,
            t.nanosecond
        ),
        (2026, 11, 10, 12, 34, 56, 0)
    );
    // The flags first, then the seven time registers.
    assert_eq!(*m.log.lock().unwrap(), [(FLAGS, Err(1)), (TIME, Err(7))]);
    assert_eq!(weekday(1 << 2), 2);
}

#[test]
fn voltage_low_means_no_time() {
    let mut r = regs();
    r[usize::from(FLAGS)] = FLAGS_V2F;
    let (m, c) = chip(r);
    assert_eq!(c.gettime(&s()), Err(Errno::ENXIO));
    assert_eq!(
        *m.log.lock().unwrap(),
        [(FLAGS, Err(1))],
        "the time is not read"
    );
    // V1F alone (below the temperature compensation's voltage) still
    // keeps time.
    r[usize::from(FLAGS)] = FLAGS_V1F;
    let (_, c) = chip(r);
    assert!(c.gettime(&s()).is_ok());
}

#[test]
fn bad_bcd_and_absent_chips() {
    let mut r = regs();
    r[0] = 0x5a;
    let (_, c) = chip(r);
    assert_eq!(c.gettime(&s()), Err(Errno::EINVAL));
    let mut r = regs();
    r[5] = 0x13;
    let (_, c) = chip(r);
    assert_eq!(c.gettime(&s()), Err(Errno::EINVAL));
    let m = Arc::new(Model {
        absent: true,
        ..Model::default()
    });
    let dev = I2cSource::new(m as Arc<dyn I2c>).device(I2cTarget {
        address: 0x32,
        ten_bit: false,
        speed: 400_000,
    });
    let c = Rx8803Clock::new("rx88030".into(), dev);
    assert_eq!(c.gettime(&s()), Err(Errno::EIO));
    assert_eq!(c.settime(&s(), Timespec::default()), Err(Errno::EIO));
}

#[test]
fn sets_the_time() {
    let mut r = regs();
    r[usize::from(FLAGS)] = FLAGS_V2F | FLAGS_V1F | 0x08;
    let (m, c) = chip(r);
    // 2027-01-01 00:00:09, a Friday, half a second in: the seconds go as
    // they are (FreeBSD's driver would add one to the BCD byte, 0x0a).
    let t = DateTime {
        year: 2027,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 9,
        nanosecond: 600_000_000,
    };
    c.settime(&s(), Timespec::of(&t).unwrap()).unwrap();
    assert_eq!(
        *m.log.lock().unwrap(),
        [
            (CTRL, Err(1)),
            (CTRL, Ok(vec![0x41])),
            (TIME, Ok(vec![0x09, 0x00, 0x00, 1 << 5, 0x01, 0x01, 0x27])),
            (CTRL, Ok(vec![0x40])),
            (FLAGS, Err(1)),
            (FLAGS, Ok(vec![0x08])),
        ]
    );
    m.log.lock().unwrap().clear();
    // It reads back, the voltage flags cleared.
    let back = c.gettime(&s()).unwrap();
    assert_eq!(back, DateTime { nanosecond: 0, ..t });
}

/// Registered with the framework: read half a second on (its resolution
/// is a second, and it truncates), and found by name.
#[test]
fn registers_as_a_one_second_clock() {
    let (_, c) = chip(regs());
    let c: Arc<dyn RealTimeClock> = Arc::new(c);
    clock::register(Arc::clone(&c), 1_000_000, 0);
    clock::schedule(&c, 1);
    assert!(clock::registered().contains(&("rx88030".into(), 1_000_000)));
    let (name, ts) = clock::read(&s()).unwrap();
    assert_eq!(name, "rx88030");
    assert_eq!(ts.nsec, 500_000_000);
    assert_eq!(
        ts.to_datetime()
            .map(|d| (d.year, d.month, d.day, d.hour, d.minute, d.second)),
        Some((2026, 11, 10, 12, 34, 56))
    );
    clock::unregister(&s(), &c);
    assert!(clock::registered().is_empty());
}
