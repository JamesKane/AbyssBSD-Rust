// SPDX-License-Identifier: BSD-2-Clause
//! `iic`'s requests over a fake EEPROM and a fake user address space.

extern crate std;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use std::sync::Mutex as StdMutex;

use abi::iic::{IIC_M_WR, IicMsg};
use bus::i2c::I2c;

use super::*;

/// A 256-byte EEPROM at 0x50: a write sets the pointer (its first byte)
/// and stores the rest; a read reads on from the pointer. Every transfer
/// is logged as its messages' (address, read, length).
#[derive(Default)]
struct Eeprom {
    mem: StdMutex<(Vec<u8>, usize)>,
    log: StdMutex<Vec<Vec<(u16, bool, usize)>>>,
}

impl I2c for Eeprom {
    fn transfer(&self, _: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        let mut m = self.mem.lock().unwrap();
        if m.0.is_empty() {
            m.0 = (0..=255u8).collect();
        }
        self.log.lock().unwrap().push(
            msgs.iter()
                .map(|x| (x.address, matches!(x.op, Op::Read(_)), x.op.len()))
                .collect(),
        );
        for x in msgs.iter_mut() {
            if x.address != 0x50 {
                return Err(ProviderError::NoAck);
            }
            match &mut x.op {
                Op::Write(b) => {
                    if let Some((&p, rest)) = b.split_first() {
                        m.1 = usize::from(p);
                        for &v in rest {
                            let i = m.1 % 256;
                            m.0[i] = v;
                            m.1 += 1;
                        }
                    }
                }
                Op::Read(b) => {
                    for v in b.iter_mut() {
                        *v = m.0[m.1 % 256];
                        m.1 += 1;
                    }
                }
            }
        }
        Ok(())
    }
    fn speed(&self) -> u32 {
        100_000
    }
}

/// User memory: one region from `BASE`.
struct Mem(StdMutex<Vec<u8>>);

const BASE: u64 = 0x10_0000;

impl UserMemory for Mem {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        let m = self.0.lock().unwrap();
        let at = usize::try_from(src.checked_sub(BASE).ok_or(Errno::EFAULT)?).unwrap();
        dst.copy_from_slice(m.get(at..at + dst.len()).ok_or(Errno::EFAULT)?);
        Ok(())
    }
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        let mut m = self.0.lock().unwrap();
        let at = usize::try_from(dst.checked_sub(BASE).ok_or(Errno::EFAULT)?).unwrap();
        m.get_mut(at..at + src.len())
            .ok_or(Errno::EFAULT)?
            .copy_from_slice(src);
        Ok(())
    }
}

impl Mem {
    fn new() -> Self {
        Self(StdMutex::new(vec![0; 4096]))
    }
    fn put(&self, at: u64, b: &[u8]) {
        self.copy_out(BASE + at, b).unwrap();
    }
    fn get(&self, at: u64, n: usize) -> Vec<u8> {
        let mut b = vec![0; n];
        self.copy_in(&mut b, BASE + at).unwrap();
        b
    }
    /// Places `msgs` at 0 and returns the `I2CRDWR` argument for them.
    fn msgs(&self, msgs: &[IicMsg]) -> Vec<u8> {
        for (i, m) in msgs.iter().enumerate() {
            self.put(16 * i as u64, &m.to_bytes());
        }
        let mut arg = vec![0u8; 16];
        arg[0..8].copy_from_slice(&BASE.to_ne_bytes());
        arg[8..12].copy_from_slice(&u32::try_from(msgs.len()).unwrap().to_ne_bytes());
        arg
    }
}

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    #[allow(unsafe_code)]
    unsafe {
        Sleepable::assume()
    }
}

fn msg(slave: u16, flags: u16, len: u16, buf: u64) -> IicMsg {
    IicMsg {
        slave,
        flags,
        len,
        buf: BASE + buf,
    }
}

fn setup() -> (Arc<Eeprom>, Arc<I2cSource>, Mem) {
    let e = Arc::new(Eeprom::default());
    let src = I2cSource::new(e.clone());
    (e, src, Mem::new())
}

#[test]
fn plans_follow_the_flags() {
    let w = |a: u16, read: bool, parts: Vec<usize>| Wire {
        address: a,
        read,
        parts,
    };
    // i2c(8)'s write with an offset: one message on the wire.
    let p = plan(&[
        msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0),
        msg(0xa0, IIC_M_WR | IIC_M_NOSTART, 4, 0),
    ])
    .unwrap();
    assert_eq!(p, vec![vec![w(0x50, false, vec![0, 1])]]);
    // Its read with an offset: a repeated START.
    let p = plan(&[
        msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0),
        msg(0xa0, IIC_M_RD, 4, 0),
    ])
    .unwrap();
    assert_eq!(
        p,
        vec![vec![w(0x50, false, vec![0]), w(0x50, true, vec![1])]]
    );
    // Without NOSTOP: a STOP between, two transfers.
    let p = plan(&[msg(0xa0, IIC_M_WR, 1, 0), msg(0x64, IIC_M_RD, 1, 0)]).unwrap();
    assert_eq!(
        p,
        vec![vec![w(0x50, false, vec![0])], vec![w(0x32, true, vec![1])]]
    );
    // A last NOSTOP is a STOP all the same.
    let p = plan(&[msg(0xa0, IIC_M_RD | IIC_M_NOSTOP, 1, 0)]).unwrap();
    assert_eq!(p, vec![vec![w(0x50, true, vec![0])]]);
    // NOSTART with nothing held, or turning around: not supported.
    assert_eq!(
        plan(&[msg(0xa0, IIC_M_NOSTART, 1, 0)]),
        Err(IicStatus::NotSupported)
    );
    assert_eq!(
        plan(&[msg(0xa0, IIC_M_WR, 1, 0), msg(0xa0, IIC_M_NOSTART, 1, 0)]),
        Err(IicStatus::NotSupported)
    );
    assert_eq!(
        plan(&[
            msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0),
            msg(0xa0, IIC_M_RD | IIC_M_NOSTART, 1, 0)
        ]),
        Err(IicStatus::NotSupported)
    );
    assert_eq!(plan(&[]), Ok(vec![]));
}

#[test]
fn rdwr_reads_and_writes_as_i2c_does() {
    let s = s();
    let (e, src, mem) = setup();
    let mut st = OpenState::default();
    // i2c -a 0x50 -d w -o 0x10 -c 3 (transfer mode).
    mem.put(0x200, &[0x10]);
    mem.put(0x300, &[0xaa, 0xbb, 0xcc]);
    let arg = mem.msgs(&[
        msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0x200),
        msg(0xa0, IIC_M_WR | IIC_M_NOSTART, 3, 0x300),
    ]);
    ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false).unwrap();
    // i2c -a 0x50 -d r -o 0x0f -c 5.
    mem.put(0x200, &[0x0f]);
    let arg = mem.msgs(&[
        msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0x200),
        msg(0xa0, IIC_M_RD, 5, 0x400),
    ]);
    ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false).unwrap();
    assert_eq!(mem.get(0x400, 5), [0x0f, 0xaa, 0xbb, 0xcc, 0x13]);
    assert_eq!(
        *e.log.lock().unwrap(),
        vec![
            vec![(0x50, false, 4)],
            vec![(0x50, false, 1), (0x50, true, 5)]
        ]
    );
    // Read continued with NOSTART: one read on the wire, split back.
    mem.put(0x200, &[0x10]);
    let arg = mem.msgs(&[
        msg(0xa0, IIC_M_WR | IIC_M_NOSTOP, 1, 0x200),
        msg(0xa0, IIC_M_RD | IIC_M_NOSTOP, 2, 0x500),
        msg(0xa0, IIC_M_RD | IIC_M_NOSTART, 1, 0x600),
    ]);
    ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false).unwrap();
    assert_eq!(mem.get(0x500, 2), [0xaa, 0xbb]);
    assert_eq!(mem.get(0x600, 1), [0xcc]);
    assert_eq!(
        e.log.lock().unwrap().last().unwrap(),
        &vec![(0x50, false, 1), (0x50, true, 3)]
    );
}

#[test]
fn rdwr_errors_are_freebsds() {
    let s = s();
    let (_, src, mem) = setup();
    let mut st = OpenState::default();
    // Nothing at 0x51: IIC_ENOACK, as is (2, ENOENT); nothing copied out.
    mem.put(0x400, &[0xee]);
    let arg = mem.msgs(&[msg(0xa2, IIC_M_RD, 1, 0x400)]);
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::ENOENT)
    );
    assert_eq!(mem.get(0x400, 1), [0xee]);
    // Too many messages.
    let mut arg = mem.msgs(&[]);
    arg[8..12].copy_from_slice(&43u32.to_ne_bytes());
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::EINVAL)
    );
    // Messages, or a write's data, outside the caller's memory.
    let mut arg = mem.msgs(&[msg(0xa0, IIC_M_RD, 1, 0)]);
    arg[0..8].copy_from_slice(&8u64.to_ne_bytes());
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::EFAULT)
    );
    let arg = mem.msgs(&[msg(0xa0, IIC_M_WR, 16, 4090)]);
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::EFAULT)
    );
    // A read whose buffer cannot be written back.
    let arg = mem.msgs(&[msg(0xa0, IIC_M_RD, 16, 4090)]);
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::EFAULT)
    );
    // A NOSTART that cannot be honored: IIC_ENOTSUPP (8, ENOEXEC).
    let arg = mem.msgs(&[msg(0xa0, IIC_M_NOSTART, 1, 0x200)]);
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::ENOEXEC)
    );
    // The bus held elsewhere: O_NONBLOCK gets IIC_EBUSBSY (4, EINTR).
    let held = src.acquire(&s).unwrap();
    let arg = mem.msgs(&[msg(0xa0, IIC_M_RD, 1, 0x400)]);
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, true),
        Err(Errno::EINTR)
    );
    drop(held);
    ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, true).unwrap();
    // The controller gone.
    src.revoke();
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRDWR, &arg, false),
        Err(Errno::ENXIO)
    );
}

#[test]
fn start_mode_requests_are_unsupported() {
    let s = s();
    let (e, src, mem) = setup();
    let mut st = OpenState::default();
    let cmd = [0u8; 24];
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CSTART, &cmd, false),
        Err(Errno::ENODEV)
    );
    assert_eq!(ioctl(&mut st, &src, &s, &mem, I2CSTOP, &[], false), Ok(()));
    for req in [I2CWRITE, I2CREAD, I2CRPTSTART] {
        assert_eq!(
            ioctl(&mut st, &src, &s, &mem, req, &cmd, false),
            Err(Errno::EINVAL)
        );
    }
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, I2CRSTCARD, &cmd, false),
        Ok(())
    );
    assert_eq!(
        ioctl(&mut st, &src, &s, &mem, 0x2000_69ff, &[], false),
        Err(Errno::ENOTTY)
    );
    // read(2) and write(2): no address yet, then no START.
    assert_eq!(uio(&st, &src, &s, false), Err(Errno::ENXIO));
    ioctl(&mut st, &src, &s, &mem, I2CSADDR, &[0xa0], false).unwrap();
    assert_eq!(st.addr, 0xa0);
    assert_eq!(uio(&st, &src, &s, false), Err(Errno::ENODEV));
    assert!(e.log.lock().unwrap().is_empty(), "nothing reached the bus");
}
