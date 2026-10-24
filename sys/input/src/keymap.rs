// SPDX-License-Identifier: BSD-2-Clause
//! The kbd layer's tables, which every keyboard starts with a copy of:
//! FreeBSD's default keymap (US, ISO 8859-1), its accent map, and the
//! function key strings, from sys/dev/kbd/kbdtables.h. `GIO_KEYMAP`
//! returns them as they are, so they must match FreeBSD's entry for
//! entry.

use alloc::vec::Vec;

use abi::kbio::spcl::*;
use abi::kbio::{
    AccT, AccentMap, FkeyTab, KeyEnt, Keymap, MAXFK, NUM_ACCENTCHARS, NUM_DEADKEYS, NUM_FKEYS,
    NUM_KEYS,
};

/// A keyboard's tables, on the heap (a whole keymap would not fit on a
/// kernel stack).
#[derive(Clone, Debug)]
pub struct Maps {
    /// `keymap_t`: `n_keys` and [`NUM_KEYS`] entries.
    pub n_keys: u16,
    pub keys: Vec<KeyEnt>,
    /// `accentmap_t`: `n_accs` and [`NUM_DEADKEYS`] entries.
    pub n_accs: u16,
    pub accents: Vec<AccT>,
    /// [`NUM_FKEYS`] function key strings.
    pub fkeys: Vec<FkeyTab>,
}

impl Maps {
    /// FreeBSD's defaults.
    #[must_use]
    pub fn default_maps() -> Self {
        let mut keys = alloc::vec![KeyEnt::default(); NUM_KEYS];
        for (k, &(map, spcl, flgs)) in keys.iter_mut().zip(KEYS.iter()) {
            *k = KeyEnt { map, spcl, flgs };
        }
        let mut accents = alloc::vec![AccT::EMPTY; NUM_DEADKEYS];
        for (a, &(accchar, pairs)) in accents.iter_mut().zip(ACCENTS.iter()) {
            a.accchar = accchar;
            for (slot, &(from, to)) in a.map.iter_mut().zip(pairs).take(NUM_ACCENTCHARS) {
                *slot = [u32::from(from), u32::from(to)];
            }
        }
        let mut fkeys = alloc::vec![FkeyTab::default(); NUM_FKEYS];
        for (f, s) in fkeys.iter_mut().zip(FKEYS) {
            let n = s.len().min(MAXFK);
            f.str[..n].copy_from_slice(&s[..n]);
            f.len = u8::try_from(n).unwrap_or(0);
        }
        Self {
            n_keys: 0x6d,
            keys,
            n_accs: 15,
            accents,
            fkeys,
        }
    }

    /// Writes the keymap as `keymap_t` to `b` ([`Keymap::SIZE`] bytes).
    pub fn write_keymap(&self, b: &mut [u8]) {
        b[..4].fill(0);
        b[..2].copy_from_slice(&self.n_keys.to_ne_bytes());
        for (k, chunk) in self.keys.iter().zip(b[4..].chunks_mut(KeyEnt::SIZE)) {
            chunk.fill(0);
            k.write(chunk);
        }
    }

    /// Loads a `keymap_t`; `false` if `b` is short.
    pub fn read_keymap(&mut self, b: &[u8]) -> bool {
        if b.len() < Keymap::SIZE {
            return false;
        }
        self.n_keys = u16::from_ne_bytes([b[0], b[1]]);
        for (k, chunk) in self.keys.iter_mut().zip(b[4..].chunks(KeyEnt::SIZE)) {
            *k = KeyEnt::read(chunk).unwrap_or_default();
        }
        true
    }

    /// Writes the accent map as `accentmap_t` to `b` ([`AccentMap::SIZE`]
    /// bytes).
    pub fn write_accents(&self, b: &mut [u8]) {
        b[..4].fill(0);
        b[..2].copy_from_slice(&self.n_accs.to_ne_bytes());
        for (a, chunk) in self.accents.iter().zip(b[4..].chunks_mut(AccT::SIZE)) {
            a.write(chunk);
        }
    }

    /// Loads an `accentmap_t`; `false` if `b` is short.
    pub fn read_accents(&mut self, b: &[u8]) -> bool {
        if b.len() < AccentMap::SIZE {
            return false;
        }
        self.n_accs = u16::from_ne_bytes([b[0], b[1]]);
        for (a, chunk) in self.accents.iter_mut().zip(b[4..].chunks(AccT::SIZE)) {
            *a = AccT::read(chunk).unwrap_or(AccT::EMPTY);
        }
        true
    }

    /// Empties the accent map (a new keymap does).
    pub fn clear_accents(&mut self) {
        self.n_accs = 0;
        self.accents.fill(AccT::EMPTY);
    }
}

/// A character's code (`From` is not usable in a constant).
const fn ch(c: char) -> u32 {
    c as u32
}

/// `key_map`: 109 keys (`n_keys` 0x6d).
#[rustfmt::skip]
const KEYS: [([u32; 8], u8, u8); 109] = [
    /* 00 */ ([NOP, NOP, NOP, NOP, NOP, NOP, NOP, NOP], 0xff, 0x00),
    /* 01 */ ([0x1b, 0x1b, 0x1b, 0x1b, 0x1b, 0x1b, DBG, 0x1b], 0x02, 0x00),
    /* 02 */ ([ch('1'), ch('!'), NOP, NOP, ch('1'), ch('!'), NOP, NOP], 0x33, 0x00),
    /* 03 */ ([ch('2'), ch('@'), 0x00, 0x00, ch('2'), ch('@'), 0x00, 0x00], 0x00, 0x00),
    /* 04 */ ([ch('3'), ch('#'), NOP, NOP, ch('3'), ch('#'), NOP, NOP], 0x33, 0x00),
    /* 05 */ ([ch('4'), ch('$'), NOP, NOP, ch('4'), ch('$'), NOP, NOP], 0x33, 0x00),
    /* 06 */ ([ch('5'), ch('%'), NOP, NOP, ch('5'), ch('%'), NOP, NOP], 0x33, 0x00),
    /* 07 */ ([ch('6'), ch('^'), 0x1e, 0x1e, ch('6'), ch('^'), 0x1e, 0x1e], 0x00, 0x00),
    /* 08 */ ([ch('7'), ch('&'), NOP, NOP, ch('7'), ch('&'), NOP, NOP], 0x33, 0x00),
    /* 09 */ ([ch('8'), ch('*'), NOP, NOP, ch('8'), ch('*'), NOP, NOP], 0x33, 0x00),
    /* 0a */ ([ch('9'), ch('('), NOP, NOP, ch('9'), ch('('), NOP, NOP], 0x33, 0x00),
    /* 0b */ ([ch('0'), ch(')'), NOP, NOP, ch('0'), ch(')'), NOP, NOP], 0x33, 0x00),
    /* 0c */ ([ch('-'), ch('_'), 0x1f, 0x1f, ch('-'), ch('_'), 0x1f, 0x1f], 0x00, 0x00),
    /* 0d */ ([ch('='), ch('+'), NOP, NOP, ch('='), ch('+'), NOP, NOP], 0x33, 0x00),
    /* 0e */ ([0x08, 0x08, 0x7f, 0x7f, 0x08, 0x08, 0x7f, 0x7f], 0x00, 0x00),
    /* 0f */ ([0x09, BTAB, NOP, NOP, 0x09, BTAB, NOP, NOP], 0x77, 0x00),
    /* 10 */ ([ch('q'), ch('Q'), 0x11, 0x11, ch('q'), ch('Q'), 0x11, 0x11], 0x00, 0x01),
    /* 11 */ ([ch('w'), ch('W'), 0x17, 0x17, ch('w'), ch('W'), 0x17, 0x17], 0x00, 0x01),
    /* 12 */ ([ch('e'), ch('E'), 0x05, 0x05, ch('e'), ch('E'), 0x05, 0x05], 0x00, 0x01),
    /* 13 */ ([ch('r'), ch('R'), 0x12, 0x12, ch('r'), ch('R'), 0x12, 0x12], 0x00, 0x01),
    /* 14 */ ([ch('t'), ch('T'), 0x14, 0x14, ch('t'), ch('T'), 0x14, 0x14], 0x00, 0x01),
    /* 15 */ ([ch('y'), ch('Y'), 0x19, 0x19, ch('y'), ch('Y'), 0x19, 0x19], 0x00, 0x01),
    /* 16 */ ([ch('u'), ch('U'), 0x15, 0x15, ch('u'), ch('U'), 0x15, 0x15], 0x00, 0x01),
    /* 17 */ ([ch('i'), ch('I'), 0x09, 0x09, ch('i'), ch('I'), 0x09, 0x09], 0x00, 0x01),
    /* 18 */ ([ch('o'), ch('O'), 0x0f, 0x0f, ch('o'), ch('O'), 0x0f, 0x0f], 0x00, 0x01),
    /* 19 */ ([ch('p'), ch('P'), 0x10, 0x10, ch('p'), ch('P'), 0x10, 0x10], 0x00, 0x01),
    /* 1a */ ([ch('['), ch('{'), 0x1b, 0x1b, ch('['), ch('{'), 0x1b, 0x1b], 0x00, 0x00),
    /* 1b */ ([ch(']'), ch('}'), 0x1d, 0x1d, ch(']'), ch('}'), 0x1d, 0x1d], 0x00, 0x00),
    /* 1c */ ([0x0d, 0x0d, 0x0a, 0x0a, 0x0d, 0x0d, 0x0a, 0x0a], 0x00, 0x00),
    /* 1d */ ([LCTR, LCTR, LCTR, LCTR, LCTR, LCTR, LCTR, LCTR], 0xff, 0x00),
    /* 1e */ ([ch('a'), ch('A'), 0x01, 0x01, ch('a'), ch('A'), 0x01, 0x01], 0x00, 0x01),
    /* 1f */ ([ch('s'), ch('S'), 0x13, 0x13, ch('s'), ch('S'), 0x13, 0x13], 0x00, 0x01),
    /* 20 */ ([ch('d'), ch('D'), 0x04, 0x04, ch('d'), ch('D'), 0x04, 0x04], 0x00, 0x01),
    /* 21 */ ([ch('f'), ch('F'), 0x06, 0x06, ch('f'), ch('F'), 0x06, 0x06], 0x00, 0x01),
    /* 22 */ ([ch('g'), ch('G'), 0x07, 0x07, ch('g'), ch('G'), 0x07, 0x07], 0x00, 0x01),
    /* 23 */ ([ch('h'), ch('H'), 0x08, 0x08, ch('h'), ch('H'), 0x08, 0x08], 0x00, 0x01),
    /* 24 */ ([ch('j'), ch('J'), 0x0a, 0x0a, ch('j'), ch('J'), 0x0a, 0x0a], 0x00, 0x01),
    /* 25 */ ([ch('k'), ch('K'), 0x0b, 0x0b, ch('k'), ch('K'), 0x0b, 0x0b], 0x00, 0x01),
    /* 26 */ ([ch('l'), ch('L'), 0x0c, 0x0c, ch('l'), ch('L'), 0x0c, 0x0c], 0x00, 0x01),
    /* 27 */ ([ch(';'), ch(':'), NOP, NOP, ch(';'), ch(':'), NOP, NOP], 0x33, 0x00),
    /* 28 */ ([ch('\''), ch('"'), NOP, NOP, ch('\''), ch('"'), NOP, NOP], 0x33, 0x00),
    /* 29 */ ([ch('`'), ch('~'), NOP, NOP, ch('`'), ch('~'), NOP, NOP], 0x33, 0x00),
    /* 2a */ ([LSH, LSH, LSH, LSH, LSH, LSH, LSH, LSH], 0xff, 0x00),
    /* 2b */ ([ch('\\'), ch('|'), 0x1c, 0x1c, ch('\\'), ch('|'), 0x1c, 0x1c], 0x00, 0x00),
    /* 2c */ ([ch('z'), ch('Z'), 0x1a, 0x1a, ch('z'), ch('Z'), 0x1a, 0x1a], 0x00, 0x01),
    /* 2d */ ([ch('x'), ch('X'), 0x18, 0x18, ch('x'), ch('X'), 0x18, 0x18], 0x00, 0x01),
    /* 2e */ ([ch('c'), ch('C'), 0x03, 0x03, ch('c'), ch('C'), 0x03, 0x03], 0x00, 0x01),
    /* 2f */ ([ch('v'), ch('V'), 0x16, 0x16, ch('v'), ch('V'), 0x16, 0x16], 0x00, 0x01),
    /* 30 */ ([ch('b'), ch('B'), 0x02, 0x02, ch('b'), ch('B'), 0x02, 0x02], 0x00, 0x01),
    /* 31 */ ([ch('n'), ch('N'), 0x0e, 0x0e, ch('n'), ch('N'), 0x0e, 0x0e], 0x00, 0x01),
    /* 32 */ ([ch('m'), ch('M'), 0x0d, 0x0d, ch('m'), ch('M'), 0x0d, 0x0d], 0x00, 0x01),
    /* 33 */ ([ch(','), ch('<'), NOP, NOP, ch(','), ch('<'), NOP, NOP], 0x33, 0x00),
    /* 34 */ ([ch('.'), ch('>'), NOP, NOP, ch('.'), ch('>'), NOP, NOP], 0x33, 0x00),
    /* 35 */ ([ch('/'), ch('?'), NOP, NOP, ch('/'), ch('?'), NOP, NOP], 0x33, 0x00),
    /* 36 */ ([RSH, RSH, RSH, RSH, RSH, RSH, RSH, RSH], 0xff, 0x00),
    /* 37 */ ([ch('*'), ch('*'), ch('*'), ch('*'), ch('*'), ch('*'), ch('*'), ch('*')], 0x00, 0x00),
    /* 38 */ ([LALT, LALT, LALT, LALT, LALT, LALT, LALT, LALT], 0xff, 0x00),
    /* 39 */ ([ch(' '), ch(' '), 0x00, ch(' '), ch(' '), ch(' '), SUSP, ch(' ')], 0x02, 0x00),
    /* 3a */ ([CLK, CLK, CLK, CLK, CLK, CLK, CLK, CLK], 0xff, 0x00),
    /* 3b */ ([f(1), f(13), f(25), f(37), s(1), s(11), s(1), s(11)], 0xff, 0x00),
    /* 3c */ ([f(2), f(14), f(26), f(38), s(2), s(12), s(2), s(12)], 0xff, 0x00),
    /* 3d */ ([f(3), f(15), f(27), f(39), s(3), s(13), s(3), s(13)], 0xff, 0x00),
    /* 3e */ ([f(4), f(16), f(28), f(40), s(4), s(14), s(4), s(14)], 0xff, 0x00),
    /* 3f */ ([f(5), f(17), f(29), f(41), s(5), s(15), s(5), s(15)], 0xff, 0x00),
    /* 40 */ ([f(6), f(18), f(30), f(42), s(6), s(16), s(6), s(16)], 0xff, 0x00),
    /* 41 */ ([f(7), f(19), f(31), f(43), s(7), s(7), s(7), s(7)], 0xff, 0x00),
    /* 42 */ ([f(8), f(20), f(32), f(44), s(8), s(8), s(8), s(8)], 0xff, 0x00),
    /* 43 */ ([f(9), f(21), f(33), f(45), s(9), s(9), s(9), s(9)], 0xff, 0x00),
    /* 44 */ ([f(10), f(22), f(34), f(46), s(10), s(10), s(10), s(10)], 0xff, 0x00),
    /* 45 */ ([NLK, NLK, NLK, NLK, NLK, NLK, NLK, NLK], 0xff, 0x00),
    /* 46 */ ([SLK, SLK, SLK, SLK, SLK, SLK, SLK, SLK], 0xff, 0x00),
    /* 47 */ ([f(49), ch('7'), ch('7'), ch('7'), ch('7'), ch('7'), ch('7'), ch('7')], 0x80, 0x02),
    /* 48 */ ([f(50), ch('8'), ch('8'), ch('8'), ch('8'), ch('8'), ch('8'), ch('8')], 0x80, 0x02),
    /* 49 */ ([f(51), ch('9'), ch('9'), ch('9'), ch('9'), ch('9'), ch('9'), ch('9')], 0x80, 0x02),
    /* 4a */ ([f(52), ch('-'), ch('-'), ch('-'), ch('-'), ch('-'), ch('-'), ch('-')], 0x80, 0x02),
    /* 4b */ ([f(53), ch('4'), ch('4'), ch('4'), ch('4'), ch('4'), ch('4'), ch('4')], 0x80, 0x02),
    /* 4c */ ([f(54), ch('5'), ch('5'), ch('5'), ch('5'), ch('5'), ch('5'), ch('5')], 0x80, 0x02),
    /* 4d */ ([f(55), ch('6'), ch('6'), ch('6'), ch('6'), ch('6'), ch('6'), ch('6')], 0x80, 0x02),
    /* 4e */ ([f(56), ch('+'), ch('+'), ch('+'), ch('+'), ch('+'), ch('+'), ch('+')], 0x80, 0x02),
    /* 4f */ ([f(57), ch('1'), ch('1'), ch('1'), ch('1'), ch('1'), ch('1'), ch('1')], 0x80, 0x02),
    /* 50 */ ([f(58), ch('2'), ch('2'), ch('2'), ch('2'), ch('2'), ch('2'), ch('2')], 0x80, 0x02),
    /* 51 */ ([f(59), ch('3'), ch('3'), ch('3'), ch('3'), ch('3'), ch('3'), ch('3')], 0x80, 0x02),
    /* 52 */ ([f(60), ch('0'), ch('0'), ch('0'), ch('0'), ch('0'), ch('0'), ch('0')], 0x80, 0x02),
    /* 53 */ ([0x7f, ch('.'), ch('.'), ch('.'), ch('.'), ch('.'), RBT, RBT], 0x03, 0x02),
    /* 54 */ ([NOP, NOP, NOP, NOP, NOP, NOP, NOP, NOP], 0xff, 0x00),
    /* 55 */ ([NOP, NOP, NOP, NOP, NOP, NOP, NOP, NOP], 0xff, 0x00),
    /* 56 */ ([NOP, NOP, NOP, NOP, NOP, NOP, NOP, NOP], 0xff, 0x00),
    /* 57 */ ([f(11), f(23), f(35), f(47), s(11), s(11), s(11), s(11)], 0xff, 0x00),
    /* 58 */ ([f(12), f(24), f(36), f(48), s(12), s(12), s(12), s(12)], 0xff, 0x00),
    /* 59 */ ([0x0d, 0x0d, 0x0a, 0x0a, 0x0d, 0x0d, 0x0a, 0x0a], 0x00, 0x00),
    /* 5a */ ([RCTR, RCTR, RCTR, RCTR, RCTR, RCTR, RCTR, RCTR], 0xff, 0x00),
    /* 5b */ ([ch('/'), ch('/'), ch('/'), ch('/'), ch('/'), ch('/'), ch('/'), ch('/')], 0x00, 0x02),
    /* 5c */ ([NEXT, PREV, DBG, DBG, NOP, NOP, NOP, NOP], 0xff, 0x00),
    /* 5d */ ([RALT, RALT, RALT, RALT, RALT, RALT, RALT, RALT], 0xff, 0x00),
    /* 5e */ ([f(49), f(49), f(49), f(49), f(49), f(49), f(49), f(49)], 0xff, 0x00),
    /* 5f */ ([f(50), f(50), f(50), f(50), f(50), f(50), f(50), f(50)], 0xff, 0x00),
    /* 60 */ ([f(51), f(51), f(51), f(51), f(51), f(51), f(51), f(51)], 0xff, 0x00),
    /* 61 */ ([f(53), f(53), f(53), f(53), f(53), f(53), f(53), f(53)], 0xff, 0x00),
    /* 62 */ ([f(55), f(55), f(55), f(55), f(55), f(55), f(55), f(55)], 0xff, 0x00),
    /* 63 */ ([f(57), f(57), f(57), f(57), f(57), f(57), f(57), f(57)], 0xff, 0x00),
    /* 64 */ ([f(58), f(58), f(58), f(58), f(58), f(58), f(58), f(58)], 0xff, 0x00),
    /* 65 */ ([f(59), f(59), f(59), f(59), f(59), f(59), f(59), f(59)], 0xff, 0x00),
    /* 66 */ ([f(60), PASTE, f(60), f(60), f(60), f(60), f(60), f(60)], 0xff, 0x00),
    /* 67 */ ([f(61), f(61), f(61), f(61), f(61), f(61), RBT, f(61)], 0xff, 0x00),
    /* 68 */ ([SLK, SPSC, SLK, SPSC, SUSP, NOP, SUSP, NOP], 0xff, 0x00),
    /* 69 */ ([f(62), f(62), f(62), f(62), f(62), f(62), f(62), f(62)], 0xff, 0x00),
    /* 6a */ ([f(63), f(63), f(63), f(63), f(63), f(63), f(63), f(63)], 0xff, 0x00),
    /* 6b */ ([f(64), f(64), f(64), f(64), f(64), f(64), f(64), f(64)], 0xff, 0x00),
    /* 6c */ ([NOP, NOP, NOP, NOP, NOP, NOP, NOP, NOP], 0xff, 0x00),
];

/// `accent_map` (ISO 8859-1): each dead key's character and pairs.
#[rustfmt::skip]
const ACCENTS: [(u32, &[(u8, u8)]); 15] = [
    /* dgra */ (96, &[(b'a', 0xe0), (b'A', 0xc0), (b'e', 0xe8), (b'E', 0xc8), (b'i', 0xec), (b'I', 0xcc), (b'o', 0xf2), (b'O', 0xd2), (b'u', 0xf9), (b'U', 0xd9)]),
    /* dacu */ (0xb4, &[(b'a', 0xe1), (b'A', 0xc1), (b'e', 0xe9), (b'E', 0xc9), (b'i', 0xed), (b'I', 0xcd), (b'o', 0xf3), (b'O', 0xd3), (b'u', 0xfa), (b'U', 0xda), (b'y', 0xfd), (b'Y', 0xdd)]),
    /* dcir */ (94, &[(b'a', 0xe2), (b'A', 0xc2), (b'e', 0xea), (b'E', 0xca), (b'i', 0xee), (b'I', 0xce), (b'o', 0xf4), (b'O', 0xd4), (b'u', 0xfb), (b'U', 0xdb)]),
    /* dtil */ (126, &[(b'a', 0xe3), (b'A', 0xc3), (b'n', 0xf1), (b'N', 0xd1), (b'o', 0xf5), (b'O', 0xd5)]),
    /* dmac */ (0, &[]),
    /* dbre */ (0, &[]),
    /* ddot */ (0, &[]),
    /* duml */ (0xa8, &[(b'a', 0xe4), (b'A', 0xc4), (b'e', 0xeb), (b'E', 0xcb), (b'i', 0xef), (b'I', 0xcf), (b'o', 0xf6), (b'O', 0xd6), (b'u', 0xfc), (b'U', 0xdc), (b'y', 0xff)]),
    /* dsla */ (0, &[]),
    /* drin */ (0xb0, &[(b'a', 0xe5), (b'A', 0xc5)]),
    /* dced */ (0xb8, &[(b'c', 0xe7), (b'C', 0xc7)]),
    /* dapo */ (0, &[]),
    /* ddac */ (0, &[]),
    /* dogo */ (0, &[]),
    /* dcar */ (0, &[]),
];

/// `fkey_tab`: the strings `genkbd` reads give function keys.
#[rustfmt::skip]
const FKEYS: [&[u8]; 96] = [
    b"\x1b[M", b"\x1b[N", b"\x1b[O", b"\x1b[P",
    b"\x1b[Q", b"\x1b[R", b"\x1b[S", b"\x1b[T",
    b"\x1b[U", b"\x1b[V", b"\x1b[W", b"\x1b[X",
    b"\x1b[Y", b"\x1b[Z", b"\x1b[a", b"\x1b[b",
    b"\x1b[c", b"\x1b[d", b"\x1b[e", b"\x1b[f",
    b"\x1b[g", b"\x1b[h", b"\x1b[i", b"\x1b[j",
    b"\x1b[k", b"\x1b[l", b"\x1b[m", b"\x1b[n",
    b"\x1b[o", b"\x1b[p", b"\x1b[q", b"\x1b[r",
    b"\x1b[s", b"\x1b[t", b"\x1b[u", b"\x1b[v",
    b"\x1b[w", b"\x1b[x", b"\x1b[y", b"\x1b[z",
    b"\x1b[@", b"\x1b[[", b"\x1b[\\", b"\x1b[]",
    b"\x1b[^", b"\x1b[_", b"\x1b[`", b"\x1b[{",
    b"\x1b[H", b"\x1b[A", b"\x1b[I", b"-",
    b"\x1b[D", b"\x1b[E", b"\x1b[C", b"+",
    b"\x1b[F", b"\x1b[B", b"\x1b[G", b"\x1b[L",
    b"\x7f", b"\x1b[J", b"\x1b[~", b"\x1b[}",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
    b"", b"", b"", b"",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_round_trip_through_their_c_layouts() {
        let m = Maps::default_maps();
        let mut b = alloc::vec![0u8; Keymap::SIZE];
        m.write_keymap(&mut b);
        assert_eq!(u16::from_ne_bytes([b[0], b[1]]), 0x6d);
        let mut n = Maps::default_maps();
        n.keys.fill(KeyEnt::default());
        assert!(n.read_keymap(&b));
        assert_eq!(n.keys, m.keys);
        let mut b = alloc::vec![0u8; AccentMap::SIZE];
        m.write_accents(&mut b);
        n.clear_accents();
        assert!(n.read_accents(&b));
        assert_eq!((n.n_accs, &n.accents), (15, &m.accents));
    }
}
