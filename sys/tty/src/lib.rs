// SPDX-License-Identifier: BSD-2-Clause
//! The terminal line discipline (FreeBSD's `termios` discipline): what a
//! terminal does with the bytes typed at it and the bytes written to it,
//! as its `termios` says.
//!
//! Typed bytes go through [`Discipline::input`]: the signal characters
//! (`^C`, `^\`, `^Z`, `^T`) ask for a signal to the foreground group;
//! `^S` and `^Q` stop and start output; CR and NL are mapped; and in
//! canonical mode the line being typed is edited (erase, kill, word erase,
//! reprint, literal next) until a newline, `VEOL`, or `VEOF` makes it
//! readable. Echo, with control characters shown as `^X`, goes to the
//! output queue. Without `ICANON` every byte is readable at once.
//!
//! Written bytes go through [`Discipline::write`]: with `OPOST`, NL
//! becomes CR NL (`ONLCR`), tabs expand (`TAB3`), and the like. The
//! driver drains the output queue with [`Discipline::take_output`], which
//! gives nothing while output is stopped.
//!
//! Both queues are sized as FreeBSD's `tty_watermarks` sizes them: two
//! seconds of data at the terminal's speed, in whole blocks, at most
//! 64 KiB, with no input queue without `CREAD`. Space is counted as the
//! block queues count it: what a read consumed of the first block stays
//! taken until the block is done. A full input queue drops a byte (and
//! rings, with `IMAXBEL`) unless lines are waiting to be read, when it
//! refuses it ([`Input::Full`]) so a writer can wait for room.
//!
//! The discipline does no locking and no waiting; the kernel's terminal
//! does both, and sends the signals.
//!
//! Behavior reference: sys/kern/tty_ttydisc.c, sys/kern/tty_inq.c,
//! sys/kern/tty_outq.c, sys/kern/tty.c (`tty_watermarks`, the high
//! watermarks).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;

use abi::signal::{SIGINFO, SIGINT, SIGQUIT, SIGTSTP};
use abi::termios::*;

/// The input queue's block (`TTYINQ_DATASIZE`).
pub const INQ_BLOCK: usize = 128;
/// The output queue's block (`TTYOUTQ_DATASIZE`: 256 bytes less a link).
pub const OUTQ_BLOCK: usize = 248;
/// The most either queue is sized for (`TTYBUF_MAX`).
pub const BUF_MAX: usize = 65536;
/// Chunks the discipline handles output in (`TTY_STACKBUF`).
pub const STACKBUF: usize = 256;

const TAB: u8 = b'\t';
const NL: u8 = b'\n';
const CR: u8 = b'\r';
const BS: u8 = 0x08;
const DEL: u8 = 0x7f;

/// A control character (processed on output).
const fn ctl_valid(c: u8) -> bool {
    c == DEL || c < 0x20
}

/// A control character processed when echoed: BS, TAB, NL, CR, unquoted.
const fn ctl_echo(c: u8, quote: bool) -> bool {
    !quote && matches!(c, BS | TAB | NL | CR)
}

/// A control character echoed as `^X`.
const fn ctl_print(c: u8, quote: bool) -> bool {
    c == DEL || (c < 0x20 && (quote || (c != TAB && c != NL)))
}

const fn ctl_white(c: u8) -> bool {
    c == b' ' || c == TAB
}

const fn ctl_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

/// A typed byte, and whether it was quoted (`VLNEXT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    c: u8,
    quote: bool,
}

/// The discipline's state.
#[derive(Debug)]
pub struct Discipline {
    t: Termios,
    /// Typed bytes. The first `canon` are readable; the rest are the line
    /// being edited.
    inq: VecDeque<Cell>,
    canon: usize,
    /// Where in `inq` the edited line was last echoed from, after the
    /// program last wrote (FreeBSD's reprint position).
    reprint: usize,
    outq: VecDeque<u8>,
    /// What reads took of each queue's first block (`ti_begin`,
    /// `to_begin`): still taken until the block is done.
    in_skip: usize,
    out_skip: usize,
    /// The queues' sizes, in whole blocks (0 while closed), and their low
    /// watermarks (`t_inlow`, `t_outlow`).
    in_size: usize,
    out_size: usize,
    in_low: usize,
    out_low: usize,
    /// Input was refused, or output filled (`TF_HIWAT_IN`,
    /// `TF_HIWAT_OUT`).
    hiwat_in: bool,
    hiwat_out: bool,
    /// The output column, and where the program's own output left it.
    column: u32,
    writepos: u32,
    /// The next byte is literal (`VLNEXT` came).
    literal: bool,
    /// Output is stopped (`VSTOP`, `TIOCSTOP`).
    stopped: bool,
}

impl Default for Discipline {
    fn default() -> Self {
        Self::new()
    }
}

/// What a canonical read found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    /// No complete line yet.
    NotYet,
    /// This many bytes, the line's end among them (0 for an end of file
    /// typed at the line's start).
    Read(usize),
    /// This many bytes of a line that goes on past them.
    Part(usize),
}

/// What became of a typed byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// Taken (or dropped, as the discipline says).
    Taken,
    /// Taken, asking for this signal to the foreground process group.
    Signal(u32),
    /// Refused: the input queue is full and lines wait to be read
    /// (`ttydisc_rint`'s -1). The byte may be offered again.
    Full,
}

impl Discipline {
    /// A terminal in its default state ([`Termios::defaults`]), open.
    #[must_use]
    pub fn new() -> Self {
        let mut d = Self {
            t: Termios::defaults(),
            inq: VecDeque::new(),
            canon: 0,
            reprint: 0,
            outq: VecDeque::new(),
            in_skip: 0,
            out_skip: 0,
            in_size: 0,
            out_size: 0,
            in_low: 0,
            out_low: 0,
            hiwat_in: false,
            hiwat_out: false,
            column: 0,
            writepos: 0,
            literal: false,
            stopped: false,
        };
        d.set_watermarks();
        d
    }

    /// Sizes the queues for two seconds of data at the terminal's speeds,
    /// with low watermarks at 90% free (`tty_watermarks`), on opening and
    /// when the speeds change. A queue holding more keeps it until read.
    pub fn set_watermarks(&mut self) {
        let size = |speed: u32, block: usize| {
            let bytes = usize::try_from(speed / 5).unwrap_or(BUF_MAX).min(BUF_MAX);
            bytes.div_ceil(block) * block
        };
        self.in_size = if self.t.c_cflag & CREAD != 0 {
            size(self.t.c_ispeed, INQ_BLOCK)
        } else {
            0
        };
        self.in_low = self.in_size * 9 / 10;
        self.out_size = size(self.t.c_ospeed, OUTQ_BLOCK);
        self.out_low = self.out_size * 9 / 10;
    }

    /// The last close (`ttydisc_close`, then the queues freed): output
    /// starts again, `FLUSHO` goes, and both queues are emptied and freed.
    pub fn close(&mut self) {
        self.stopped = false;
        self.hiwat_in = false;
        self.hiwat_out = false;
        self.t.c_lflag &= !FLUSHO;
        self.flush(true, true);
        self.in_size = 0;
        self.out_size = 0;
        self.in_low = 0;
        self.out_low = 0;
    }

    /// Room left in the input queue (`ttyinq_bytesleft`).
    fn in_left(&self) -> usize {
        self.in_size.saturating_sub(self.in_skip + self.inq.len())
    }

    /// Room left in the output queue (`ttyoutq_bytesleft`): what a
    /// writer could queue now (`ttydisc_write_poll`).
    #[must_use]
    pub fn output_room(&self) -> usize {
        self.out_size
            .saturating_sub(self.out_skip + self.outq.len())
    }

    /// The input queue's size, as allocated.
    #[must_use]
    pub const fn input_size(&self) -> usize {
        self.in_size
    }

    /// `n` bytes left a queue's front: they free their block once it is
    /// all read, or once the queue is empty.
    fn consumed(skip: &mut usize, n: usize, empty: bool, block: usize) {
        *skip = if empty { 0 } else { (*skip + n) % block };
    }

    /// How much a device could type now (`ttydisc_rint_poll`): 1 for a
    /// full queue that has not refused input, which drops it.
    #[must_use]
    pub fn input_room(&self) -> usize {
        match self.in_left() {
            0 if !self.hiwat_in => 1,
            l => l,
        }
    }

    /// Input was refused (`tty_hiwat_in_block`): with `IXOFF`, only once
    /// `VSTOP` is queued to tell the other end.
    fn hiwat_in_block(&mut self) {
        let stop = self.t.c_cc[VSTOP];
        if !self.hiwat_in && self.iflag(IXOFF) && stop != POSIX_VDISABLE {
            if self.out(&[stop]) {
                self.hiwat_in = true;
            }
        } else {
            self.hiwat_in = true;
        }
    }

    /// Room again (`tty_hiwat_in_unblock`): with `IXOFF`, only once
    /// `VSTART` is queued.
    fn hiwat_in_unblock(&mut self) {
        let start = self.t.c_cc[VSTART];
        if self.hiwat_in && self.iflag(IXOFF) && start != POSIX_VDISABLE {
            if self.out(&[start]) {
                self.hiwat_in = false;
            }
        } else {
            self.hiwat_in = false;
        }
    }

    /// After a read (the end of `ttydisc_read`): `true` when the input
    /// queue has room enough, or nothing readable, and the device should
    /// hear of it (`ttydevsw_inwakeup`).
    pub fn read_done(&mut self) -> bool {
        if self.in_left() >= self.in_low || self.canon == 0 {
            self.hiwat_in_unblock();
            return true;
        }
        false
    }

    /// Output filled and the writer will wait (`TF_HIWAT_OUT`).
    pub fn set_hiwat_out(&mut self) {
        self.hiwat_out = true;
    }

    /// Whether a writer still waits for room: the device's draining
    /// clears it.
    #[must_use]
    pub const fn hiwat_out(&self) -> bool {
        self.hiwat_out
    }

    /// Whether input needs no processing at all (`ttydisc_optimize`'s
    /// `TF_BYPASS`).
    #[must_use]
    pub const fn can_bypass(&self) -> bool {
        let i = self.t.c_iflag;
        i & (ICRNL | IGNCR | IMAXBEL | INLCR | ISTRIP | IXON) == 0
            && (i & BRKINT == 0 || i & IGNBRK != 0)
            && (i & PARMRK == 0 || i & (IGNPAR | IGNBRK) == IGNPAR | IGNBRK)
            && self.t.c_lflag & (ECHO | ICANON | IEXTEN | ISIG | PENDIN) == 0
    }

    /// Types `data` (`ttydisc_rint_simple`): all at once when no byte
    /// needs processing, else one at a time, `signal` told of each
    /// signal asked for. Returns how much was taken; less when the queue
    /// is full.
    pub fn input_bulk(&mut self, data: &[u8], signal: &mut dyn FnMut(u32)) -> usize {
        if self.can_bypass() {
            let n = data.len().min(self.in_left());
            self.inq
                .extend(data[..n].iter().map(|&c| Cell { c, quote: false }));
            self.canon = self.inq.len();
            if n < data.len() {
                self.hiwat_in_block();
            }
            return n;
        }
        for (i, &c) in data.iter().enumerate() {
            match self.input(c) {
                Input::Taken => {}
                Input::Signal(sig) => signal(sig),
                Input::Full => return i,
            }
        }
        data.len()
    }

    #[must_use]
    pub const fn termios(&self) -> &Termios {
        &self.t
    }

    const fn lflag(&self, f: u32) -> bool {
        self.t.c_lflag & f != 0
    }

    const fn iflag(&self, f: u32) -> bool {
        self.t.c_iflag & f != 0
    }

    const fn oflag(&self, f: u32) -> bool {
        self.t.c_oflag & f != 0
    }

    #[must_use]
    pub const fn canonical(&self) -> bool {
        self.lflag(ICANON)
    }

    /// Sets the terminal's state (`TIOCSETA`). Turning `ICANON` on or off,
    /// or changing `VEOF` or `VEOL`, re-decides what is readable.
    pub fn set_termios(&mut self, mut t: Termios) {
        if t.c_cflag & CIGNORE != 0 {
            t.c_cflag = self.t.c_cflag;
        }
        let again = (t.c_lflag ^ self.t.c_lflag) & ICANON != 0
            || t.c_cc[VEOF] != self.t.c_cc[VEOF]
            || t.c_cc[VEOL] != self.t.c_cc[VEOL];
        self.t = t;
        if again {
            self.canonicalize();
        }
    }

    /// Whether `c` ends a line: NL, `VEOF`, `VEOL` (unquoted).
    fn breaks(&self, cell: Cell) -> bool {
        !cell.quote && (cell.c == NL || self.t.is(VEOF, cell.c) || self.t.is(VEOL, cell.c))
    }

    /// Makes what is readable what the mode says: everything without
    /// `ICANON`, up to the last line end with it.
    fn canonicalize(&mut self) {
        if self.canonical() {
            self.canon = self
                .inq
                .iter()
                .rposition(|&c| self.breaks(c))
                .map_or(0, |i| i + 1);
        } else {
            self.canon = self.inq.len();
        }
        self.reprint = self.reprint.max(self.canon);
    }

    /// The edited line is complete: all of it becomes readable.
    fn end_line(&mut self) {
        self.canon = self.inq.len();
        self.reprint = self.canon;
    }

    /// Bytes `read` would return now (`FIONREAD`): in canonical mode the
    /// first line, without a `VEOF` that ends it.
    #[must_use]
    pub fn available(&self) -> usize {
        if !self.canonical() {
            return self.canon;
        }
        match self
            .inq
            .iter()
            .take(self.canon)
            .position(|&c| self.breaks(c))
        {
            Some(i) if self.t.is(VEOF, self.inq[i].c) => i,
            Some(i) => i + 1,
            None => 0,
        }
    }

    /// Whether a read would return now (data, or an end of file).
    #[must_use]
    pub fn readable(&self) -> bool {
        if self.canonical() {
            self.inq.iter().take(self.canon).any(|&c| self.breaks(c))
        } else {
            self.canon > 0
        }
    }

    /// The bytes ready to read (`ttydisc_read_poll`: those canonicalized,
    /// whether or not a line ends among them).
    #[must_use]
    pub const fn read_poll(&self) -> usize {
        self.canon
    }

    /// Takes `n` bytes off the input queue's front.
    fn take_input(&mut self, n: usize) {
        self.canon -= n;
        self.reprint = self.reprint.saturating_sub(n);
        Self::consumed(&mut self.in_skip, n, self.inq.is_empty(), INQ_BLOCK);
    }

    /// Reads a line, or as much of one as fits in `out`; a `VEOF` that
    /// ends it is consumed, not returned.
    pub fn read_line(&mut self, out: &mut [u8]) -> Line {
        let Some(end) = self
            .inq
            .iter()
            .take(self.canon)
            .position(|&c| self.breaks(c))
        else {
            return Line::NotYet;
        };
        let eof = self.t.is(VEOF, self.inq[end].c);
        let len = if eof { end } else { end + 1 };
        let n = len.min(out.len());
        for (o, cell) in out.iter_mut().zip(self.inq.drain(..n)) {
            *o = cell.c;
        }
        let mut taken = n;
        // The whole line went: drop its `VEOF` too.
        if eof && n == len {
            self.inq.pop_front();
            taken += 1;
        }
        self.take_input(taken);
        if n < len {
            Line::Part(n)
        } else {
            Line::Read(n)
        }
    }

    /// Reads up to `out.len()` readable bytes (non-canonical mode).
    pub fn read_raw(&mut self, out: &mut [u8]) -> usize {
        let n = self.canon.min(out.len());
        for (o, cell) in out.iter_mut().zip(self.inq.drain(..n)) {
            *o = cell.c;
        }
        self.take_input(n);
        n
    }

    /// Discards queued input, output, or both (`tty_flush`; the kernel's
    /// terminal wakes and tells the device).
    pub fn flush(&mut self, input: bool, output: bool) {
        if output {
            self.hiwat_out = false;
            self.outq.clear();
            self.out_skip = 0;
        }
        if input {
            self.hiwat_in_unblock();
            self.inq.clear();
            self.in_skip = 0;
            self.canon = 0;
            self.reprint = 0;
            self.literal = false;
        }
    }

    /// A signal went to the foreground group (`tty_signal_pgrp`): output
    /// starts again and is no longer discarded.
    pub fn signaled(&mut self) {
        self.stopped = false;
        self.t.c_lflag &= !FLUSHO;
    }

    pub const fn stopped(&self) -> bool {
        self.stopped
    }

    /// Stops or starts output (`TIOCSTOP`, `TIOCSTART`).
    pub fn set_stopped(&mut self, stopped: bool) {
        self.stopped = stopped;
    }

    /// Bytes waiting to go out (`TIOCOUTQ`).
    #[must_use]
    pub fn output_len(&self) -> usize {
        self.outq.len()
    }

    /// Moves queued output into `buf` for the device; nothing while
    /// output is stopped.
    pub fn take_output(&mut self, buf: &mut [u8]) -> usize {
        self.getc(buf).0
    }

    /// Moves queued output into `buf` for the device (`ttydisc_getc`);
    /// nothing while output is stopped. Also says whether writers should
    /// be woken: the queue is below its low watermark after filling, or
    /// has room (`ttydisc_wakeup_watermark`).
    pub fn getc(&mut self, buf: &mut [u8]) -> (usize, bool) {
        if self.stopped {
            return (0, false);
        }
        let n = self.outq.len().min(buf.len());
        let (a, b) = self.outq.as_slices();
        let first = a.len().min(n);
        buf[..first].copy_from_slice(&a[..first]);
        buf[first..n].copy_from_slice(&b[..n - first]);
        self.outq.drain(..n);
        Self::consumed(&mut self.out_skip, n, self.outq.is_empty(), OUTQ_BLOCK);
        let room = self.output_room();
        let wake = if self.hiwat_out {
            if room < self.out_low {
                false
            } else {
                self.hiwat_out = false;
                true
            }
        } else {
            room != 0
        };
        (n, wake)
    }

    /// Queues `bytes` whole or not at all (`ttyoutq_write_nofrag`).
    fn out(&mut self, bytes: &[u8]) -> bool {
        if self.output_room() < bytes.len() {
            return false;
        }
        self.outq.extend(bytes);
        true
    }

    /// Output processing for one control character (FreeBSD's
    /// `ttydisc_write_oproc`); `false` if the queue is full.
    fn oproc(&mut self, c: u8) -> bool {
        match c {
            // End of text is dropped (`ONOEOT`).
            CEOF if self.oflag(ONOEOT) => true,
            BS => {
                let ok = self.out(&[BS]);
                if ok {
                    self.column = self.column.saturating_sub(1);
                }
                ok
            }
            TAB => {
                let n = 8 - (self.column & 7);
                let ok = if self.t.c_oflag & TABDLY == TAB3 {
                    self.out(&b"        "[..n as usize])
                } else {
                    self.out(&[TAB])
                };
                if ok {
                    self.column += n;
                }
                ok
            }
            NL => {
                let ok = if self.oflag(ONLCR) {
                    self.out(b"\r\n")
                } else {
                    self.out(&[NL])
                };
                if ok && self.oflag(ONLCR | ONLRET) {
                    self.column = 0;
                    self.writepos = 0;
                    self.reprint = self.inq.len();
                }
                ok
            }
            CR => {
                if self.oflag(ONOCR) && self.column == 0 {
                    return true;
                }
                let c = if self.oflag(OCRNL) { NL } else { CR };
                let ok = self.out(&[c]);
                if ok {
                    self.column = 0;
                    self.writepos = 0;
                    self.reprint = self.inq.len();
                }
                ok
            }
            // Invisible: no column.
            c => self.out(&[c]),
        }
    }

    /// Whether the kernel's status line is shown on `VSTATUS` and
    /// `TIOCSTAT` (`NOKERNINFO` unset), and whether output is at the start
    /// of a line (`t_column == 0`), as `tty_info` asks.
    #[must_use]
    pub const fn status_wanted(&self) -> (bool, bool) {
        (!self.lflag(abi::termios::NOKERNINFO), self.column == 0)
    }

    /// Queues the kernel's status line, through output processing and
    /// past `FLUSHO` (`tty_info`'s `sbuf_tty_drain`, `tty_putstrn`);
    /// what does not fit is dropped.
    pub fn status(&mut self, text: &[u8]) {
        let flusho = self.lflag(FLUSHO);
        self.t.c_lflag &= !FLUSHO;
        self.write(text);
        if flusho {
            self.t.c_lflag |= FLUSHO;
        }
    }

    /// Queues program output (`write`); returns how much of `data` was
    /// taken, short when the queue fills.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if self.lflag(FLUSHO) {
            return data.len();
        }
        let opost = self.oflag(OPOST);
        let mut done = 0;
        while done < data.len() {
            // A run of characters with no output processing goes as one
            // (`ttydisc_findchar`).
            let run = data[done..]
                .iter()
                .position(|&c| opost && ctl_valid(c))
                .unwrap_or(data.len() - done);
            if run > 0 {
                let n = run.min(self.output_room());
                self.outq.extend(&data[done..done + n]);
                self.column += u32::try_from(n).unwrap_or(u32::MAX);
                done += n;
                self.writepos = self.column;
                self.reprint = self.inq.len();
                if n < run {
                    break;
                }
                continue;
            }
            if !self.oproc(data[done]) {
                break;
            }
            done += 1;
            self.writepos = self.column;
            self.reprint = self.inq.len();
        }
        done
    }

    /// Echoes `c` whatever `ECHO` says.
    fn echo_force(&mut self, c: u8, quote: bool) {
        if self.lflag(FLUSHO) {
            return;
        }
        if self.oflag(OPOST) && ctl_echo(c, quote) {
            self.oproc(c);
        } else if self.lflag(ECHOCTL) && ctl_print(c, quote) {
            let x = if c == DEL { b'?' } else { c + b'A' - 1 };
            if !quote && self.t.is(VEOF, c) {
                // `^D` then back over it: an end of file leaves no trace.
                self.out(&[b'^', x, BS, BS]);
            } else {
                self.column += 2;
                self.out(&[b'^', x]);
            }
        } else {
            self.column += 1;
            self.out(&[c]);
        }
    }

    /// Echoes `c` if `ECHO` (or, for a newline, `ECHONL`) says to.
    fn echo(&mut self, c: u8, quote: bool) {
        if !self.lflag(ECHO) && (!self.lflag(ECHONL) || c != NL || quote) {
            return;
        }
        self.echo_force(c, quote);
    }

    /// Echoes `^R`, a newline, and the line being edited.
    fn reprint_line(&mut self) {
        let r = self.t.c_cc[VREPRINT];
        if r != POSIX_VDISABLE {
            self.echo(r, false);
        }
        self.echo(NL, false);
        self.reprint = self.canon;
        for i in self.canon..self.inq.len() {
            let cell = self.inq[i];
            self.echo(cell.c, cell.quote);
        }
    }

    /// The column the edited line reaches from the program's last output.
    fn line_length(&self) -> u32 {
        let mut len = self.writepos;
        for cell in self.inq.iter().skip(self.reprint) {
            if ctl_print(cell.c, cell.quote) {
                if self.lflag(ECHOCTL) {
                    len += 2;
                }
            } else if cell.c == TAB {
                len += 8 - (len & 7);
            } else {
                len += 1;
            }
        }
        len
    }

    /// Takes the last byte of the edited line back off the screen.
    fn unput(&mut self) -> Option<Cell> {
        if self.inq.len() <= self.canon {
            return None;
        }
        let cell = self.inq.pop_back()?;
        self.reprint = self.reprint.min(self.inq.len());
        Some(cell)
    }

    /// Erases one character of the edited line (`VERASE`); `false` if it
    /// is empty.
    fn rubchar(&mut self) -> bool {
        let Some(cell) = self.unput() else {
            return false;
        };
        if !self.lflag(ECHO) {
            return true;
        }
        if self.writepos >= self.column {
            // The program wrote over the line: type it again.
            self.reprint_line();
        } else if self.lflag(ECHOE) {
            if ctl_print(cell.c, cell.quote) {
                if self.lflag(ECHOCTL) {
                    self.column = self.column.saturating_sub(2);
                    self.out(b"\x08\x08  \x08\x08");
                }
            } else if cell.c == b' ' {
                self.column = self.column.saturating_sub(1);
                self.out(&[BS]);
            } else if cell.c == TAB {
                let prev = self.line_length();
                let n = if prev >= self.column {
                    1
                } else {
                    (self.column - prev).min(8)
                };
                self.column = prev;
                self.out(&b"\x08\x08\x08\x08\x08\x08\x08\x08"[..n as usize]);
            } else {
                // A UTF-8 character goes whole: its continuation bytes and
                // its first, one column (wide characters are not told
                // apart).
                if self.iflag(IUTF8) && cell.c & 0xc0 == 0x80 {
                    while let Some(prev) = self.inq.back().copied() {
                        if self.inq.len() <= self.canon || prev.c & 0x80 == 0 {
                            break;
                        }
                        self.unput();
                        if prev.c & 0xc0 != 0x80 {
                            break;
                        }
                    }
                }
                self.column = self.column.saturating_sub(1);
                self.out(b"\x08 \x08");
            }
        } else {
            let e = self.t.c_cc[VERASE];
            self.echo(e, false);
        }
        true
    }

    /// Erases the last word (`VWERASE`): trailing blanks, then up to the
    /// next blank (or, with `ALTWERASE`, the next change between
    /// alphanumeric and not).
    fn rubword(&mut self) {
        loop {
            match self.inq.back() {
                Some(c) if self.inq.len() > self.canon && ctl_white(c.c) => {
                    self.rubchar();
                }
                Some(_) if self.inq.len() > self.canon => break,
                _ => return,
            }
        }
        let alnum = self.inq.back().is_some_and(|c| ctl_alnum(c.c));
        loop {
            self.rubchar();
            match self.inq.back() {
                Some(c) if self.inq.len() > self.canon => {
                    if ctl_white(c.c) || (self.lflag(ALTWERASE) && ctl_alnum(c.c) != alnum) {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// Processes one typed byte (FreeBSD's `ttydisc_rint`): whether it
    /// was taken, and the signal to send to the foreground process group
    /// if it asks for one (queues were already flushed as `NOFLSH` says,
    /// and output started again).
    pub fn input(&mut self, c: u8) -> Input {
        if self.can_bypass() {
            return self.store(c, false);
        }
        let mut c = c;
        if self.iflag(IXANY) {
            self.stopped = false;
            self.t.c_lflag &= !FLUSHO;
        }
        if self.iflag(ISTRIP) {
            c &= 0x7f;
        }
        let mut quote = false;
        if self.literal {
            self.literal = false;
            quote = true;
        } else {
            if self.lflag(IEXTEN) {
                if self.t.is(VLNEXT, c) {
                    if self.lflag(ECHO) {
                        if self.lflag(ECHOE) {
                            self.out(b"^\x08");
                        } else {
                            self.echo(c, false);
                        }
                    }
                    self.literal = true;
                    return Input::Taken;
                }
                if self.t.is(VDISCARD, c) {
                    if self.lflag(FLUSHO) {
                        self.t.c_lflag &= !FLUSHO;
                    } else {
                        self.flush(false, true);
                        self.echo(c, false);
                        if self.inq.len() > self.canon {
                            self.reprint_line();
                        }
                        self.t.c_lflag |= FLUSHO;
                    }
                }
            }
            if self.lflag(ISIG) {
                if self.t.c_lflag & (ICANON | IEXTEN) == ICANON | IEXTEN && self.t.is(VSTATUS, c) {
                    self.signaled();
                    return Input::Signal(SIGINFO);
                }
                let sig = if self.t.is(VINTR, c) {
                    Some(SIGINT)
                } else if self.t.is(VQUIT, c) {
                    Some(SIGQUIT)
                } else if self.t.is(VSUSP, c) {
                    Some(SIGTSTP)
                } else {
                    None
                };
                if let Some(sig) = sig {
                    if !self.lflag(NOFLSH) {
                        self.flush(true, true);
                    }
                    self.echo(c, false);
                    self.signaled();
                    return Input::Signal(sig);
                }
            }
            if self.iflag(IXON) {
                if self.t.is(VSTOP, c) {
                    if !self.stopped {
                        self.stopped = true;
                        return Input::Taken;
                    }
                    // `VSTART` == `VSTOP` toggles.
                    if !self.t.is(VSTART, c) {
                        return Input::Taken;
                    }
                }
                if self.t.is(VSTART, c) {
                    self.stopped = false;
                    return Input::Taken;
                }
            }
            match c {
                CR if self.iflag(IGNCR) => return Input::Taken,
                CR if self.iflag(ICRNL) => c = NL,
                NL if self.iflag(INLCR) => c = CR,
                _ => {}
            }
            if self.canonical() {
                if self.t.is(VERASE, c) || self.t.is(VERASE2, c) {
                    self.rubchar();
                    return Input::Taken;
                }
                if self.t.is(VKILL, c) {
                    while self.rubchar() {}
                    return Input::Taken;
                }
                if self.lflag(IEXTEN) {
                    if self.t.is(VWERASE, c) {
                        self.rubword();
                        return Input::Taken;
                    }
                    if self.t.is(VREPRINT, c) {
                        self.reprint_line();
                        return Input::Taken;
                    }
                }
            }
        }
        self.store(c, quote)
    }

    /// Queues a typed byte (from `ttydisc_rint`'s `processed`): a full
    /// queue rings with `IMAXBEL`, then drops the byte if nothing is
    /// readable, or refuses it.
    fn store(&mut self, c: u8, quote: bool) -> Input {
        // `PARMRK` doubles a 0xff so it cannot be taken for a mark.
        let (count, quote) = if self.iflag(PARMRK) && c == 0xff {
            (2, true)
        } else {
            (1, quote)
        };
        if self.in_left() < count {
            if self.iflag(IMAXBEL) {
                self.out(b"\x07");
            }
            if self.canon == 0 {
                return Input::Taken;
            }
            self.hiwat_in_block();
            return Input::Full;
        }
        let cell = Cell { c, quote };
        for _ in 0..count {
            self.inq.push_back(cell);
        }
        if !self.canonical() || self.breaks(cell) {
            self.end_line();
        }
        self.echo(c, quote);
        Input::Taken
    }
}

#[cfg(test)]
mod tests;
