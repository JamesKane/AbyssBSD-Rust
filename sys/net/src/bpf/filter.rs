// SPDX-License-Identifier: BSD-2-Clause
//! The classic BPF machine: a program's validation (`bpf_validate`) and
//! its interpretation over a packet (`bpf_filter`).
//!
//! A program is checked once, when it is set; the interpreter then trusts
//! its jumps and scratch indices but never the packet: a load outside the
//! packet ends the program with 0 (the packet is not taken), as a
//! division by a zero `X` does.
//!
//! Behavior reference: sys/net/bpf_filter.c.

use abi::bpf::{BpfInsn, MEMWORDS, op};
use mbuf::Chain;

/// Valid opcodes, a bit per opcode in each 16-entry row
/// (`bpf_code_map`).
const CODE_MAP: [u16; 16] = [
    0x10ff, 0x3070, 0x3131, 0x3031, 0x3131, 0x1011, 0x1013, 0x1010, 0x0093, 0x1010, 0x1010, 0x0002,
    0, 0, 0, 0,
];

/// `BPF_VALIDATE_CODE`.
const fn valid_code(c: u16) -> bool {
    c <= 0xff && CODE_MAP[(c >> 4) as usize] & (1 << (c & 0xf)) != 0
}

/// Whether `prog` is a program the machine may run (`bpf_validate`):
/// known opcodes, forward jumps that land inside it, scratch indices in
/// range, no constant division by zero, and a return at the end. An
/// empty program is valid (it takes everything).
#[must_use]
pub fn validate(prog: &[BpfInsn]) -> bool {
    let len = prog.len();
    for (i, p) in prog.iter().enumerate() {
        if !valid_code(p.code) {
            return false;
        }
        if op::class(p.code) == op::JMP {
            let off = if p.code == op::JMP | op::JA {
                p.k as usize
            } else {
                usize::from(p.jt.max(p.jf))
            };
            if off >= len - i - 1 {
                return false;
            }
            continue;
        }
        if matches!(p.code, c if c == op::ST || c == op::STX || c == op::LD | op::MEM || c == op::LDX | op::MEM)
        {
            if p.k as usize >= MEMWORDS {
                return false;
            }
            continue;
        }
        if (p.code == op::ALU | op::DIV | op::K || p.code == op::ALU | op::MOD | op::K) && p.k == 0
        {
            return false;
        }
    }
    prog.last().is_none_or(|l| op::class(l.code) == op::RET)
}

/// What a program reads: a packet's bytes.
pub trait Source {
    /// The bytes there are (`buflen`).
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copies `dst.len()` bytes from `off`; `false` if they are not all
    /// there.
    fn read(&self, off: usize, dst: &mut [u8]) -> bool;
}

impl Source for [u8] {
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }

    fn read(&self, off: usize, dst: &mut [u8]) -> bool {
        match self.get(off..).and_then(|t| t.get(..dst.len())) {
            Some(b) => {
                dst.copy_from_slice(b);
                true
            }
            None => false,
        }
    }
}

impl Source for Chain {
    fn len(&self) -> usize {
        Chain::len(self)
    }

    fn read(&self, off: usize, dst: &mut [u8]) -> bool {
        off.checked_add(dst.len())
            .is_some_and(|e| e <= Chain::len(self))
            && self.copy_out(off, dst) == dst.len()
    }
}

/// A link header before a packet's bytes (`bpf_mtap2`'s `data`), which
/// the program sees as one with them.
pub struct Prefixed<'a, S: Source + ?Sized> {
    pub head: &'a [u8],
    pub rest: &'a S,
}

impl<S: Source + ?Sized> core::fmt::Debug for Prefixed<'_, S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Prefixed")
            .field("head", &self.head.len())
            .finish_non_exhaustive()
    }
}

impl<S: Source + ?Sized> Source for Prefixed<'_, S> {
    fn len(&self) -> usize {
        self.head.len() + self.rest.len()
    }

    fn read(&self, off: usize, dst: &mut [u8]) -> bool {
        let Some(end) = off.checked_add(dst.len()) else {
            return false;
        };
        if end > self.len() {
            return false;
        }
        let h = self.head.len();
        let mut at = 0;
        if off < h {
            let n = (h - off).min(dst.len());
            dst[..n].copy_from_slice(&self.head[off..off + n]);
            at = n;
        }
        if at < dst.len() {
            let from = off + at - h;
            return self.rest.read(from, &mut dst[at..]);
        }
        true
    }
}

fn load<const N: usize, S: Source + ?Sized>(p: &S, off: u64) -> Option<[u8; N]> {
    let off = usize::try_from(off).ok()?;
    let mut b = [0; N];
    p.read(off, &mut b).then_some(b)
}

/// Runs `prog` over `pkt`, which was `wirelen` bytes on the wire
/// (`bpf_filter`): the bytes to capture, 0 to pass the packet by. No
/// program takes everything. `prog` must have passed [`validate`].
#[must_use]
pub fn run<S: Source + ?Sized>(prog: &[BpfInsn], pkt: &S, wirelen: u32) -> u32 {
    if prog.is_empty() {
        return u32::MAX;
    }
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut mem = [0u32; MEMWORDS];
    let mut pc = 0usize;
    loop {
        let Some(&i) = prog.get(pc) else {
            return 0;
        };
        pc += 1;
        let k = i.k;
        let ind = u64::from(x) + u64::from(k);
        const LD_W_ABS: u16 = op::LD | op::W | op::ABS;
        const LD_H_ABS: u16 = op::LD | op::H | op::ABS;
        const LD_B_ABS: u16 = op::LD | op::B | op::ABS;
        const LD_W_IND: u16 = op::LD | op::W | op::IND;
        const LD_H_IND: u16 = op::LD | op::H | op::IND;
        const LD_B_IND: u16 = op::LD | op::B | op::IND;
        const LD_W_LEN: u16 = op::LD | op::W | op::LEN;
        const LDX_W_LEN: u16 = op::LDX | op::W | op::LEN;
        const LDX_MSH: u16 = op::LDX | op::MSH | op::B;
        const LD_IMM: u16 = op::LD | op::IMM;
        const LDX_IMM: u16 = op::LDX | op::IMM;
        const LD_MEM: u16 = op::LD | op::MEM;
        const LDX_MEM: u16 = op::LDX | op::MEM;
        const RET_K: u16 = op::RET | op::K;
        const RET_A: u16 = op::RET | op::A;
        const JA: u16 = op::JMP | op::JA;
        const JGT_K: u16 = op::JMP | op::JGT | op::K;
        const JGE_K: u16 = op::JMP | op::JGE | op::K;
        const JEQ_K: u16 = op::JMP | op::JEQ | op::K;
        const JSET_K: u16 = op::JMP | op::JSET | op::K;
        const JGT_X: u16 = op::JMP | op::JGT | op::X;
        const JGE_X: u16 = op::JMP | op::JGE | op::X;
        const JEQ_X: u16 = op::JMP | op::JEQ | op::X;
        const JSET_X: u16 = op::JMP | op::JSET | op::X;
        const ALU: u16 = op::ALU;
        const TAX: u16 = op::MISC | op::TAX;
        const TXA: u16 = op::MISC | op::TXA;
        let cond = |c: bool| usize::from(if c { i.jt } else { i.jf });
        match i.code {
            RET_K => return k,
            RET_A => return a,
            LD_W_ABS => match load::<4, _>(pkt, k.into()) {
                Some(b) => a = u32::from_be_bytes(b),
                None => return 0,
            },
            LD_H_ABS => match load::<2, _>(pkt, k.into()) {
                Some(b) => a = u32::from(u16::from_be_bytes(b)),
                None => return 0,
            },
            LD_B_ABS => match load::<1, _>(pkt, k.into()) {
                Some(b) => a = u32::from(b[0]),
                None => return 0,
            },
            LD_W_IND => match load::<4, _>(pkt, ind) {
                Some(b) => a = u32::from_be_bytes(b),
                None => return 0,
            },
            LD_H_IND => match load::<2, _>(pkt, ind) {
                Some(b) => a = u32::from(u16::from_be_bytes(b)),
                None => return 0,
            },
            LD_B_IND => match load::<1, _>(pkt, ind) {
                Some(b) => a = u32::from(b[0]),
                None => return 0,
            },
            LD_W_LEN => a = wirelen,
            LDX_W_LEN => x = wirelen,
            LDX_MSH => match load::<1, _>(pkt, k.into()) {
                Some(b) => x = u32::from(b[0] & 0xf) << 2,
                None => return 0,
            },
            LD_IMM => a = k,
            LDX_IMM => x = k,
            LD_MEM => a = mem[k as usize % MEMWORDS],
            LDX_MEM => x = mem[k as usize % MEMWORDS],
            op::ST => mem[k as usize % MEMWORDS] = a,
            op::STX => mem[k as usize % MEMWORDS] = x,
            JA => pc += k as usize,
            JGT_K => pc += cond(a > k),
            JGE_K => pc += cond(a >= k),
            JEQ_K => pc += cond(a == k),
            JSET_K => pc += cond(a & k != 0),
            JGT_X => pc += cond(a > x),
            JGE_X => pc += cond(a >= x),
            JEQ_X => pc += cond(a == x),
            JSET_X => pc += cond(a & x != 0),
            c if op::class(c) == ALU => {
                let v = if c & op::X != 0 { x } else { k };
                a = match c & 0xf0 {
                    op::ADD => a.wrapping_add(v),
                    op::SUB => a.wrapping_sub(v),
                    op::MUL => a.wrapping_mul(v),
                    op::DIV => match a.checked_div(v) {
                        Some(q) => q,
                        None => return 0,
                    },
                    op::MOD => match a.checked_rem(v) {
                        Some(r) => r,
                        None => return 0,
                    },
                    op::AND => a & v,
                    op::OR => a | v,
                    op::XOR => a ^ v,
                    op::LSH => a.wrapping_shl(v),
                    op::RSH => a.wrapping_shr(v),
                    op::NEG if c & op::X == 0 => a.wrapping_neg(),
                    _ => return 0,
                };
            }
            TAX => x = a,
            TXA => a = x,
            _ => return 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn stmt(code: u16, k: u32) -> BpfInsn {
        BpfInsn {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }

    const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> BpfInsn {
        BpfInsn { code, jt, jf, k }
    }

    /// `tcpdump -dd arp`.
    const ARP: [BpfInsn; 4] = [
        stmt(op::LD | op::H | op::ABS, 12),
        jump(op::JMP | op::JEQ | op::K, 0x0806, 0, 1),
        stmt(op::RET | op::K, 262_144),
        stmt(op::RET | op::K, 0),
    ];

    fn frame(ty: u16, len: usize) -> std::vec::Vec<u8> {
        let mut f = std::vec![0u8; len];
        f[12..14].copy_from_slice(&ty.to_be_bytes());
        f
    }

    #[test]
    fn validates_as_freebsd() {
        assert!(validate(&[]));
        assert!(validate(&ARP));
        // No return at the end.
        assert!(!validate(&ARP[..2]));
        // A jump past the end.
        assert!(!validate(&[
            jump(op::JMP | op::JEQ | op::K, 0, 1, 0),
            stmt(op::RET | op::K, 0)
        ]));
        assert!(!validate(&[
            stmt(op::JMP | op::JA, 1),
            stmt(op::RET | op::K, 0)
        ]));
        // Scratch out of range, constant division by zero, a bad opcode.
        assert!(!validate(&[stmt(op::ST, 16), stmt(op::RET | op::K, 0)]));
        assert!(validate(&[stmt(op::ST, 15), stmt(op::RET | op::K, 0)]));
        assert!(!validate(&[
            stmt(op::ALU | op::DIV | op::K, 0),
            stmt(op::RET | op::K, 0)
        ]));
        assert!(!validate(&[
            stmt(op::ALU | op::MOD | op::K, 0),
            stmt(op::RET | op::K, 0)
        ]));
        assert!(!validate(&[stmt(0x18, 0), stmt(op::RET | op::K, 0)]));
        assert!(!validate(&[stmt(0x100 | op::RET, 0)]));
    }

    #[test]
    fn code_map_matches_the_opcodes() {
        let every = [
            op::LD | op::W | op::ABS,
            op::LD | op::H | op::ABS,
            op::LD | op::B | op::ABS,
            op::LD | op::W | op::IND,
            op::LD | op::H | op::IND,
            op::LD | op::B | op::IND,
            op::LD | op::W | op::LEN,
            op::LDX | op::W | op::LEN,
            op::LDX | op::MSH | op::B,
            op::LD | op::IMM,
            op::LDX | op::IMM,
            op::LD | op::MEM,
            op::LDX | op::MEM,
            op::ST,
            op::STX,
            op::RET | op::K,
            op::RET | op::A,
            op::MISC | op::TAX,
            op::MISC | op::TXA,
            op::ALU | op::NEG,
            op::JMP | op::JA,
        ];
        let mut n = every.len();
        for c in every {
            assert!(valid_code(c), "{c:#x}");
        }
        for o in [op::JGT, op::JGE, op::JEQ, op::JSET] {
            for s in [op::K, op::X] {
                assert!(valid_code(op::JMP | o | s));
                n += 1;
            }
        }
        for o in [
            op::ADD,
            op::SUB,
            op::MUL,
            op::DIV,
            op::MOD,
            op::AND,
            op::OR,
            op::XOR,
            op::LSH,
            op::RSH,
        ] {
            for s in [op::K, op::X] {
                assert!(valid_code(op::ALU | o | s));
                n += 1;
            }
        }
        assert_eq!((0..256).filter(|&c| valid_code(c)).count(), n);
    }

    #[test]
    fn filters_arp() {
        assert_eq!(run(&ARP, &frame(0x0806, 60)[..], 60), 262_144);
        assert_eq!(run(&ARP, &frame(0x0800, 60)[..], 60), 0);
        // Too short to hold the type.
        assert_eq!(run(&ARP, &frame(0x0806, 60)[..13], 60), 0);
        assert_eq!(run(&[], &[][..], 0), u32::MAX);
    }

    #[test]
    fn indexes_through_the_ip_header() {
        // `tcpdump -dd udp dst port 68` on an untagged IPv4 frame, short.
        let prog = [
            stmt(op::LD | op::H | op::ABS, 12),
            jump(op::JMP | op::JEQ | op::K, 0x0800, 0, 8),
            stmt(op::LD | op::B | op::ABS, 23),
            jump(op::JMP | op::JEQ | op::K, 17, 0, 6),
            stmt(op::LD | op::H | op::ABS, 20),
            jump(op::JMP | op::JSET | op::K, 0x1fff, 4, 0),
            stmt(op::LDX | op::MSH | op::B, 14),
            stmt(op::LD | op::H | op::IND, 16),
            jump(op::JMP | op::JEQ | op::K, 68, 0, 1),
            stmt(op::RET | op::K, 1500),
            stmt(op::RET | op::K, 0),
        ];
        assert!(validate(&prog));
        let mut f = frame(0x0800, 14 + 24 + 8);
        f[14] = 0x46; // IPv4, 24-byte header.
        f[23] = 17;
        f[14 + 24 + 2..14 + 24 + 4].copy_from_slice(&68u16.to_be_bytes());
        assert_eq!(run(&prog, &f[..], 46), 1500);
        // A fragment is passed by.
        f[20] = 0x20;
        f[21] = 1;
        assert_eq!(run(&prog, &f[..], 46), 0);
        // And the same through a chain, split mid-header.
        f[20] = 0;
        f[21] = 0;
        let mut c = Chain::from_slice(&f[..20]).unwrap();
        c.extend_from_slice(&f[20..]).unwrap();
        assert_eq!(run(&prog, &c, 46), 1500);
        // With the link header given apart.
        let p = Prefixed {
            head: &f[..14],
            rest: &f[14..],
        };
        assert_eq!(run(&prog, &p, 46), 1500);
    }

    #[test]
    fn arithmetic_wraps_and_division_by_zero_rejects() {
        let prog = [
            stmt(op::LD | op::IMM, 7),
            stmt(op::LDX | op::IMM, 0),
            stmt(op::ALU | op::DIV | op::X, 0),
            stmt(op::RET | op::A, 0),
        ];
        assert_eq!(run(&prog, &[][..], 0), 0);
        let prog = [
            stmt(op::LD | op::IMM, 1),
            stmt(op::ALU | op::NEG, 0),
            stmt(op::ST, 3),
            stmt(op::LDX | op::MEM, 3),
            stmt(op::MISC | op::TXA, 0),
            stmt(op::ALU | op::ADD | op::K, 2),
            stmt(op::RET | op::A, 0),
        ];
        assert_eq!(run(&prog, &[][..], 0), 1);
        let prog = [
            stmt(op::LD | op::W | op::LEN, 0),
            stmt(op::ALU | op::MOD | op::K, 7),
            stmt(op::RET | op::A, 0),
        ];
        assert_eq!(run(&prog, &[][..], 100), 2);
    }
}
