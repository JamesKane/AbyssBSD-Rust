// SPDX-License-Identifier: BSD-2-Clause
//! MD5 (RFC 1321), for the one place IPv6 needs it: the node information
//! group an address joins, named by a hash of the host's name.
//!
//! Behavior reference: sys/kern/md5c.c (`MD5Init`, `MD5Update`,
//! `MD5Final`).

/// The digest of `data`.
#[must_use]
pub(crate) fn digest(data: &[u8]) -> [u8; 16] {
    let mut h = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let bits = (data.len() as u64).wrapping_mul(8);
    let mut tail = [0u8; 128];
    let full = data.len() - data.len() % 64;
    let rest = &data[full..];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let tlen = if rest.len() < 56 { 64 } else { 128 };
    tail[tlen - 8..tlen].copy_from_slice(&bits.to_le_bytes());
    let (blocks, _) = data[..full].as_chunks::<64>();
    for b in blocks {
        block(&mut h, b);
    }
    let (blocks, _) = tail[..tlen].as_chunks::<64>();
    for b in blocks {
        block(&mut h, b);
    }
    let mut out = [0u8; 16];
    for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = w.to_le_bytes();
    }
    out
}

/// The shift amounts of each round's steps.
const S: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];

/// `T[i]`: the integer part of `2^32 * |sin(i + 1)|`.
const T: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// One 64-byte block into state `h`.
fn block(h: &mut [u32; 4], b: &[u8; 64]) {
    let mut x = [0u32; 16];
    for (w, c) in x.iter_mut().zip(b.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*c);
    }
    let [mut a, mut bb, mut c, mut d] = *h;
    for i in 0..64 {
        let (f, g) = match i / 16 {
            0 => ((bb & c) | (!bb & d), i),
            1 => ((bb & d) | (c & !d), (5 * i + 1) % 16),
            2 => (bb ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (bb | !d), (7 * i) % 16),
        };
        let s = S[(i / 16) * 4 + i % 4];
        let t = a.wrapping_add(f).wrapping_add(x[g]).wrapping_add(T[i]);
        a = d;
        d = c;
        c = bb;
        bb = bb.wrapping_add(t.rotate_left(s));
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(bb);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
}

#[cfg(test)]
mod tests {
    use super::digest;

    fn hex(d: [u8; 16]) -> alloc::string::String {
        d.iter().map(|b| alloc::format!("{b:02x}")).collect()
    }

    /// RFC 1321's test suite.
    #[test]
    fn rfc1321_vectors() {
        assert_eq!(hex(digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(digest(b"a")), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(digest(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(digest(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }
}
