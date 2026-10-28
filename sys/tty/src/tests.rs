// SPDX-License-Identifier: BSD-2-Clause
//! The discipline against what FreeBSD's prints and returns for the same
//! keystrokes.

extern crate std;

use std::vec::Vec;

use super::*;

/// Types `keys`; returns the signals asked for.
fn typed(d: &mut Discipline, keys: &[u8]) -> Vec<u32> {
    keys.iter()
        .filter_map(|&k| match d.input(k) {
            Input::Signal(sig) => Some(sig),
            _ => None,
        })
        .collect()
}

fn echoed(d: &mut Discipline) -> Vec<u8> {
    let mut out = std::vec![0; 4096];
    let n = d.take_output(&mut out);
    out.truncate(n);
    out
}

fn line(d: &mut Discipline) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    match d.read_line(&mut buf) {
        Line::NotYet => None,
        Line::Read(n) | Line::Part(n) => Some(buf[..n].to_vec()),
    }
}

#[test]
fn a_line_is_readable_once_ended() {
    let mut d = Discipline::new();
    typed(&mut d, b"ls");
    assert!(!d.readable());
    assert_eq!(line(&mut d), None);
    // Return becomes a newline (ICRNL), echoed as CR NL (ONLCR).
    typed(&mut d, b"\r");
    assert_eq!(echoed(&mut d), b"ls\r\n");
    assert_eq!(d.available(), 3);
    assert_eq!(line(&mut d).unwrap(), b"ls\n");
    assert!(!d.readable());
}

#[test]
fn erasing() {
    let mut d = Discipline::new();
    typed(&mut d, b"lx\x7fs\n");
    assert_eq!(echoed(&mut d), b"lx\x08 \x08s\r\n");
    assert_eq!(line(&mut d).unwrap(), b"ls\n");
    // Kill the line, then a word.
    typed(&mut d, b"abc\x15def\n");
    assert_eq!(line(&mut d).unwrap(), b"def\n");
    typed(&mut d, b"foo bar  \x17baz\n");
    assert_eq!(line(&mut d).unwrap(), b"foo baz\n");
    // Nothing to erase: nothing happens.
    echoed(&mut d);
    typed(&mut d, b"\x7f");
    assert_eq!(echoed(&mut d), b"");
}

#[test]
fn control_characters_echo_as_carets() {
    let mut d = Discipline::new();
    typed(&mut d, b"\x01");
    assert_eq!(echoed(&mut d), b"^A");
    typed(&mut d, b"\x7f");
    assert_eq!(echoed(&mut d), b"\x08\x08  \x08\x08");
}

#[test]
fn end_of_file() {
    let mut d = Discipline::new();
    // At the start of a line: a zero-length read, and ^D backed over.
    typed(&mut d, b"\x04");
    assert_eq!(echoed(&mut d), b"^D\x08\x08");
    assert_eq!(line(&mut d).unwrap(), b"");
    assert!(!d.readable());
    // After text: the text, without a newline.
    typed(&mut d, b"ab\x04");
    assert_eq!(d.available(), 2);
    assert_eq!(line(&mut d).unwrap(), b"ab");
    assert!(!d.readable());
}

#[test]
fn signal_characters() {
    let mut d = Discipline::new();
    assert_eq!(typed(&mut d, b"abc\x03"), [SIGINT]);
    // The typed text and its echo are gone (no NOFLSH); then ^C echoes.
    assert_eq!(echoed(&mut d), b"^C");
    typed(&mut d, b"\n");
    assert_eq!(line(&mut d).unwrap(), b"\n");
    assert_eq!(typed(&mut d, b"\x1a\x1c\x14"), [SIGTSTP, SIGQUIT, SIGINFO]);
    let mut t = *d.termios();
    t.c_lflag |= NOFLSH;
    d.set_termios(t);
    typed(&mut d, b"keep\x03\n");
    assert_eq!(line(&mut d).unwrap(), b"keep\n");
    t.c_lflag &= !ISIG;
    d.set_termios(t);
    assert_eq!(typed(&mut d, b"\x03\n"), []);
    assert_eq!(line(&mut d).unwrap(), b"\x03\n");
}

#[test]
fn signal_echo_shows_the_character() {
    let mut d = Discipline::new();
    d.flush(false, true);
    typed(&mut d, b"\x03");
    assert_eq!(echoed(&mut d), b"^C");
}

#[test]
fn literal_next() {
    let mut d = Discipline::new();
    assert_eq!(typed(&mut d, b"\x16\x03\n"), []);
    assert_eq!(echoed(&mut d), b"^\x08^C\r\n");
    assert_eq!(line(&mut d).unwrap(), b"\x03\n");
    // A quoted newline does not end the line.
    typed(&mut d, b"a\x16\nb\n");
    assert_eq!(line(&mut d).unwrap(), b"a\nb\n");
}

#[test]
fn reprint() {
    let mut d = Discipline::new();
    typed(&mut d, b"ab\x12");
    assert_eq!(echoed(&mut d), b"ab^R\r\nab");
}

#[test]
fn raw_mode() {
    let mut d = Discipline::new();
    typed(&mut d, b"par");
    let mut t = *d.termios();
    t.c_lflag &= !(ICANON | ECHO);
    d.set_termios(t);
    // The partial line becomes readable at once, and so does what follows.
    typed(&mut d, b"x\x7f");
    assert_eq!(echoed(&mut d), b"par");
    let mut buf = [0u8; 3];
    assert_eq!(d.read_raw(&mut buf), 3);
    assert_eq!(&buf, b"par");
    assert_eq!(d.read_raw(&mut buf), 2);
    assert_eq!(&buf[..2], b"x\x7f");
    assert_eq!(d.read_raw(&mut buf), 0);
}

#[test]
fn long_lines_are_read_in_pieces() {
    let mut d = Discipline::new();
    typed(&mut d, b"abcdef\n");
    let mut buf = [0u8; 4];
    assert_eq!(d.read_line(&mut buf), Line::Part(4));
    assert_eq!(&buf, b"abcd");
    assert_eq!(d.read_line(&mut buf), Line::Read(3));
    assert_eq!(&buf[..3], b"ef\n");
}

#[test]
fn flow_control() {
    let mut d = Discipline::new();
    d.write(b"x");
    typed(&mut d, b"\x13");
    assert!(d.stopped());
    assert_eq!(echoed(&mut d), b"");
    typed(&mut d, b"\x11");
    assert_eq!(echoed(&mut d), b"x");
    // IXANY: any key restarts.
    typed(&mut d, b"\x13");
    d.write(b"y");
    typed(&mut d, b"z");
    assert_eq!(echoed(&mut d), b"yz");
}

#[test]
fn output_processing() {
    let mut d = Discipline::new();
    assert_eq!(d.write(b"a\tb\n"), 4);
    assert_eq!(echoed(&mut d), b"a\tb\r\n");
    let mut t = *d.termios();
    t.c_oflag |= TAB3;
    d.set_termios(t);
    d.write(b"a\tb");
    assert_eq!(echoed(&mut d), b"a       b");
    t.c_oflag &= !OPOST;
    d.set_termios(t);
    d.write(b"\n");
    assert_eq!(echoed(&mut d), b"\n");
}

#[test]
fn no_echo() {
    let mut d = Discipline::new();
    let mut t = *d.termios();
    t.c_lflag &= !ECHO;
    t.c_lflag |= ECHONL;
    d.set_termios(t);
    typed(&mut d, b"secret\n");
    assert_eq!(echoed(&mut d), b"\r\n");
    assert_eq!(line(&mut d).unwrap(), b"secret\n");
}

#[test]
fn a_full_queue_rings() {
    let mut d = Discipline::new();
    for _ in 0..d.input_size() {
        d.input(b'a');
    }
    echoed(&mut d);
    // Nothing is readable (one long line): the byte is dropped.
    assert_eq!(d.input(b'a'), Input::Taken);
    assert_eq!(echoed(&mut d), b"\x07");
}

#[test]
fn queues_hold_two_seconds_in_whole_blocks() {
    let mut d = Discipline::new();
    // 9600 bits a second: 1920 bytes, 15 input blocks and 8 output ones.
    assert_eq!(d.input_size(), 1920);
    assert_eq!(d.output_room(), 1984);
    let mut t = *d.termios();
    t.c_ispeed = 115_200;
    t.c_ospeed = 115_200;
    d.set_termios(t);
    d.set_watermarks();
    assert_eq!(d.input_size(), 23_040);
    assert_eq!(d.output_room(), 23_064);
    // No input queue without `CREAD`.
    t.c_cflag &= !CREAD;
    d.set_termios(t);
    d.set_watermarks();
    assert_eq!(d.input_size(), 0);
    d.close();
    assert_eq!(d.output_room(), 0);
}

#[test]
fn a_full_queue_with_lines_refuses_input() {
    let mut d = Discipline::new();
    let text = [b'x'; 63];
    for _ in 0..30 {
        typed(&mut d, &text);
        typed(&mut d, b"\n");
    }
    // 1920 bytes typed: the queue is full and lines wait. Until a byte
    // is refused, a device may offer one.
    assert_eq!(d.input_room(), 1);
    assert_eq!(d.input(b'y'), Input::Full);
    assert_eq!(d.input_room(), 0);
    // Reading one line frees nothing (its block is still in use), the
    // second its first block.
    assert_eq!(line(&mut d).unwrap().len(), 64);
    assert_eq!(d.input_room(), 0);
    assert!(!d.read_done());
    assert_eq!(line(&mut d).unwrap().len(), 64);
    assert_eq!(d.input_room(), 128);
    assert_eq!(d.input(b'y'), Input::Taken);
}

#[test]
fn raw_input_bypasses_processing() {
    let mut d = Discipline::new();
    let mut t = *d.termios();
    t.c_iflag = 0;
    t.c_lflag = 0;
    d.set_termios(t);
    assert!(d.can_bypass());
    let data = [b'\r'; 2000];
    let mut signals = 0;
    assert_eq!(d.input_bulk(&data, &mut |_| signals += 1), 1920);
    assert_eq!(signals, 0);
    assert_eq!(d.read_poll(), 1920);
    assert_eq!(d.input_room(), 0);
    let mut buf = [0u8; 2000];
    assert_eq!(d.read_raw(&mut buf), 1920);
    assert!(buf[..1920].iter().all(|&c| c == b'\r'));
    assert!(d.read_done());
    assert_eq!(d.input_room(), 1920);
}

#[test]
fn writers_wake_below_the_low_watermark() {
    let mut d = Discipline::new();
    let mut t = *d.termios();
    t.c_oflag = 0;
    d.set_termios(t);
    let data = [b'a'; 4096];
    assert_eq!(d.write(&data), 1984);
    d.set_hiwat_out();
    let mut buf = [0u8; 1000];
    // 1000 bytes out leaves 1000 free of 1984: under 90%.
    assert_eq!(d.getc(&mut buf), (1000, false));
    assert!(d.hiwat_out());
    assert_eq!(d.getc(&mut buf), (984, true));
    assert!(!d.hiwat_out());
}

#[test]
fn utf8_characters_erase_whole() {
    let mut d = Discipline::new();
    typed(&mut d, "xé".as_bytes());
    echoed(&mut d);
    typed(&mut d, b"\x7f\n");
    assert_eq!(echoed(&mut d), b"\x08 \x08\r\n");
    assert_eq!(line(&mut d).unwrap(), b"x\n");
}

#[test]
fn erasing_after_program_output_retypes() {
    let mut d = Discipline::new();
    typed(&mut d, b"ab");
    d.write(b"!");
    echoed(&mut d);
    typed(&mut d, b"\x7f");
    // The program wrote after "ab": the line is typed again.
    assert_eq!(echoed(&mut d), b"^R\r\na");
}
