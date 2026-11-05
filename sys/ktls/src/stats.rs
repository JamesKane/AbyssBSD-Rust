// SPDX-License-Identifier: BSD-2-Clause
//! `kern.ipc.tls`'s knobs and counters.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering::Relaxed};

use crate::Suite;

/// `kern.ipc.tls.enable`.
pub static ENABLE: AtomicBool = AtomicBool::new(true);
/// `kern.ipc.tls.cbc_enable`.
pub static CBC_ENABLE: AtomicBool = AtomicBool::new(true);
/// `kern.ipc.tls.stats.threads`.
pub static THREADS: AtomicI32 = AtomicI32::new(0);
/// `kern.ipc.tls.stats.enable_calls`.
pub static ENABLE_CALLS: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.stats.offload_total`.
pub static OFFLOAD_TOTAL: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.stats.active`.
pub static ACTIVE: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.stats.corrupted_records`.
pub static CORRUPTED_RECORDS: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.stats.failed_crypto`.
pub static FAILED_CRYPTO: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.stats.sw_rx_inqueue`.
pub static SW_RX_INQUEUE: AtomicU64 = AtomicU64::new(0);
/// `kern.ipc.tls.sw.{cbc,gcm,chacha20}`: sessions in software.
pub static SW_CBC: AtomicU64 = AtomicU64::new(0);
pub static SW_GCM: AtomicU64 = AtomicU64::new(0);
pub static SW_CHACHA20: AtomicU64 = AtomicU64::new(0);

/// `kern.ipc.tls.stats.ocf.*`: records by kind, in the order of [`OCF_NAMES`].
pub static OCF: [AtomicU64; OCF_NAMES.len()] = [const { AtomicU64::new(0) }; OCF_NAMES.len()];

/// Each counter of [`OCF`], with its description.
pub const OCF_NAMES: [(&str, &str); 16] = [
    (
        "tls10_cbc_encrypts",
        "Total number of OCF TLS 1.0 CBC encryption operations",
    ),
    (
        "tls11_cbc_decrypts",
        "Total number of OCF TLS 1.1/1.2 CBC decryption operations",
    ),
    (
        "tls11_cbc_encrypts",
        "Total number of OCF TLS 1.1/1.2 CBC encryption operations",
    ),
    (
        "tls12_gcm_decrypts",
        "Total number of OCF TLS 1.2 GCM decryption operations",
    ),
    (
        "tls12_gcm_encrypts",
        "Total number of OCF TLS 1.2 GCM encryption operations",
    ),
    (
        "tls12_gcm_recrypts",
        "Total number of OCF TLS 1.2 GCM re-encryption operations",
    ),
    (
        "tls12_chacha20_decrypts",
        "Total number of OCF TLS 1.2 Chacha20-Poly1305 decryption operations",
    ),
    (
        "tls12_chacha20_encrypts",
        "Total number of OCF TLS 1.2 Chacha20-Poly1305 encryption operations",
    ),
    (
        "tls13_gcm_decrypts",
        "Total number of OCF TLS 1.3 GCM decryption operations",
    ),
    (
        "tls13_gcm_encrypts",
        "Total number of OCF TLS 1.3 GCM encryption operations",
    ),
    (
        "tls13_gcm_recrypts",
        "Total number of OCF TLS 1.3 GCM re-encryption operations",
    ),
    (
        "tls13_chacha20_decrypts",
        "Total number of OCF TLS 1.3 Chacha20-Poly1305 decryption operations",
    ),
    (
        "tls13_chacha20_encrypts",
        "Total number of OCF TLS 1.3 Chacha20-Poly1305 encryption operations",
    ),
    ("inplace", "Total number of OCF in-place operations"),
    (
        "separate_output",
        "Total number of OCF operations with a separate output buffer",
    ),
    ("retries", "Number of OCF encryption operation retries"),
];

/// Indexes of [`OCF`].
pub(crate) const TLS10_CBC_ENCRYPTS: usize = 0;
pub(crate) const TLS11_CBC_DECRYPTS: usize = 1;
pub(crate) const TLS11_CBC_ENCRYPTS: usize = 2;
pub(crate) const TLS12_GCM_DECRYPTS: usize = 3;
pub(crate) const TLS12_GCM_ENCRYPTS: usize = 4;
pub(crate) const TLS12_CHACHA20_DECRYPTS: usize = 6;
pub(crate) const TLS12_CHACHA20_ENCRYPTS: usize = 7;
pub(crate) const TLS13_GCM_DECRYPTS: usize = 8;
pub(crate) const TLS13_GCM_ENCRYPTS: usize = 9;
pub(crate) const TLS13_CHACHA20_DECRYPTS: usize = 11;
pub(crate) const TLS13_CHACHA20_ENCRYPTS: usize = 12;
pub(crate) const INPLACE: usize = 13;

/// Counts one operation of kind `k`.
pub(crate) fn ocf(k: usize) {
    OCF[k].fetch_add(1, Relaxed);
}

/// Counts one more session (`ktls_create_session`, `ktls_use_sw`).
pub(crate) fn session_made(kind: &AtomicU64) {
    ACTIVE.fetch_add(1, Relaxed);
    kind.fetch_add(1, Relaxed);
}

/// Counts one less (`ktls_cleanup`).
pub(crate) fn session_gone(suite: Suite) {
    ACTIVE.fetch_sub(1, Relaxed);
    let kind = match suite {
        Suite::Cbc => &SW_CBC,
        Suite::Gcm => &SW_GCM,
        Suite::Chacha => &SW_CHACHA20,
    };
    kind.fetch_sub(1, Relaxed);
}
