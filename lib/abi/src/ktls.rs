// SPDX-License-Identifier: BSD-2-Clause
//! Kernel TLS (`<sys/ktls.h>`, and the algorithm numbers of
//! `<crypto/cryptodev.h>` it takes).

/// `TCP_TXTLS_ENABLE` and `TCP_RXTLS_ENABLE`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TlsEnable {
    pub cipher_key: u64,
    pub iv: u64,
    pub auth_key: u64,
    pub cipher_algorithm: i32,
    pub cipher_key_len: i32,
    pub iv_len: i32,
    pub auth_algorithm: i32,
    pub auth_key_len: i32,
    pub flags: i32,
    pub tls_vmajor: u8,
    pub tls_vminor: u8,
    pub rec_seq: [u8; 8],
    pub _pad: [u8; 6],
}

/// The size of the argument before `rec_seq` came (`struct tls_enable_v0`).
pub const TLS_ENABLE_V0_SIZE: usize = 56;

/// `TLS_GET_RECORD`'s control message.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TlsGetRecord {
    pub tls_type: u8,
    pub tls_vmajor: u8,
    pub tls_vminor: u8,
    pub tls_length: u16,
}

pub const TLS_MAX_MSG_SIZE_V10_2: usize = 16384;
pub const TLS_MAX_PARAM_SIZE: i32 = 1024;
pub const TLS_AEAD_GCM_LEN: usize = 4;
pub const TLS_1_3_GCM_IV_LEN: usize = 12;
pub const TLS_CHACHA20_IV_LEN: usize = 12;
pub const TLS_CBC_IMPLICIT_IV_LEN: usize = 16;

pub const TLS_RLTYPE_ALERT: u8 = 21;
pub const TLS_RLTYPE_HANDSHAKE: u8 = 22;
pub const TLS_RLTYPE_APP: u8 = 23;

pub const TLS_MAJOR_VER_ONE: u8 = 3;
pub const TLS_MINOR_VER_ZERO: u8 = 1;
pub const TLS_MINOR_VER_ONE: u8 = 2;
pub const TLS_MINOR_VER_TWO: u8 = 3;
pub const TLS_MINOR_VER_THREE: u8 = 4;

pub const CRYPTO_SHA1_HMAC: i32 = 7;
pub const CRYPTO_AES_CBC: i32 = 11;
pub const CRYPTO_SHA2_256_HMAC: i32 = 18;
pub const CRYPTO_SHA2_384_HMAC: i32 = 19;
pub const CRYPTO_AES_NIST_GCM_16: i32 = 25;
pub const CRYPTO_AES_128_NIST_GMAC: i32 = 26;
pub const CRYPTO_AES_192_NIST_GMAC: i32 = 27;
pub const CRYPTO_AES_256_NIST_GMAC: i32 = 28;
pub const CRYPTO_CHACHA20_POLY1305: i32 = 41;

use crate::layout::{Arches, ConstValue};

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c(
        "TLS_MAX_MSG_SIZE_V10_2",
        "sys/ktls.h",
        TLS_MAX_MSG_SIZE_V10_2 as i64,
    ),
    c(
        "TLS_MAX_PARAM_SIZE",
        "sys/ktls.h",
        TLS_MAX_PARAM_SIZE as i64,
    ),
    c("TLS_AEAD_GCM_LEN", "sys/ktls.h", TLS_AEAD_GCM_LEN as i64),
    c(
        "TLS_1_3_GCM_IV_LEN",
        "sys/ktls.h",
        TLS_1_3_GCM_IV_LEN as i64,
    ),
    c(
        "TLS_CHACHA20_IV_LEN",
        "sys/ktls.h",
        TLS_CHACHA20_IV_LEN as i64,
    ),
    c(
        "TLS_CBC_IMPLICIT_IV_LEN",
        "sys/ktls.h",
        TLS_CBC_IMPLICIT_IV_LEN as i64,
    ),
    c("TLS_RLTYPE_ALERT", "sys/ktls.h", TLS_RLTYPE_ALERT as i64),
    c(
        "TLS_RLTYPE_HANDSHAKE",
        "sys/ktls.h",
        TLS_RLTYPE_HANDSHAKE as i64,
    ),
    c("TLS_RLTYPE_APP", "sys/ktls.h", TLS_RLTYPE_APP as i64),
    c("TLS_MAJOR_VER_ONE", "sys/ktls.h", TLS_MAJOR_VER_ONE as i64),
    c(
        "TLS_MINOR_VER_ZERO",
        "sys/ktls.h",
        TLS_MINOR_VER_ZERO as i64,
    ),
    c("TLS_MINOR_VER_ONE", "sys/ktls.h", TLS_MINOR_VER_ONE as i64),
    c("TLS_MINOR_VER_TWO", "sys/ktls.h", TLS_MINOR_VER_TWO as i64),
    c(
        "TLS_MINOR_VER_THREE",
        "sys/ktls.h",
        TLS_MINOR_VER_THREE as i64,
    ),
];
