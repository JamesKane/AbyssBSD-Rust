// SPDX-License-Identifier: BSD-2-Clause
//! Driving a QEMU guest through its QMP socket: typing on its keyboard
//! and capturing its screen, for tests of the framebuffer console and
//! USB input (ROADMAP U.0).
//!
//! `cargo xtask run --qmp SOCK` exposes the socket; `cargo xtask qmp
//! --sock SOCK --type TEXT` or `--screendump FILE` uses it. Input goes
//! to the devices bound to no display, unless `--device ID` names a
//! display: then to those bound to it (`run --usb` binds the virtio
//! keyboard and tablet to `video0`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::Args;
use crate::util::Result;

/// How long each chord is held down. USB keyboards report every 8 or
/// 10 ms, one key event a report, from a queue of 16 in QEMU that drops
/// events silently when full: a shifted chord is four events, so it is
/// held, and followed by a gap, long enough for the guest to drain them.
const HOLD_MS: u64 = 50;
/// The pause after a chord's release.
const GAP_MS: u64 = 50;

/// One key press, with the modifiers held across it (a US layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chord {
    /// QEMU's name for the key (its `QKeyCode`).
    pub(crate) key: &'static str,
    pub(crate) shift: bool,
    pub(crate) ctrl: bool,
}

impl Chord {
    const fn plain(key: &'static str) -> Self {
        Self {
            key,
            shift: false,
            ctrl: false,
        }
    }

    const fn shifted(key: &'static str) -> Self {
        Self {
            key,
            shift: true,
            ctrl: false,
        }
    }
}

/// QEMU's key names for `a` to `z`, in order.
const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

/// And for `0` to `9`.
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// The characters on a US keyboard's punctuation keys: unshifted, shifted,
/// and the key.
const PUNCTUATION: [(char, char, &str); 11] = [
    ('-', '_', "minus"),
    ('=', '+', "equal"),
    ('[', '{', "bracket_left"),
    (']', '}', "bracket_right"),
    ('\\', '|', "backslash"),
    (';', ':', "semicolon"),
    ('\'', '"', "apostrophe"),
    ('`', '~', "grave_accent"),
    (',', '<', "comma"),
    ('.', '>', "dot"),
    ('/', '?', "slash"),
];

/// The shifted digits, `)` for 0 through `(` for 9.
const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

/// The chord that types `c` on a US layout. Control characters are Ctrl
/// with a letter, except those with keys of their own (tab, return,
/// backspace, escape).
pub(crate) fn chord(c: char) -> Option<Chord> {
    let chord = match c {
        '\n' | '\r' => Chord::plain("ret"),
        '\t' => Chord::plain("tab"),
        '\u{8}' => Chord::plain("backspace"),
        '\u{1b}' => Chord::plain("esc"),
        ' ' => Chord::plain("spc"),
        'a'..='z' => Chord::plain(LETTERS[usize::from(u8::try_from(c).ok()? - b'a')]),
        'A'..='Z' => Chord::shifted(LETTERS[usize::from(u8::try_from(c).ok()? - b'A')]),
        '0'..='9' => Chord::plain(DIGITS[usize::from(u8::try_from(c).ok()? - b'0')]),
        '\u{1}'..='\u{1a}' => Chord {
            key: LETTERS[usize::from(u8::try_from(c).ok()? - 1)],
            shift: false,
            ctrl: true,
        },
        c => match SHIFTED_DIGITS.iter().position(|&s| s == c) {
            Some(d) => Chord::shifted(DIGITS[d]),
            None => {
                let &(plain, _, key) = PUNCTUATION
                    .iter()
                    .find(|&&(plain, shifted, _)| c == plain || c == shifted)?;
                if c == plain {
                    Chord::plain(key)
                } else {
                    Chord::shifted(key)
                }
            }
        },
    };
    Some(chord)
}

/// The chords that type `text`, or the first character that has none.
pub(crate) fn chords(text: &str) -> Result<Vec<Chord>> {
    text.chars()
        .map(|c| chord(c).ok_or_else(|| format!("no key types {c:?} on a US layout").into()))
        .collect()
}

/// A QMP session, past the capabilities handshake.
pub(crate) struct Qmp {
    out: UnixStream,
    replies: BufReader<UnixStream>,
}

impl Qmp {
    pub(crate) fn connect(sock: &Path) -> Result<Self> {
        let out = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
        out.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut q = Self {
            replies: BufReader::new(out.try_clone()?),
            out,
        };
        // The greeting, then the handshake.
        let mut greeting = String::new();
        q.replies.read_line(&mut greeting)?;
        q.execute(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(q)
    }

    /// Sends one command and returns its reply, skipping the events QEMU
    /// interleaves. An `error` reply is an error.
    pub(crate) fn execute(&mut self, command: &str) -> Result<String> {
        writeln!(self.out, "{command}")?;
        loop {
            let mut line = String::new();
            if self.replies.read_line(&mut line)? == 0 {
                return Err("QMP: the socket closed".into());
            }
            if line.starts_with(r#"{"return""#) {
                return Ok(line);
            }
            if line.starts_with(r#"{"error""#) {
                return Err(format!("QMP: {}", line.trim_end()).into());
            }
        }
    }

    /// Types `text` on the guest's keyboard (`device`, or QEMU's
    /// choice), one chord at a time.
    pub(crate) fn type_text(&mut self, text: &str, device: Option<&str>) -> Result<()> {
        for c in chords(text)? {
            match device {
                None => {
                    self.execute(&send_key(c))?;
                    // The release comes `HOLD_MS` after the press; wait it
                    // out so chords never overlap.
                    std::thread::sleep(Duration::from_millis(HOLD_MS + GAP_MS));
                }
                Some(d) => {
                    self.execute(&chord_events(c, true, d))?;
                    std::thread::sleep(Duration::from_millis(HOLD_MS));
                    self.execute(&chord_events(c, false, d))?;
                    std::thread::sleep(Duration::from_millis(GAP_MS));
                }
            }
        }
        Ok(())
    }

    /// Presses `keys` (QEMU key codes, as `alt-f2`) together on the
    /// guest's keyboard (QEMU's choice).
    pub(crate) fn send_keys(&mut self, keys: &str) -> Result<()> {
        let keys: Vec<String> = keys
            .split('-')
            .map(|k| format!(r#"{{"type":"qcode","data":{}}}"#, json_string(k)))
            .collect();
        self.execute(&format!(
            r#"{{"execute":"send-key","arguments":{{"keys":[{}],"hold-time":{HOLD_MS}}}}}"#,
            keys.join(",")
        ))?;
        std::thread::sleep(Duration::from_millis(HOLD_MS + GAP_MS));
        Ok(())
    }

    /// Moves the guest's relative pointer (`device`, or QEMU's choice) by
    /// `dx`, `dy`.
    pub(crate) fn move_pointer(&mut self, dx: i32, dy: i32, device: Option<&str>) -> Result<()> {
        self.execute(&move_rel(dx, dy, device))?;
        Ok(())
    }

    /// Puts the guest's absolute pointer (`device`, or QEMU's choice) at
    /// `x`, `y`, each from 0 to 0x7fff.
    pub(crate) fn point(&mut self, x: i32, y: i32, device: Option<&str>) -> Result<()> {
        self.execute(&move_abs(x, y, device))?;
        Ok(())
    }

    /// Writes the guest's screen to `file` as a PPM image.
    pub(crate) fn screendump(&mut self, file: &Path) -> Result<()> {
        let file = std::path::absolute(file)?;
        self.execute(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":{}}}}}"#,
            json_string(&file.to_string_lossy())
        ))?;
        Ok(())
    }
}

/// The `send-key` command for one chord, modifiers first.
fn send_key(c: Chord) -> String {
    let mut keys = Vec::new();
    if c.ctrl {
        keys.push("ctrl");
    }
    if c.shift {
        keys.push("shift");
    }
    keys.push(c.key);
    let keys: Vec<String> = keys
        .iter()
        .map(|k| format!(r#"{{"type":"qcode","data":"{k}"}}"#))
        .collect();
    format!(
        r#"{{"execute":"send-key","arguments":{{"keys":[{}],"hold-time":{HOLD_MS}}}}}"#,
        keys.join(",")
    )
}

/// The `input-send-event` command for `events`, to `device` if given.
fn send_events(events: &[String], device: Option<&str>) -> String {
    let device = device.map_or(String::new(), |d| {
        format!(r#""device":{},"#, json_string(d))
    });
    format!(
        r#"{{"execute":"input-send-event","arguments":{{{device}"events":[{}]}}}}"#,
        events.join(",")
    )
}

/// The `input-send-event` command that presses (modifiers first) or
/// releases (modifiers last) one chord's keys on `device`.
fn chord_events(c: Chord, down: bool, device: &str) -> String {
    let mut keys = Vec::new();
    if c.ctrl {
        keys.push("ctrl");
    }
    if c.shift {
        keys.push("shift");
    }
    keys.push(c.key);
    if !down {
        keys.reverse();
    }
    let events: Vec<String> = keys
        .iter()
        .map(|k| {
            format!(
                r#"{{"type":"key","data":{{"down":{down},"key":{{"type":"qcode","data":"{k}"}}}}}}"#
            )
        })
        .collect();
    send_events(&events, Some(device))
}

/// The `input-send-event` command for a relative motion.
fn move_rel(dx: i32, dy: i32, device: Option<&str>) -> String {
    send_events(
        &[
            format!(r#"{{"type":"rel","data":{{"axis":"x","value":{dx}}}}}"#),
            format!(r#"{{"type":"rel","data":{{"axis":"y","value":{dy}}}}}"#),
        ],
        device,
    )
}

/// The `input-send-event` command for an absolute position.
fn move_abs(x: i32, y: i32, device: Option<&str>) -> String {
    send_events(
        &[
            format!(r#"{{"type":"abs","data":{{"axis":"x","value":{x}}}}}"#),
            format!(r#"{{"type":"abs","data":{{"axis":"y","value":{y}}}}}"#),
        ],
        device,
    )
}

/// `s` as a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An RGB image, as a `screendump` writes it.
#[derive(Debug)]
pub(crate) struct Ppm {
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// Three bytes a pixel, rows top to bottom.
    pub(crate) rgb: Vec<u8>,
}

impl Ppm {
    /// Parses a binary (`P6`) PPM with 8-bit channels.
    pub(crate) fn parse(data: &[u8]) -> Result<Self> {
        // The header: magic, width, height, maxval, each followed by
        // white space; `#` comments to the end of a line.
        let mut fields = Vec::new();
        let mut i = 0;
        while fields.len() < 4 {
            match data.get(i) {
                None => return Err("PPM: truncated header".into()),
                Some(b'#') => {
                    while data.get(i).is_some_and(|&b| b != b'\n') {
                        i += 1;
                    }
                }
                Some(b) if b.is_ascii_whitespace() => i += 1,
                Some(_) => {
                    let start = i;
                    while data.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
                        i += 1;
                    }
                    fields.push(&data[start..i]);
                }
            }
        }
        // One white-space byte ends the header.
        let body = data.get(i + 1..).ok_or("PPM: no pixels")?;
        let number = |f: &[u8]| -> Result<usize> {
            std::str::from_utf8(f)
                .ok()
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| "PPM: bad number".into())
        };
        if fields[0] != b"P6" {
            return Err("PPM: not a binary PPM (P6)".into());
        }
        let (width, height) = (number(fields[1])?, number(fields[2])?);
        if number(fields[3])? != 255 {
            return Err("PPM: only 8-bit channels".into());
        }
        let len = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(3))
            .ok_or("PPM: too large")?;
        let rgb = body.get(..len).ok_or("PPM: truncated pixels")?.to_vec();
        Ok(Self { width, height, rgb })
    }

    /// How many pixels are not black: none means a blank screen.
    pub(crate) fn lit(&self) -> usize {
        self.rgb
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p != [0, 0, 0])
            .count()
    }

    /// The pixel at `(x, y)`.
    pub(crate) fn pixel(&self, x: usize, y: usize) -> Option<[u8; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * 3;
        Some([self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]])
    }

    /// The text on a `vt` screen in the built-in font, a line a row with
    /// trailing blanks cut: the text area is centered as `vt` lays it out
    /// (`vt_compute_drawable_area`), and each cell is matched against the
    /// font's glyphs, in any colors, reversed too (the cursor). A cell no
    /// glyph matches reads as `\u{fffd}`.
    pub(crate) fn text(&self) -> Vec<String> {
        let (fw, fh) = (font::WIDTH, font::HEIGHT);
        let mut glyphs = std::collections::HashMap::new();
        for c in (' '..='~').rev() {
            glyphs.insert(*font::glyph(c), c);
        }
        let (cols, rows) = (self.width / fw, self.height / fh);
        let (x0, y0) = ((self.width % fw) / 2, (self.height % fh) / 2);
        (0..rows)
            .map(|r| {
                let line: String = (0..cols)
                    .map(|c| {
                        let (x, y) = (x0 + c * fw, y0 + r * fh);
                        // The last column and row of a cell are blank in
                        // every glyph but box drawing: its background.
                        let bg = self.pixel(x + fw - 1, y + fh - 1);
                        let mut ink = [0u8; font::HEIGHT];
                        for (dy, row) in ink.iter_mut().enumerate() {
                            for dx in 0..fw {
                                if self.pixel(x + dx, y + dy) != bg {
                                    *row |= 0x80 >> dx;
                                }
                            }
                        }
                        let reversed = ink.map(|b| !b);
                        glyphs
                            .get(&ink)
                            .or_else(|| glyphs.get(&reversed))
                            .copied()
                            .unwrap_or('\u{fffd}')
                    })
                    .collect();
                line.trim_end().to_string()
            })
            .collect()
    }
}

/// `cargo xtask qmp --sock SOCK [--device ID] [--type TEXT] [--move DX,DY]
/// [--abs X,Y] [--screendump FILE] [--unplug ID]`. A screen dumped is
/// also printed as text.
pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let sock = PathBuf::from(args.take_value("--sock")?.ok_or("qmp needs --sock SOCK")?);
    let text = args.take_value("--type")?;
    let motion = args.take_value("--move")?;
    let position = args.take_value("--abs")?;
    let dump = args.take_value("--screendump")?;
    let device = args.take_value("--device")?;
    let keys = args.take_value("--key")?;
    let unplug = args.take_value("--unplug")?;
    args.finish()?;
    if text.is_none()
        && motion.is_none()
        && position.is_none()
        && dump.is_none()
        && keys.is_none()
        && unplug.is_none()
    {
        return Err(
            "qmp: nothing to do (--type TEXT, --key KEYS, --move DX,DY, --abs X,Y, --screendump FILE, --unplug ID)".into(),
        );
    }
    let pair = |v: &str, what: &str| -> Result<(i32, i32)> {
        v.split_once(',')
            .and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))
            .ok_or_else(|| format!("qmp: {what}").into())
    };
    let mut q = Qmp::connect(&sock)?;
    if let Some(m) = motion {
        let (dx, dy) = pair(&m, "--move takes DX,DY")?;
        q.move_pointer(dx, dy, device.as_deref())?;
    }
    if let Some(p) = position {
        let (x, y) = pair(&p, "--abs takes X,Y")?;
        q.point(x, y, device.as_deref())?;
    }
    if let Some(text) = text {
        // `\n` in the argument means return, as a shell cannot easily
        // pass a newline.
        q.type_text(&text.replace("\\n", "\n"), device.as_deref())?;
    }
    if let Some(k) = keys {
        q.send_keys(&k)?;
    }
    if let Some(id) = unplug {
        // A hot unplug, as pulling the device out.
        q.execute(&format!(
            "{{\"execute\":\"device_del\",\"arguments\":{{\"id\":{}}}}}",
            json_string(&id)
        ))?;
    }
    if let Some(file) = dump {
        let file = PathBuf::from(file);
        q.screendump(&file)?;
        let img = Ppm::parse(&std::fs::read(&file)?)?;
        eprintln!(
            "xtask: qmp: {} ({}x{}, {} pixels lit)",
            file.display(),
            img.width,
            img.height,
            img.lit()
        );
        for line in img.text() {
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen of `w` by `h` pixels with `lines` drawn as `vt` draws
    /// them, the cell at `cursor` reversed.
    fn screen(w: usize, h: usize, lines: &[&str], cursor: (usize, usize)) -> Ppm {
        let (bg, fg) = ([0, 0, 0xaa], [0xaa, 0xaa, 0xaa]);
        let mut rgb: Vec<u8> = std::iter::repeat_n(bg, w * h).flatten().collect();
        let (x0, y0) = ((w % font::WIDTH) / 2, (h % font::HEIGHT) / 2);
        for (r, line) in lines.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                let g = font::glyph(ch);
                for (dy, bits) in g.iter().enumerate() {
                    for dx in 0..font::WIDTH {
                        let on = (bits & (0x80 >> dx) != 0) != ((r, c) == cursor);
                        let (x, y) = (x0 + c * font::WIDTH + dx, y0 + r * font::HEIGHT + dy);
                        let i = (y * w + x) * 3;
                        rgb[i..i + 3].copy_from_slice(if on { &fg } else { &bg });
                    }
                }
            }
        }
        Ppm {
            width: w,
            height: h,
            rgb,
        }
    }

    #[test]
    fn screen_text_reads_back() {
        let img = screen(
            8 * 20 + 5,
            16 * 3 + 9,
            &["# echo USB-$((6*7))", "USB-42", "# "],
            (2, 2),
        );
        assert_eq!(img.text(), ["# echo USB-$((6*7))", "USB-42", "#"]);
    }

    #[test]
    fn letters_digits_and_shift() {
        assert_eq!(chord('a'), Some(Chord::plain("a")));
        assert_eq!(chord('Z'), Some(Chord::shifted("z")));
        assert_eq!(chord('7'), Some(Chord::plain("7")));
        assert_eq!(chord('&'), Some(Chord::shifted("7")));
        assert_eq!(chord(')'), Some(Chord::shifted("0")));
        assert_eq!(chord(' '), Some(Chord::plain("spc")));
        assert_eq!(chord('\n'), Some(Chord::plain("ret")));
    }

    #[test]
    fn punctuation() {
        assert_eq!(chord('-'), Some(Chord::plain("minus")));
        assert_eq!(chord('_'), Some(Chord::shifted("minus")));
        assert_eq!(chord('|'), Some(Chord::shifted("backslash")));
        assert_eq!(chord('"'), Some(Chord::shifted("apostrophe")));
        assert_eq!(chord('~'), Some(Chord::shifted("grave_accent")));
        assert_eq!(chord('/'), Some(Chord::plain("slash")));
        assert_eq!(chord('?'), Some(Chord::shifted("slash")));
    }

    #[test]
    fn controls() {
        let ctrl_c = chord('\u{3}').unwrap();
        assert_eq!((ctrl_c.key, ctrl_c.ctrl, ctrl_c.shift), ("c", true, false));
        assert_eq!(chord('\t'), Some(Chord::plain("tab")));
        assert_eq!(chord('\u{8}'), Some(Chord::plain("backspace")));
        assert_eq!(chord('\u{1b}'), Some(Chord::plain("esc")));
        assert!(chord('é').is_none());
        assert!(chords("ls -l\n").is_ok());
        assert!(chords("naïve").is_err());
    }

    #[test]
    fn every_printable_ascii_types() {
        for b in 0x20u8..0x7f {
            let c = char::from(b);
            assert!(chord(c).is_some(), "{c:?}");
        }
    }

    #[test]
    fn send_key_json() {
        assert_eq!(
            send_key(chord('A').unwrap()),
            format!(
                r#"{{"execute":"send-key","arguments":{{"keys":[{{"type":"qcode","data":"shift"}},{{"type":"qcode","data":"a"}}],"hold-time":{HOLD_MS}}}}}"#
            )
        );
    }

    #[test]
    fn move_rel_json() {
        assert_eq!(
            move_rel(3, -4, None),
            r#"{"execute":"input-send-event","arguments":{"events":[{"type":"rel","data":{"axis":"x","value":3}},{"type":"rel","data":{"axis":"y","value":-4}}]}}"#
        );
    }

    #[test]
    fn chord_events_json() {
        let c = Chord::shifted("a");
        assert_eq!(
            chord_events(c, false, "vkbd"),
            r#"{"execute":"input-send-event","arguments":{"device":"vkbd","events":[{"type":"key","data":{"down":false,"key":{"type":"qcode","data":"a"}}},{"type":"key","data":{"down":false,"key":{"type":"qcode","data":"shift"}}}]}}"#
        );
    }

    #[test]
    fn json_strings() {
        assert_eq!(json_string(r#"/a "b"\c"#), r#""/a \"b\"\\c""#);
    }

    #[test]
    fn ppm() {
        let mut data = b"P6\n# a comment\n2 1\n255\n".to_vec();
        data.extend([1, 2, 3, 4, 5, 6]);
        let img = Ppm::parse(&data).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.pixel(1, 0), Some([4, 5, 6]));
        assert_eq!(img.pixel(2, 0), None);
        assert_eq!(img.lit(), 2);
        assert!(Ppm::parse(b"P6\n2 1\n255\n\x01").is_err());
        assert!(Ppm::parse(b"P3\n1 1\n255\n1 2 3").is_err());
    }
}
