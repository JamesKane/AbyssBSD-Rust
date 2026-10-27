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
//! The discipline does no locking and no waiting; the kernel's terminal
//! does both, and sends the signals.
//!
//! Behavior reference: sys/kern/tty_ttydisc.c, sys/kern/tty_inq.c.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;

use abi::signal::{SIGINFO, SIGINT, SIGQUIT, SIGTSTP};
use abi::termios::*;

/// The most bytes the input queue holds (typed, not yet read).
pub const INPUT_MAX: usize = 4096;
/// The most bytes the output queue holds before a writer waits.
pub const OUTPUT_MAX: usize = 4096;

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
    /// This many bytes (0 for an end of file typed at the line's start).
    Read(usize),
}

impl Discipline {
    /// A terminal in its default state ([`Termios::defaults`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            t: Termios::defaults(),
            inq: VecDeque::new(),
            canon: 0,
            reprint: 0,
            outq: VecDeque::new(),
            column: 0,
            writepos: 0,
            literal: false,
            stopped: false,
        }
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

    /// Sets the terminal's state (`TIOCSETA`). Turning `ICANON` on or off
    /// re-decides what is readable.
    pub fn set_termios(&mut self, mut t: Termios) {
        if t.c_cflag & CIGNORE != 0 {
            t.c_cflag = self.t.c_cflag;
        }
        let was = self.canonical();
        self.t = t;
        if was != self.canonical() {
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

    /// How many bytes a read could take now, 0 if it would wait
    /// (`ttydisc_read_poll`).
    #[must_use]
    pub fn read_poll(&self) -> usize {
        if self.readable() {
            self.canon.max(1)
        } else {
            0
        }
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
        self.canon -= taken;
        self.reprint -= taken;
        Line::Read(n)
    }

    /// Reads up to `out.len()` readable bytes (non-canonical mode).
    pub fn read_raw(&mut self, out: &mut [u8]) -> usize {
        let n = self.canon.min(out.len());
        for (o, cell) in out.iter_mut().zip(self.inq.drain(..n)) {
            *o = cell.c;
        }
        self.canon -= n;
        self.reprint -= n;
        n
    }

    /// Discards queued input, output, or both (`TIOCFLUSH`, signals).
    pub fn flush(&mut self, input: bool, output: bool) {
        if input {
            self.inq.clear();
            self.canon = 0;
            self.reprint = 0;
            self.literal = false;
        }
        if output {
            self.outq.clear();
        }
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
        if self.stopped {
            return 0;
        }
        let n = self.outq.len().min(buf.len());
        let (a, b) = self.outq.as_slices();
        let first = a.len().min(n);
        buf[..first].copy_from_slice(&a[..first]);
        buf[first..n].copy_from_slice(&b[..n - first]);
        self.outq.drain(..n);
        n
    }

    fn out(&mut self, bytes: &[u8]) -> bool {
        if self.outq.len() + bytes.len() > OUTPUT_MAX {
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
                let n = run.min(OUTPUT_MAX.saturating_sub(self.outq.len()));
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

    /// Processes one typed byte (FreeBSD's `ttydisc_rint`). Returns the
    /// signal to send to the foreground process group, if it asks for one
    /// (queues were already flushed as `NOFLSH` says).
    pub fn input(&mut self, c: u8) -> Option<u32> {
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
                    return None;
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
                    return Some(SIGINFO);
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
                if sig.is_some() {
                    if !self.lflag(NOFLSH) {
                        self.flush(true, true);
                    }
                    self.echo(c, false);
                    return sig;
                }
            }
            if self.iflag(IXON) {
                if self.t.is(VSTOP, c) {
                    if !self.stopped {
                        self.stopped = true;
                        return None;
                    }
                    // `VSTART` == `VSTOP` toggles.
                    if !self.t.is(VSTART, c) {
                        return None;
                    }
                }
                if self.t.is(VSTART, c) {
                    self.stopped = false;
                    return None;
                }
            }
            match c {
                CR if self.iflag(IGNCR) => return None,
                CR if self.iflag(ICRNL) => c = NL,
                NL if self.iflag(INLCR) => c = CR,
                _ => {}
            }
            if self.canonical() {
                if self.t.is(VERASE, c) || self.t.is(VERASE2, c) {
                    self.rubchar();
                    return None;
                }
                if self.t.is(VKILL, c) {
                    while self.rubchar() {}
                    return None;
                }
                if self.lflag(IEXTEN) {
                    if self.t.is(VWERASE, c) {
                        self.rubword();
                        return None;
                    }
                    if self.t.is(VREPRINT, c) {
                        self.reprint_line();
                        return None;
                    }
                }
            }
        }
        // `PARMRK` doubles a 0xff so it cannot be taken for a mark.
        let (count, quote) = if self.iflag(PARMRK) && c == 0xff {
            (2, true)
        } else {
            (1, quote)
        };
        if self.inq.len() + count > INPUT_MAX {
            if self.iflag(IMAXBEL) {
                self.out(b"\x07");
            }
            return None;
        }
        let cell = Cell { c, quote };
        for _ in 0..count {
            self.inq.push_back(cell);
        }
        if !self.canonical() || self.breaks(cell) {
            self.end_line();
        }
        self.echo(c, quote);
        None
    }
}

#[cfg(test)]
mod tests;
