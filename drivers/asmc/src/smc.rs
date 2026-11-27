// SPDX-License-Identifier: BSD-2-Clause
//! The SMC's key protocol over its two back ends, as FreeBSD's
//! `asmc_mmio.c` and `asmc_pio.c`: read, write, key information, and a key
//! by index.
//!
//! Where FreeBSD retries without bound in time while holding a spin lock,
//! every transaction here has a budget ([`BUDGET_US`]). An SMC that does
//! not answer at all (a command never acknowledged on the ports, the memory
//! window never ready) [`RETIRE_AFTER`] times in a row is set aside: every
//! access fails at once with [`SmcError::Retired`] for [`BACKOFF_US`], then
//! one is let through to see whether it is back. A jammed SMC so cannot
//! stall the threads that read it (spec `apple-macpro6-1-thermal` §04
//! item 5), and one that recovers is used again. A key the SMC lacks is
//! not a stall: on the ports it ends in a timeout after an acknowledged
//! command ([`SmcError::Timeout`]), in memory in an error code. Writes are
//! checked against the key's length, and the length against [`MAX_VALUE`].

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use sync::SpinLock;

/// A key name: four ASCII bytes (`"F0Mn"`, `"#KEY"`, `"REV "`).
pub type Key = [u8; 4];

/// The longest value (`ASMC_MAXVAL`).
pub const MAX_VALUE: usize = 32;

/// One transaction's time budget, retries included, in microseconds.
pub const BUDGET_US: u64 = 200_000;

/// Transactions in a row with no answer at all before the SMC is set
/// aside.
pub const RETIRE_AFTER: u32 = 3;

/// How long an SMC set aside is left alone before one access is tried.
pub const BACKOFF_US: u64 = 30_000_000;

/// What failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmcError {
    /// The SMC took the command but did not finish it within the budget:
    /// on the ports, what a key the SMC lacks looks like.
    Timeout,
    /// The SMC did not take the command at all (no acknowledgement on the
    /// ports, never ready in memory).
    NoAnswer,
    /// The SMC answered with an error code (0x84: no such key).
    Refused(u8),
    /// A value longer than [`MAX_VALUE`], or not the key's length.
    Length,
    /// The SMC gave [`SmcError::NoAnswer`] [`RETIRE_AFTER`] times in a row
    /// and is left alone for [`BACKOFF_US`].
    Retired,
}

/// A key's length and type (`asmc_key_getinfo`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// The value's length in bytes.
    pub len: u8,
    /// The type: `"fpe2"`, `"sp78"`, `"ui8 "`, `"flt "`, ...
    pub ty: [u8; 4],
}

/// Byte access to the port window (`0x300`, 32 ports).
pub trait PortIo: Send + Sync {
    /// Reads the port at `off`.
    fn read8(&self, off: u16) -> u8;
    /// Writes the port at `off`.
    fn write8(&self, off: u16, v: u8);
}

/// Access to the memory window (`0xfef00000`, 64 KiB).
pub trait MmioIo: Send + Sync {
    /// The window's length.
    fn len(&self) -> usize;
    /// Whether the window is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Reads the byte at `off`.
    fn read8(&self, off: usize) -> u8;
    /// Writes the byte at `off`.
    fn write8(&self, off: usize, v: u8);
    /// Writes the 32-bit word at `off`, as the bytes in memory order.
    fn write32(&self, off: usize, v: u32);
}

/// Time: busy-waits and a microsecond clock.
pub trait Clock: Send + Sync {
    /// Busy-waits `us` microseconds.
    fn delay_us(&self, us: u64);
    /// A monotonic microsecond count.
    fn now_us(&self) -> u64;
}

/// Commands (`ASMC_CMD*`).
mod cmd {
    pub(super) const READ: u8 = 0x10;
    pub(super) const WRITE: u8 = 0x11;
    pub(super) const BY_INDEX: u8 = 0x12;
    pub(super) const INFO: u8 = 0x13;
}

/// The memory window's registers (`asmc_mmio.h`).
mod mmio {
    pub(super) const DATA: usize = 0x0000;
    pub(super) const KEY: usize = 0x0078;
    pub(super) const DATA_LEN: usize = 0x007d;
    pub(super) const SMC_ID: usize = 0x007e;
    pub(super) const CMD: usize = 0x007f;
    pub(super) const STATUS: usize = 0x4005;
    pub(super) const MIN_SIZE: usize = 0x4006;
    pub(super) const READY: u8 = 0x20;
}

/// The port window's registers and status values (`asmcvar.h`).
mod port {
    pub(super) const DATA: u16 = 0x00;
    pub(super) const CMD: u16 = 0x04;
    pub(super) const STATUS_MASK: u8 = 0x0f;
    pub(super) const CMD_ACK: u8 = 0x0c;
    pub(super) const AWAIT_DATA: u8 = 0x04;
    pub(super) const DATA_READY: u8 = 0x05;
    /// A key information answer: length, type, attributes.
    pub(super) const INFO_LEN: u8 = 6;
}

/// Which window the SMC is reached through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Memory-mapped (`LDKN` >= 2): the Mac Pro (Late 2013), T2 Macs.
    Mmio,
    /// Ports: older Macs.
    Port,
}

/// A transaction's deadline.
struct Deadline<'a, C: Clock> {
    clock: &'a C,
    end: u64,
}

impl<C: Clock> Deadline<'_, C> {
    fn passed(&self) -> bool {
        self.clock.now_us() >= self.end
    }
}

enum Io<P, M> {
    Mmio(M),
    Port(P),
}

/// The SMC.
pub struct Smc<P, M, C> {
    io: SpinLock<Io<P, M>>,
    clock: C,
    /// [`SmcError::NoAnswer`]s in a row; [`RETIRE_AFTER`] or more is set
    /// aside.
    stalls: AtomicU32,
    /// While set aside: when the next access may try it.
    retry_at: AtomicU64,
}

impl<P, M, C> core::fmt::Debug for Smc<P, M, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Smc")
            .field("stalls", &self.stalls.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<P: PortIo, M: MmioIo, C: Clock> Smc<P, M, C> {
    /// The SMC through the memory window, if it answers there
    /// (`asmc_mmio_probe`: the window is big enough, its status is not
    /// 0xff, and `LDKN` reads 2 or more). Gives the window back otherwise.
    pub fn probe_mmio(m: M, clock: C) -> Result<Self, (M, C)> {
        if m.len() < mmio::MIN_SIZE || m.read8(mmio::STATUS) == 0xff {
            return Err((m, clock));
        }
        let d = Deadline {
            clock: &clock,
            end: clock.now_us().saturating_add(BUDGET_US),
        };
        let mut ldkn = [0u8; 1];
        let ok = mmio_read(&m, &clock, &d, *b"LDKN", &mut ldkn).is_ok() && ldkn[0] >= 2;
        if !ok {
            return Err((m, clock));
        }
        Ok(Self {
            io: SpinLock::new(Io::Mmio(m)),
            clock,
            stalls: AtomicU32::new(0),
            retry_at: AtomicU64::new(0),
        })
    }

    /// The SMC through the port window.
    pub fn with_ports(p: P, clock: C) -> Self {
        Self {
            io: SpinLock::new(Io::Port(p)),
            clock,
            stalls: AtomicU32::new(0),
            retry_at: AtomicU64::new(0),
        }
    }

    /// The clock the SMC is timed by.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The window in use.
    pub fn backend(&self) -> Backend {
        match *self.io.lock() {
            Io::Mmio(_) => Backend::Mmio,
            Io::Port(_) => Backend::Port,
        }
    }

    /// Whether the SMC is set aside: it stopped answering, and has not
    /// answered since.
    pub fn retired(&self) -> bool {
        self.stalls.load(Ordering::Acquire) >= RETIRE_AFTER
    }

    /// Runs one transaction under the lock and the budget.
    fn transact<T>(
        &self,
        f: impl FnOnce(&Io<P, M>, &Deadline<'_, C>) -> Result<T, SmcError>,
    ) -> Result<T, SmcError> {
        if self.retired() && self.clock.now_us() < self.retry_at.load(Ordering::Acquire) {
            return Err(SmcError::Retired);
        }
        let io = self.io.lock();
        let d = Deadline {
            clock: &self.clock,
            end: self.clock.now_us().saturating_add(BUDGET_US),
        };
        let r = f(&io, &d);
        drop(io);
        if matches!(r, Err(SmcError::NoAnswer)) {
            let n = self.stalls.fetch_add(1, Ordering::AcqRel) + 1;
            if n >= RETIRE_AFTER {
                self.retry_at.store(
                    self.clock.now_us().saturating_add(BACKOFF_US),
                    Ordering::Release,
                );
            }
        } else {
            // Any answer, even an error code or a key's timeout, means the
            // SMC is there.
            self.stalls.store(0, Ordering::Release);
        }
        r
    }

    /// Reads `buf.len()` bytes of `key` (`asmc_key_read`): a shorter
    /// answer is zero-filled, as FreeBSD's.
    pub fn read(&self, key: Key, buf: &mut [u8]) -> Result<(), SmcError> {
        if buf.len() > MAX_VALUE {
            return Err(SmcError::Length);
        }
        self.transact(|io, d| match io {
            Io::Mmio(m) => mmio_read(m, &self.clock, d, key, buf),
            Io::Port(p) => retry(d, || port_read(p, &self.clock, d, key, buf)),
        })
    }

    /// Writes `value` to `key` (`asmc_key_write`).
    pub fn write(&self, key: Key, value: &[u8]) -> Result<(), SmcError> {
        if value.len() > MAX_VALUE {
            return Err(SmcError::Length);
        }
        self.transact(|io, d| match io {
            Io::Mmio(m) => mmio_write(m, &self.clock, d, key, value),
            Io::Port(p) => retry(d, || port_write(p, &self.clock, d, key, value)),
        })
    }

    /// Writes `value` to `key` only if it is the key's length: a short or
    /// long write to a fan key is refused, not passed on.
    pub fn write_checked(&self, key: Key, value: &[u8]) -> Result<(), SmcError> {
        if usize::from(self.info(key)?.len) != value.len() {
            return Err(SmcError::Length);
        }
        self.write(key, value)
    }

    /// The length and type of `key` (`asmc_key_getinfo`).
    pub fn info(&self, key: Key) -> Result<KeyInfo, SmcError> {
        self.transact(|io, d| match io {
            Io::Mmio(m) => mmio_info(m, &self.clock, d, key),
            Io::Port(p) => retry(d, || port_info(p, &self.clock, d, key)),
        })
    }

    /// The name of the `index`th key (`asmc_key_getbyindex`).
    pub fn key_at(&self, index: u32) -> Result<Key, SmcError> {
        self.transact(|io, d| match io {
            Io::Mmio(m) => mmio_by_index(m, &self.clock, d, index),
            Io::Port(p) => retry(d, || port_by_index(p, &self.clock, d, index)),
        })
    }
}

/// FreeBSD's port back end retries a failed transaction up to ten times;
/// here, while the budget lasts. The last failure is the answer.
fn retry<T, C: Clock>(
    d: &Deadline<'_, C>,
    mut f: impl FnMut() -> Result<T, SmcError>,
) -> Result<T, SmcError> {
    let mut last = Err(SmcError::NoAnswer);
    for _ in 0..10 {
        last = f();
        match last {
            Err(SmcError::Timeout | SmcError::NoAnswer) if !d.passed() => continue,
            _ => return last,
        }
    }
    last
}

// The memory window (`asmc_mmio.c`).

/// Waits for the ready bit with FreeBSD's backoff (10 µs doubling to
/// 3.2 ms, 24 polls), within the budget.
fn mmio_wait<M: MmioIo, C: Clock>(m: &M, clock: &C, d: &Deadline<'_, C>) -> Result<(), SmcError> {
    let mut us = 10;
    for _ in 0..24 {
        if m.read8(mmio::STATUS) & mmio::READY != 0 {
            return Ok(());
        }
        if d.passed() {
            break;
        }
        clock.delay_us(us);
        us = (us * 2).min(3200);
    }
    Err(SmcError::NoAnswer)
}

fn mmio_command<M: MmioIo, C: Clock>(
    m: &M,
    clock: &C,
    d: &Deadline<'_, C>,
    key: [u8; 4],
    command: u8,
) -> Result<(), SmcError> {
    m.write32(mmio::KEY, u32::from_ne_bytes(key));
    m.write8(mmio::SMC_ID, 0);
    m.write8(mmio::CMD, command);
    mmio_wait(m, clock, d)?;
    match m.read8(mmio::CMD) {
        0 => Ok(()),
        e => Err(SmcError::Refused(e)),
    }
}

fn mmio_read<M: MmioIo, C: Clock>(
    m: &M,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
    buf: &mut [u8],
) -> Result<(), SmcError> {
    if m.read8(mmio::STATUS) != 0 {
        m.write8(mmio::STATUS, 0);
    }
    mmio_command(m, clock, d, key, cmd::READ)?;
    let n = usize::from(m.read8(mmio::DATA_LEN)).min(buf.len());
    for (i, b) in buf.iter_mut().enumerate() {
        *b = if i < n { m.read8(mmio::DATA + i) } else { 0 };
    }
    Ok(())
}

fn mmio_write<M: MmioIo, C: Clock>(
    m: &M,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
    value: &[u8],
) -> Result<(), SmcError> {
    m.write8(mmio::STATUS, 0);
    for (i, &b) in value.iter().enumerate() {
        m.write8(mmio::DATA + i, b);
    }
    m.write8(
        mmio::DATA_LEN,
        u8::try_from(value.len()).map_err(|_| SmcError::Length)?,
    );
    mmio_command(m, clock, d, key, cmd::WRITE)
}

fn mmio_info<M: MmioIo, C: Clock>(
    m: &M,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
) -> Result<KeyInfo, SmcError> {
    m.write8(mmio::STATUS, 0);
    mmio_command(m, clock, d, key, cmd::INFO)?;
    // Type in data 0..3, length in data 5.
    let ty = core::array::from_fn(|i| m.read8(mmio::DATA + i));
    Ok(KeyInfo {
        len: m.read8(mmio::DATA + 5),
        ty,
    })
}

fn mmio_by_index<M: MmioIo, C: Clock>(
    m: &M,
    clock: &C,
    d: &Deadline<'_, C>,
    index: u32,
) -> Result<Key, SmcError> {
    m.write8(mmio::STATUS, 0);
    mmio_command(m, clock, d, index.to_be_bytes(), cmd::BY_INDEX)?;
    Ok(core::array::from_fn(|i| m.read8(mmio::DATA + i)))
}

// The port window (`asmc_pio.c`).

/// Waits for status `want` in the command port's low nibble, polling every
/// 10 µs up to `polls` times, within the budget.
fn port_wait<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    want: u8,
    polls: u32,
) -> Result<(), SmcError> {
    for _ in 0..polls {
        if p.read8(port::CMD) & port::STATUS_MASK == want {
            return Ok(());
        }
        if d.passed() {
            break;
        }
        clock.delay_us(10);
    }
    Err(SmcError::Timeout)
}

/// Sends `command`, rewriting it up to ten times until acknowledged
/// (`asmc_command`).
fn port_command<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    command: u8,
) -> Result<(), SmcError> {
    for _ in 0..10 {
        p.write8(port::CMD, command);
        if port_wait(p, clock, d, port::CMD_ACK, 100).is_ok() {
            return Ok(());
        }
        if d.passed() {
            break;
        }
    }
    Err(SmcError::NoAnswer)
}

fn port_send<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    bytes: &[u8],
) -> Result<(), SmcError> {
    for &b in bytes {
        p.write8(port::DATA, b);
        port_wait(p, clock, d, port::AWAIT_DATA, 1000)?;
    }
    Ok(())
}

fn port_recv<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    buf: &mut [u8],
) -> Result<(), SmcError> {
    for b in buf {
        port_wait(p, clock, d, port::DATA_READY, 1000)?;
        *b = p.read8(port::DATA);
    }
    Ok(())
}

fn port_read<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
    buf: &mut [u8],
) -> Result<(), SmcError> {
    port_command(p, clock, d, cmd::READ)?;
    port_send(p, clock, d, &key)?;
    p.write8(
        port::DATA,
        u8::try_from(buf.len()).map_err(|_| SmcError::Length)?,
    );
    port_recv(p, clock, d, buf)
}

fn port_write<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
    value: &[u8],
) -> Result<(), SmcError> {
    port_command(p, clock, d, cmd::WRITE)?;
    port_send(p, clock, d, &key)?;
    p.write8(
        port::DATA,
        u8::try_from(value.len()).map_err(|_| SmcError::Length)?,
    );
    for &b in value {
        port_wait(p, clock, d, port::AWAIT_DATA, 1000)?;
        p.write8(port::DATA, b);
    }
    Ok(())
}

fn port_info<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    key: Key,
) -> Result<KeyInfo, SmcError> {
    port_command(p, clock, d, cmd::INFO)?;
    port_send(p, clock, d, &key)?;
    p.write8(port::DATA, port::INFO_LEN);
    let mut info = [0u8; port::INFO_LEN as usize];
    port_recv(p, clock, d, &mut info)?;
    Ok(KeyInfo {
        len: info[0],
        ty: [info[1], info[2], info[3], info[4]],
    })
}

fn port_by_index<P: PortIo, C: Clock>(
    p: &P,
    clock: &C,
    d: &Deadline<'_, C>,
    index: u32,
) -> Result<Key, SmcError> {
    port_command(p, clock, d, cmd::BY_INDEX)?;
    port_send(p, clock, d, &index.to_be_bytes())?;
    p.write8(port::DATA, 4);
    let mut key = [0u8; 4];
    port_recv(p, clock, d, &mut key)?;
    Ok(key)
}
