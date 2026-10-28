# Terminal emulator

- Status: **Draft**. A forward design for `sable-term`, the terminal emulator
  [DESKTOP.md](DESKTOP.md) §8.9 defers to its own document. Decisions here join the M-D ADR.
- Date: 2026-10-27
- Scope: the terminal's data model, parser, renderer, process and sandbox structure, configuration,
  testing, and performance. The pty layer is the kernel's (MULTIUSER.md §9) and is in scope only for
  what the terminal needs from it (§9). The `vt(4)` console and `lib/vtterm` are out of scope beyond
  §2.3.
- Inputs: [DESKTOP.md](DESKTOP.md) (§2.1, §4.7.3, §4.7.4, §6, §7, §8.9),
  [MULTIUSER.md](MULTIUSER.md) §9 (terminals, ptys), [ROADMAP.md](ROADMAP.md) (M-D, P6.6, U.4),
  [ENGINEERING.md](ENGINEERING.md), [PERFORMANCE.md](PERFORMANCE.md), ADR-0007, ADR-0014

The short version: **a terminal whose cost is a scan and a memcpy per byte, and whose rendering
cost depends only on the size of the window.** The pty is drained in large reads into a receive
buffer. A parser scans it for control bytes a word at a time, and plain text is appended to a
scrollback arena in a compact byte form, never expanded into cells. Cells exist only for the rows on
screen, and only when a frame is drawn. The renderer uploads those cells and draws the whole grid
in one GPU pass from a glyph atlas filled on demand, keyed by a hash of each grapheme cluster. The
process that parses untrusted output runs in Capsicum capability mode, and a small unsandboxed
spawner makes ptys and starts shells with `pdfork(2)`. There are no tabs or splits: windows are the
compositor's job.

## 1. Principles

### 1.1 What we take from refterm

In 2021 Casey Muratori published **refterm**, a reference terminal renderer, to show that terminal
throughput and latency problems are not inherent. His argument, as stated in his public talks and
writing, is that a terminal does very little work per byte. A careful but unheroic design
("non-pessimized" rather than "optimized", in his words) is limited by memory bandwidth and by the
pipe feeding it, not by parsing or rendering.

**refterm is GPL-2.0. Its source is not consulted**, as for all GPL code (ADR-0007). What follows
uses the publicly described ideas, not the code. Every mechanism below is designed here, for
AbyssBSD.

| # | Principle | Here |
|---|---|---|
| R1 | **Throughput is bounded by the pipe and memory bandwidth.** Printing a gigabyte should take about as long as moving a gigabyte | §3, §4. The `term.pty-ratio` diagnostic (§10.2) measures the terminal against the bare pty |
| R2 | **Work scales with what is visible, not with what arrived.** Output is parsed as it comes, but laid out and drawn only once per displayed frame | §3.4, §5.1. Rows that scroll past between frames are never laid out or rasterized |
| R3 | **Scrollback is bytes, not cells.** A circular byte buffer with a line index, using a mirrored virtual-memory mapping so no read or decode ever splits at the wrap | §3.2 |
| R4 | **One GPU pass draws the grid.** Cells (glyph, colors, flags) go to a GPU buffer, and one shader reads them and the glyph atlas | §5.4 |
| R5 | **Glyphs are cached by the text that produced them.** The cache key is a hash of the cluster and its style, not a code point. A miss shapes and rasterizes once, into one or more atlas tiles | §5.3 |
| R6 | **A fast path for simple text, the full machinery only when needed.** Only clusters outside the fast path pay for segmentation and shaping | §4.2, §5.3 |
| R7 | **Correct text is not optional.** Combining marks, wide characters, emoji sequences, and every script render correctly, without a cost to plain ASCII | §5.2, §5.3 |

### 1.2 What AbyssBSD adds

| # | Principle | Consequence |
|---|---|---|
| A1 | **AbyssBSD-native** (DESKTOP.md P6) | kqueue for every wait, `pdfork(2)` and `EVFILT_PROCDESC` for children, Capsicum for the parser, `kern.proc.cwd` (not escape sequences) for "open a new window here", base's `libulog` for sessions |
| A2 | **Untrusted output** | Every byte a program prints is untrusted, including `cat` of a file from the internet. The parser is safe Rust, fuzzed, bounded in memory, and runs in capability mode (§7) |
| A3 | **One canonical way** (DESKTOP.md P4) | No tabs, splits, or session multiplexing: the compositor tiles windows, and tmux from packages exists for those who want it. One shader language (SSL), one config tree, one theme |
| A4 | **Agree with libc** | Column widths come from FreeBSD 15.1's `wcwidth(3)` data, because programs place the cursor by libc's widths. A terminal that disagrees with libc corrupts every full-screen program (§5.2) |
| A5 | **Measured** | A `desk.term` component under ADR-0014, against foot on sway on FreeBSD 15.1 (§10) |

### 1.3 Non-goals

- Tabs, splits, and built-in multiplexing (A3).
- Sixel, the kitty graphics protocol, and other inline images in the first release (§12 question 4).
- Bidirectional text reordering. Terminals cannot reorder without breaking cursor addressing, and
  the standards for doing it (ECMA-48 bidi modes) have little application support. Right-to-left
  text is shaped correctly in logical order.
- Subpixel (LCD) antialiasing. Wayland surfaces may be scaled, transformed, or captured, so glyphs
  are grayscale, blended in linear space as the compositor's are (DESKTOP.md §4.7.3).
- A custom `TERM` name in the first release (§6.1).
- Running on the `vt(4)` console. That is `lib/vtterm`'s job (§2.3).

## 2. Context

### 2.1 Where it sits

`sable-term` is an ordinary Wayland application (DESKTOP.md §8.9): an `xdg_toplevel` rendered
client-side with Vulkan, on the shared toolkit (`lib/scene-render`) for its chrome (scrollbar, search
bar, the IME preedit) and a dedicated cell pass for the grid (§5.4). It follows DESKTOP.md §2.1's
dependency rule: the inventory's xkbcommon and HarfBuzz, base's `libc` and `libulog`, Mesa's Vulkan
driver, and nothing else foreign.

```
            keyboard, IME, pointer (Wayland)                       frames (Vulkan WSI)
                         │                                                ▲
 ┌───────────────────────▼────────────────────────────────────────────────┴──────────┐
 │ sable-term (capability mode)                                                      │
 │                                                                                   │
 │  UI thread ─── key encoding ──▶ write(master)        render thread                │
 │      │                                                ▲  snapshot rows            │
 │      │ resize: TIOCSWINSZ                             │  glyph cache, atlas       │
 │      ▼                                                │  cell upload, one pass    │
 │  pty thread: read(master) ─▶ receive ring ─▶ parser ─▶ screen + scrollback arena  │
 │      ▲ EVFILT_READ, EVFILT_PROCDESC                                               │
 └──────┼───────────────────────────────────────────────▲───────────────────────────┘
        │ master fds                                    │ spawn(profile, size, cwd-of)
        │                                               ▼   → (master fd, process descriptor)
   /dev/pts/N ◀── shell ◀── pdfork, setsid, TIOCSCTTY ── sable-term-spawn (unsandboxed, small)
```

### 2.2 Crates

| Crate | Kind | Contents |
|---|---|---|
| `lib/termcore` | `no_std` + `alloc`, `#![forbid(unsafe_code)]` | Parser, screen model, row language, scrollback arena and line index over a storage trait, reflow, selection, search, key encoding. Host-tested and fuzzed |
| `lib/unicode` | `no_std`, generated tables | Grapheme cluster breaks (UAX #29), emoji properties, and the column-width table generated from FreeBSD 15.1's locale data (§5.2). `lib/textlayout` and `lib/vtterm` may adopt it later |
| `desk/term` | `std` | `sable-term`: Wayland, Vulkan, the glyph cache, threads, the mirrored rings (the only `unsafe`, in one module), configuration |
| `desk/term-spawn` | `std` | `sable-term-spawn`: ptys, `pdfork`, `exec`, `libulog`. Small enough to read in one sitting |

`lib/termcore` knows nothing of Wayland, Vulkan, or ptys, so the whole terminal model runs in host
tests on byte streams, and the same crate drives the headless test harness (§10.1).

### 2.3 Relationship to `lib/vtterm`

`lib/vtterm` is FreeBSD's `teken` and `vt_buf`: it matches `teken` byte for byte, including its
`cons25` behavior, because the `vt(4)` console must behave as FreeBSD's does (U.4). `sable-term`
needs a different subset (xterm's, §6), a different storage model (bytes, not cell rows), and
different widths (libc's, where `teken` has its own). Sharing the parser would make one of the two
wrong. The two share no code at first. `lib/unicode` is written so that `vtterm` *could* take its
width table if FreeBSD's `teken` ever moves to libc's widths, and lessons from either side's
benchmarks cross over.

## 3. The data model

### 3.1 Three stores

| Store | Holds | Size |
|---|---|---|
| **Receive ring** | Raw bytes as read from the pty master | 1 MiB per terminal, mirrored |
| **Scrollback arena** | Committed and in-progress lines in the **row language** (§3.3) | `term.scrollback`, default 64 MiB, mirrored |
| **Screen** | One entry per visible row: a reference into the arena, or materialized cells | Rows × columns, small |

Plus the **line index**: a ring of fixed-size `LineRec` records, one per logical line, ordered by
when the line entered history.

```rust
/// A monotonic position in the arena. Never wraps; the arena is `off % capacity`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArenaOff(u64);

pub struct LineRec {
    start: ArenaOff,
    len: u32,
    flags: LineFlags,       // WRAPPED_FROM_PREVIOUS, PROMPT (OSC 133), …
}

pub enum Row {
    /// A visual slice of a logical line that is still pure append: its bytes are in the arena.
    Span { line: LineId, first_col: u32 },
    /// A row something has edited in place (cursor addressing, CR overwrite, erase, insert).
    Cells(CellRow),
}
```

An `ArenaOff` is valid while `tail − off ≤ capacity`, so whether a line is still in history is one
subtraction.

### 3.2 Mirrored rings

Both rings are a `memfd_create` object mapped twice, back to back, inside a `MAP_GUARD`
reservation with `MAP_FIXED` (MULTIUSER.md §6.1 lists both). Byte `capacity + i` is byte `i`. A
`read(2)` into the free space, a decoder walking a line that crosses the end, and the search scan
(§3.6) all see contiguous memory and have no wrap case. This is the only `unsafe` in `desk/term`,
behind a `MirroredRing` type whose methods hand out bounded slices. `lib/termcore` sees it through
a storage trait, which host tests implement with a `Vec`.

### 3.3 The row language

The arena does not store raw pty output. It stores a **canonical byte form of a line**: UTF-8
text, SGR sequences (`CSI … m`), and OSC 8 hyperlink sequences, and nothing else. Every logical line
**begins with the full SGR and hyperlink state** in effect at its start, when that state is not the
default, so any line decodes on its own, without replaying what came before.

The row language is a strict subset of what the parser reads, so one decoder serves both: laying
out a row from the arena is the parser in a restricted mode. It is also compact. A line of 80 ASCII
characters in one color costs 80 bytes plus a few for its SGR prefix, where a cell grid costs
80 × 16 bytes whatever the line holds. A 64 MiB arena holds about 800 000 typical lines.

### 3.4 The fast path

Output that only appends (printable text, SGR, OSC 8, LF, CR LF, soft wraps at the right margin)
never becomes cells. The parser copies printable runs and canonical SGR into the arena's tail as
part of the current logical line, and the screen's bottom rows are `Span`s over that line, cut at
the window's width. When the screen scrolls, the top row's line enters the line index, which costs
no copy because its bytes are already in the arena.

For `cat` of a large file, then, the per-byte work is the scan for control bytes (§4.2) and a
copy. Cells are made only for the rows visible when a frame is drawn (§5.1).

### 3.5 The slow path

Anything that changes text already on screen (cursor movement followed by printing, CR followed by
printing, BS, erase, insert or delete of characters or lines, scroll regions, the alternate screen)
**materializes** the affected rows into `Cells`, once, and edits them in place from then on. Full-screen
programs (vi, top, less) therefore run on a cell grid of the window's size, as on any terminal.

When a `Cells` row scrolls off the top of the primary screen, it is serialized into the row language
at the arena's tail and its `LineRec` enters the index. A progress bar that redraws itself ten
thousand times with CR leaves one line in history, not ten thousand.

Two rules keep the arena consistent:

- **On-screen spans stay valid.** A `Span` row whose bytes the arena's tail is about to overwrite
  (possible only for rows that stay on screen, outside a scroll region, while a lot of output
  passes) is materialized first. The check runs when the tail crosses a multiple of
  `capacity / 8`, over at most one screen of rows.
- **History is a suffix of valid lines.** Lines enter the index in scroll order, but a serialized
  `Cells` row's bytes are newer than a `Span` row's committed after it, so eviction can invalidate
  a line before an older one. Scrollback shows the index's newest records up to the first invalid
  one, and the rest is dropped. At most a screen's worth of lines is lost early.

The alternate screen (`?1049`) is always `Cells` and never enters history, as xterm behaves.

### 3.6 Reflow, selection, search

- **Reflow on resize** is free for `Span` rows: they are slices of logical lines, recut at the new
  width. `Cells` rows on the primary screen reflow by their soft-wrap flags, as xterm and foot do. The
  history needs no work at all, because history is logical lines, and the line index records which
  are continuations. Resizing a window with a million lines of scrollback costs the visible rows.
- **Selection** is a pair of positions in logical-line coordinates (`LineId`, byte offset), so it
  survives scrolling and reflow. Copy decodes the selected range to plain UTF-8 (or keeps SGR, as an
  action option).
- **Search** scans arena bytes directly, skipping SGR and OSC 8, at memory speed. Matches are
  reported as logical-line positions. There is no search index.

### 3.7 Memory bound

The arena, the receive ring, the line index (a fixed number of records, one per 16 arena bytes;
if it fills before the arena does, the oldest lines go), the screen, and the glyph cache (§5.3) are all fixed at configuration time. No byte stream
can make a terminal grow past them: the fuzzers (§10.1) check that resident memory stays under the
configured bound for every input.

## 4. Reading and parsing

### 4.1 The pty thread

One thread per `sable-term` process owns every pty master and runs a `sable-rt` executor on its own
kqueue:

1. `EVFILT_READ` on a master fires.
2. The thread reads with **one `read(2)` into the receive ring's free space** (up to the ring's free
   space, so as much as the kernel's output queue holds, §9), then parses that range.
3. It repeats until `EAGAIN`, releasing the model lock (§5.1) between reads so the render thread is
   never held off for more than one read's parse.
4. If anything visible changed and a frame is wanted, it wakes the render thread with `EVFILT_USER`.

The pty thread never waits for the GPU, the compositor, or the render thread. If output outruns the
display, frames show later states and skip earlier ones (R2). Reading is never throttled for
rendering, because a stalled read stalls the program writing to the terminal.

`EVFILT_PROCDESC` with `NOTE_EXIT` on each child's process descriptor reports the shell's exit with no
`SIGCHLD` handler and no PID race. `term.on-exit` decides whether the window closes, holds, or
offers to restart.

### 4.2 The parser

The parser is a state machine over ECMA-48 (ground, escape, CSI, OSC, DCS, string states) with one
optimization that matters: **in the ground state, it finds the next byte that is not printable
ASCII eight bytes at a time.** A `u64` word test (any byte below 0x20, equal to 0x7F, or at or above
0x80) is portable, safe Rust, and the same code on both architectures. Runs of printable ASCII are
copied to the arena in one `copy_from_slice`, and their column count is their length.

Bytes at or above 0x80 go to a UTF-8 decoder and the cluster segmenter (§5.2), which tracks each
cluster's width. Malformed UTF-8 becomes U+FFFD, one per maximal invalid subsequence, as the
Unicode standard recommends.

If profiling shows the word scan to be the bottleneck, it moves to SSE2 and NEON through
`core::arch` intrinsics in `desk/term` behind the same interface (intrinsics are not inline
assembly, and `lib/termcore` keeps the portable version). The design does not assume it will be
needed.

**Limits.** Every string state has a bound: CSI at 32 parameters, OSC at 4 KiB (64 KiB for OSC 52,
§6.3), DCS at 4 KiB and then ignored. A sequence that exceeds its bound is dropped whole and the
parser returns to ground, never panicking and never allocating past the bound.

## 5. Rendering

### 5.1 Frames

The render thread draws when the compositor's frame callback has fired **and** something changed.
It:

1. **Snapshots** the visible rows under the model lock: for each row, either its `Span` reference
   or a copy of its cells, plus the cursor, selection, and modes. This is at most one screen of
   data, and the lock is held for microseconds.
2. **Lays out** `Span` rows from the arena (the parser's restricted mode, §3.3) into cells. A
   per-row cache keyed by `(LineId, first_col, width)` skips rows that were already laid out.
3. **Resolves glyphs** for each cell's cluster (§5.3).
4. **Uploads** changed rows' cells (§5.4).
5. **Draws** the grid in one pass, then the chrome through `lib/scene-render`, in one command
   buffer, and presents with damage limited to changed rows (`wl_surface.damage_buffer`), so the
   compositor composes less too.

**Synchronized output** (mode `?2026`): while an application holds it, the render thread does not
draw, so full-screen redraws never show half-drawn states. Parsing continues. A hold longer than
`term.sync-timeout` (150 ms) is released.

**Latency.** A key press is encoded and written to the master on the UI thread at once, without
batching. When echoed output arrives within `term.input-window` (50 ms) of a key press, the render
thread draws as soon as it is woken instead of waiting for the next frame callback. With `wp_presentation` feedback the terminal knows when its
frames reached the screen, which the latency benchmark uses (§10.2).

### 5.2 Clusters and widths

A cell holds one **grapheme cluster** (UAX #29, extended), not one code point. Its width is 1 or 2
columns, and the width is **the width FreeBSD 15.1's `wcwidth(3)` gives**, with one rule for
clusters libc can't see as a whole:

- The cluster's width is its first code point's `wcwidth`, plus nothing for marks and joiners that
  libc gives width 0. This is what an application computing widths with libc arrives at, so cursor
  positions agree.
- A cluster libc would count as wider (an emoji ZWJ sequence that libc sees as two width-2 code
  points) is a known disagreement between terminals and applications. Mode `?2027` (grapheme cluster
  width, which several terminals implement) lets an application that understands it opt in to
  cluster widths. Without it, the terminal follows libc.

`tools/unicodegen` generates `lib/unicode`'s width table from FreeBSD 15.1's locale sources (fetched
from `release/15.1.0`), and a test on `abyss-build` checks the table against `wcwidth(3)` there for
every code point. A FreeBSD release that changes its Unicode version changes the table through the
generator, as `syscallgen` does for syscalls.

### 5.3 The glyph cache

The atlas is a grid of **tiles, each one cell in size** (in physical pixels at the window's scale).
Coverage glyphs go in an `R8_UNORM` atlas, color glyphs (emoji) in an `R8G8B8A8_SRGB` atlas, and a
cell flag picks which. A cluster of width 2 takes two tiles, and each of its cells names its own.

| Cluster | Lookup |
|---|---|
| Printable ASCII in a regular, bold, italic, or bold-italic face | **Direct table**: 95 × 4 tiles rasterized when the font loads. No hash, no shaping |
| Anything else | **Hash table** keyed by `(hash(cluster UTF-8), face, presentation)` to tile indices, with LRU eviction. A miss shapes the cluster with HarfBuzz, rasterizes it with `lib/raster` into its tiles, and uploads them |

- **Font fallback** runs on a miss only: the primary face, then the configured fallback list, then
  the emoji face, choosing the first that covers every code point in the cluster.
- **Overhang.** Each tile is clipped to its cell. Italic overhang and oversized icons are cut at
  the cell edge, which keeps the grid one pass with no overlap ordering. Glyphs that are wider than
  their cell by design (Nerd Font icons, some CJK punctuation) are scaled to fit their width.
- **Box drawing and block elements** (U+2500–U+259F) and Powerline's separators are drawn by
  `lib/raster` from their definitions at the cell's exact size, not taken from the font, so lines
  join between cells at any size, as most modern terminals do.
- **Ligatures**, off by default (`term.font.ligatures`), shape runs of same-style text in one
  HarfBuzz call and cache the result by the run's hash. The run's glyphs are cut into the run's
  tiles, so the grid stays one tile per cell.
- **Atlas size** is bounded (`term.font.cache`, default 4096 tiles a face). When it is full, the
  least recently used tiles are reused. A cluster in use by a visible cell is never evicted during
  the frame that uses it.

### 5.4 The cell pass

```rust
#[repr(C)]
struct GpuCell {
    tile: u32,      // atlas tile index; bit 31 selects the color atlas
    fg: u32,        // RGBA8, already resolved from the palette and SGR
    bg: u32,
    deco: u32,      // underline style and color index, strike, overline, blink phase
}
```

The cells of the visible grid live in a host-visible storage buffer, one per frame in flight, and
only changed rows are written. One draw of a full-window triangle runs a fragment shader that finds
its cell from the pixel position, reads the cell and its tile's texel, and blends background and
foreground in linear space. Cursor, selection, and underline styles are computed in the same
shader from the cell's flags and uniforms. Damage scissors the pass to changed rows.

The shader is SSL, compiled at build time by `tools/shadec` like the compositor's (DESKTOP.md
§4.7.4). It needs read-only storage buffers, which `lib/scene-render`'s instanced draws already
need. On lavapipe the pass is a few million fragment invocations at 4K, so the software path (any
UEFI machine, and QEMU before Venus) stays usable.

### 5.5 Scale

The terminal follows `wp_fractional_scale_v1`, renders at the preferred scale through
`wp_viewporter`, and sizes cells in whole physical pixels: the font's advance and line height at
that scale, rounded. A scale change re-rasterizes the direct table at once and the hash table
lazily.

## 6. Behavior

### 6.1 `TERM`

`TERM=xterm-256color` and `COLORTERM=truecolor`. FreeBSD's base termcap has `xterm-256color`, as
does every terminfo database a remote host is likely to have, so `ssh` to anything works. A
`sable` terminfo entry would describe the terminal exactly but break remote hosts that lack it
(§12 question 3). Applications that want to detect features ask: Primary and Secondary Device
Attributes, XTVERSION, DECRQM for modes, and XTGETTCAP for capabilities.

### 6.2 Supported sequences

The behavior reference is **xterm's** (its `ctlseqs` documentation, X11-licensed) where xterm and
the ECMA-48 and DEC documents agree, and xterm's behavior where they don't, since that is what
applications are tested against.

| Area | First release |
|---|---|
| Core | C0 controls, ECMA-48 cursor, erase, insert and delete, scroll regions (DECSTBM, DECSLRM), tab stops, character sets (DEC Special Graphics), DECSC/DECRC, DECSTR, RIS |
| Attributes | SGR: bold, faint, italic, underline styles and colors (`4:x`, `58`), blink, inverse, invisible, strike, overline; 16, 256, and direct colors (both `;` and `:` forms) |
| Modes | Origin, autowrap, insert, cursor visibility and style (DECSCUSR), application cursor and keypad, alternate screen (`?47`, `?1047`, `?1049`), bracketed paste (`?2004`), focus events (`?1004`), synchronized output (`?2026`), grapheme widths (`?2027`) |
| Mouse | X10, normal, button-event, and any-event tracking (`?9`, `?1000`, `?1002`, `?1003`) with SGR encoding (`?1006`) and SGR pixel encoding (`?1016`). Shift bypasses mouse reporting for selection |
| Keyboard | xterm's key encoding with `modifyOtherKeys` levels 1 and 2. The kitty keyboard protocol later (§12 question 5) |
| OSC | Titles (0, 2), palette query and set (4, 10, 11, 12, 104, 110–112), hyperlinks (8), clipboard (52, §6.3), working directory (7, shown, not trusted), shell marks (133) |
| Reports | DA1, DA2, XTVERSION, DSR (status, cursor position), DECRQM, DECRQSS for SGR and DECSTBM, XTGETTCAP, window size in cells and pixels (`CSI 14 t`, `CSI 18 t`). No report that returns the title (it echoes untrusted text into the input) |

### 6.3 Escape sequences are untrusted

| Sequence | Policy (each is a config key, P5) |
|---|---|
| OSC 52 clipboard write | Allowed while the window has keyboard focus (`term.osc52 "focused"`). `"always"` and `"off"` are the alternatives |
| OSC 52 clipboard read | Off. The user can turn it on, and the settings GUI says what that means: any program the terminal runs, including on a remote host, can read the clipboard |
| OSC 8 hyperlinks | Shown with an underline on hover, opened only on Ctrl+click, and the link's target is shown before opening. Only `http`, `https`, and `file` schemes open by default |
| Title set | Allowed, with control characters stripped and a length bound. Title *reports* are not implemented |
| OSC 7 working directory | Displayed in the title template if the user asks. Never used to choose where a new shell starts (§7.2) |
| Bell | `term.bell`: `"visual"`, `"urgent"` (sets `xdg_activation` urgency), or `"none"` |

### 6.4 Input

- **Keys** come through xkbcommon (the inventory's) with the keymap the compositor sends. The UI
  thread encodes them and writes the result to the master.
- **Paste** is bracketed when `?2004` is on. Bracketed or not, a paste containing control
  characters other than tab, CR, and LF has them shown as an error, and the user is asked before
  it is sent. `term.paste.confirm` turns that off.
- **IME**: `zwp_text_input_v3`, with the preedit drawn at the cursor through the chrome layer and
  the cursor rectangle reported back so candidate windows sit next to it.
- **Selection** is the clipboard (`wl_data_device`) on copy. Selecting text sets the primary
  selection, and middle click pastes it (CLIPBOARD.md §2.2, §4.4).

## 7. Processes and the sandbox

### 7.1 Why the split

`posix_openpt(2)` and `execve(2)` are not allowed in capability mode: one creates a device node by
name, and the other loads a program by path. The parser is the part that handles hostile input.
So the terminal is two processes, and the one that parses never needs either call.

| Process | Holds | Sandbox |
|---|---|---|
| `sable-term-spawn` | The user's credentials, the environment, profiles | None. It parses no terminal output and no fonts, and it reads only its own requests over a socket pair |
| `sable-term` | Its Wayland connection, a Vulkan render node, font file descriptors, the pty master fds, the spawn channel, process descriptors | Capability mode after init |

`sable-term` is what the user starts (the launcher or `bind … action="apps.launch" app="sable-term"`).
At startup it creates a socket pair, `pdfork`s the spawner on one end, initializes Wayland and
Vulkan (Mesa opens its ICD files and driver nodes, and the terminal creates its pipelines, before `cap_enter`), opens its
fonts, and enters capability mode. From then on:

- **The spawn channel** is a `SOCK_SEQPACKET` socket pair with three messages (spawn, spawned,
  failed), decoded by generated Conduit code (DESKTOP.md §6.3) even though it is private to the two
  processes. Resizes don't use it: the terminal sets the size on the master with `TIOCSWINSZ`.
- **Master fds** arrive by `SCM_RIGHTS`, already narrowed by the spawner with `cap_rights_limit`
  (read, write, event, ioctl) and `cap_ioctls_limit` (`TIOCSWINSZ`, `TIOCGWINSZ`, `FIONREAD`,
  `TIOCGPGRP`).
- **Process descriptors** arrive the same way, narrowed to `CAP_PDWAIT | CAP_PDKILL | CAP_EVENT`, so
  the terminal can watch its shell and close it (`pdkill`) and nothing more.

A compromise of the parser gives the attacker a sandboxed process that can write to the ptys it
already has and draw in its own window. It cannot open files, connect anywhere, or start programs.

### 7.2 Spawning

For each request, `sable-term-spawn`:

1. Opens a pty with `posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC)`, `grantpt`, `unlockpt`, and sets
   the window size.
2. Sets the slave's speed to `B115200` (§9.1).
3. `pdfork`s. The child calls `setsid`, opens the slave, takes it as its controlling terminal with
   `TIOCSCTTY`, puts it on fds 0–2, `closefrom(3)`, sets `TERM`, `COLORTERM`, and the profile's
   environment, changes directory, and `execve`s the profile's command (the user's login shell by
   default, from `getpwuid`).
4. Records the session with base's `libulog` (`ulog_login_pseudo`), when `term.utmp` is on, so `who`
   and `w` list it, and logs it out when the child exits.
5. Returns the master fd and the process descriptor.

**"Open a terminal here"** asks the spawner for a new shell in the *current* terminal's directory.
The spawner finds it from the kernel, not from the terminal's output: the foreground process group
of that pty (`TIOCGPGRP` on the master, passed back by the UI) and its working directory from
`kern.proc.cwd`. A program that prints a forged OSC 7 cannot steer where the next shell starts.

### 7.3 One process, several windows

One `sable-term` process serves every terminal window the user opens while it runs. New windows
(the `term.window.new` action, or `apps.launch` while an instance is running, which `sabled` routes
to that instance over the channel it already holds) share the Vulkan device, the font faces, and
the glyph atlases, so a new window draws its first frame without loading anything. A `sable-term`
started outside `sabled` is a new process.

The cost is that a crash closes every window, and every shell gets `SIGHUP`. The alternative, one
process per window, costs startup time (§10.2 `term.start`). §12 question 1 asks which to choose,
with a measurement to settle it.

## 8. Configuration

Configuration is the `term` subtree of the one tree (DESKTOP.md §7). Colors are theme tokens, so a
rice themes terminals as it themes everything else, and a rice cannot change shells or security keys.

```kdl
theme {
    token "font.mono" "JetBrains Mono"
    token "term.fg" (color)"#cdd6f4"
    token "term.bg" (color)"#1e1e2ef0"
    token "term.cursor" "@accent"
    token "term.ansi.0" (color)"#45475a"
    // … term.ansi.1 through term.ansi.15
}

term {
    font "@font.mono" size=(pt)11 fallback="Noto Sans CJK JP" "Noto Color Emoji"
    font-features ligatures=#false
    scrollback (MiB)64
    cursor shape="block" blink=#false
    bell "visual"
    osc52 "focused"
    on-exit "close"
    utmp #true

    profile "default" { command "$SHELL" "-l" }
    profile "build"   { command "/bin/sh" "-l" ; cwd "~/src" ; env "MAKEFLAGS" "-j20" }
}

bind {
    key "Super+Return"  action="apps.launch" app="sable-term"
    key "Ctrl+Shift+C"  action="term.copy"   app="sable-term"
    key "Ctrl+Shift+V"  action="term.paste"  app="sable-term"
    key "Ctrl+Shift+F"  action="term.search" app="sable-term"
}
```

- The schema is `#[derive(Schema)]` in `desk/term`, so the settings GUI, the manual page, and the
  language server come from it, as for any component (DESKTOP.md §7.4).
- `sable-term` subscribes to `term` and the theme tokens it references, and applies changes live:
  a palette change redraws, a font change rebuilds the atlases, a scrollback change resizes the
  arena (keeping the newest lines that fit).
- Its actions (`term.copy`, `term.paste`, `term.search`, `term.window.new`, `term.font.bigger`,
  `term.font.smaller`, `term.scroll.prompt-previous` and `-next` from OSC 133 marks, …) are exported
  like any component's and bound in `bind`. Application-scoped bindings (the `app=` argument) are
  §12 question 6.
- **Reaching `sabled`.** Config, theme, and `apps.open` (for hyperlinks) come over Conduit channels
  `sabled` grants when it launches the terminal (DESKTOP.md §6.4). A `sable-term` started some
  other way, from another terminal or a script, uses built-in defaults and copies links instead of
  opening them, rather than reaching for the control socket.

## 9. Kernel and platform requirements

| Need | Used for | Status |
|---|---|---|
| `posix_openpt`, `grantpt`/`unlockpt`/`ptsname` ioctls, `TIOCSWINSZ` and `SIGWINCH`, `TIOCSCTTY`, `TIOCGPGRP` | Ptys | P6.6, in progress |
| `pdfork`, `pdkill`, `EVFILT_PROCDESC` | Children | P4.5, done |
| `memfd_create`, `MAP_GUARD` with `MAP_FIXED` | Mirrored rings | P6.3, done (a test for the double mapping to add) |
| `SOCK_SEQPACKET`, `SCM_RIGHTS` | The spawn channel | P6.5, done |
| Capsicum: capability mode, `cap_rights_limit`, `cap_ioctls_limit`, process descriptor rights | The sandbox | P6.7, planned |
| `kern.proc.cwd` | "Open a terminal here" | P6.8, planned |
| `libulog` and its `ulog-helper` | Session records | Base, with P6's userland |

### 9.1 Pty output queue size

FreeBSD sizes a terminal's output queue from its output speed: two seconds of data, `c_ospeed / 5`
bytes, at most 64 KiB (`tty_watermarks`), and `sys/tty` does the same. A pty starts at
`TTYDEF_SPEED`, 9600, which makes the queue **about 2 KiB**. The program writing to the terminal
sleeps whenever it is full, so `cat` of a gigabyte through a default pty is about half a million
sleep-wake cycles on each side. FreeBSD clamps a pty's speed at 115200 (`ttydevsw_defparam`), which
allows a queue of **about 22.5 KiB**.

`sable-term-spawn` sets the slave to `B115200` before the shell starts, which is ordinary termios
and needs no deviation. It cuts the cycles tenfold. Whether the clamp itself should rise on AbyssBSD
is §12 question 7: it would be a recorded deviation, observable only through timing and
`TIOCOUTQ`, and it is worth proposing only if `term.pty-ratio` shows the pty, not the terminal, to
be the limit.

## 10. Testing and performance

### 10.1 Testing

- **Host, `lib/termcore`**: unit tests per sequence; **screen tests** that feed a byte stream and
  compare the resulting screen and history (text, attributes, cursor, modes) with an expected dump;
  reflow tests at many widths that check text survives a round trip; and selection and search
  over wrapped lines.
- **Expected screens** come from xterm on FreeBSD 15.1 (its behavior is the reference, §6.2), recorded
  once per test on `abyss-build` and checked in. Where xterm and this design intentionally differ
  (`?2027`, cluster widths, policies in §6.3), the test says so.
- **Fuzzing**: the parser, the row-language decoder, the key encoder, and the spawn channel's
  decoder. Every input must leave the model valid (an invariant checker runs after each chunk), never
  panic, and stay within the memory bound of §3.7. A differential fuzzer feeds the same bytes in
  random chunkings and checks the screen is the same, since reads split sequences anywhere.
- **Widths**: `lib/unicode`'s table against `wcwidth(3)` on FreeBSD 15.1 for every code point (§5.2).
- **vttest** from packages, driven by scripted input in the headless harness, with its screens
  compared against stored ones. Its license is to be confirmed as permissive before it is used.
- **Headless**: `sable-term` against the compositor's headless backend on lavapipe (DESKTOP.md
  §10.1), comparing pixels for rendering tests (box drawing at several sizes, wide and emoji clusters,
  fallback, underline styles, cursor shapes, scale 1, 1.25, and 2).
- **Programs**: vi, less, top, `tmux` and `vim` from packages, and `ssh` to `abyss-build`, driven
  through the headless harness in QEMU, screen-compared.

### 10.2 Performance

The terminal is a component under ADR-0014: **`desk.term`**, measured against **foot on sway on
FreeBSD 15.1 with `drm-kmod`, on the same machine**. foot is the baseline because it is the fastest
widely used Wayland terminal in FreeBSD's packages. The gate is the geometric mean no more than 2%
worse. The aim, in refterm's spirit, is to be several times faster on throughput and never the
reason a program waits.

| Benchmark | Measures |
|---|---|
| `term.cat` | Wall time to `cat` 1 GiB of ASCII text to a visible window |
| `term.unicode` | The same for mixed text: CJK, combining marks, emoji sequences |
| `term.sgr` | A stream that changes direct colors every few characters |
| `term.scroll` | Scrolling inside a region (DECSTBM) one line at a time |
| `term.cursor` | Full-screen cursor-addressed redraws, as `top` and editors produce |
| `term.latency` | Key report (evdev timestamp) to the presentation of the frame showing its echo, p99, from a shell in raw mode |
| `term.start` | Launch to first frame, for the first window and for another window |
| `term.resize` | A resize with 1 million lines of scrollback, to the next frame |
| `term.rss` | Resident memory with 1 million lines of typical build output in history |

Workloads are written in-house or taken from vtebench (Apache-2.0) where it has them.

**Diagnostics**, not gated:

- **`term.pty-ratio`**: `term.cat`'s throughput over the throughput of the same `cat` into a program
  that only drains the master. R1 holds when this is above 0.9: the terminal costs less than a
  tenth of what the pty costs.
- **`sable-term` on FreeBSD 15.1**: AbyssBSD runs FreeBSD binaries, and `sable-term` is one, so it
  also runs on sway on stock FreeBSD. Against foot on the same kernel and compositor, this isolates
  the terminal from AbyssBSD's pty and the compositor.

## 11. Relationship to other documents

| Document | Here |
|---|---|
| DESKTOP.md §8.9 (terminal emulator) | This document |
| DESKTOP.md §2.1 (dependencies) | §2.1: xkbcommon, HarfBuzz, base's `libulog`, nothing else foreign |
| DESKTOP.md §4.7.3, §4.7.4 (text, SSL) | §5.3, §5.4: the same `lib/raster` and HarfBuzz path, one more SSL shader |
| DESKTOP.md §7 (configuration) | §8 |
| MULTIUSER.md §9 (terminals) | §7.2, §9 |
| ROADMAP U.4 (`vt`, `lib/vtterm`) | §2.3 |

## 12. Open questions

1. **One process or one per window** (§7.3). Shared atlases and instant windows, or crash isolation.
   Proposed: one process, decided by `term.start` and by how often the fuzzers find anything once
   they have run for a while. foot offers both, and its users mostly run the server.
2. **Primary selection.** `zwp_primary_selection_device_manager_v1` is not in DESKTOP.md §4.2.1.
   Developers expect middle-click paste. Proposed: add it to the compositor's baseline, focus-gated
   like the clipboard. **Settled** by CLIPBOARD.md §2.2, which does so.
3. **A `sable` terminfo entry.** Exact capabilities locally (undercurl, synchronized output, the
   `Tc`/`RGB` flags) at the price of `TERM` failures on hosts without the entry. Proposed: stay on
   `xterm-256color` and answer XTGETTCAP, as several modern terminals do.
4. **Images.** Sixel and the kitty graphics protocol need a second kind of cell content and a third
   atlas. Proposed: after the first release, kitty's protocol first, in the same pass.
5. **Kitty keyboard protocol.** It fixes long-standing key ambiguities and editors increasingly
   use it. Proposed: in the first release if M-D.6's schedule allows, otherwise right after.
6. **Application-scoped bindings.** DESKTOP.md §7.6 binds component actions in `bind`. Whether an
   application's actions take an `app=` scope there, or the application owns a `bind` subtree of its
   own, is a DESKTOP.md decision this document assumes the former of.
7. **The pty speed clamp** (§9.1). Keep FreeBSD's 115200 limit, or raise it as a deviation.
   Proposed: keep it unless `term.pty-ratio` shows the pty to be the limit.
8. **When.** DESKTOP.md puts `sable-term` in M-D.6. Its only hard needs are M-D.2 (a compositor)
   and M-D.3's text stack, and a developer desktop without a terminal is not usable for its own
   development. Proposed: move it to M-D.3 (§13).

## 13. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **T.1** | `lib/termcore` (parser, row language, arena over a `Vec`, screen, reflow, selection, search, key encoding) and `lib/unicode` with `tools/unicodegen` | Nothing (host only) | Screen tests recorded from xterm pass, fuzzers run clean for 24 hours, widths match FreeBSD 15.1's `wcwidth` |
| **T.2** | `sable-term` and `sable-term-spawn`: Wayland, Vulkan, mirrored rings, the cell pass, the glyph cache, xkbcommon input, capability mode | M-D.2, M-D.3 text, P6.7 Capsicum | vi, less, top, and tmux run in QEMU on lavapipe, screen-compared |
| **T.3** | Configuration and theme, selection and clipboard, search, OSC 8, 52, 133, IME, `libulog`, "open here" | M-D.3 | `desk.term` within the gate against foot, on QEMU's `virtio-gpu` first and then the reference machines |
| **T.4** | §12's later items (images, kitty keyboard) as decided | T.3 | Their own tests |
