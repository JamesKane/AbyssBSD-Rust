// SPDX-License-Identifier: BSD-2-Clause
//! SHA-512 and SHA-384 (FIPS 180-4 §6.4, §6.5).

use crate::Hash;

const K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

const IV512: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

const IV384: [u64; 8] = [
    0xcbbb_9d5d_c105_9ed8,
    0x629a_292a_367c_d507,
    0x9159_015a_3070_dd17,
    0x152f_ecd8_f70e_5939,
    0x6733_2667_ffc0_0b31,
    0x8eb4_4a87_6858_1511,
    0xdb0c_2e0d_64f9_8fa7,
    0x47b5_481d_befa_4fa4,
];

/// The SHA-512 compression state, shared by SHA-512 and SHA-384.
#[derive(Clone, Debug)]
struct State {
    h: [u64; 8],
    block: [u8; 128],
    fill: usize,
    len: u128,
}

impl State {
    const fn new(h: [u64; 8]) -> Self {
        Self {
            h,
            block: [0; 128],
            fill: 0,
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u128);
        if self.fill != 0 {
            let n = (128 - self.fill).min(data.len());
            self.block[self.fill..self.fill + n].copy_from_slice(&data[..n]);
            self.fill += n;
            data = &data[n..];
            if self.fill < 128 {
                return;
            }
            let b = self.block;
            self.compress(&b);
            self.fill = 0;
        }
        let (blocks, rest) = data.as_chunks::<128>();
        for b in blocks {
            self.compress(b);
        }
        self.block[..rest.len()].copy_from_slice(rest);
        self.fill = rest.len();
    }

    fn finish(mut self) -> [u64; 8] {
        let bits = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        if self.fill > 112 {
            self.update(&[0; 128][..128 - self.fill]);
        }
        self.update(&[0; 128][..112 - self.fill]);
        self.update(&bits.to_be_bytes());
        self.h
    }

    fn compress(&mut self, b: &[u8; 128]) {
        let mut w = [0u64; 80];
        for (i, c) in b.as_chunks::<8>().0.iter().enumerate() {
            w[i] = u64::from_be_bytes(*c);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for (k, wi) in K.iter().zip(w) {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(*k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (h, v) in self.h.iter_mut().zip([a, bb, c, d, e, f, g, h]) {
            *h = h.wrapping_add(v);
        }
    }
}

fn output<const N: usize>(h: [u64; 8]) -> [u8; N] {
    let mut out = [0u8; N];
    for (o, h) in out.as_chunks_mut::<8>().0.iter_mut().zip(h) {
        *o = h.to_be_bytes();
    }
    out
}

/// A SHA-512 computation in progress.
#[derive(Clone, Debug)]
pub struct Sha512(State);

/// A SHA-384 computation in progress.
#[derive(Clone, Debug)]
pub struct Sha384(State);

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for Sha384 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    #[must_use]
    pub const fn new() -> Self {
        Self(State::new(IV512))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    #[must_use]
    pub fn finish(self) -> [u8; 64] {
        output(self.0.finish())
    }
}

impl Sha384 {
    #[must_use]
    pub const fn new() -> Self {
        Self(State::new(IV384))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    #[must_use]
    pub fn finish(self) -> [u8; 48] {
        output(self.0.finish())
    }
}

impl Hash for Sha512 {
    const BLOCK: usize = 128;
    type Digest = [u8; 64];
    fn new() -> Self {
        Self::new()
    }
    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }
    fn finish(self) -> [u8; 64] {
        self.finish()
    }
}

impl Hash for Sha384 {
    const BLOCK: usize = 128;
    type Digest = [u8; 48];
    fn new() -> Self {
        Self::new()
    }
    fn update(&mut self, data: &[u8]) {
        self.update(data);
    }
    fn finish(self) -> [u8; 48] {
        self.finish()
    }
}
