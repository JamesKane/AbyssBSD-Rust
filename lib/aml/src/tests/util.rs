// SPDX-License-Identifier: BSD-2-Clause
//! Test support: a memory-backed host that records accesses, and a small
//! AML assembler for building definition blocks by hand.

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::host::{BufferTransfer, GpioAccess, Host, HostError, RegionAccess, RegionSpace};
use crate::{Config, Interpreter, NodeId, Value};

/// One recorded region access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Access {
    pub(crate) write: bool,
    pub(crate) space: RegionSpace,
    pub(crate) address: u64,
    pub(crate) bytes: u64,
    pub(crate) value: u64,
}

/// Regions backed by sparse memory (unwritten bytes read as `fill`), with
/// every access recorded.
#[derive(Debug, Default)]
pub(crate) struct FakeHost {
    pub(crate) mem: BTreeMap<(u8, u64), u8>,
    pub(crate) fill: u8,
    pub(crate) log: Vec<Access>,
    pub(crate) notifies: Vec<(NodeId, u64)>,
    pub(crate) debug: Vec<Value>,
    pub(crate) sleeps: u64,
    pub(crate) transfers: Vec<(u64, Option<Vec<u8>>)>,
    pub(crate) gpio: BTreeMap<u64, bool>,
    pub(crate) gl_acquires: u32,
    pub(crate) gl_releases: u32,
    pub(crate) stalls: u64,
    pub(crate) fatals: Vec<(u8, u32, u64)>,
    /// `timer` returns `clock` and then advances it by `clock_step`.
    pub(crate) clock: u64,
    pub(crate) clock_step: u64,
}

const LOG_CAP: usize = 100_000;

/// The key a PCI configuration access is stored under.
fn key(a: &RegionAccess, i: u64) -> (u8, u64) {
    let mut addr = a.address().wrapping_add(i);
    if let Some(p) = a.pci {
        addr = (u64::from(p.segment) << 48)
            | (u64::from(p.bus) << 40)
            | (u64::from(p.device) << 32)
            | (u64::from(p.function) << 24)
            | (addr & 0xff_ffff);
    }
    if a.space == RegionSpace::EmbeddedControl || a.space == RegionSpace::Pcc {
        // Several ECs or PCC channels would collide; one is enough here.
        addr = a.base.wrapping_mul(0x1_0000).wrapping_add(a.offset + i);
    }
    (a.space.to_u8(), addr)
}

impl FakeHost {
    pub(crate) fn poke(&mut self, space: RegionSpace, addr: u64, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            self.mem.insert((space.to_u8(), addr + i as u64), b);
        }
    }

    pub(crate) fn peek(&self, space: RegionSpace, addr: u64, n: u64) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            let b = self
                .mem
                .get(&(space.to_u8(), addr + i))
                .copied()
                .unwrap_or(self.fill);
            v |= u64::from(b) << (8 * i);
        }
        v
    }
}

impl Host for FakeHost {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        let mut v = 0u64;
        for i in 0..a.width.bytes() {
            let b = self.mem.get(&key(a, i)).copied().unwrap_or(self.fill);
            v |= u64::from(b) << (8 * i);
        }
        if self.log.len() < LOG_CAP {
            self.log.push(Access {
                write: false,
                space: a.space,
                address: a.address(),
                bytes: a.width.bytes(),
                value: v,
            });
        }
        Ok(v)
    }

    fn write(&mut self, a: &RegionAccess, value: u64) -> Result<(), HostError> {
        for i in 0..a.width.bytes() {
            self.mem
                .insert(key(a, i), value.to_le_bytes()[usize::try_from(i).unwrap()]);
        }
        if self.log.len() < LOG_CAP {
            self.log.push(Access {
                write: true,
                space: a.space,
                address: a.address(),
                bytes: a.width.bytes(),
                value,
            });
        }
        Ok(())
    }

    fn transfer(&mut self, req: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        if self.transfers.len() < LOG_CAP {
            self.transfers
                .push((req.command, req.write.map(<[u8]>::to_vec)));
        }
        // Status 0, length 2, data.
        Ok(alloc::vec![0, 2, 0x34, 0x12])
    }

    fn gpio_read(&mut self, a: &GpioAccess<'_>) -> Result<u64, HostError> {
        let mut v = 0;
        for i in 0..a.count.min(64) {
            if self.gpio.get(&(a.pin + i)).copied().unwrap_or(false) {
                v |= 1 << i;
            }
        }
        Ok(v)
    }

    fn gpio_write(&mut self, a: &GpioAccess<'_>, value: u64) -> Result<(), HostError> {
        for i in 0..a.count.min(64) {
            self.gpio.insert(a.pin + i, value & (1 << i) != 0);
        }
        Ok(())
    }

    fn sleep(&mut self, ms: u64) {
        self.sleeps = self.sleeps.saturating_add(ms);
    }

    fn stall(&mut self, us: u64) {
        self.stalls = self.stalls.saturating_add(us);
    }

    fn timer(&mut self) -> u64 {
        let t = self.clock;
        self.clock = self.clock.wrapping_add(self.clock_step);
        t
    }

    fn fatal(&mut self, kind: u8, code: u32, arg: u64) {
        self.fatals.push((kind, code, arg));
    }

    fn notify(&mut self, node: NodeId, value: u64) {
        if self.notifies.len() < LOG_CAP {
            self.notifies.push((node, value));
        }
    }

    fn acquire_global_lock(&mut self, _timeout_ms: u16) -> bool {
        self.gl_acquires += 1;
        true
    }

    fn release_global_lock(&mut self) {
        self.gl_releases += 1;
    }

    fn debug(&mut self, value: &Value) {
        if self.debug.len() < LOG_CAP {
            self.debug.push(value.clone());
        }
    }
}

/// The default configuration with a stack budget for the 2 MiB test
/// threads: unoptimized frames are larger than a kernel build's, and the
/// stack limit has its own tests (`control::default_depth_*`).
pub(crate) fn test_config() -> Config {
    let mut c = Config::default();
    c.limits.stack_budget = 1 << 20;
    c
}

/// Wraps AML in a definition block header with a valid checksum.
pub(crate) fn table(sig: &[u8; 4], revision: u8, aml: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(sig);
    t.extend_from_slice(&u32::try_from(36 + aml.len()).unwrap().to_le_bytes());
    t.push(revision);
    t.push(0);
    t.extend_from_slice(b"ABYSS ");
    t.extend_from_slice(b"TESTTABL");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(b"ABYS");
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(aml);
    fix_checksum(&mut t);
    t
}

pub(crate) fn fix_checksum(t: &mut [u8]) {
    if t.len() < 36 {
        return;
    }
    t[9] = 0;
    let sum = t.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    t[9] = 0u8.wrapping_sub(sum);
}

/// Encodes a package length for a body of `n` bytes (the length includes
/// its own encoding).
pub(crate) fn pkg_len(n: usize) -> Vec<u8> {
    if n + 1 < 0x40 {
        return alloc::vec![u8::try_from(n + 1).unwrap()];
    }
    for extra in 1..=3usize {
        let total = n + 1 + extra;
        if total < 1 << (4 + 8 * extra) {
            let mut v = alloc::vec![u8::try_from((extra << 6) | (total & 0xf)).unwrap()];
            for i in 0..extra {
                v.push(u8::try_from((total >> (4 + 8 * i)) & 0xff).unwrap());
            }
            return v;
        }
    }
    panic!("package too large");
}

/// `op PkgLength body`.
pub(crate) fn pkg(op: &[u8], body: &[u8]) -> Vec<u8> {
    let mut v = op.to_vec();
    v.extend(pkg_len(body.len()));
    v.extend_from_slice(body);
    v
}

/// Concatenates byte pieces.
pub(crate) fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// A `NameString` for ASL text such as `\_SB.PCI0` or `FOO`.
pub(crate) fn name(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    let mut rest = s;
    if let Some(r) = rest.strip_prefix('\\') {
        v.push(b'\\');
        rest = r;
    }
    while let Some(r) = rest.strip_prefix('^') {
        v.push(b'^');
        rest = r;
    }
    let segs: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('.').collect()
    };
    match segs.len() {
        0 => v.push(0),
        1 => {}
        2 => v.push(0x2e),
        n => {
            v.push(0x2f);
            v.push(u8::try_from(n).unwrap());
        }
    }
    for s in segs {
        let mut seg = [b'_'; 4];
        seg[..s.len()].copy_from_slice(s.as_bytes());
        v.extend_from_slice(&seg);
    }
    v
}

/// An integer constant in its shortest encoding.
pub(crate) fn int(v: u64) -> Vec<u8> {
    match v {
        0 => alloc::vec![0x00],
        1 => alloc::vec![0x01],
        2..=0xff => alloc::vec![0x0a, u8::try_from(v).unwrap()],
        0x100..=0xffff => cat(&[&[0x0b], &u16::try_from(v).unwrap().to_le_bytes()]),
        0x1_0000..=0xffff_ffff => cat(&[&[0x0c], &u32::try_from(v).unwrap().to_le_bytes()]),
        _ => cat(&[&[0x0e], &v.to_le_bytes()]),
    }
}

/// A string constant.
pub(crate) fn string(s: &str) -> Vec<u8> {
    cat(&[&[0x0d], s.as_bytes(), &[0]])
}

/// `Buffer (n) { bytes }`.
pub(crate) fn buffer(bytes: &[u8]) -> Vec<u8> {
    pkg(&[0x11], &cat(&[&int(bytes.len() as u64), bytes]))
}

/// `Package () { elements }`.
pub(crate) fn package(elems: &[Vec<u8>]) -> Vec<u8> {
    let mut body = alloc::vec![u8::try_from(elems.len()).unwrap()];
    for e in elems {
        body.extend_from_slice(e);
    }
    pkg(&[0x12], &body)
}

/// `Method (name, args) { body }`.
pub(crate) fn method(n: &str, flags: u8, body: &[u8]) -> Vec<u8> {
    pkg(&[0x14], &cat(&[&name(n), &[flags], body]))
}

/// `Name (name, value)`.
pub(crate) fn name_op(n: &str, value: &[u8]) -> Vec<u8> {
    cat(&[&[0x08], &name(n), value])
}

/// `Device (name) { body }`.
pub(crate) fn device(n: &str, body: &[u8]) -> Vec<u8> {
    pkg(&[0x5b, 0x82], &cat(&[&name(n), body]))
}

/// `Scope (name) { body }`.
pub(crate) fn scope(n: &str, body: &[u8]) -> Vec<u8> {
    pkg(&[0x10], &cat(&[&name(n), body]))
}

/// `Return (x)`.
pub(crate) fn ret(x: &[u8]) -> Vec<u8> {
    cat(&[&[0xa4], x])
}

/// `Store (src, dst)`.
pub(crate) fn store(src: &[u8], dst: &[u8]) -> Vec<u8> {
    cat(&[&[0x70], src, dst])
}

/// `If (pred) { body }`.
pub(crate) fn if_(pred: &[u8], body: &[u8]) -> Vec<u8> {
    pkg(&[0xa0], &cat(&[pred, body]))
}

/// `Else { body }`.
pub(crate) fn else_(body: &[u8]) -> Vec<u8> {
    pkg(&[0xa1], body)
}

/// `While (pred) { body }`.
pub(crate) fn while_(pred: &[u8], body: &[u8]) -> Vec<u8> {
    pkg(&[0xa2], &cat(&[pred, body]))
}

pub(crate) const LOCAL0: u8 = 0x60;
pub(crate) const ARG0: u8 = 0x68;

/// Loads a 64-bit (revision 2) DSDT built from `aml`.
pub(crate) fn load(aml: &[u8]) -> (Interpreter, FakeHost) {
    load_rev(aml, 2)
}

pub(crate) fn load_rev(aml: &[u8], revision: u8) -> (Interpreter, FakeHost) {
    load_cfg(aml, revision, test_config())
}

pub(crate) fn load_cfg(aml: &[u8], revision: u8, config: Config) -> (Interpreter, FakeHost) {
    let mut i = Interpreter::new(config);
    let mut h = FakeHost::default();
    let r = i
        .load_table(&table(b"DSDT", revision, aml), &mut h)
        .unwrap();
    assert!(r.warnings.is_empty(), "load warnings: {:?}", r.warnings);
    (i, h)
}

/// Evaluates a path to a value, panicking on error.
pub(crate) fn eval(i: &mut Interpreter, h: &mut FakeHost, path: &str, args: Vec<Value>) -> Value {
    match i.evaluate_path(path, args, h) {
        Ok(v) => v,
        Err(e) => panic!("{path}: {e:?}"),
    }
}

/// Evaluates a method that takes no arguments to an integer.
pub(crate) fn eval_int(i: &mut Interpreter, h: &mut FakeHost, path: &str) -> u64 {
    match eval(i, h, path, Vec::new()) {
        Value::Integer(v) => v,
        v => panic!("{path}: not an integer: {v:?}"),
    }
}

/// Runs `body` as a method `\TST_` and returns its result.
pub(crate) fn run(body: &[u8]) -> Value {
    let (mut i, mut h) = load(&method("TST", 0, body));
    eval(&mut i, &mut h, "\\TST", Vec::new())
}

pub(crate) fn run_err(body: &[u8]) -> crate::Error {
    run_err_cfg(body, test_config())
}

pub(crate) fn run_err_cfg(body: &[u8], config: Config) -> crate::Error {
    let (mut i, mut h) = load_cfg(&method("TST", 0, body), 2, config);
    i.evaluate_path("\\TST", Vec::new(), &mut h).unwrap_err()
}
