// SPDX-License-Identifier: BSD-2-Clause
//! The `GenericSerialBus` handler against a model controller: the Radxa
//! Q8B's real-time clock, which its AML reads over `IC13` (the only
//! serial-bus region in the corpus a bus driver serves; the Orange Pi 6
//! Plus has none), and each access attribute through hand-built fields.

extern crate std;

use alloc::string::String;
use alloc::vec;
use std::sync::Mutex as StdMutex;

use acpi::opregion::RegionHandler;
use aml::asm::{cat, device, method, name, pkg, ret, store, table};
use aml::{GpioAccess, Host, Interpreter, RegionAccess, Value};
use bus::i2c::I2c;

use super::*;

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    #[allow(unsafe_code)]
    unsafe {
        Sleepable::assume()
    }
}

/// One message as the model saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seen {
    W(u16, Vec<u8>),
    R(u16, usize),
}

/// Register files at their addresses: a write's first byte sets the
/// pointer, the rest store from it; a read reads on from it. A message
/// to any other address is not acknowledged.
#[derive(Default)]
struct Model {
    devices: StdMutex<Vec<(u16, [u8; 256], usize)>>,
    log: StdMutex<Vec<Vec<Seen>>>,
}

impl Model {
    fn with(addr: u16, regs: &[(u8, &[u8])]) -> Arc<Self> {
        let mut m = [0u8; 256];
        for &(at, bytes) in regs {
            let at = usize::from(at);
            m[at..at + bytes.len()].copy_from_slice(bytes);
        }
        let me = Arc::new(Self::default());
        me.devices.lock().unwrap().push((addr, m, 0));
        me
    }

    fn take_log(&self) -> Vec<Vec<Seen>> {
        core::mem::take(&mut *self.log.lock().unwrap())
    }
}

impl I2c for Model {
    fn transfer(&self, _: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        self.log.lock().unwrap().push(
            msgs.iter()
                .map(|m| match &m.op {
                    Op::Write(b) => Seen::W(m.address, b.to_vec()),
                    Op::Read(b) => Seen::R(m.address, b.len()),
                })
                .collect(),
        );
        let mut devs = self.devices.lock().unwrap();
        for m in msgs.iter_mut() {
            let d = devs
                .iter_mut()
                .find(|d| d.0 == m.address)
                .ok_or(ProviderError::NoAck)?;
            match &mut m.op {
                Op::Write(b) => {
                    if let Some((&p, rest)) = b.split_first() {
                        d.2 = usize::from(p);
                        for &v in rest {
                            d.1[d.2 % 256] = v;
                            d.2 += 1;
                        }
                    }
                }
                Op::Read(b) => {
                    for v in b.iter_mut() {
                        *v = d.1[d.2 % 256];
                        d.2 += 1;
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

/// A host for the interpreter that serves memory as the corpus machine
/// does and gives each serial-bus transfer to the handler on the node
/// the interpreter names, as the kernel's does.
struct Dispatch<'a, H: Host> {
    inner: H,
    on: Option<aml::NodeId>,
    handler: &'a SpaceHandler,
}

impl<H: Host> Host for Dispatch<'_, H> {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        self.inner.read(a)
    }

    fn write(&mut self, a: &RegionAccess, v: u64) -> Result<(), HostError> {
        self.inner.write(a, v)
    }

    fn transfer(&mut self, req: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        if req.handler.is_none() || req.handler != self.on {
            return Err(HostError::Unsupported);
        }
        self.handler.transfer(&s(), req)
    }

    fn gpio_read(&mut self, _: &GpioAccess<'_>) -> Result<u64, HostError> {
        Err(HostError::Unsupported)
    }
}

fn int(i: &mut Interpreter, h: &mut dyn Host, path: &str) -> u64 {
    match i.evaluate_path(path, Vec::new(), h).unwrap() {
        Value::Integer(v) => v,
        v => panic!("{path}: {v:?}"),
    }
}

/// The Q8B's `IRTC` (`ACPI000E`) reads the clock behind `IC13` at 0x68
/// as seven bytes from register 0 (`AttribBytes (7)`), once `IC13`'s
/// `_REG` says the region is there.
#[test]
fn q8b_time_and_alarm_device_reads_its_clock() {
    let (mut i, machine) = acpi_corpus::load("radxa-q8b");
    let ic13 = i
        .lookup(&aml::AmlPath::parse("\\_SB.IC13").unwrap())
        .unwrap();
    // Seconds, minutes, hours, weekday, day, month, year: BCD, 2026-11-10
    // 12:34:56, a Tuesday.
    let model = Model::with(0x68, &[(0, &[0x56, 0x34, 0x12, 0x03, 0x10, 0x11, 0x26])]);
    let handler = SpaceHandler::new(I2cSource::new(Arc::clone(&model) as Arc<dyn I2c>));
    let mut h = Dispatch {
        inner: machine,
        on: None,
        handler: &handler,
    };
    assert_eq!(int(&mut i, &mut h, "\\_SB.IC13.AVBL"), 0, "before _REG");
    // Before a handler, nothing reaches the clock.
    let _ = i.evaluate_path("\\_SB.IRTC._GRT", Vec::new(), &mut h);
    assert!(model.take_log().is_empty());
    h.on = Some(ic13);
    let (ran, failed) = i.install_handler(ic13, RegionSpace::GenericSerialBus, &mut h);
    assert_eq!((ran, failed), (1, Vec::new()));
    assert_eq!(int(&mut i, &mut h, "\\_SB.IC13.AVBL"), 1, "_REG(9, 1) ran");
    let grt = i
        .evaluate_path("\\_SB.IRTC._GRT", Vec::new(), &mut h)
        .unwrap();
    let log = model.take_log();
    assert_eq!(log, [vec![Seen::W(0x68, vec![0]), Seen::R(0x68, 7)]]);
    // `_GRT`'s buffer (ACPI 6.5 §9.18.3): year, month, day, hour, minute,
    // second.
    let Value::Buffer(b) = grt else {
        panic!("{grt:?}")
    };
    assert_eq!(u16::from_le_bytes([b[0], b[1]]), 2026, "{b:x?}");
    assert_eq!(&b[2..7], &[11, 10, 12, 34, 56], "{b:x?}");
    // Removing the handler runs `_REG(9, 0)`.
    let (ran, _) = i.remove_handler(ic13, RegionSpace::GenericSerialBus, &mut h);
    assert_eq!(ran, 1);
    assert_eq!(int(&mut i, &mut h, "\\_SB.IC13.AVBL"), 0);
}

/// `I2cSerialBusV2 (addr, ControllerInitiated, 100000, AddressingMode7Bit,
/// "\\_SB.I2C0")`, without its end tag (as `Connection` holds it).
fn i2c_connection(addr: u16, ten_bit: bool) -> Vec<u8> {
    let src = b"\\_SB.I2C0\0";
    // Tag, length, revision 2, source index, I2C, consumer, type flags,
    // type revision 1, six bytes of type data.
    let mut d = vec![0x8e, 0, 0, 2, 0, 1, 2, 0, 0, 1, 6, 0];
    d[7] = u8::from(ten_bit);
    d.extend_from_slice(&100_000u32.to_le_bytes());
    d.extend_from_slice(&addr.to_le_bytes());
    d.extend_from_slice(src);
    let len = u16::try_from(d.len() - 3).unwrap();
    d[1..3].copy_from_slice(&len.to_le_bytes());
    d
}

/// A controller `I2C0` with a serial-bus region and one field per
/// attribute, at command 0x10, and a method each to read and write it.
fn attribute_table(addr: u16, ten_bit: bool) -> Vec<u8> {
    const BUFFER_ACC: u8 = 5;
    let conn = i2c_connection(addr, ten_bit);
    let mut list = vec![0x02, 0x11];
    list.extend(aml::asm::pkg_len(conn.len() + 2));
    list.push(0x0a);
    list.push(u8::try_from(conn.len()).unwrap());
    list.extend_from_slice(&conn);
    // Offset (0x10).
    list.extend_from_slice(&[0x00, 0x40, 0x08]);
    let fields: [(&str, &[u8]); 10] = [
        ("FQK", &[0x01, BUFFER_ACC, 0x02]),
        ("FSR", &[0x01, BUFFER_ACC, 0x04]),
        ("FBY", &[0x01, BUFFER_ACC, 0x06]),
        ("FWD", &[0x01, BUFFER_ACC, 0x08]),
        ("FBK", &[0x01, BUFFER_ACC, 0x0a]),
        ("FBS", &[0x01, BUFFER_ACC | 0x40, 3]),
        ("FPC", &[0x01, BUFFER_ACC, 0x0c]),
        ("FBP", &[0x01, BUFFER_ACC, 0x0d]),
        ("FRB", &[0x01, BUFFER_ACC | 0x80, 2]),
        ("FRP", &[0x01, BUFFER_ACC | 0xc0, 2]),
    ];
    let mut methods = Vec::new();
    for (n, access) in fields {
        list.extend_from_slice(access);
        let mut seg = [b'_'; 4];
        seg[..n.len()].copy_from_slice(n.as_bytes());
        list.extend_from_slice(&seg);
        // No width, so every field sits at the command, 0x10.
        list.push(0);
        let rd = String::from("R") + &n[1..];
        let wr = String::from("W") + &n[1..];
        methods.extend(method(&rd, 0, &ret(&name(n))));
        methods.extend(method(&wr, 1, &ret(&store(&[0x68], &name(n)))));
    }
    let field = pkg(&[0x5b, 0x81], &cat(&[&name("GSB"), &[BUFFER_ACC], &list]));
    let region = cat(&[&[0x5b, 0x80], &name("GSB"), &[9, 0x00, 0x0b, 0x00, 0x01]]);
    let ctl = device("I2C0", &cat(&[&region, &field, &methods]));
    table(b"DSDT", 2, &aml::asm::scope("\\_SB", &ctl))
}

/// Each attribute's messages and result buffer.
#[test]
fn each_access_attribute() {
    let model = Model::with(0x2a, &[(0x10, &[0xa1, 0xb2, 0xc3, 0xd4, 0xe5])]);
    let handler = SpaceHandler::new(I2cSource::new(Arc::clone(&model) as Arc<dyn I2c>));
    let mut i = Interpreter::new(aml::Config::default());
    let mut h = Dispatch {
        inner: acpi_corpus::Machine::default(),
        on: None,
        handler: &handler,
    };
    i.load_table(&attribute_table(0x2a, false), &mut h).unwrap();
    let ctl = i
        .lookup(&aml::AmlPath::parse("\\_SB.I2C0").unwrap())
        .unwrap();
    h.on = Some(ctl);
    i.install_handler(ctl, RegionSpace::GenericSerialBus, &mut h);
    let mut call = |i: &mut Interpreter, m: &str, arg: Option<Vec<u8>>| {
        let args = arg.map(|b| vec![Value::Buffer(b)]).unwrap_or_default();
        let path = alloc::format!("\\_SB.I2C0.{m}");
        match i.evaluate_path(&path, args, &mut h).unwrap() {
            Value::Buffer(b) => (b, model.take_log()),
            v => panic!("{m}: {v:?}"),
        }
    };
    let r = |n| Seen::R(0x2a, n);
    let w = |b: &[u8]| Seen::W(0x2a, b.to_vec());
    // Reads: the buffer and the messages. The model's pointer moves on
    // with each byte, as a register file's does.
    let (b, log) = call(&mut i, "RBY", None);
    assert_eq!((b, log), (vec![0, 1, 0xa1], vec![vec![w(&[0x10]), r(1)]]));
    let (b, log) = call(&mut i, "RRB", None);
    assert_eq!((&b[..4], log), (&[0, 2, 0xb2, 0xc3][..], vec![vec![r(2)]]));
    let (b, log) = call(&mut i, "RSR", None);
    assert_eq!((b, log), (vec![0, 1, 0xd4], vec![vec![r(1)]]));
    let (b, log) = call(&mut i, "RWD", None);
    assert_eq!(
        (b, log),
        (vec![0, 2, 0xa1, 0xb2], vec![vec![w(&[0x10]), r(2)]])
    );
    let (b, log) = call(&mut i, "RBS", None);
    assert_eq!(b.len(), 257);
    assert_eq!(
        (&b[..5], log),
        (&[0, 3, 0xa1, 0xb2, 0xc3][..], vec![vec![w(&[0x10]), r(3)]])
    );
    // A block: the count (0xa1 here, past the limit), then at most 32
    // bytes of the 32 read after it.
    let (b, log) = call(&mut i, "RBK", None);
    assert_eq!(
        (&b[..5], log),
        (
            &[0, 0xa1, 0xb2, 0xc3, 0xd4][..],
            vec![vec![w(&[0x10]), r(33)]]
        )
    );
    let (b, log) = call(&mut i, "RQK", None);
    assert_eq!((b, log), (vec![0, 0], vec![vec![r(0)]]));
    // Writes take the data after the status and length bytes.
    let (b, log) = call(&mut i, "WBY", Some(vec![0, 1, 0x55]));
    assert_eq!((b, log), (vec![0, 1, 0x55], vec![vec![w(&[0x10, 0x55])]]));
    let (_, log) = call(&mut i, "WSR", Some(vec![0, 1, 0x66]));
    assert_eq!(log, vec![vec![w(&[0x66])]]);
    let (_, log) = call(&mut i, "WWD", Some(vec![0, 2, 1, 2]));
    assert_eq!(log, vec![vec![w(&[0x10, 1, 2])]]);
    let (_, log) = call(&mut i, "WBS", Some(vec![0, 3, 7, 8, 9]));
    assert_eq!(log, vec![vec![w(&[0x10, 7, 8, 9])]]);
    let (_, log) = call(&mut i, "WBK", Some(vec![0, 2, 0xee, 0xff]));
    assert_eq!(log, vec![vec![w(&[0x10, 2, 0xee, 0xff])]]);
    let (_, log) = call(&mut i, "WRB", Some(vec![0, 2, 0x33, 0x44]));
    assert_eq!(log, vec![vec![w(&[0x33, 0x44])]]);
    let (_, log) = call(&mut i, "WQK", Some(vec![0, 0]));
    assert_eq!(log, vec![vec![w(&[])]]);
    // A block write of nothing is refused (`EINVAL` in the status byte),
    // with nothing sent.
    let (b, log) = call(&mut i, "WBK", Some(vec![0, 0]));
    assert_eq!((b[0], log), (22, Vec::new()));
    // Process calls: written, then read back after a repeated START.
    let (b, log) = call(&mut i, "WPC", Some(vec![0, 2, 0x10, 0x20]));
    assert_eq!(
        (b.len(), b[0], b[1], log),
        (4, 0, 2, vec![vec![w(&[0x10, 0x10, 0x20]), r(2)]])
    );
    let (b, log) = call(&mut i, "WRP", Some(vec![0, 2, 0x10, 0xa1]));
    assert_eq!(
        (b[0], b[1], log),
        (0, 2, vec![vec![w(&[0x10, 0xa1]), r(2)]])
    );
    let (b, log) = call(&mut i, "WBP", Some(vec![0, 1, 0x05]));
    assert_eq!(b[0], 0);
    assert_eq!(log, vec![vec![w(&[0x10, 1, 0x05]), r(33)]]);
    // Nothing at the address: the status is `IIC_ENOACK`.
    let model = Model::with(0x2b, &[]);
    let handler = SpaceHandler::new(I2cSource::new(Arc::clone(&model) as Arc<dyn I2c>));
    let mut h = Dispatch {
        inner: acpi_corpus::Machine::default(),
        on: Some(ctl),
        handler: &handler,
    };
    match i
        .evaluate_path("\\_SB.I2C0.RBY", Vec::new(), &mut h)
        .unwrap()
    {
        // `IIC_ENOACK`.
        Value::Buffer(b) => assert_eq!(b, [2, 0, 0]),
        v => panic!("{v:?}"),
    }
}

/// A 10-bit connection is refused as FreeBSD refuses it: the AML
/// operation fails.
#[test]
fn ten_bit_connections_are_refused() {
    let model = Model::with(0x2a, &[]);
    let handler = SpaceHandler::new(I2cSource::new(Arc::clone(&model) as Arc<dyn I2c>));
    let mut i = Interpreter::new(aml::Config::default());
    let mut h = Dispatch {
        inner: acpi_corpus::Machine::default(),
        on: None,
        handler: &handler,
    };
    i.load_table(&attribute_table(0x2a, true), &mut h).unwrap();
    let ctl = i
        .lookup(&aml::AmlPath::parse("\\_SB.I2C0").unwrap())
        .unwrap();
    h.on = Some(ctl);
    i.install_handler(ctl, RegionSpace::GenericSerialBus, &mut h);
    assert!(
        i.evaluate_path("\\_SB.I2C0.RBY", Vec::new(), &mut h)
            .is_err()
    );
    assert!(model.take_log().is_empty());
}
