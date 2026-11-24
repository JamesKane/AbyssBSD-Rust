// SPDX-License-Identifier: BSD-2-Clause
//! The display core's hardware I2C engine on Raphael (DCN 3.1.5), used to
//! read a monitor's EDID over the HDMI connector's DDC line (08 section 4.7).
//!
//! The engine has a buffer of 538 bytes and up to four transactions per
//! run. An EDID block is a run of two or three transactions: [the segment
//! pointer,] the offset write, and the read. The display firmware can use
//! the engine too, so every run is bracketed by the arbitration register.

use alloc::vec::Vec;

use crate::regs::Window;

/// `DC_I2C_CONTROL`: bit 0 GO, bit 1 soft reset, bit 3 software-status
/// reset, bits 10:8 line select, bits 21:20 transaction count minus one.
pub const CONTROL: usize = 0x14d60;
/// `DC_I2C_ARBITRATION`: bits 3:2 owner status (1 us, 2 the display
/// firmware), bit 20 request, bit 21 done.
pub const ARBITRATION: usize = 0x14d64;
/// `DC_I2C_SW_STATUS`.
pub const SW_STATUS: usize = 0x14d6c;
/// `DDC1_HW_STATUS`.
pub const HW_STATUS: usize = 0x14d70;
/// `DDC1_SPEED`: the prescale in bits 31:16.
pub const SPEED: usize = 0x14d88;
/// `DDC1_SETUP`: bit 6 enable, bits 31:24 the time limit.
pub const SETUP: usize = 0x14d8c;
/// `DC_I2C_TRANSACTION0` (transaction `n` is `+ 4 n`).
pub const TRANSACTION: usize = 0x14db8;
/// `DC_I2C_DATA`: bit 0 read, bits 15:8 the data, bits 25:16 the index, bit
/// 31 index write.
pub const DATA: usize = 0x14dc8;
/// The two memory light-sleep registers (read, not driven yet).
pub const LIGHT_SLEEP: [usize; 2] = [0x14e78, 0x14e74];
/// `MICROSECOND_TIME_BASE_DIV`: the prescale depends on it.
pub const TIME_BASE_DIV: usize = 0x004ec;
/// The value of `MICROSECOND_TIME_BASE_DIV` the prescale of [`PRESCALE`] is
/// for (the DCN 2.0 default). The X870E reads `0x00120218`, so the speed is
/// not measured: the caller tries this prescale and then the platform's.
pub const TIME_BASE_DEFAULT: u32 = 0x0012_0264;
/// The prescale for 100 kHz with that time base.
pub const PRESCALE: u32 = 500;
/// The DDC pin group of line `n` (0 = DDC1): the `MASK` register; `A`, `EN`
/// and `Y` follow at +4, +8 and +12; the clock is bit 0 and the data bit 8.
#[must_use]
pub const fn gpio(line: usize) -> usize {
    0x17640 + 0x10 * line
}
/// `DDC_PAD1_I2CMODE`: bit 12.
pub const PAD_I2C_MODE: usize = 0x17774;
/// `AUX1_PAD_RXSEL` register: bits 11:10 = 1 for I2C.
pub const PAD_RXSEL: usize = 0x176fc;

/// The EDID's DDC address (write form; the read form is `| 1`).
const EDID_ADDRESS: u8 = 0xa0;
/// The segment pointer's address (write form).
const SEGMENT_ADDRESS: u8 = 0x60;

/// Why a read did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The display firmware holds the engine (owner status 2).
    Busy,
    /// The engine did not grant us (owner status never read 1).
    NotGranted(u32),
    /// The run did not finish; the last `SW_STATUS`.
    Timeout(u32),
    /// The run ended with an abort, a time-out or a NACK: `SW_STATUS`.
    Failed(u32),
}

/// A transaction word: read or write, stop on NACK, start, stop, and the
/// byte count (the address byte is not counted).
#[must_use]
pub const fn transaction(read: bool, stop: bool, count: u32) -> u32 {
    (read as u32) | (1 << 8) | (1 << 12) | ((stop as u32) << 13) | (count << 16)
}

/// Sets the bits of `mask` to `value`.
fn update<W: Window + ?Sized>(w: &W, at: usize, mask: u32, value: u32) {
    w.write32(at, (w.read32(at) & !mask) | (value & mask));
}

/// What the pins were before the engine took them, to restore.
struct Saved {
    group: [u32; 3],
    i2c_mode: u32,
    rxsel: u32,
}

/// Hands the pins of `line` to the engine and returns what they were.
fn take_pins<W: Window + ?Sized>(w: &W, line: usize) -> Saved {
    let g = gpio(line);
    let saved = Saved {
        group: [w.read32(g), w.read32(g + 4), w.read32(g + 8)],
        i2c_mode: w.read32(PAD_I2C_MODE),
        rxsel: w.read32(PAD_RXSEL),
    };
    // Clock and data not under software control; the pad in I2C mode.
    update(w, g, 0x101, 0);
    update(w, PAD_I2C_MODE, 1 << 12, 1 << 12);
    update(w, PAD_RXSEL, 3 << 10, 1 << 10);
    saved
}

/// Puts the pins back.
fn give_pins_back<W: Window + ?Sized>(w: &W, line: usize, s: &Saved) {
    let g = gpio(line);
    w.write32(PAD_RXSEL, s.rxsel);
    w.write32(PAD_I2C_MODE, s.i2c_mode);
    w.write32(g + 8, s.group[2]);
    w.write32(g + 4, s.group[1]);
    w.write32(g, s.group[0]);
}

/// Waits for owner status 1 after a request (08 section 4.7).
fn acquire<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) -> Result<(), Error> {
    if (w.read32(ARBITRATION) >> 2) & 3 == 2 {
        return Err(Error::Busy);
    }
    update(w, ARBITRATION, 1 << 20, 1 << 20);
    let mut last = 0;
    for _ in 0..100 {
        last = w.read32(ARBITRATION);
        if (last >> 2) & 3 == 1 {
            return Ok(());
        }
        pause_us(100);
    }
    update(w, ARBITRATION, 1 << 20, 0);
    Err(Error::NotGranted(last))
}

/// Releases the engine: the done bit, written again if it does not stick.
fn release<W: Window + ?Sized>(w: &W, pause_us: &dyn Fn(u32)) {
    for _ in 0..3 {
        update(w, ARBITRATION, 1 << 21, 1 << 21);
        pause_us(10);
        if (w.read32(ARBITRATION) >> 2) & 3 != 1 {
            return;
        }
    }
}

/// One run of up to four transactions, with the buffer's contents `buffer`
/// written first. Returns the `SW_STATUS` at the end if the run completed
/// without an abort, time-out or NACK.
fn run<W: Window + ?Sized>(
    w: &W,
    line: usize,
    transactions: &[u32],
    buffer: &[u8],
    prescale: u32,
    pause_us: &dyn Fn(u32),
) -> Result<(), Error> {
    w.write32(SETUP, w.read32(SETUP) | (1 << 6));
    update(w, SPEED, 0xffff_0000, prescale << 16);
    // Clear the status, set the line and the count.
    w.write32(CONTROL, 1 << 3);
    let count = (u32::try_from(transactions.len()).unwrap_or(1) - 1) << 20;
    let line_select = (u32::try_from(line).unwrap_or(0) & 7) << 8;
    for (i, t) in transactions.iter().enumerate() {
        w.write32(TRANSACTION + 4 * i, *t);
    }
    for (i, b) in buffer.iter().enumerate() {
        let index = if i == 0 { 1 << 31 } else { 0 };
        w.write32(DATA, index | u32::from(*b) << 8);
    }
    w.write32(CONTROL, count | line_select);
    w.write32(CONTROL, count | line_select | 1);
    let mut status = 0;
    for _ in 0..2000 {
        status = w.read32(SW_STATUS);
        if status & 3 != 1 && status & 0x4 != 0 {
            break;
        }
        pause_us(100);
    }
    if status & 0x4 == 0 {
        return Err(Error::Timeout(status));
    }
    // Aborted (4), timed out (5), stopped on a NACK (8), per-transaction NACK.
    if status & ((1 << 4) | (1 << 5) | (1 << 8) | (0xf << 12)) != 0 {
        return Err(Error::Failed(status));
    }
    Ok(())
}

/// Reads EDID block `block` (0 to 255) of the monitor on DDC line `line`
/// (0 = DDC1) into `out`, with the engine's clock `prescale`: blocks 2 and above write the segment pointer
/// first.
///
/// # Errors
///
/// [`Error`]: the engine busy or not granted, a run that did not finish or that the monitor NACKed.
pub fn read_block<W: Window + ?Sized>(
    w: &W,
    line: usize,
    block: usize,
    out: &mut [u8; 128],
    prescale: u32,
    pause_us: &dyn Fn(u32),
) -> Result<(), Error> {
    let segment = u8::try_from(block / 2).unwrap_or(0);
    let offset = if block % 2 == 1 { 128u8 } else { 0 };
    acquire(w, pause_us)?;
    let pins = take_pins(w, line);
    let mut transactions = Vec::new();
    let mut buffer = Vec::new();
    if segment > 0 {
        transactions.push(transaction(false, false, 1));
        buffer.extend([SEGMENT_ADDRESS, segment]);
    }
    transactions.push(transaction(false, false, 1));
    buffer.extend([EDID_ADDRESS, offset]);
    transactions.push(transaction(true, true, 128));
    buffer.push(EDID_ADDRESS | 1);
    let first_read = u32::try_from(buffer.len()).unwrap_or(0);
    let result = run(w, line, &transactions, &buffer, prescale, pause_us);
    if result.is_ok() {
        w.write32(DATA, (1 << 31) | (first_read << 16) | 1);
        for b in out.iter_mut() {
            *b = ((w.read32(DATA) >> 8) & 0xff) as u8;
        }
    }
    give_pins_back(w, line, &pins);
    release(w, pause_us);
    result
}

/// Reads the EDID: block 0, then as many extension blocks as it says (at
/// most `max_blocks` in all).
///
/// # Errors
///
/// [`Error`] from the first block; a later block that fails ends the read
/// with what was got.
pub fn read_edid<W: Window + ?Sized>(
    w: &W,
    line: usize,
    max_blocks: usize,
    prescale: u32,
    pause_us: &dyn Fn(u32),
) -> Result<Vec<u8>, Error> {
    let mut edid = Vec::new();
    let mut block = [0u8; 128];
    read_block(w, line, 0, &mut block, prescale, pause_us)?;
    let extensions = usize::from(block[126]);
    edid.extend_from_slice(&block);
    for n in 1..=extensions.min(max_blocks.saturating_sub(1)) {
        if read_block(w, line, n, &mut block, prescale, pause_us).is_err() {
            break;
        }
        edid.extend_from_slice(&block);
    }
    Ok(edid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;
    use std::collections::BTreeMap;
    use std::vec;

    /// The engine and a monitor behind it: records the bytes written into the
    /// buffer and the transactions, and on GO answers like a monitor with
    /// `edid` at address 0x50, with the segment pointer at 0x30.
    struct Engine {
        regs: RefCell<BTreeMap<usize, u32>>,
        buffer: RefCell<Vec<u8>>,
        cursor: RefCell<usize>,
        edid: Vec<u8>,
        /// Whether the monitor answers (else a NACK).
        present: bool,
        /// Whether the display firmware holds the engine.
        firmware_owns: bool,
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Engine {
        fn new(edid: Vec<u8>) -> Self {
            let e = Self {
                regs: RefCell::new(BTreeMap::new()),
                buffer: RefCell::new(vec![0; 538]),
                cursor: RefCell::new(0),
                edid,
                present: true,
                firmware_owns: false,
                log: RefCell::new(Vec::new()),
            };
            e.regs.borrow_mut().insert(TIME_BASE_DIV, TIME_BASE_DEFAULT);
            e
        }
        fn get(&self, o: usize) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
        fn go(&self) {
            let control = self.get(CONTROL);
            let n = ((control >> 20) & 3) as usize + 1;
            let mut at = 0;
            let mut segment = 0usize;
            let mut offset = 0usize;
            let mut status = 1 << 2;
            for i in 0..n {
                let t = self.get(TRANSACTION + 4 * i);
                let count = ((t >> 16) & 0x3ff) as usize;
                let address = self.buffer.borrow()[at];
                at += 1;
                if !self.present {
                    status |= (1 << 8) | (1 << (12 + i));
                    break;
                }
                if t & 1 == 0 {
                    // Write: the payload.
                    let payload: Vec<u8> = self.buffer.borrow()[at..at + count].to_vec();
                    at += count;
                    if address == SEGMENT_ADDRESS {
                        segment = usize::from(payload[0]);
                    } else {
                        offset = usize::from(payload[0]);
                    }
                } else {
                    // Read: fill the buffer from `at` with the EDID.
                    let from = segment * 256 + offset;
                    for k in 0..count {
                        self.buffer.borrow_mut()[at + k] = self.edid[from + k];
                    }
                }
            }
            self.regs.borrow_mut().insert(SW_STATUS, status);
        }
    }

    impl Window for Engine {
        fn read32(&self, o: usize) -> u32 {
            match o {
                DATA => {
                    let c = *self.cursor.borrow();
                    *self.cursor.borrow_mut() += 1;
                    u32::from(self.buffer.borrow()[c]) << 8
                }
                ARBITRATION => {
                    let v = self.get(ARBITRATION);
                    if self.firmware_owns {
                        (v & !(3 << 2)) | (2 << 2)
                    } else if v & (1 << 20) != 0 && v & (1 << 21) == 0 {
                        (v & !(3 << 2)) | (1 << 2)
                    } else {
                        v & !(3 << 2)
                    }
                }
                _ => self.get(o),
            }
        }
        fn write32(&self, o: usize, v: u32) {
            self.log.borrow_mut().push((o, v));
            match o {
                DATA => {
                    if v & (1 << 31) != 0 {
                        *self.cursor.borrow_mut() = ((v >> 16) & 0x3ff) as usize;
                        if v & 1 == 0 {
                            // Index write for writing: the first byte too.
                            self.buffer.borrow_mut()[*self.cursor.borrow()] =
                                ((v >> 8) & 0xff) as u8;
                            *self.cursor.borrow_mut() += 1;
                        }
                    } else {
                        let c = *self.cursor.borrow();
                        self.buffer.borrow_mut()[c] = ((v >> 8) & 0xff) as u8;
                        *self.cursor.borrow_mut() += 1;
                    }
                }
                CONTROL => {
                    self.regs.borrow_mut().insert(o, v);
                    if v & 1 != 0 {
                        self.go();
                    }
                }
                ARBITRATION => {
                    // The done bit releases: the request and the done bit clear.
                    let new = if v & (1 << 21) != 0 {
                        v & !((1 << 20) | (1 << 21))
                    } else {
                        v
                    };
                    self.regs.borrow_mut().insert(o, new);
                }
                _ => {
                    self.regs.borrow_mut().insert(o, v);
                }
            }
        }
    }

    fn edid(blocks: usize) -> Vec<u8> {
        let mut e = vec![0u8; 128 * blocks];
        for (i, b) in e.iter_mut().enumerate() {
            *b = u8::try_from(i % 251).unwrap_or(0);
        }
        e[126] = u8::try_from(blocks - 1).unwrap_or(0);
        e
    }

    #[test]
    fn block_zero_is_the_worked_example() {
        let e = Engine::new(edid(1));
        let mut out = [0u8; 128];
        read_block(&e, 0, 0, &mut out, PRESCALE, &|_| {}).unwrap();
        assert_eq!(&out[..], &e.edid[..128]);
        let wrote = |reg: usize| -> Vec<u32> {
            e.log
                .borrow()
                .iter()
                .filter(|(r, _)| *r == reg)
                .map(|(_, v)| *v)
                .collect()
        };
        assert_eq!(wrote(TRANSACTION), [0x0001_1100]);
        assert_eq!(wrote(TRANSACTION + 4), [0x0080_3101]);
        // The buffer: index write of 0xA0, the offset, the read address; then
        // the readback index.
        let data = wrote(DATA);
        assert_eq!(&data[..3], [0x8000_a000, 0x0000_0000, 0x0000_a100]);
        assert_eq!(data[3], 0x8003_0001);
        // Control: two transactions, then GO.
        let control = wrote(CONTROL);
        assert_eq!(&control[control.len() - 2..], [0x0010_0000, 0x0010_0001]);
    }

    #[test]
    fn the_second_block_reads_at_offset_128_and_the_third_uses_the_segment_pointer() {
        let e = Engine::new(edid(4));
        let mut out = [0u8; 128];
        read_block(&e, 0, 1, &mut out, PRESCALE, &|_| {}).unwrap();
        assert_eq!(&out[..], &e.edid[128..256]);
        read_block(&e, 0, 2, &mut out, PRESCALE, &|_| {}).unwrap();
        assert_eq!(&out[..], &e.edid[256..384]);
        // Block 2: three transactions, the readback from index 5.
        let data: Vec<u32> = e
            .log
            .borrow()
            .iter()
            .filter(|(r, _)| *r == DATA)
            .map(|(_, v)| *v)
            .collect();
        assert!(data.contains(&0x8005_0001));
        read_block(&e, 0, 3, &mut out, PRESCALE, &|_| {}).unwrap();
        assert_eq!(&out[..], &e.edid[384..512]);
    }

    #[test]
    fn read_edid_follows_the_extension_count() {
        let e = Engine::new(edid(3));
        let all = read_edid(&e, 0, 4, PRESCALE, &|_| {}).unwrap();
        assert_eq!(all, e.edid);
        // The cap stops it.
        let e = Engine::new(edid(3));
        assert_eq!(read_edid(&e, 0, 2, PRESCALE, &|_| {}).unwrap().len(), 256);
    }

    #[test]
    fn the_pins_are_handed_over_and_given_back() {
        let e = Engine::new(edid(1));
        let g = gpio(0);
        e.regs.borrow_mut().insert(g, 0x101);
        e.regs.borrow_mut().insert(g + 4, 0x100);
        e.regs.borrow_mut().insert(g + 8, 0x1);
        e.regs.borrow_mut().insert(PAD_RXSEL, 0xabc);
        let mut out = [0u8; 128];
        read_block(&e, 0, 0, &mut out, PRESCALE, &|_| {}).unwrap();
        // During the run the mask was cleared (log), after it is as it was.
        assert!(
            e.log
                .borrow()
                .iter()
                .any(|(r, v)| *r == g && *v & 0x101 == 0)
        );
        assert_eq!(e.get(g), 0x101);
        assert_eq!(e.get(g + 4), 0x100);
        assert_eq!(e.get(g + 8), 1);
        assert_eq!(e.get(PAD_RXSEL), 0xabc);
        assert_eq!(e.get(PAD_I2C_MODE), 0);
    }

    #[test]
    fn a_monitor_that_nacks_is_an_error_and_the_pins_still_come_back() {
        let mut e = Engine::new(edid(1));
        e.present = false;
        e.regs.borrow_mut().insert(gpio(0), 0x5);
        let mut out = [0u8; 128];
        let r = read_block(&e, 0, 0, &mut out, PRESCALE, &|_| {});
        assert!(matches!(r, Err(Error::Failed(s)) if s & (1 << 8) != 0));
        assert_eq!(e.get(gpio(0)), 0x5);
    }

    #[test]
    fn the_display_firmware_holding_the_engine_is_busy_and_nothing_is_written() {
        let mut e = Engine::new(edid(1));
        e.firmware_owns = true;
        let mut out = [0u8; 128];
        assert_eq!(
            read_block(&e, 0, 0, &mut out, PRESCALE, &|_| {}),
            Err(Error::Busy)
        );
        assert!(e.log.borrow().is_empty());
    }

    #[test]
    fn the_prescale_goes_in_the_upper_half_of_the_speed_register() {
        let e = Engine::new(edid(1));
        e.regs.borrow_mut().insert(SPEED, 0x00f0_0002);
        let mut out = [0u8; 128];
        read_block(&e, 0, 0, &mut out, 0x1f4, &|_| {}).unwrap();
        assert_eq!(e.get(SPEED), 0x01f4_0002);
    }

    #[test]
    fn the_gpio_groups_are_the_specifications() {
        assert_eq!(gpio(0), 0x17640);
        assert_eq!(gpio(0) + 4, 0x17644);
        assert_eq!(gpio(4), 0x17680);
    }
}
