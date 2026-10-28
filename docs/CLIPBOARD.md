# Copy and paste

- Status: **Draft**. A forward design for copy, paste, the primary selection, drag and drop, and
  clipboard history on the desktop of [DESKTOP.md](DESKTOP.md). Decisions here join the M-D ADR.
- Date: 2026-10-27
- Scope: the selection model, the compositor's transfer and retention of clipboard data, the
  security rules, the shared keyboard and pointer conventions, clipboard history, typing the
  clipboard into programs that can't paste, screenshots and colors to the clipboard, X11 and
  scripting interoperability, configuration, testing, and performance. How each application uses
  the clipboard is that application's document (TERMINAL.md §6.3 and §6.4, FILEMANAGER.md §4.2).
- Inputs: [DESKTOP.md](DESKTOP.md) (§1.2, §4.2.1, §4.10, §5.2.5, §5.6, §6.5, §6.6, §7, §8.3),
  [TERMINAL.md](TERMINAL.md) (§6.3, §6.4, §12 question 2), [FILEMANAGER.md](FILEMANAGER.md) (§4.2),
  [ENGINEERING.md](ENGINEERING.md), [PERFORMANCE.md](PERFORMANCE.md), ADR-0014

The short version: **the clipboard keeps working after the program you copied from exits, never
shows up on stream, and can't be read or replaced from the background without a grant you can see
in your config.** Transfers are Wayland's: the copying program offers types, and bytes flow
through a pipe straight to the pasting program when it asks. The compositor adds one thing to that
model. It reads a bounded copy of the common types the moment something is copied, without
interpreting them, so the copy survives its source. Reading the clipboard needs keyboard focus, and
setting it needs a recent key press or click. Clipboard history is an opt-out Facet that keeps
entries in memory only, skips entries marked secret, and is left out of every capture. For games,
remote consoles, and fields that refuse a paste, one action types the clipboard instead. The same
keys, pointer gestures, and rules apply in every native application and every Facet, because they
are implemented once.

## 1. Who it is for

DESKTOP.md's audience is gamers, streamers, and developers. Each has needs that a plain
Wayland clipboard does not meet:

| User | Need | Here |
|---|---|---|
| **Everyone** | Copy in a program, close it, paste: it works | §3.3, retention |
| **Developers** | Middle-click paste from the primary selection; copy and paste in scripts (`wl-copy`/`wl-paste` style); OSC 52 from remote shells; the same Copy key in terminals and everywhere else; large pastes that don't stall | §2.2, §6.2, §8.2, TERMINAL.md §6.3, §4 |
| **Streamers** | Nothing on the clipboard leaks on stream: no history popup, no paste preview, no notification; password-manager copies never stored; screenshots and colors straight to the clipboard | §5.3, §5.4, §7 |
| **Gamers** | Paste into game chat, launchers, and Wine programs; type a clipboard into a game or a VM console that has no paste; no clipboard popups in Game Mode; X11 games can't snoop the clipboard | §6.3, §8.1, §5.4 |

## 2. Principles

### 2.1 Rules

| # | Principle | Consequence |
|---|---|---|
| C1 | **Wayland's model is the model** | The source offers media types, the receiver picks one, bytes go through a pipe. The compositor brokers descriptors and never converts formats. Native code and foreign clients see the same protocols (§3) |
| C2 | **A copy outlives its source** | The compositor retains a bounded copy of each selection's common types (§3.3). The user never has to know that, on Wayland, the data lives in the program that copied it |
| C3 | **The compositor holds bytes, never meaning** | Retained data is opaque bytes in sealed memfds. Nothing in `sable-comp` parses clipboard contents: previews, history, and conversions happen in sandboxed components (DESKTOP.md §2.1 rule 5) |
| C4 | **Focus is the authority** | Reading needs keyboard focus. Writing needs an input serial from a real key press or button press the writer received, and recent. Anything else is a grant in config (§5.1) |
| C5 | **Secrets are marked and respected** | Offers carrying the password-manager hint are never retained past their source, never in history, and never previewed (§5.2) |
| C6 | **Off-screen by default** | Clipboard UI is out of every capture sink. Showing it on stream is a choice (§5.3) |
| C7 | **One implementation of the conventions** | Keys, middle-click, selection behavior, and paste rules live in one place each: the compositor's `Input` node for Facets and `lib/scene-render` for applications (§4) |

### 2.2 Selections

| Selection | Set by | Pasted by | Protocol |
|---|---|---|---|
| **Clipboard** | Copy and Cut actions | Paste actions | `wl_data_device_manager` v3 |
| **Primary** | Selecting text (with mouse or keyboard) | Middle click | `zwp_primary_selection_device_manager_v1` |
| **Drag and drop** | Starting a drag | Dropping | `wl_data_device_manager` v3, with copy, move, and ask actions |

The primary selection is in the **baseline**, focus-gated like the clipboard (§5.1), which settles
TERMINAL.md §12 question 2. Developers rely on it, and it is a habit of every Unix desktop since
X11. Anyone who finds middle-click paste a hazard turns it off (`clipboard.primary #false`, §9),
which stops the compositor from serving it at all, so a stray middle click in a game or a browser
pastes nothing.

### 2.3 Non-goals

- Format conversion in the compositor (C1, C3). An application that offers only `text/html` is
  pasted as `text/html`. Converting it is the receiving toolkit's job (§4.3).
- Network clipboard sharing between machines. Remote shells use OSC 52 (TERMINAL.md §6.3).
- Synchronizing the clipboard with jailed applications behind the user's back. A jail sees the
  clipboard through its own Wayland connection, under the same focus rules (§5.1).
- A clipboard in the `vt(4)` console beyond what FreeBSD's `moused` and `vt` already provide.

## 3. Transfers

### 3.1 The live path

The Wayland model, unchanged:

1. The source creates a data source, offers its types (`wl_data_source.offer`), and sets the
   selection with the serial of the input event that caused the copy.
2. The compositor validates the serial (§5.1), replaces the previous selection (the old source gets
   `cancelled`), and sends the new offer to the client with keyboard focus. A client that gains focus
   later receives the current offer just before `wl_keyboard.enter`.
3. To paste, the receiver calls `wl_data_offer.receive(type, fd)` with the write end of a pipe it
   made. The compositor passes that descriptor to the source in `wl_data_source.send`. The source
   writes and closes, and the receiver reads to EOF.

The compositor touches only descriptors on this path. A paste of a gigabyte costs the pipe's
throughput and nothing in the compositor, and neither side waits on the compositor's frame loop.

### 3.2 Types

The compositor carries types as opaque strings, at most 32 types per offer and 255 bytes each (a
client that offers more gets a protocol error, DESKTOP.md §4.2's resource ledgers). Native
components offer and prefer, in order:

| Content | Offered as |
|---|---|
| Text | `text/plain;charset=utf-8`, then `text/plain`, `UTF8_STRING`, `TEXT`, `STRING` (the X11 names that toolkits from packages still look for) |
| Rich text | `text/html` in addition to the text types, from applications that have formatting |
| Files | `text/uri-list` (RFC 2483, percent-encoded `file://` URIs), plus the text types with paths one per line |
| Images | `image/png` |
| Secrets | The text types plus `x-kde-passwordManagerHint` with the data `secret`, the convention KeePassXC and KDE use (§5.2) |

### 3.3 Retention

When a selection is set, the compositor's `clip` module (in the protocol thread, all non-blocking)
**reads a copy of it at once**:

- It picks at most one type from each family in `clipboard.keep.types` (§9): by default the
  preferred text type, `text/uri-list`, and `image/png`. Other types (an office suite's internal
  formats, which can be large and slow to make) are left to the live path.
- It makes a pipe per chosen type, sends `send` to the source as if a client had asked, and reads
  with `EVFILT_READ` into a memfd until EOF, up to `clipboard.keep.limit` (default 32 MiB per
  selection, 64 MiB across both selections). A type that exceeds the limit is dropped from the
  retained set, not truncated.
- When the reads finish, the memfds are sealed (`F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK`).

When the source **goes away** (its client disconnects, or it destroys the source without a new
selection), the compositor becomes the source: it offers the retained types, and answers each
`send` by writing from the sealed memfd. The user pastes as if nothing happened. If the reads had
not finished, the retained set is the types that had. A selection with no retained types (all over
the limit, or marked secret) is gone with its source, as on any Wayland desktop. The history Facet
shows that state (§5.4).

The source sees one extra `send` per kept type for each copy. This is what `wl-clipboard`-based
clipboard managers already do through data-control on every desktop that has them. The
difference is that here it is built in, needs no grant, and never leaves the compositor.

### 3.4 Large and slow transfers

- **The compositor never blocks on a pipe.** Its retention reads are kqueue-driven and have a time
  budget (`clipboard.keep.timeout`, default 2 s). A source that stalls is left to the live path.
- **Receivers read concurrently.** The compositor's retained-data writer serves each receiving
  pipe from the memfd independently, so a slow receiver does not hold up another.
- **Toolkit receives are asynchronous** (§4.2): `lib/scene-render` and Facets read pastes on a
  `sable-rt` task with a progress indicator after 250 ms, so a paste of a large image never freezes a
  window.

## 4. Conventions

### 4.1 Keys and gestures

Implemented once in the compositor's `Input` node (for Facets) and once in `lib/scene-render` (for
applications), and bound as actions (DESKTOP.md §7.6):

| Gesture | Action | Default binding |
|---|---|---|
| Copy | `clip.copy` | Ctrl+C, Ctrl+Insert |
| Cut | `clip.cut` | Ctrl+X, Shift+Delete |
| Paste | `clip.paste` | Ctrl+V, Shift+Insert |
| Paste as plain text | `clip.paste-plain` | Ctrl+Shift+Alt+V |
| Select to primary | — | Selecting with mouse or Shift+movement |
| Paste primary | `clip.paste-primary` | Middle click |
| Clipboard history | `clip.history` | Super+V (§5.4) |
| Type the clipboard | `clip.type-out` | Super+Shift+V (§6.3) |

Terminals keep Ctrl+Shift+C and Ctrl+Shift+V (TERMINAL.md §8), because Ctrl+C and Ctrl+V belong to
the programs running in them.

### 4.2 The universal copy and paste keys

A developer moving between a terminal and an editor all day presses the wrong chord half the time.
**`clipboard.universal`** (off by default) makes **Super+C** and **Super+V** mean copy and paste
everywhere. For each window they send the chord that window's application uses, from its `app`
rule:

```kdl
clipboard { universal #true }
app "sable-term"            { copy-key "Ctrl+Shift+C" ; paste-key "Ctrl+Shift+V" }
app "org.winehq.Wine"       { copy-key "Ctrl+C"       ; paste-key "Ctrl+V" }
// applications without a rule get Ctrl+C and Ctrl+V
```

The compositor delivers the chord as ordinary key events to the focused client. Native
applications skip the round trip: they export `clip.copy` and `clip.paste`, and the compositor
invokes the action directly.

### 4.3 Paste rules in native text fields

- **Plain text first.** A field pastes `text/plain;charset=utf-8` when it's offered. A rich-text
  field takes `text/html` when offered and its schema allows formatting. Paste as plain text always
  takes text.
- **Line endings** are normalized to LF. A single-line field replaces each run of line breaks with
  one space, and shows that it did, so a pasted command never arrives as several entries.
- **Invalid UTF-8** becomes U+FFFD, one per maximal invalid subsequence, as in TERMINAL.md §4.2.
- **Control characters** other than tab and line breaks are removed, and the field says so.
- **Size.** A field with a schema bound (`range`, `max-len`) rejects a paste over the bound with a
  message, instead of truncating silently.
- **Secret fields** (passwords) accept a paste, and never put their contents on the clipboard or
  the primary selection: Copy and Cut are disabled in them, and selecting does not set primary.

### 4.4 Primary selection behavior

- Selecting sets primary as the selection grows, with **one** offer per selection gesture: the
  compositor sees a new primary on button release or the end of a Shift-movement, not on every
  intermediate change.
- Primary is not retained (§3.3) unless `clipboard.keep.primary` is on. It is short-lived by
  nature, and retaining every selection would read every selection's text.
- `clipboard.sync` (default `"none"`) can copy primary into the clipboard (`"primary-to-clipboard"`)
  or keep them identical (`"both"`), for those used to that behavior elsewhere.

## 5. Security and privacy

### 5.1 Who may read and write

| Client | Read | Write |
|---|---|---|
| Baseline Wayland client (DESKTOP.md §6.6) | Clipboard and primary, **only while it has keyboard focus** | Only with a serial from a key or button **press** it received, no older than `clipboard.serial-age` (default 10 s), and only while it still has keyboard focus or pointer focus from that press |
| Facet | Never directly. A Facet's `Input` field gets pasted text as its value, after the user pastes | Through its `Input` fields' Copy and Cut, which the compositor performs |
| Client or Facet granted `data-control` (`ext_data_control_v1`, or Conduit `clip.Monitor`) | Any time | Any time |
| `sablectl clip` over the control socket (DESKTOP.md §6.5) | Any time | Any time |

The write rule closes a well-known attack: a program in the background swapping a just-copied
cryptocurrency address or command for its own. Without focus and a fresh serial it cannot set the
selection, and with `data-control` it holds a grant that is visible in `grants`, with the file and
line that gave it (DESKTOP.md §7.7).

**Replacement notice.** When a `data-control` holder replaces a selection that a focused
application set less than two seconds earlier, the OSD shows which component did it
(`clipboard.notify-replace`, on by default). Clipboard managers that do this legitimately (to
normalize text, say) can be exempted per grant.

### 5.2 Secrets

An offer that includes `x-kde-passwordManagerHint` with `secret`:

- is **not retained** (§3.3): when the password manager exits or clears it, it is gone;
- is **not in history** (§5.4) and not sent to `data-control` holders unless their grant says
  `secrets=#true`;
- is **never previewed** anywhere, including the OSD;
- may be cleared after `clipboard.secret-timeout` (off by default; password managers have their own).

Applications that don't set the hint can be listed in `clipboard.secret-apps`, which treats every
copy from them as secret.

### 5.3 Streams and screenshots

Following DESKTOP.md §5.6.4, every clipboard surface is out of every capture sink by default:
the history Facet, the paste-progress indicator, the replacement notice, and the type-out
indicator. `clipboard.capture "show"` puts them on stream deliberately. A streamer can still show a
paste happening, but only by choosing to.

### 5.4 History

**`sable-clip`** is a Facet with the `data-control` grant (the one built-in holder; it ships
granted, and removing the grant turns history off). It keeps the last `clipboard.history.entries`
selections (default 50):

- **In memory only** by default. `clipboard.history.store "disk"` keeps them in
  `$XDG_STATE_HOME/sable/clip/` (mode 0600) across sessions; the settings GUI says plainly that
  this writes everything copied to disk. `"off"` keeps no history.
- **Without secrets** (§5.2), and without copies from `clipboard.history.exclude-apps`.
- **Pinned entries** stay until unpinned. Pinning writes nothing to config: pins are history state.
- **Previews** (text excerpts, image thumbnails) are made by `sable-clip` in capability mode. It is
  the only process that interprets clipboard contents (C3), and it holds only clipboard data.
- **Picking an entry** makes it the selection again, with `sable-clip` as the source. Pasting is
  then the user's next key press, so the history never types into a window on its own.
- **Cleared on lock** if `clipboard.history.clear-on-lock` is on (off by default), and always on
  logout unless stored on disk.
- **Game Mode** (DESKTOP.md §5.2.5) holds the history popup closed and pauses the replacement
  notice, so nothing appears over a fullscreen game. The bindings still work if the user presses
  them.

## 6. Interoperability

### 6.1 X11 and Wine through `sable-xwm`

Xwayland's X selections (`CLIPBOARD` and `PRIMARY`) are bridged by `sable-xwm` (DESKTOP.md §4.10),
which holds a Wayland data device for the X side. X11 has no focus gating: any X client can read or
own a selection at any time. The bridge restores the rules of §5.1 at the boundary:

- **Wayland to X.** The bridge takes ownership of the X selections only while an X window has
  keyboard focus, and drops ownership when focus leaves X. An X client in the background therefore
  finds the X selection empty.
- **X to Wayland.** An X client's new selection is forwarded only if that client owns the focused
  X window and the change follows an X input event the bridge saw. Otherwise it stays inside X.
- **Conversions** between X targets (`UTF8_STRING`, `TARGETS`, `INCR` for large transfers) and media
  types happen in `sable-xwm`, never in the compositor.

Wine programs reach the clipboard through Wine's own X11 or Wayland clipboard support, and get the
same rules either way.

### 6.2 Scripts: `sablectl clip`

```sh
sablectl clip copy < notes.txt                  # text/plain;charset=utf-8 by default
sablectl clip copy --type image/png < shot.png
grep -n TODO src/*.rs | sablectl clip copy
sablectl clip paste > out.txt                   # best text type
sablectl clip paste --type text/uri-list
sablectl clip paste --primary
sablectl clip types                              # list the current offer's types
sablectl clip watch                              # print a line per selection change
```

It speaks to `sabled` through the control socket (DESKTOP.md §6.5), which hands it a
`clip.Monitor` channel, so it works only for the session's user and only as that socket's mode
allows. `wl-copy` and `wl-paste` from packages work too, as compatibility, when the user grants them
`data-control`.

### 6.3 Typing the clipboard

Games' chat boxes, game launchers, VM and remote consoles, BIOS-style setup screens in QEMU, and
web forms that block paste all accept keystrokes when they won't accept a paste. **`clip.type-out`**
types the clipboard's text into the focused window:

- The compositor delivers it as `wl_keyboard` key events to the focused client. Characters not in
  the client's current keymap are typed through a temporary keymap sent with `wl_keyboard.keymap`,
  then the original is restored.
- At `clipboard.type-out.rate` (default 250 characters a second, since some programs drop faster
  input) and at most `clipboard.type-out.max` (default 8 KiB).
- Any key press or click cancels it. An indicator shows progress (off-stream, §5.3).
- Secrets can be typed (that is often the point, for a VM console's password prompt), and the
  indicator shows a count, not the text.

It works the same for native Wayland clients, Xwayland clients through `sable-xwm`, and Wine.

## 7. Screenshots and colors

Both are actions on DESKTOP.md §5.6's capture path, and both put their result on the clipboard:

- **`cast.screenshot`** with `target="clipboard"` (and `region="select"`, `"window"`, or `"output"`)
  renders the region through a one-shot capture sink and offers it as `image/png`, encoded by
  `sable-cast` (libpng), with the compositor as the source. The sink uses the **screen's** mask by
  default, so the screenshot is what the user sees; `mask="stream"` gives what the stream shows.
  `target="both"` also saves a file.
- **`cast.pick-color`** magnifies around the pointer, and a click copies the pixel's color as text,
  in `clipboard.color.format` (`"hex"`, `"rgb"`, or `"oklch"`), which suits rice authors and theme
  tokens (DESKTOP.md §7.2).

## 8. Implementation

### 8.1 Where the code lives

| Part | Crate | Notes |
|---|---|---|
| Wayland data devices, serial checks, retention | `desk/comp` (`clip` module) | Descriptors, pipes, memfds. No parsing |
| `clip.Monitor` | `desk/comp` | The native data-control interface, over Conduit |
| Native conventions | `lib/scene-render`, `desk/comp` (`Input` node) | §4, shared code in `lib/textedit` for selection and paste rules |
| History | `desk/facets/clip` | `sable-clip`, capability mode |
| X11 bridge | `desk/xwm` | §6.1 |
| Scripting | `desk/ctl` | `sablectl clip` |
| Type-out | `desk/comp` with xkbcommon | §6.3 |

`lib/textedit` (`no_std` + `alloc`, `#![forbid(unsafe_code)]`) holds the paste normalization of
§4.3, selection movement, and the line-break rules, so the compositor's `Input` node and
`lib/scene-render`'s text fields behave identically and are tested once.

### 8.2 Memory and limits

The retained data is the compositor's, bounded by `clipboard.keep.limit` across both selections,
and freed when a selection is replaced. A client's offers count against its resource ledger (types
and pending transfers). The history Facet's memory is bounded by its entry count and a total size
(`clipboard.history.limit`, default 256 MiB, oldest unpinned entries first).

## 9. Configuration

The `clipboard` subtree is owned by `sable-comp`, except `clipboard.history`, owned by `sable-clip`
(DESKTOP.md §7.4: one owner per key).

```kdl
clipboard {
    primary #true
    sync "none"                       // "none" | "primary-to-clipboard" | "both"
    universal #false
    keep types="text" "text/uri-list" "image/png" limit=(MiB)32 timeout=(s)2 primary=#false
    serial-age (s)10
    notify-replace #true
    secret-apps "org.example.LegacyVault"
    capture "hide"                    // "hide" | "show"
    type-out rate=250 max=(KiB)8
    color format="hex"

    history {
        entries 50
        store "memory"                // "memory" | "disk" | "off"
        exclude-apps "org.keepassxc.KeePassXC"
        clear-on-lock #false
    }
}

bind {
    key "Super+V"        action="clip.history"
    key "Super+Shift+V"  action="clip.type-out"
    key "Print"          action="cast.screenshot" target="clipboard" region="select"
    key "Super+Shift+C"  action="cast.pick-color"
}
```

The family name `"text"` in `keep types` means "the preferred text type in the offer" (§3.2). Every
key is in the generated settings GUI with its consequence stated, as DESKTOP.md P1 and P5 ask.

## 10. Kernel and platform requirements

| Need | Used for | Status |
|---|---|---|
| Pipes with `O_NONBLOCK`, kqueue `EVFILT_READ`/`EVFILT_WRITE`, `EV_EOF` | Every transfer | Done |
| `memfd_create` with seals | Retention, history | P6.3, done |
| `SCM_RIGHTS` | Passing pipe ends between clients and compositor | P6.5, done |
| Capsicum | `sable-clip` | P6.7, planned |
| Xwayland from packages | §6.1 | M-D.4 |

No new kernel interfaces are needed.

## 11. Testing and performance

### 11.1 Testing

- **Protocol**: scripted Wayland test clients in the headless backend (DESKTOP.md §10.1) for every
  rule of §5.1: reading without focus gets no offer, a stale or pointer-motion serial cannot set
  the selection, focus changes deliver the current offer before `wl_keyboard.enter`.
- **Retention**: a source copies text, an image, and a URI list, then exits; a second client pastes
  all three. Sources that stall, exceed the limit, offer 32 types, or close mid-write leave the
  compositor responsive, and a fuzzer drives random offer and transfer sequences against the
  resource ledgers.
- **Differential**: the same test clients, and `wl-copy`/`wl-paste` from packages, against sway on
  FreeBSD 15.1, comparing protocol events where the protocols specify them. Retention and the
  serial-age rule are listed as intentional differences.
- **`lib/textedit`**: the paste rules of §4.3 over a corpus of hostile text (control characters,
  bidi controls, invalid UTF-8, CRLF, very long lines).
- **X11**: `xclip` from packages in Xwayland, checking both directions of §6.1, including that a
  background X client reads nothing.
- **Type-out**: characters across keymaps (US, German, Japanese input) into a test client and into
  `xev`, compared with the source text.

### 11.2 Performance

`desk.clip` is a small component under ADR-0014, against **sway on FreeBSD 15.1** with
`wl-copy`/`wl-paste` as the test programs:

| Benchmark | Measures |
|---|---|
| `clip.latency` | Copy (set_selection) to a focused client's paste completing, for 100 bytes of text |
| `clip.throughput` | A 256 MiB paste through the live path |
| `clip.retained` | The same paste from retained data after the source exits (sway: not possible; recorded, not gated) |
| `clip.copy-cost` | Compositor CPU time added per copy by retention, for 1 KiB, 1 MiB, and 32 MiB selections (recorded, not gated) |
| `clip.frame` | `desk.frame` while a 256 MiB paste runs: retention and transfers must not cost frames |

## 12. Changes to DESKTOP.md

- §4.2.1 lists `wl_data_device_manager` and `zwp_primary_selection_device_manager_v1` in a new
  "Clipboard and drag and drop" row of the baseline.
- §6.6's clipboard sentence points here.

## 13. Open questions

1. **Retention by default.** Reading the common types of every copy is cheap and fixes Wayland's
   most visible surprise, but it means the compositor briefly holds everything copied. Proposed: on,
   with secrets excluded (§5.2), and `clipboard.keep.limit (MiB)0` to turn it off.
2. **The serial age.** Ten seconds allows a menu-driven copy (click, browse, click Copy) and rejects
   background writes. Some applications set the selection asynchronously after the user's action
   (copying the result of a slow export). Proposed: 10 s, measured against the test corpus of
   applications from packages before M-D.3's exit.
3. **A native secret hint.** `x-kde-passwordManagerHint` is a de facto convention, not a standard.
   Proposed: honor it and nothing else, rather than invent a second spelling (DESKTOP.md P4).
4. **History on by default.** Useful to all three audiences, and a privacy cost for anyone who
   copies something sensitive from an application that doesn't mark it. Proposed: on, in memory
   only, with the settings GUI's first-run page showing the switch.
5. **Type-out reach.** Some games read keyboards through raw input under Wine, or through evdev
   directly, and won't see Wayland key events. Typing through a virtual keyboard from `sable-input`
   (uinput, DESKTOP.md §9) would reach them too, at the cost of typing into whatever has focus at
   the kernel level. Proposed: Wayland events first, uinput later if the game corpus needs it.

## 14. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **C.1** | `wl_data_device_manager`, primary selection, focus gating, serial checks | M-D.2 | The protocol tests of §11.1 pass in the headless backend and in QEMU |
| **C.2** | Retention, `clip.Monitor`, `lib/textedit`, the native conventions, `sablectl clip`, `ext_data_control_v1` | M-D.3 | A copy survives its source; `desk.clip` within the gate against sway |
| **C.3** | `sable-clip` history, type-out, screenshots and color picking to the clipboard, the universal keys | M-D.3, M-D.5 (capture sinks) | History is out of capture in a recorded stream test; type-out passes across keymaps |
| **C.4** | The X11 bridge in `sable-xwm` | M-D.4 | `xclip` tests pass both ways, and a background X client reads nothing |
