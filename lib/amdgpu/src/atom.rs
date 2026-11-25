// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The AtomBIOS command-table interpreter (specification 12): the video
//! BIOS holds bytecode for a small register machine, and a chip reset's
//! re-initialisation runs its "ASIC init" table. This is that machine: the
//! instruction set of section 4, the operand forms of section 3, tables
//! calling tables, and nothing else (no port I/O, no PLL or memory
//! controller access: those read 0 and drop writes, as the reference's do).

use alloc::vec;
use alloc::vec::Vec;

use crate::vbios::Image;

/// What the interpreter does to the hardware.
pub trait Io {
    /// Reads the register at dword index `index` (direct access: inside the
    /// register window, or beyond it through the indirect pair).
    fn read(&self, index: u32) -> u32;
    /// Writes it.
    fn write(&self, index: u32, value: u32);
    /// Waits `us` microseconds.
    fn delay_us(&self, us: u64);
}

/// Why a table stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The command table is not in the image.
    Absent,
    /// A table or an operand runs past the image, or the bytes make no
    /// sense (a switch case that is not a case).
    Malformed,
    /// Tables called tables more than 32 deep.
    TooDeep,
    /// More instructions than any real table runs: a loop that does not end.
    Runaway,
    /// An indirect I/O method: not built (Raphael's image has none).
    IndirectIo,
}

/// The scratch area's default size (12 section 2.2).
const SCRATCH: usize = 20 * 1024;
/// Nesting limit (12 section 6).
const DEPTH: u32 = 32;
/// Instructions before a table is taken for a loop that does not end.
const STEPS: u64 = 50_000_000;

/// The operand types (12 section 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Reg,
    Ps,
    Ws,
    Fb,
    Id,
    Imm,
    Pll,
    Mc,
}

impl Kind {
    fn from(code: u8) -> Self {
        match code {
            0 => Self::Reg,
            1 => Self::Ps,
            2 => Self::Ws,
            3 => Self::Fb,
            4 => Self::Id,
            5 => Self::Imm,
            6 => Self::Pll,
            _ => Self::Mc,
        }
    }
}

/// The field of a dword an alignment names: (mask, shift) (12 section 3.2).
const FIELDS: [(u32, u32); 8] = [
    (0xffff_ffff, 0),
    (0x0000_ffff, 0),
    (0x00ff_ff00, 8),
    (0xffff_0000, 16),
    (0x0000_00ff, 0),
    (0x0000_ff00, 8),
    (0x00ff_0000, 16),
    (0xff00_0000, 24),
];

/// The destination field: from the source alignment and the destination
/// selector (12 section 3.2).
fn destination_field(align: u8, selector: u8) -> (u32, u32) {
    match align {
        0 => FIELDS[0],
        1..=3 => match selector {
            0 => FIELDS[1],
            1 => FIELDS[2],
            2 => FIELDS[3],
            _ => FIELDS[0],
        },
        _ => match selector {
            0 => FIELDS[4],
            1 => FIELDS[5],
            2 => FIELDS[6],
            _ => FIELDS[7],
        },
    }
}

/// How many bytes an immediate takes (12 section 3.1).
fn immediate_bytes(align: u8) -> usize {
    match align {
        0 => 4,
        1..=3 => 2,
        _ => 1,
    }
}

/// An operand: where a value is.
#[derive(Clone, Copy, Debug)]
struct Operand {
    kind: Kind,
    /// The register, parameter, workspace or scratch index; the image
    /// offset of an ID; the value of an immediate.
    at: u32,
}

/// The state tables share while they call each other (12 section 2).
struct Shared {
    data_block: usize,
    reg_block: u32,
    fb_base: u32,
    io_indirect: bool,
    io_attr: u32,
    shift: u32,
    quotient: u32,
    remainder: u32,
    equal: bool,
    above: bool,
    steps: u64,
}

/// The interpreter for one image over one set of registers.
#[allow(missing_debug_implementations, reason = "it holds a register window")]
pub struct Interpreter<'a, I: Io> {
    image: &'a Image,
    io: &'a I,
    fb: Vec<u8>,
}

impl<'a, I: Io> Interpreter<'a, I> {
    /// An interpreter with an empty scratch area.
    pub fn new(image: &'a Image, io: &'a I) -> Self {
        Self {
            image,
            io,
            fb: vec![0; SCRATCH],
        }
    }

    /// Runs command table `index` with the parameter space `ps`.
    ///
    /// # Errors
    ///
    /// [`Error`] if the table is absent or malformed or does not end.
    pub fn run(&mut self, index: usize, ps: &mut [u32]) -> Result<(), Error> {
        let mut shared = Shared {
            data_block: 0,
            reg_block: 0,
            fb_base: 0,
            io_indirect: false,
            io_attr: 0,
            shift: 0,
            quotient: 0,
            remainder: 0,
            equal: false,
            above: false,
            steps: 0,
        };
        self.table(index, ps, &mut shared, 0)
    }

    fn byte(&self, at: usize) -> Result<u8, Error> {
        self.image.bytes.get(at).copied().ok_or(Error::Malformed)
    }

    fn u16(&self, at: usize) -> Result<u32, Error> {
        Ok(u32::from(self.byte(at)?) | (u32::from(self.byte(at + 1)?) << 8))
    }

    fn u32(&self, at: usize) -> Result<u32, Error> {
        Ok(self.u16(at)? | (self.u16(at + 2)? << 16))
    }

    fn table(
        &mut self,
        index: usize,
        ps: &mut [u32],
        sh: &mut Shared,
        depth: u32,
    ) -> Result<(), Error> {
        if depth >= DEPTH {
            return Err(Error::TooDeep);
        }
        let start = self.image.command_table(index).ok_or(Error::Absent)?;
        let size = self.u16(start)? as usize;
        let ws_size = usize::from(self.byte(start + 4)?);
        let ps_size = usize::from(self.byte(start + 5)? & 0x7f);
        let end = (start + size).min(self.image.bytes.len());
        let mut ws = vec![0u32; ws_size];
        let mut ip = start + 6;
        while ip < end {
            sh.steps += 1;
            if sh.steps > STEPS {
                return Err(Error::Runaway);
            }
            let op = self.byte(ip)?;
            ip += 1;
            match op {
                // The end: opcode 0, EOT, or anything from 127 up.
                0 | 0x5b | 127.. => return Ok(()),
                0x5a | 0x62 | 0x63 | 0x79 => {
                    // NOP, POST_CARD (1 byte), BEEP, DEBUG (1 byte).
                    if op == 0x62 || op == 0x79 {
                        ip += 1;
                    }
                }
                // REPEAT, SAVE_REG, RESTORE_REG: not implemented, no operands.
                0x53 | 0x64 | 0x65 => {}
                0x37 => {
                    // SET_ATI_PORT: 0 is direct access.
                    sh.io_indirect = self.u16(ip)? != 0;
                    ip += 2;
                }
                0x38 | 0x39 => ip += 1,
                0x3a => {
                    sh.reg_block = self.u16(ip)?;
                    ip += 2;
                }
                0x3b => {
                    let attr = self.byte(ip)?;
                    ip += 1;
                    let (v, n) = self.source(attr, ip, ps, &ws, sh)?;
                    ip += n;
                    sh.fb_base = v;
                }
                0x42 => {
                    // SWITCH
                    let attr = self.byte(ip)?;
                    ip += 1;
                    let (v, n) = self.source(attr, ip, ps, &ws, sh)?;
                    ip += n;
                    let width = immediate_bytes((attr >> 3) & 7);
                    let mut taken = None;
                    loop {
                        if self.u16(ip)? == 0x5a5a {
                            ip += 2;
                            break;
                        }
                        if self.byte(ip)? != 0x63 {
                            return Err(Error::Malformed);
                        }
                        ip += 1;
                        let case = match width {
                            4 => self.u32(ip)?,
                            2 => self.u16(ip)?,
                            _ => u32::from(self.byte(ip)?),
                        };
                        ip += width;
                        let target = self.u16(ip)? as usize;
                        ip += 2;
                        if taken.is_none() && case == v {
                            taken = Some(target);
                        }
                    }
                    if let Some(t) = taken {
                        ip = start + t;
                    }
                }
                0x43..=0x49 => {
                    let target = self.u16(ip)? as usize;
                    ip += 2;
                    let go = match op {
                        0x43 => true,
                        0x44 => sh.equal,
                        0x45 => !sh.above && !sh.equal,
                        0x46 => sh.above,
                        0x47 => !sh.above,
                        0x48 => sh.above || sh.equal,
                        _ => !sh.equal,
                    };
                    if go {
                        ip = start + target;
                    }
                }
                0x50 => {
                    self.io.delay_us(u64::from(self.byte(ip)?) * 1000);
                    ip += 1;
                }
                0x51 => {
                    self.io.delay_us(u64::from(self.byte(ip)?));
                    ip += 1;
                }
                0x52 => {
                    // CALL_TABLE: the callee's parameters start after the
                    // caller's own.
                    let callee = usize::from(self.byte(ip)?);
                    ip += 1;
                    if self.image.command_table(callee).is_some() {
                        let skip = (ps_size / 4).min(ps.len());
                        self.table(callee, &mut ps[skip..], sh, depth + 1)?;
                    }
                }
                0x66 => {
                    // SET_DATA_BLOCK
                    let n = self.byte(ip)?;
                    ip += 1;
                    sh.data_block = match n {
                        0 => 0,
                        255 => start,
                        n => self.image.data_table(usize::from(n)).unwrap_or(0),
                    };
                }
                0x7a => {
                    // PROCESS_DS: skip inline data.
                    ip += self.u16(ip)? as usize + 2;
                }
                0x7b..=0x7e => {
                    // MUL32 / DIV32 on PS or WS.
                    let attr = self.byte(ip)?;
                    ip += 1;
                    let dest_ps = op == 0x7b || op == 0x7d;
                    let dst = Operand {
                        kind: if dest_ps { Kind::Ps } else { Kind::Ws },
                        at: u32::from(self.byte(ip)?),
                    };
                    ip += 1;
                    let (v, n) = self.source(attr, ip, ps, &ws, sh)?;
                    ip += n;
                    let d = self.load(dst, ps, &ws, sh);
                    if op <= 0x7c {
                        let p = u64::from(d) * u64::from(v);
                        sh.quotient = u32::try_from(p & 0xffff_ffff).unwrap_or(0);
                        sh.remainder = u32::try_from(p >> 32).unwrap_or(0);
                    } else if v == 0 {
                        sh.quotient = 0;
                        sh.remainder = 0;
                    } else {
                        let n = (u64::from(sh.remainder) << 32) | u64::from(d);
                        let q = n / u64::from(v);
                        sh.quotient = u32::try_from(q & 0xffff_ffff).unwrap_or(0);
                        sh.remainder = u32::try_from(q >> 32).unwrap_or(0);
                    }
                }
                0x01..=0x36 | 0x3c..=0x41 | 0x4a..=0x4f | 0x54..=0x61 | 0x67..=0x78 => {
                    ip = self.grouped(op, ip, ps, &mut ws, sh)?;
                }
            }
        }
        Ok(())
    }

    /// Reads the whole dword an operand names (a source's raw value).
    fn load(&self, o: Operand, ps: &[u32], ws: &[u32], sh: &Shared) -> u32 {
        const BAD: u32 = 0xcdcd_cdcd;
        match o.kind {
            Kind::Reg => self.io.read(o.at.wrapping_add(sh.reg_block)),
            Kind::Ps => ps.get(o.at as usize).copied().unwrap_or(BAD),
            Kind::Ws => match o.at {
                0x40 => sh.quotient,
                0x41 => sh.remainder,
                0x42 => u32::try_from(sh.data_block).unwrap_or(0),
                0x43 => sh.shift,
                0x44 => 1u32.checked_shl(sh.shift).unwrap_or(0),
                0x45 => !1u32.checked_shl(sh.shift).unwrap_or(0),
                0x46 => sh.fb_base,
                0x47 => sh.io_attr,
                0x48 => sh.reg_block,
                i => ws.get(i as usize).copied().unwrap_or(BAD),
            },
            Kind::Fb => {
                let at = sh.fb_base as usize + 4 * o.at as usize;
                match self.fb.get(at..at + 4) {
                    Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    None => 0,
                }
            }
            Kind::Id => {
                let at = o.at as usize + sh.data_block;
                self.u32(at).unwrap_or(0)
            }
            Kind::Imm => o.at,
            Kind::Pll | Kind::Mc => 0,
        }
    }

    /// Writes the whole dword an operand names.
    fn store(&mut self, o: Operand, v: u32, ps: &mut [u32], ws: &mut [u32], sh: &mut Shared) {
        match o.kind {
            Kind::Reg => {
                let index = o.at.wrapping_add(sh.reg_block);
                // Register 0 is the index register: tables hold dword
                // indices, it wants a byte address (12 section 4.1).
                let v = if index == 0 { v << 2 } else { v };
                self.io.write(index, v);
            }
            Kind::Ps => {
                if let Some(p) = ps.get_mut(o.at as usize) {
                    *p = v;
                }
            }
            Kind::Ws => match o.at {
                0x40 => sh.quotient = v,
                0x41 => sh.remainder = v,
                0x42 => sh.data_block = v as usize,
                0x43 => sh.shift = v,
                0x46 => sh.fb_base = v,
                0x47 => sh.io_attr = v,
                0x48 => sh.reg_block = v,
                0x44 | 0x45 => {}
                i => {
                    if let Some(w) = ws.get_mut(i as usize) {
                        *w = v;
                    }
                }
            },
            Kind::Fb => {
                let at = sh.fb_base as usize + 4 * o.at as usize;
                if let Some(b) = self.fb.get_mut(at..at + 4) {
                    b.copy_from_slice(&v.to_le_bytes());
                }
            }
            Kind::Id | Kind::Imm | Kind::Pll | Kind::Mc => {}
        }
    }

    /// A source operand at `at` with attribute `attr`: its value (the field
    /// the alignment names) and how many bytes it takes.
    fn source(
        &self,
        attr: u8,
        at: usize,
        ps: &[u32],
        ws: &[u32],
        sh: &Shared,
    ) -> Result<(u32, usize), Error> {
        let kind = Kind::from(attr & 7);
        let align = (attr >> 3) & 7;
        let (mask, shift) = FIELDS[usize::from(align)];
        let (o, n) = self.operand(kind, align, at)?;
        let v = if kind == Kind::Imm {
            o.at
        } else {
            (self.load(o, ps, ws, sh) & mask) >> shift
        };
        Ok((v, n))
    }

    /// The operand of type `kind` at `at`, and its length.
    fn operand(&self, kind: Kind, align: u8, at: usize) -> Result<(Operand, usize), Error> {
        Ok(match kind {
            Kind::Reg | Kind::Id => (
                Operand {
                    kind,
                    at: self.u16(at)?,
                },
                2,
            ),
            Kind::Imm => {
                let n = immediate_bytes(align);
                let v = match n {
                    4 => self.u32(at)?,
                    2 => self.u16(at)?,
                    _ => u32::from(self.byte(at)?),
                };
                (Operand { kind, at: v }, n)
            }
            _ => (
                Operand {
                    kind,
                    at: u32::from(self.byte(at)?),
                },
                1,
            ),
        })
    }

    /// The grouped instructions: one opcode per destination type, in the
    /// order register, parameter, workspace, scratch, PLL, memory controller.
    #[allow(clippy::too_many_lines, reason = "one arm for each instruction")]
    fn grouped(
        &mut self,
        op: u8,
        mut ip: usize,
        ps: &mut [u32],
        ws: &mut [u32],
        sh: &mut Shared,
    ) -> Result<usize, Error> {
        // The group: its first opcode, and the destination type by position.
        let (base, name) = match op {
            0x01..=0x06 => (0x01, "move"),
            0x07..=0x0c => (0x07, "and"),
            0x0d..=0x12 => (0x0d, "or"),
            0x13..=0x18 => (0x13, "shift_left"),
            0x19..=0x1e => (0x19, "shift_right"),
            0x1f..=0x24 => (0x1f, "mul"),
            0x25..=0x2a => (0x25, "div"),
            0x2b..=0x30 => (0x2b, "add"),
            0x31..=0x36 => (0x31, "sub"),
            0x3c..=0x41 => (0x3c, "compare"),
            0x4a..=0x4f => (0x4a, "test"),
            0x54..=0x59 => (0x54, "clear"),
            0x5c..=0x61 => (0x5c, "mask"),
            0x67..=0x6c => (0x67, "xor"),
            0x6d..=0x72 => (0x6d, "shl"),
            _ => (0x73, "shr"),
        };
        let dest_kind = Kind::from(op - base);
        let attr = self.byte(ip)?;
        ip += 1;
        let align = (attr >> 3) & 7;
        let (dst, n) = self.operand(dest_kind, 0, ip)?;
        ip += n;
        // The destination field.
        let (mask, shift) = if matches!(name, "clear" | "shift_left" | "shift_right") {
            // The field the attribute names directly.
            FIELDS[usize::from(align)]
        } else {
            destination_field(align, attr >> 6)
        };
        let saved = self.load(dst, ps, ws, sh);
        let field = (saved & mask) >> shift;
        let write = |me: &mut Self, ps: &mut [u32], ws: &mut [u32], sh: &mut Shared, value: u32| {
            let merged = (saved & !mask) | ((value << shift) & mask);
            me.store(dst, merged, ps, ws, sh);
        };
        match name {
            "clear" => write(self, ps, ws, sh, 0),
            "shift_left" | "shift_right" => {
                let count = u32::from(self.byte(ip)?);
                ip += 1;
                let v = if name == "shift_left" {
                    field.checked_shl(count).unwrap_or(0)
                } else {
                    field.checked_shr(count).unwrap_or(0)
                };
                write(self, ps, ws, sh, v);
            }
            "mask" => {
                // attr, dst, mask (immediate width by alignment), src.
                let w = immediate_bytes(align);
                let m = match w {
                    4 => self.u32(ip)?,
                    2 => self.u16(ip)?,
                    _ => u32::from(self.byte(ip)?),
                };
                ip += w;
                let (v, n) = self.source(attr, ip, ps, ws, sh)?;
                ip += n;
                write(self, ps, ws, sh, (field & m) | v);
            }
            _ => {
                let (v, n) = self.source(attr, ip, ps, ws, sh)?;
                ip += n;
                match name {
                    // A move of a whole dword replaces it (no read).
                    "move" => write(self, ps, ws, sh, v),
                    "and" => write(self, ps, ws, sh, field & v),
                    "or" => write(self, ps, ws, sh, field | v),
                    "xor" => write(self, ps, ws, sh, field ^ v),
                    "add" => write(self, ps, ws, sh, field.wrapping_add(v)),
                    "sub" => write(self, ps, ws, sh, field.wrapping_sub(v)),
                    "mul" => sh.quotient = field.wrapping_mul(v),
                    "div" => {
                        sh.quotient = field.checked_div(v).unwrap_or(0);
                        sh.remainder = field.checked_rem(v).unwrap_or(0);
                    }
                    "compare" => {
                        sh.equal = field == v;
                        sh.above = field > v;
                    }
                    "test" => sh.equal = field & v == 0,
                    // The whole saved dword shifted, then the field taken.
                    "shl" => {
                        let r = saved.checked_shl(v).unwrap_or(0);
                        write(self, ps, ws, sh, (r & mask) >> shift);
                    }
                    _ => {
                        let r = saved.checked_shr(v).unwrap_or(0);
                        write(self, ps, ws, sh, (r & mask) >> shift);
                    }
                }
            }
        }
        Ok(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// Registers in a map, with a log of the writes.
    struct Regs {
        values: RefCell<std::collections::BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        delays: RefCell<Vec<u64>>,
    }

    impl Regs {
        fn new(init: &[(u32, u32)]) -> Self {
            Self {
                values: RefCell::new(init.iter().copied().collect()),
                writes: RefCell::new(Vec::new()),
                delays: RefCell::new(Vec::new()),
            }
        }
        fn get(&self, r: u32) -> u32 {
            self.values.borrow().get(&r).copied().unwrap_or(0)
        }
    }

    impl Io for Regs {
        fn read(&self, index: u32) -> u32 {
            self.get(index)
        }
        fn write(&self, index: u32, value: u32) {
            self.values.borrow_mut().insert(index, value);
            self.writes.borrow_mut().push((index, value));
        }
        fn delay_us(&self, us: u64) {
            self.delays.borrow_mut().push(us);
        }
    }

    /// An image with command tables at the given indices (each a body of
    /// instructions after the 6-byte header: size, revisions, WS size, PS
    /// size), the command list at 0x100 and a data list with entry 22 = 0x300.
    fn image(tables: &[(usize, u8, u8, &[u8])]) -> Image {
        let mut b = vec![0u8; 0x1000];
        b[0] = 0x55;
        b[1] = 0xaa;
        b[0x48..0x4a].copy_from_slice(&0x60u16.to_le_bytes());
        b[0x64..0x68].copy_from_slice(b"ATOM");
        b[0x60 + 0x1e..0x60 + 0x20].copy_from_slice(&0x100u16.to_le_bytes());
        b[0x60 + 0x20..0x60 + 0x22].copy_from_slice(&0x200u16.to_le_bytes());
        // 24 command entries; data list with 24 entries, 22 at 0x300.
        b[0x100..0x102].copy_from_slice(&52u16.to_le_bytes());
        b[0x200..0x202].copy_from_slice(&52u16.to_le_bytes());
        b[0x204 + 44..0x204 + 46].copy_from_slice(&0x300u16.to_le_bytes());
        // Data block 22: a few dwords.
        b[0x300..0x304].copy_from_slice(&0x1111_2222u32.to_le_bytes());
        b[0x304..0x308].copy_from_slice(&0xaabb_ccddu32.to_le_bytes());
        let mut at = 0x400;
        for (index, ws, ps, body) in tables {
            let size = 6 + body.len();
            b[0x104 + 2 * index..0x104 + 2 * index + 2]
                .copy_from_slice(&u16::try_from(at).unwrap().to_le_bytes());
            b[at..at + 2].copy_from_slice(&u16::try_from(size).unwrap().to_le_bytes());
            b[at + 2] = 2;
            b[at + 3] = 1;
            b[at + 4] = *ws;
            b[at + 5] = *ps;
            b[at + 6..at + 6 + body.len()].copy_from_slice(body);
            at += (size + 15) & !15;
        }
        Image::parse(b).unwrap()
    }

    #[test]
    fn a_move_and_an_or_change_fields_of_a_register() {
        // MOVE REG 0x10 <- IMM 0x12345678 (dword); OR REG 0x10 byte 3 |= 0x80;
        // MOVE to WS... then EOT.
        let t: &[u8] = &[
            0x01, 0x05, 0x10, 0x00, 0x78, 0x56, 0x34, 0x12,
            // OR dword, destination selector for a byte source: attr 0x25 =
            // source imm (5), alignment 4 (byte 7:0)... use byte alignment 7
            // (bits 31:24) with selector 0 = field 7:0 of the source's
            // alignment: the destination field is 7:0.
            0x0d, 0x25, 0x10, 0x00, 0x03, 0x5b,
        ];
        let img = image(&[(0, 0, 0, t)]);
        let io = Regs::new(&[]);
        Interpreter::new(&img, &io).run(0, &mut [0; 4]).unwrap();
        assert_eq!(io.get(0x10), 0x1234_567b, "bits 7:0 or'd with 3");
    }

    #[test]
    fn workspace_arithmetic_and_the_special_remainder_index() {
        // The ASIC init's pattern: WS 0x41 <- REG 0xde3; WS 0x41 -= 1;
        // WS 0x41 >>= 11; REG 6 <- WS 0x41.
        let t: &[u8] = &[
            0x03, 0x00, 0x41, 0xe3, 0x0d, // MOVE WS, REG (dword)
            0x33, 0x05, 0x41, 0x01, 0x00, 0x00, 0x00, // SUB WS, IMM 1
            0x1b, 0x05, 0x41, 0x0b, // SHIFT_RIGHT WS by 11 (attr: field 0)
            0x01, 0x02, 0x06, 0x00, 0x41, // MOVE REG 6, WS 0x41
            0x5b,
        ];
        let img = image(&[(0, 0, 0, t)]);
        // 4096 MiB of memory: (4096 - 1) >> 11 = 1.
        let io = Regs::new(&[(0xde3, 4096)]);
        Interpreter::new(&img, &io).run(0, &mut [0; 4]).unwrap();
        assert_eq!(io.get(6), 1);
        // 512 MiB: 0.
        let io = Regs::new(&[(0xde3, 512)]);
        Interpreter::new(&img, &io).run(0, &mut [0; 4]).unwrap();
        assert_eq!(io.get(6), 0);
    }

    #[test]
    fn a_table_calls_another_with_its_parameters_advanced() {
        // Table 1 (PS size 8 bytes) calls table 2, which stores 7 into its
        // own PS 0: the caller's PS dword 2.
        let callee: &[u8] = &[0x02, 0x05, 0x00, 0x07, 0x00, 0x00, 0x00, 0x5b];
        let caller: &[u8] = &[0x52, 0x02, 0x5b];
        let img = image(&[(1, 0, 8, caller), (2, 0, 0, callee)]);
        let io = Regs::new(&[]);
        let mut ps = [0u32; 8];
        Interpreter::new(&img, &io).run(1, &mut ps).unwrap();
        assert_eq!(ps, [0, 0, 7, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn compare_and_the_jumps_choose_a_branch() {
        // COMPARE PS0 with IMM 5; JUMP if equal to the "then" part which
        // moves 1 to PS 1; otherwise PS 1 gets 2.
        // Offsets are from the table start (header 6 bytes).
        let t: &[u8] = &[
            0x3d, 0x05, 0x00, 0x05, 0x00, 0x00, 0x00, // +6 COMPARE PS0, IMM 5
            0x44, 0x1b, 0x00, // +13 JUMP if equal to +0x1b
            0x02, 0x05, 0x01, 0x02, 0x00, 0x00, 0x00, // +16 MOVE PS1, IMM 2
            0x43, 0x22, 0x00, // +23 JUMP to +0x22 (the end)
            0x5a, 0x5a, 0x5a, 0x5a, // padding +26
            0x02, 0x05, 0x01, 0x01, 0x00, 0x00, 0x00, // +27 (0x1b) MOVE PS1, IMM 1
            0x5b, // +34 (0x22) EOT
        ];
        let img = image(&[(0, 0, 0, t)]);
        let io = Regs::new(&[]);
        let mut ps = [5u32, 0];
        Interpreter::new(&img, &io).run(0, &mut ps).unwrap();
        assert_eq!(ps[1], 1, "equal");
        let mut ps = [4u32, 0];
        Interpreter::new(&img, &io).run(0, &mut ps).unwrap();
        assert_eq!(ps[1], 2, "not equal");
    }

    #[test]
    fn a_register_block_and_the_index_register_shift() {
        // SET_REG_BLOCK 0x20; MOVE REG 0x1 <- IMM 9 (register 0x21);
        // MOVE REG 0x0 ... with the block cleared: register 0 takes the
        // value shifted left by 2.
        let t: &[u8] = &[
            0x3a, 0x20, 0x00, 0x01, 0x05, 0x01, 0x00, 0x09, 0x00, 0x00, 0x00, 0x3a, 0x00, 0x00,
            0x01, 0x05, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x5b,
        ];
        let img = image(&[(0, 0, 0, t)]);
        let io = Regs::new(&[]);
        Interpreter::new(&img, &io).run(0, &mut [0; 1]).unwrap();
        assert_eq!(io.get(0x21), 9);
        assert_eq!(io.get(0), 3 << 2);
    }

    #[test]
    fn an_id_operand_reads_the_data_block() {
        // SET_DATA_BLOCK 22; MOVE PS0 <- ID 0x4 (dword at 0x300 + 4).
        // MOVE PS: opcode 2, attr 4 (ID source, align 0), dst PS0, src ID 4.
        let t: Vec<u8> = vec![0x66, 22, 0x02, 0x04, 0x00, 0x04, 0x00, 0x5b];
        let img = image(&[(0, 0, 0, &t)]);
        let io = Regs::new(&[]);
        let mut ps = [0u32; 1];
        Interpreter::new(&img, &io).run(0, &mut ps).unwrap();
        assert_eq!(ps[0], 0xaabb_ccdd);
    }

    #[test]
    fn a_table_that_never_ends_is_stopped_and_a_missing_one_is_reported() {
        // JUMP to the start of the code, for ever.
        let t: &[u8] = &[0x43, 0x06, 0x00];
        let img = image(&[(0, 0, 0, t)]);
        let io = Regs::new(&[]);
        assert_eq!(
            Interpreter::new(&img, &io).run(0, &mut [0; 1]),
            Err(Error::Runaway)
        );
        assert_eq!(
            Interpreter::new(&img, &io).run(5, &mut [0; 1]),
            Err(Error::Absent)
        );
        // A table that calls itself runs out of depth.
        let t: &[u8] = &[0x52, 0x00, 0x5b];
        let img = image(&[(0, 0, 0, t)]);
        assert_eq!(
            Interpreter::new(&img, &io).run(0, &mut [0; 1]),
            Err(Error::TooDeep)
        );
    }

    #[test]
    fn delays_are_passed_on_and_unknown_opcodes_end_the_table() {
        // DELAY_MS 2, DELAY_US 5, then opcode 0x7f (ends), then a MOVE that
        // must not run.
        let t: &[u8] = &[
            0x50, 0x02, 0x51, 0x05, 0x7f, 0x01, 0x05, 0x10, 0x00, 0x01, 0x00, 0x00, 0x00,
        ];
        let img = image(&[(0, 0, 0, t)]);
        let io = Regs::new(&[]);
        Interpreter::new(&img, &io).run(0, &mut [0; 1]).unwrap();
        assert_eq!(*io.delays.borrow(), [2000, 5]);
        assert!(io.writes.borrow().is_empty());
    }

    #[test]
    fn the_raphael_asic_init_table_runs_against_the_real_image_when_it_is_here() {
        // The video BIOS is the vendor's, so it is not in the repository:
        // this runs when a copy is at tests/corpus/x870e/vbios.bin.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/corpus/x870e/vbios.bin"
        );
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let img = Image::parse(bytes).unwrap();
        // The command list of the specification's test vector.
        assert_eq!(img.command_table(0), Some(0xa516));
        assert_eq!(img.command_table(13), Some(0xaa44));
        assert_eq!(img.command_table(22), Some(0xa596));
        let (engine, memory) = img.boot_clocks().unwrap();
        let io = Regs::new(&[(0xde3, 512)]);
        let mut ps = [0u32; 16];
        ps[0] = engine;
        ps[1] = memory;
        Interpreter::new(&img, &io).run(0, &mut ps).unwrap();
        // The effects the specification lists (12 section 8): the index high
        // register set from the memory size, two registers or'd, and the
        // "ASIC init complete" bit of BIOS scratch 7 (register 0x53).
        assert_eq!(io.get(6), 0);
        assert_eq!(io.get(0xe20) & 3, 3);
        assert_eq!(io.get(0xc05) & 2, 2);
        assert_eq!(io.get(0x53) & 0x200, 0x200);
    }
}
