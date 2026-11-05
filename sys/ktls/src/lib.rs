// SPDX-License-Identifier: BSD-2-Clause
//! Kernel TLS records: the sessions `TCP_TXTLS_ENABLE` and
//! `TCP_RXTLS_ENABLE` set up, sealing a record on send and opening one on
//! receive, for TLS 1.0–1.3 with AES-CBC and HMAC, AES-GCM, and
//! ChaCha20-Poly1305. A socket owns what is around this: `sys/inet` keeps
//! the buffers and decides when a record is whole.
//!
//! A record is one contiguous buffer here: `[header][payload][trailer]`,
//! sealed and opened in place, so that a file's pages lent to a socket are
//! never written to.
//!
//! Behavior reference: sys/kern/uipc_ktls.c (`ktls_create_session`,
//! `ktls_frame`, `ktls_decrypt`, `tls13_find_record_type`) and
//! sys/opencrypto/ktls_ocf.c.

#![no_std]

extern crate alloc;

use abi::errno::Errno;
use abi::ktls::{
    CRYPTO_AES_128_NIST_GMAC, CRYPTO_AES_192_NIST_GMAC, CRYPTO_AES_256_NIST_GMAC, CRYPTO_AES_CBC,
    CRYPTO_AES_NIST_GCM_16, CRYPTO_CHACHA20_POLY1305, CRYPTO_SHA1_HMAC, CRYPTO_SHA2_256_HMAC,
    CRYPTO_SHA2_384_HMAC, TLS_1_3_GCM_IV_LEN, TLS_AEAD_GCM_LEN, TLS_CBC_IMPLICIT_IV_LEN,
    TLS_CHACHA20_IV_LEN, TLS_MAJOR_VER_ONE, TLS_MAX_MSG_SIZE_V10_2, TLS_MAX_PARAM_SIZE,
    TLS_MINOR_VER_ONE, TLS_MINOR_VER_THREE, TLS_MINOR_VER_TWO, TLS_MINOR_VER_ZERO, TLS_RLTYPE_APP,
};
use crypto::hmac;
use crypto::sha1::Sha1;
use crypto::sha256::Sha256;
use crypto::sha512::Sha384;
use kcrypto::{Cbc, Gcm, cbc, chacha20poly1305, gcm};

pub mod stats;

#[cfg(test)]
mod tests;

/// A record's header (`struct tls_record_layer`).
pub const HDR: usize = 5;

/// An AES block; also the longest CBC padding.
const BLOCK: usize = 16;

/// The most an IV may be (`params.iv`, `XKTLS_SESSION_IV_BUF_LEN`).
const IV_MAX: i32 = 32;

/// Which way a session runs (`KTLS_TX`, `KTLS_RX`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Tx,
    Rx,
}

/// What `TCP_TXTLS_ENABLE` and `TCP_RXTLS_ENABLE` give, its keys copied in.
#[derive(Clone, Copy, Debug)]
pub struct Enable<'a> {
    pub cipher_key: &'a [u8],
    pub iv: &'a [u8],
    pub auth_key: &'a [u8],
    pub cipher_algorithm: i32,
    pub auth_algorithm: i32,
    pub flags: i32,
    pub vmajor: u8,
    pub vminor: u8,
    pub rec_seq: [u8; 8],
}

/// `ktls_copyin_tls_enable`'s limits on the lengths user space gave.
///
/// # Errors
///
/// `EINVAL` for one out of range, or for no cipher key.
pub fn check_lengths(cipher_key: i32, iv: i32, auth_key: i32) -> Result<(), Errno> {
    let max = TLS_MAX_PARAM_SIZE;
    if !(1..=max).contains(&cipher_key) || !(0..=IV_MAX).contains(&iv) {
        return Err(Errno::EINVAL);
    }
    if !(0..=max).contains(&auth_key) {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// An HMAC key.
#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "one per session")]
enum Hmac {
    Sha1(hmac::Key<Sha1>),
    Sha256(hmac::Key<Sha256>),
    Sha384(hmac::Key<Sha384>),
}

/// The longest MAC.
const MAC_MAX: usize = 48;

impl Hmac {
    fn len(&self) -> usize {
        match self {
            Self::Sha1(_) => 20,
            Self::Sha256(_) => 32,
            Self::Sha384(_) => 48,
        }
    }

    /// The MAC of `ad` and `data`; its first [`Hmac::len`] bytes count.
    fn mac(&self, ad: &[u8], data: &[u8]) -> [u8; MAC_MAX] {
        fn run<H: crypto::Hash>(k: &hmac::Key<H>, ad: &[u8], data: &[u8]) -> [u8; MAC_MAX] {
            let mut m = k.start();
            m.update(ad);
            m.update(data);
            let d = m.finish();
            let d = d.as_ref();
            let mut out = [0; MAC_MAX];
            out[..d.len()].copy_from_slice(d);
            out
        }
        match self {
            Self::Sha1(k) => run(k, ad, data),
            Self::Sha256(k) => run(k, ad, data),
            Self::Sha384(k) => run(k, ad, data),
        }
    }
}

/// A ChaCha20-Poly1305 key.
struct ChaKey([u8; 32]);

impl core::fmt::Debug for ChaKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never the key.
        f.debug_struct("ChaKey").finish_non_exhaustive()
    }
}

impl Drop for ChaKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// A session's keys.
#[derive(Debug)]
#[allow(clippy::large_enum_variant, reason = "one per session")]
enum Key {
    Gcm(Gcm),
    Cbc(Cbc, Hmac),
    Chacha(ChaKey),
}

/// A session's record sizes, which do not change: what a sender needs to
/// lay a record out before it has the session's lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The header with any explicit IV (`tls_hlen`).
    pub hlen: usize,
    /// The most a trailer takes (`tls_tlen`).
    pub tlen: usize,
    /// AES-CBC: the trailer ends on a block.
    cbc: bool,
    /// TLS 1.0 CBC.
    empty_ok: bool,
}

impl Layout {
    /// A whole record of `len` bytes of payload (`ktls_frame`: CBC pads to
    /// a block, with at least one byte).
    #[must_use]
    pub const fn record_len(&self, len: usize) -> usize {
        let delta = if self.cbc {
            (len + self.tlen) & (BLOCK - 1)
        } else {
            0
        };
        self.hlen + len + self.tlen - delta
    }

    /// `ktls_permit_empty_frames`: a send of nothing is a record.
    #[must_use]
    pub const fn permit_empty(&self) -> bool {
        self.empty_ok
    }
}

/// The most payload a record takes (`max_frame_len`).
pub const MAX_FRAME: usize = TLS_MAX_MSG_SIZE_V10_2;

/// What a session is, for the counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Suite {
    Cbc,
    Gcm,
    Chacha,
}

/// A TLS session: its keys and fixed parameters (`struct ktls_session`).
#[derive(Debug)]
pub struct Session {
    vmajor: u8,
    vminor: u8,
    layout: Layout,
    /// The implicit part of the nonce, or TLS 1.0's first IV.
    iv: [u8; 16],
    key: Key,
    suite: Suite,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.iv.fill(0);
        stats::session_gone(self.suite);
    }
}

/// `ktls_create_session`'s checks of `en`.
fn check_create(en: &Enable<'_>) -> Result<(), Errno> {
    if en.vmajor != TLS_MAJOR_VER_ONE {
        return Err(Errno::EINVAL);
    }
    if !(TLS_MINOR_VER_ZERO..=TLS_MINOR_VER_THREE).contains(&en.vminor) {
        return Err(Errno::EINVAL);
    }
    if en.flags != 0 {
        return Err(Errno::EINVAL);
    }
    match en.cipher_algorithm {
        CRYPTO_AES_NIST_GCM_16 => {
            // GMAC's values are permitted for compatibility, unused.
            if !matches!(
                en.auth_algorithm,
                0 | CRYPTO_AES_128_NIST_GMAC | CRYPTO_AES_192_NIST_GMAC | CRYPTO_AES_256_NIST_GMAC
            ) || !en.auth_key.is_empty()
            {
                return Err(Errno::EINVAL);
            }
            let want = match en.vminor {
                TLS_MINOR_VER_TWO => TLS_AEAD_GCM_LEN,
                TLS_MINOR_VER_THREE => TLS_1_3_GCM_IV_LEN,
                _ => return Err(Errno::EINVAL),
            };
            if en.iv.len() != want {
                return Err(Errno::EINVAL);
            }
        }
        CRYPTO_AES_CBC => {
            match en.auth_algorithm {
                CRYPTO_SHA1_HMAC => {}
                CRYPTO_SHA2_256_HMAC | CRYPTO_SHA2_384_HMAC if en.vminor == TLS_MINOR_VER_TWO => {}
                _ => return Err(Errno::EINVAL),
            }
            if en.auth_key.is_empty() {
                return Err(Errno::EINVAL);
            }
            // TLS 1.0 has an implicit IV; 1.1 and 1.2 ignore any given.
            match en.vminor {
                TLS_MINOR_VER_ZERO if en.iv.len() == TLS_CBC_IMPLICIT_IV_LEN => {}
                TLS_MINOR_VER_ONE | TLS_MINOR_VER_TWO => {}
                _ => return Err(Errno::EINVAL),
            }
        }
        CRYPTO_CHACHA20_POLY1305 => {
            if en.auth_algorithm != 0 || !en.auth_key.is_empty() {
                return Err(Errno::EINVAL);
            }
            if en.vminor != TLS_MINOR_VER_TWO && en.vminor != TLS_MINOR_VER_THREE {
                return Err(Errno::EINVAL);
            }
            if en.iv.len() != TLS_CHACHA20_IV_LEN {
                return Err(Errno::EINVAL);
            }
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(())
}

/// `ktls_ocf_try`'s checks of the key's length and what the suite runs on.
fn make_key(en: &Enable<'_>, dir: Direction) -> Result<Key, Errno> {
    let aes_len = matches!(en.cipher_key.len(), 16 | 32);
    match en.cipher_algorithm {
        CRYPTO_AES_NIST_GCM_16 => {
            if !aes_len {
                return Err(Errno::EINVAL);
            }
            Gcm::new(en.cipher_key).map(Key::Gcm).ok_or(Errno::EINVAL)
        }
        CRYPTO_AES_CBC => {
            if !aes_len {
                return Err(Errno::EINVAL);
            }
            if en.vminor > TLS_MINOR_VER_TWO {
                return Err(Errno::EPROTONOSUPPORT);
            }
            if dir == Direction::Rx && en.vminor == TLS_MINOR_VER_ZERO {
                return Err(Errno::EPROTONOSUPPORT);
            }
            let mac = match en.auth_algorithm {
                CRYPTO_SHA1_HMAC => Hmac::Sha1(hmac::Key::new(en.auth_key)),
                CRYPTO_SHA2_256_HMAC => Hmac::Sha256(hmac::Key::new(en.auth_key)),
                CRYPTO_SHA2_384_HMAC => Hmac::Sha384(hmac::Key::new(en.auth_key)),
                _ => return Err(Errno::EINVAL),
            };
            let aes = Cbc::new(en.cipher_key).ok_or(Errno::EINVAL)?;
            Ok(Key::Cbc(aes, mac))
        }
        CRYPTO_CHACHA20_POLY1305 => {
            let k = en.cipher_key.try_into().map_err(|_| Errno::EINVAL)?;
            Ok(Key::Chacha(ChaKey(k)))
        }
        _ => Err(Errno::EPROTONOSUPPORT),
    }
}

impl Session {
    /// `ktls_create_session` and `ktls_ocf_try`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for parameters no suite has; `EPROTONOSUPPORT` for a suite
    /// not run in this direction.
    pub fn new(en: &Enable<'_>, dir: Direction) -> Result<Self, Errno> {
        check_create(en)?;
        let key = make_key(en, dir)?;
        let tls13 = en.vminor == TLS_MINOR_VER_THREE;
        let (mut hlen, mut tlen) = (HDR, 0);
        let suite = match &key {
            Key::Gcm(_) => {
                // TLS 1.2's explicit nonce.
                if !tls13 {
                    hlen += 8;
                }
                tlen = gcm::TAG;
                Suite::Gcm
            }
            Key::Cbc(_, mac) => {
                if en.vminor != TLS_MINOR_VER_ZERO {
                    hlen += BLOCK;
                }
                tlen = BLOCK + mac.len();
                Suite::Cbc
            }
            Key::Chacha(_) => {
                tlen += chacha20poly1305::TAG;
                Suite::Chacha
            }
        };
        // The real record type.
        if tls13 {
            tlen += 1;
        }
        let cbc = suite == Suite::Cbc;
        let mut iv = [0; 16];
        if !cbc || en.vminor == TLS_MINOR_VER_ZERO {
            iv[..en.iv.len()].copy_from_slice(en.iv);
        }
        stats::session_made(match suite {
            Suite::Cbc => &stats::SW_CBC,
            Suite::Gcm => &stats::SW_GCM,
            Suite::Chacha => &stats::SW_CHACHA20,
        });
        Ok(Self {
            vmajor: en.vmajor,
            vminor: en.vminor,
            layout: Layout {
                hlen,
                tlen,
                cbc,
                empty_ok: cbc && en.vminor == TLS_MINOR_VER_ZERO,
            },
            iv,
            key,
            suite,
        })
    }

    #[must_use]
    pub const fn layout(&self) -> Layout {
        self.layout
    }

    #[must_use]
    pub const fn is_cbc(en: &Enable<'_>) -> bool {
        en.cipher_algorithm == CRYPTO_AES_CBC
    }

    const fn tls13(&self) -> bool {
        self.vminor == TLS_MINOR_VER_THREE
    }

    /// The nonce of TLS 1.3 and of ChaCha20: the IV with the sequence
    /// number over its last 8 bytes.
    fn xor_nonce(&self, seq: u64) -> [u8; 12] {
        let mut n = [0; 12];
        n.copy_from_slice(&self.iv[..12]);
        for (b, s) in n[4..].iter_mut().zip(seq.to_be_bytes()) {
            *b ^= s;
        }
        n
    }

    /// `struct tls_aead_data` and `struct tls_mac_data`, which are alike.
    fn seq_ad(seq: u64, hdr: &[u8], len: usize) -> [u8; 13] {
        let s = seq.to_be_bytes();
        let l = u16::try_from(len).unwrap_or(u16::MAX).to_be_bytes();
        [
            s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7], hdr[0], hdr[1], hdr[2], l[0], l[1],
        ]
    }

    /// `ktls_decrypt`'s look at a record's first `hlen` bytes: the length
    /// of the whole record.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a version or (under TLS 1.3) type it cannot have;
    /// `EMSGSIZE` for a length it cannot have. The stream is out of step,
    /// so the connection is lost.
    pub fn check_header(&self, h: &[u8]) -> Result<usize, Errno> {
        let vminor = if self.tls13() {
            TLS_MINOR_VER_TWO
        } else {
            self.vminor
        };
        let len = HDR + usize::from(u16::from_be_bytes([h[3], h[4]]));
        if h[1] != self.vmajor || h[2] != vminor {
            return Err(Errno::EINVAL);
        }
        if self.tls13() && h[0] != TLS_RLTYPE_APP {
            return Err(Errno::EINVAL);
        }
        let l = self.layout;
        if len < l.hlen || len > l.hlen + TLS_MAX_MSG_SIZE_V10_2 + l.tlen {
            return Err(Errno::EMSGSIZE);
        }
        Ok(len)
    }

    /// Opens `rec`, a whole record that passed [`Session::check_header`],
    /// in place (`ktls_ocf_decrypt`, `tls13_find_record_type`).
    ///
    /// # Errors
    ///
    /// `EMSGSIZE` for a record too short for its suite; `EBADMSG` if it
    /// does not check out.
    pub fn open(&self, rec: &mut [u8], seq: u64) -> Result<Opened, Errno> {
        match &self.key {
            Key::Cbc(aes, mac) => self.open_cbc(aes, mac, rec, seq),
            _ if self.tls13() => self.open13(rec, seq),
            _ => self.open12(rec, seq),
        }
    }

    /// Counts an AEAD record, sealed or opened.
    fn count_aead(&self, sealed: bool) {
        let k = match (matches!(self.key, Key::Gcm(_)), self.tls13(), sealed) {
            (true, false, false) => stats::TLS12_GCM_DECRYPTS,
            (true, false, true) => stats::TLS12_GCM_ENCRYPTS,
            (true, true, false) => stats::TLS13_GCM_DECRYPTS,
            (true, true, true) => stats::TLS13_GCM_ENCRYPTS,
            (false, false, false) => stats::TLS12_CHACHA20_DECRYPTS,
            (false, false, true) => stats::TLS12_CHACHA20_ENCRYPTS,
            (false, true, false) => stats::TLS13_CHACHA20_DECRYPTS,
            (false, true, true) => stats::TLS13_CHACHA20_ENCRYPTS,
        };
        stats::ocf(k);
    }

    /// `ktls_ocf_tls_cbc_decrypt`. A padding or MAC failure both end in
    /// `EBADMSG`, so a peer cannot tell them apart by the answer.
    fn open_cbc(&self, aes: &Cbc, mac: &Hmac, rec: &mut [u8], seq: u64) -> Result<Opened, Errno> {
        let tls_len = rec.len() - HDR;
        let mac_len = mac.len();
        if !tls_len.is_multiple_of(BLOCK) || tls_len < BLOCK + (mac_len + 1).next_multiple_of(BLOCK)
        {
            return Err(Errno::EMSGSIZE);
        }
        let (head, body) = rec.split_at_mut(self.layout.hlen);
        let mut iv = [0; BLOCK];
        iv.copy_from_slice(&head[HDR..]);
        stats::ocf(stats::TLS11_CBC_DECRYPTS);
        aes.with(|e| cbc::Engine::cbc_decrypt(e, &mut iv, body));
        let last = body[body.len() - 1];
        let pad = usize::from(last);
        let trailer = mac_len + pad + 1;
        if BLOCK + trailer > tls_len {
            return Err(Errno::EBADMSG);
        }
        let len = body.len() - trailer;
        let mut bad = 0;
        for b in &body[body.len() - pad - 1..] {
            bad |= *b ^ last;
        }
        let ad = Self::seq_ad(seq, head, len);
        let want = mac.mac(&ad, &body[..len]);
        let ok = crypto::ct_eq(&want[..mac_len], &body[len..len + mac_len]);
        if bad != 0 || !ok {
            return Err(Errno::EBADMSG);
        }
        Ok(Opened {
            rtype: head[0],
            start: self.layout.hlen,
            len,
        })
    }

    /// `ktls_ocf_tls12_aead_decrypt`.
    fn open12(&self, rec: &mut [u8], seq: u64) -> Result<Opened, Errno> {
        let l = self.layout;
        if rec.len() < l.hlen + l.tlen {
            return Err(Errno::EMSGSIZE);
        }
        let len = rec.len() - l.hlen - l.tlen;
        let (head, body) = rec.split_at_mut(l.hlen);
        let (data, tag) = body.split_at_mut(len);
        let ad = Self::seq_ad(seq, head, len);
        let nonce = match &self.key {
            Key::Gcm(_) => {
                let mut n = [0; 12];
                n[..TLS_AEAD_GCM_LEN].copy_from_slice(&self.iv[..TLS_AEAD_GCM_LEN]);
                n[TLS_AEAD_GCM_LEN..].copy_from_slice(&head[HDR..]);
                n
            }
            _ => self.xor_nonce(seq),
        };
        if !self.aead_open(&nonce, &ad, data, tag) {
            return Err(Errno::EBADMSG);
        }
        Ok(Opened {
            rtype: head[0],
            start: l.hlen,
            len,
        })
    }

    /// `ktls_ocf_tls13_aead_decrypt`, then the record type: the last byte
    /// of the plaintext that is not padding.
    fn open13(&self, rec: &mut [u8], seq: u64) -> Result<Opened, Errno> {
        let tag_len = self.layout.tlen - 1;
        if rec.len() < HDR + tag_len + 1 {
            return Err(Errno::EMSGSIZE);
        }
        let len = rec.len() - HDR - tag_len;
        let (head, body) = rec.split_at_mut(HDR);
        let (data, tag) = body.split_at_mut(len);
        let nonce = self.xor_nonce(seq);
        if !self.aead_open(&nonce, head, data, tag) {
            return Err(Errno::EBADMSG);
        }
        // All padding has no type (FreeBSD's search runs on into the
        // header, and takes a byte of its length for one).
        let Some(last) = data.iter().rposition(|b| *b != 0) else {
            return Err(Errno::EBADMSG);
        };
        Ok(Opened {
            rtype: data[last],
            start: HDR,
            len: last,
        })
    }

    fn aead_open(&self, nonce: &[u8; 12], ad: &[u8], data: &mut [u8], tag: &[u8]) -> bool {
        self.count_aead(false);
        match &self.key {
            Key::Gcm(k) => tag.try_into().is_ok_and(|t| k.open(nonce, ad, data, t)),
            Key::Chacha(k) => tag
                .try_into()
                .is_ok_and(|t| chacha20poly1305::open(&k.0, nonce, ad, data, t)),
            Key::Cbc(..) => false,
        }
    }

    fn aead_seal(&self, nonce: &[u8; 12], ad: &[u8], data: &mut [u8], tag: &mut [u8]) {
        self.count_aead(true);
        match &self.key {
            Key::Gcm(k) => tag.copy_from_slice(&k.seal(nonce, ad, data)),
            Key::Chacha(k) => {
                tag.copy_from_slice(&chacha20poly1305::seal(&k.0, nonce, ad, data));
            }
            Key::Cbc(..) => {}
        }
    }
}

/// What opening a record found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub rtype: u8,
    /// The payload's place in the record.
    pub start: usize,
    pub len: usize,
}

/// A session's sending side: the state one record leaves for the next.
#[derive(Debug)]
pub struct Tx {
    s: Session,
    seq: Marks,
}

/// What sealing a record changes.
#[derive(Clone, Copy, Debug)]
pub struct Marks {
    /// The next record's sequence number (`sb_tls_seqno`).
    seqno: u64,
    /// TLS 1.2 GCM's next explicit nonce.
    nonce: u64,
    /// TLS 1.0's next implicit IV.
    iv: [u8; 16],
}

impl Tx {
    /// A session for sending; `random` fills TLS 1.2 GCM's first nonce.
    ///
    /// # Errors
    ///
    /// As [`Session::new`].
    pub fn new(en: &Enable<'_>, random: &mut dyn FnMut(&mut [u8])) -> Result<Self, Errno> {
        let s = Session::new(en, Direction::Tx)?;
        let mut nonce = [0; 8];
        random(&mut nonce);
        let seq = Marks {
            seqno: u64::from_be_bytes(en.rec_seq),
            nonce: u64::from_ne_bytes(nonce),
            iv: s.iv,
        };
        Ok(Self { s, seq })
    }

    #[must_use]
    pub const fn layout(&self) -> Layout {
        self.s.layout
    }

    /// Where the session stands, to go back to if a sealed record cannot
    /// be queued after all.
    #[must_use]
    pub const fn mark(&self) -> Marks {
        self.seq
    }

    /// Goes back to `m`: nothing sealed since went anywhere.
    pub const fn reset(&mut self, m: Marks) {
        self.seq = m;
    }

    /// `ktls_frame` and `ktls_ocf_encrypt` of `rec`, which is
    /// [`Layout::record_len`] of `len` long with the payload in place.
    /// `random` makes CBC's explicit IVs.
    pub fn seal(
        &mut self,
        rtype: u8,
        rec: &mut [u8],
        len: usize,
        random: &mut dyn FnMut(&mut [u8]),
    ) {
        stats::ocf(stats::INPLACE);
        let s = &self.s;
        let l = s.layout;
        let seqno = self.seq.seqno;
        self.seq.seqno = seqno.wrapping_add(1);
        // TLS 1.3 goes as TLS 1.2 application data.
        let (wire_type, wire_minor) = if s.tls13() {
            (TLS_RLTYPE_APP, TLS_MINOR_VER_TWO)
        } else {
            (rtype, s.vminor)
        };
        let body = u16::try_from(rec.len() - HDR).unwrap_or(u16::MAX);
        rec[0] = wire_type;
        rec[1] = s.vmajor;
        rec[2] = wire_minor;
        rec[3..HDR].copy_from_slice(&body.to_be_bytes());
        let (head, rest) = rec.split_at_mut(l.hlen);
        match &s.key {
            Key::Cbc(aes, mac) => {
                let ad = Session::seq_ad(seqno, head, len);
                let m = mac.mac(&ad, &rest[..len]);
                let mac_len = mac.len();
                rest[len..len + mac_len].copy_from_slice(&m[..mac_len]);
                let pad = rest.len() - len - mac_len - 1;
                rest[len + mac_len..].fill(u8::try_from(pad).unwrap_or(0));
                let mut iv = self.seq.iv;
                if l.hlen > HDR {
                    random(&mut head[HDR..]);
                    iv.copy_from_slice(&head[HDR..]);
                }
                stats::ocf(if l.hlen > HDR {
                    stats::TLS11_CBC_ENCRYPTS
                } else {
                    stats::TLS10_CBC_ENCRYPTS
                });
                aes.with(|e| cbc::Engine::cbc_encrypt(e, &mut iv, rest));
                // The last ciphertext block is TLS 1.0's next IV.
                self.seq.iv = iv;
            }
            _ if s.tls13() => {
                rest[len] = rtype;
                let (data, tag) = rest.split_at_mut(len + 1);
                s.aead_seal(&s.xor_nonce(seqno), head, data, tag);
            }
            key => {
                let ad = Session::seq_ad(seqno, head, len);
                let nonce = if matches!(key, Key::Gcm(_)) {
                    let e = self.seq.nonce;
                    self.seq.nonce = e.wrapping_add(1);
                    head[HDR..].copy_from_slice(&e.to_be_bytes());
                    let mut n = [0; 12];
                    n[..TLS_AEAD_GCM_LEN].copy_from_slice(&s.iv[..TLS_AEAD_GCM_LEN]);
                    n[TLS_AEAD_GCM_LEN..].copy_from_slice(&e.to_be_bytes());
                    n
                } else {
                    s.xor_nonce(seqno)
                };
                let (data, tag) = rest.split_at_mut(len);
                s.aead_seal(&nonce, &ad, data, tag);
            }
        }
    }
}
