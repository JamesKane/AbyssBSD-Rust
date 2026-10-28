// SPDX-License-Identifier: BSD-2-Clause
//! Terminals (`sys/_termios.h`, `sys/ttydefaults.h`, `sys/_winsize.h`):
//! `struct termios` with its flags and control characters, the defaults a
//! terminal starts with, and `struct winsize`.

use crate::layout::{Arches, ConstValue};

/// `NCCS`: control characters in `c_cc`.
pub const NCCS: usize = 20;

/// `struct termios`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_cc: [u8; NCCS],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

/// `struct winsize`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// `c_cc` indices.
pub const VEOF: usize = 0;
pub const VEOL: usize = 1;
pub const VEOL2: usize = 2;
pub const VERASE: usize = 3;
pub const VWERASE: usize = 4;
pub const VKILL: usize = 5;
pub const VREPRINT: usize = 6;
pub const VERASE2: usize = 7;
pub const VINTR: usize = 8;
pub const VQUIT: usize = 9;
pub const VSUSP: usize = 10;
pub const VDSUSP: usize = 11;
pub const VSTART: usize = 12;
pub const VSTOP: usize = 13;
pub const VLNEXT: usize = 14;
pub const VDISCARD: usize = 15;
pub const VMIN: usize = 16;
pub const VTIME: usize = 17;
pub const VSTATUS: usize = 18;

/// A control character set to this is disabled.
pub const POSIX_VDISABLE: u8 = 0xff;

/// `c_iflag`.
pub const IGNBRK: u32 = 0x0000_0001;
pub const BRKINT: u32 = 0x0000_0002;
pub const IGNPAR: u32 = 0x0000_0004;
pub const PARMRK: u32 = 0x0000_0008;
pub const INPCK: u32 = 0x0000_0010;
pub const ISTRIP: u32 = 0x0000_0020;
pub const INLCR: u32 = 0x0000_0040;
pub const IGNCR: u32 = 0x0000_0080;
pub const ICRNL: u32 = 0x0000_0100;
pub const IXON: u32 = 0x0000_0200;
pub const IXOFF: u32 = 0x0000_0400;
pub const IXANY: u32 = 0x0000_0800;
pub const IMAXBEL: u32 = 0x0000_2000;
pub const IUTF8: u32 = 0x0000_4000;

/// `c_oflag`.
pub const OPOST: u32 = 0x0000_0001;
pub const ONLCR: u32 = 0x0000_0002;
pub const TABDLY: u32 = 0x0000_0004;
pub const TAB0: u32 = 0x0000_0000;
pub const TAB3: u32 = 0x0000_0004;
pub const ONOEOT: u32 = 0x0000_0008;
pub const OCRNL: u32 = 0x0000_0010;
pub const ONOCR: u32 = 0x0000_0020;
pub const ONLRET: u32 = 0x0000_0040;

/// `c_cflag`.
pub const CIGNORE: u32 = 0x0000_0001;
pub const CSIZE: u32 = 0x0000_0300;
pub const CS5: u32 = 0x0000_0000;
pub const CS6: u32 = 0x0000_0100;
pub const CS7: u32 = 0x0000_0200;
pub const CS8: u32 = 0x0000_0300;
pub const CSTOPB: u32 = 0x0000_0400;
pub const CREAD: u32 = 0x0000_0800;
pub const PARENB: u32 = 0x0000_1000;
pub const PARODD: u32 = 0x0000_2000;
pub const HUPCL: u32 = 0x0000_4000;
pub const CLOCAL: u32 = 0x0000_8000;
pub const CCTS_OFLOW: u32 = 0x0001_0000;
pub const CRTS_IFLOW: u32 = 0x0002_0000;
pub const CRTSCTS: u32 = CCTS_OFLOW | CRTS_IFLOW;
pub const CDTR_IFLOW: u32 = 0x0004_0000;
pub const CDSR_OFLOW: u32 = 0x0008_0000;
pub const CCAR_OFLOW: u32 = 0x0010_0000;
/// Do not raise RTS and DTR on opening.
pub const CNO_RTSDTR: u32 = 0x0020_0000;

/// `c_lflag`.
pub const ECHOKE: u32 = 0x0000_0001;
pub const ECHOE: u32 = 0x0000_0002;
pub const ECHOK: u32 = 0x0000_0004;
pub const ECHO: u32 = 0x0000_0008;
pub const ECHONL: u32 = 0x0000_0010;
pub const ECHOPRT: u32 = 0x0000_0020;
pub const ECHOCTL: u32 = 0x0000_0040;
pub const ISIG: u32 = 0x0000_0080;
pub const ICANON: u32 = 0x0000_0100;
pub const ALTWERASE: u32 = 0x0000_0200;
pub const IEXTEN: u32 = 0x0000_0400;
pub const EXTPROC: u32 = 0x0000_0800;
pub const TOSTOP: u32 = 0x0040_0000;
pub const FLUSHO: u32 = 0x0080_0000;
pub const NOKERNINFO: u32 = 0x0200_0000;
pub const PENDIN: u32 = 0x2000_0000;
pub const NOFLSH: u32 = 0x8000_0000;

/// `tcsetattr`'s actions.
pub const TCSANOW: u32 = 0;
pub const TCSADRAIN: u32 = 1;
pub const TCSAFLUSH: u32 = 2;

/// The flags a terminal keeps (`TTYSUP_*`); the rest are dropped.
pub const TTYSUP_IFLAG: u32 = IGNBRK
    | BRKINT
    | IGNPAR
    | PARMRK
    | INPCK
    | ISTRIP
    | INLCR
    | IGNCR
    | ICRNL
    | IXON
    | IXOFF
    | IXANY
    | IMAXBEL
    | IUTF8;
pub const TTYSUP_OFLAG: u32 = OPOST | ONLCR | TAB3 | ONOEOT | OCRNL | ONOCR | ONLRET;
pub const TTYSUP_LFLAG: u32 = ECHOKE
    | ECHOE
    | ECHOK
    | ECHO
    | ECHONL
    | ECHOPRT
    | ECHOCTL
    | ISIG
    | ICANON
    | ALTWERASE
    | IEXTEN
    | TOSTOP
    | FLUSHO
    | NOKERNINFO
    | NOFLSH;
pub const TTYSUP_CFLAG: u32 = CIGNORE
    | CSIZE
    | CSTOPB
    | CREAD
    | PARENB
    | PARODD
    | HUPCL
    | CLOCAL
    | CCTS_OFLOW
    | CRTS_IFLOW
    | CDTR_IFLOW
    | CDSR_OFLOW
    | CCAR_OFLOW
    | CNO_RTSDTR;

/// A terminal's starting state (`TTYDEF_*`).
pub const TTYDEF_IFLAG: u32 = BRKINT | ICRNL | IMAXBEL | IXON | IXANY | IUTF8;
pub const TTYDEF_OFLAG: u32 = OPOST | ONLCR;
pub const TTYDEF_LFLAG: u32 = ICANON | ISIG | IEXTEN | ECHO | ECHOE | ECHOKE | ECHOCTL;
pub const TTYDEF_CFLAG: u32 = CREAD | CS8 | HUPCL;
pub const TTYDEF_SPEED: u32 = 9600;

/// `CTRL(x)`.
const fn ctrl(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + 1
    } else {
        (c - b'A' + 1) & 0x7f
    }
}

/// The default control characters (`ttydefchars`).
pub const CEOF: u8 = ctrl(b'd');
pub const CEOL: u8 = 0xff;
pub const CERASE: u8 = 0x7f;
pub const CERASE2: u8 = ctrl(b'h');
pub const CINTR: u8 = ctrl(b'c');
pub const CSTATUS: u8 = ctrl(b't');
pub const CKILL: u8 = ctrl(b'u');
pub const CMIN: u8 = 1;
pub const CQUIT: u8 = 0x1c;
pub const CSUSP: u8 = ctrl(b'z');
pub const CTIME: u8 = 0;
pub const CDSUSP: u8 = ctrl(b'y');
pub const CSTART: u8 = ctrl(b'q');
pub const CSTOP: u8 = ctrl(b's');
pub const CLNEXT: u8 = ctrl(b'v');
pub const CDISCARD: u8 = ctrl(b'o');
pub const CWERASE: u8 = ctrl(b'w');
pub const CREPRINT: u8 = ctrl(b'r');

impl Termios {
    /// The state a terminal opens with (FreeBSD's `tty_init_termios`).
    #[must_use]
    pub const fn defaults() -> Self {
        let mut cc = [POSIX_VDISABLE; NCCS];
        cc[VEOF] = CEOF;
        cc[VEOL] = CEOL;
        cc[VEOL2] = CEOL;
        cc[VERASE] = CERASE;
        cc[VWERASE] = CWERASE;
        cc[VKILL] = CKILL;
        cc[VREPRINT] = CREPRINT;
        cc[VERASE2] = CERASE2;
        cc[VINTR] = CINTR;
        cc[VQUIT] = CQUIT;
        cc[VSUSP] = CSUSP;
        cc[VDSUSP] = CDSUSP;
        cc[VSTART] = CSTART;
        cc[VSTOP] = CSTOP;
        cc[VLNEXT] = CLNEXT;
        cc[VDISCARD] = CDISCARD;
        cc[VMIN] = CMIN;
        cc[VTIME] = CTIME;
        cc[VSTATUS] = CSTATUS;
        Self {
            c_iflag: TTYDEF_IFLAG,
            c_oflag: TTYDEF_OFLAG,
            c_cflag: TTYDEF_CFLAG,
            c_lflag: TTYDEF_LFLAG,
            c_cc: cc,
            c_ispeed: TTYDEF_SPEED,
            c_ospeed: TTYDEF_SPEED,
        }
    }

    /// Whether control character `i` is `c` (and not disabled).
    #[must_use]
    pub const fn is(&self, i: usize, c: u8) -> bool {
        self.c_cc[i] != POSIX_VDISABLE && self.c_cc[i] == c
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("NCCS", NCCS as i64),
    c("VEOF", VEOF as i64),
    c("VEOL", VEOL as i64),
    c("VEOL2", VEOL2 as i64),
    c("VERASE", VERASE as i64),
    c("VWERASE", VWERASE as i64),
    c("VKILL", VKILL as i64),
    c("VREPRINT", VREPRINT as i64),
    c("VERASE2", VERASE2 as i64),
    c("VINTR", VINTR as i64),
    c("VQUIT", VQUIT as i64),
    c("VSUSP", VSUSP as i64),
    c("VDSUSP", VDSUSP as i64),
    c("VSTART", VSTART as i64),
    c("VSTOP", VSTOP as i64),
    c("VLNEXT", VLNEXT as i64),
    c("VDISCARD", VDISCARD as i64),
    c("VMIN", VMIN as i64),
    c("VTIME", VTIME as i64),
    c("VSTATUS", VSTATUS as i64),
    c("_POSIX_VDISABLE", POSIX_VDISABLE as i64),
    c("IGNBRK", IGNBRK as i64),
    c("BRKINT", BRKINT as i64),
    c("IGNPAR", IGNPAR as i64),
    c("PARMRK", PARMRK as i64),
    c("INPCK", INPCK as i64),
    c("ISTRIP", ISTRIP as i64),
    c("INLCR", INLCR as i64),
    c("IGNCR", IGNCR as i64),
    c("ICRNL", ICRNL as i64),
    c("IXON", IXON as i64),
    c("IXOFF", IXOFF as i64),
    c("IXANY", IXANY as i64),
    c("IMAXBEL", IMAXBEL as i64),
    c("IUTF8", IUTF8 as i64),
    c("OPOST", OPOST as i64),
    c("ONLCR", ONLCR as i64),
    c("TABDLY", TABDLY as i64),
    c("TAB0", TAB0 as i64),
    c("TAB3", TAB3 as i64),
    c("ONOEOT", ONOEOT as i64),
    c("OCRNL", OCRNL as i64),
    c("ONOCR", ONOCR as i64),
    c("ONLRET", ONLRET as i64),
    c("CIGNORE", CIGNORE as i64),
    c("CSIZE", CSIZE as i64),
    c("CS5", CS5 as i64),
    c("CS6", CS6 as i64),
    c("CS7", CS7 as i64),
    c("CS8", CS8 as i64),
    c("CSTOPB", CSTOPB as i64),
    c("CREAD", CREAD as i64),
    c("PARENB", PARENB as i64),
    c("PARODD", PARODD as i64),
    c("HUPCL", HUPCL as i64),
    c("CLOCAL", CLOCAL as i64),
    c("CCTS_OFLOW", CCTS_OFLOW as i64),
    c("CRTS_IFLOW", CRTS_IFLOW as i64),
    c("CRTSCTS", CRTSCTS as i64),
    c("CDTR_IFLOW", CDTR_IFLOW as i64),
    c("CDSR_OFLOW", CDSR_OFLOW as i64),
    c("CCAR_OFLOW", CCAR_OFLOW as i64),
    c("CNO_RTSDTR", CNO_RTSDTR as i64),
    c("ECHOKE", ECHOKE as i64),
    c("ECHOE", ECHOE as i64),
    c("ECHOK", ECHOK as i64),
    c("ECHO", ECHO as i64),
    c("ECHONL", ECHONL as i64),
    c("ECHOPRT", ECHOPRT as i64),
    c("ECHOCTL", ECHOCTL as i64),
    c("ISIG", ISIG as i64),
    c("ICANON", ICANON as i64),
    c("ALTWERASE", ALTWERASE as i64),
    c("IEXTEN", IEXTEN as i64),
    c("EXTPROC", EXTPROC as i64),
    c("TOSTOP", TOSTOP as i64),
    c("FLUSHO", FLUSHO as i64),
    c("NOKERNINFO", NOKERNINFO as i64),
    c("PENDIN", PENDIN as i64),
    c("NOFLSH", NOFLSH as i64),
    c("TCSANOW", TCSANOW as i64),
    c("TCSADRAIN", TCSADRAIN as i64),
    c("TCSAFLUSH", TCSAFLUSH as i64),
    d("TTYDEF_IFLAG", TTYDEF_IFLAG as i64),
    d("TTYDEF_OFLAG", TTYDEF_OFLAG as i64),
    d("TTYDEF_LFLAG", TTYDEF_LFLAG as i64),
    d("TTYDEF_CFLAG", TTYDEF_CFLAG as i64),
    d("TTYDEF_SPEED", TTYDEF_SPEED as i64),
    d("CEOF", CEOF as i64),
    d("CEOL", CEOL as i64),
    d("CERASE", CERASE as i64),
    d("CERASE2", CERASE2 as i64),
    d("CINTR", CINTR as i64),
    d("CSTATUS", CSTATUS as i64),
    d("CKILL", CKILL as i64),
    d("CMIN", CMIN as i64),
    d("CQUIT", CQUIT as i64),
    d("CSUSP", CSUSP as i64),
    d("CTIME", CTIME as i64),
    d("CDSUSP", CDSUSP as i64),
    d("CSTART", CSTART as i64),
    d("CSTOP", CSTOP as i64),
    d("CLNEXT", CLNEXT as i64),
    d("CDISCARD", CDISCARD as i64),
    d("CWERASE", CWERASE as i64),
    d("CREPRINT", CREPRINT as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "termios.h",
        value,
        arches: Arches::All,
    }
}

const fn d(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/ttydefaults.h",
        value,
        arches: Arches::All,
    }
}
