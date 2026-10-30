# The AbyssBSD Reference Manuals

- Status: **Draft**. A forward design for the system's own documentation. It becomes an ADR when
  the first step (§11, D1) starts, at the latest with P9's documentation item
- Date: 2026-10-27
- Scope: the documentation AbyssBSD installs and ships to the people who use it, program for it,
  and write drivers for it: what it contains, its source format, how it is built, checked,
  installed, searched, and read. The design records in `docs/` (this file, ADRs, ROADMAP) are out
  of scope except where §3.3 says how the two relate
- Inputs: the Amiga ROM Kernel Reference Manuals as a model of tone and structure, FreeBSD's
  manual pages and `mandoc` as the base it must sit beside, [ENGINEERING.md](ENGINEERING.md) §6
  (doc comments), [compat-deviations.md](compat-deviations.md), `tools/syscallgen`, `lib/abi`,
  and `sys/sysctl`

The short version: **a small set of bound volumes, versioned with the kernel, written in one strict
markup, part generated from the code and checked against it, installed under
`/usr/share/doc/abyss`, and read three ways: `man` from the first day, a terminal reader, and later
a desktop browser.** All three use one parser and one search index, written here in Rust with no
external crates.

## 1. What we take from the RKMs

The ROM Kernel Reference Manuals worked because of a few properties, none of which depend on the
1980s:

| RKM property | What it means here |
|---|---|
| **Written by the system's own developers, authoritative** | The manual is the contract for AbyssBSD's own interfaces. Where it and the code disagree, one of them has a bug, and a check (§8) finds it |
| **Narrative volumes and reference volumes, separated** | *Libraries* taught a subsystem from first principles with whole programs; *Includes & Autodocs* gave every function in a fixed shape. We keep the split (§2) |
| **Autodocs: one function, one fixed set of headings** | NAME, SYNOPSIS, FUNCTION, INPUTS, RESULT, EXAMPLE, NOTES, BUGS, SEE ALSO. Readers knew where to look. Our reference entries have fixed headings too (§5) |
| **Complete example programs that compile** | Every example in the manuals builds and runs in CI on both architectures (§8.2) |
| **A volume per audience** | Applications programmers, device programmers, hardware people, and UI designers each had a book. We keep the audiences (§2) |
| **A hardware reference** | The machines we support, described as the kernel sees them (Volume 4) |
| **A user-interface style guide** | Volume 6: the desktop's own guide, from its monorepo (§2) |
| **Versioned with the system** (1.3, 2.0, 3.1) | A bundle names the kernel it describes; readers warn on a mismatch (§7.4) |

What we drop: print layout as the primary form, and copying material another source already owns.
FreeBSD's manual pages remain the reference for the FreeBSD ABI we implement. We link to them and
document only where AbyssBSD adds, differs, or has not caught up.

## 2. The volumes

| Vol. | Title | Audience | RKM counterpart | Content | Arrives |
|---|---|---|---|---|---|
| 0 | **User's Guide** | People installing and running the system | (the Workbench manuals) | Installation, boot options (`abyss.*`), the first-use log and `absent.txt`, upgrading, supported hardware summary, where AbyssBSD differs for an administrator, troubleshooting a boot | P9 (its installation guide and hardware list are P9 deliverables) |
| 1 | **System Programming** | Programmers of userland on AbyssBSD | *Libraries* | Narrative chapters, each with whole programs: processes and threads, memory, files and the VFS, events (kqueue, eventfd, timers), signals, time, IPC (pipes, shared memory, umtx), terminals, devices from userland, credentials, jails and Capsicum. Written as "how this works on AbyssBSD", pointing to FreeBSD's pages for the call-by-call contract | D2 onward, chapter by chapter |
| 2 | **Devices** | Programmers of device-facing userland | *Devices* | One chapter per device class as userland sees it: the node, `open`/`read`/`ioctl`/`mmap`/`kqueue` semantics, every ioctl, an example. Storage, tty and vt, evdev, USB (`ugen`), DRM (M-G), network interfaces (P7), audio (later) | With each class |
| 3 | **Interface Reference** | Everyone; looked up, not read | *Includes & Autodocs* | Generated entries (§5): every system call with its AbyssBSD status, every ioctl, every sysctl, every ABI structure with its layout per architecture, errno values, loader variables and kernel environment, devfs names | D1, generated |
| 4 | **Platforms and Hardware** | Hardware people, porters | *Hardware Reference Manual* | Boot (loader, UEFI services used, handover), ACPI as the kernel consumes it, each architecture's traps and memory model as seen from outside, each reference machine and board, the supported-hardware list | P8 onward |
| 5 | **Kernel and Driver Guide** | People who change the kernel | (none; Amiga drivers were outside the ROM) | The architecture, the driver model and onboarding, the VFS, unsafe policy, performance rules, and generated reference for kernel-internal crates (§5.3). Internal: no stability promise | D2 |
| 6 | **Desktop Style Guide** | Writers of desktop applications | *User Interface Style Guide* | The Swift desktop's interface conventions, Aqua toolkit, menu protocol, and portals, the help protocol (§7.3). Written in the desktop's monorepo with its code ([ADR-0018](adr/0018-desktop-architecture.md)) and included here | M-D |
| A | **Design Record** | The curious | none | The ADRs and design documents from `docs/`, included verbatim as an appendix volume, so an installed system carries its own rationale | D1 |

Each volume has a stable short name used in links and paths: `user`, `sysprog`, `devices`, `ref`,
`hardware`, `kernel`, `style`, `design`.

## 3. Relation to what exists

### 3.1 FreeBSD's manual pages

AbyssBSD runs FreeBSD's userland, so `man 2 open` on AbyssBSD shows FreeBSD's page, from FreeBSD's
own `base.txz`. We do not replace or patch those pages. Instead:

- **Volume 3 entries for FreeBSD interfaces are thin.** An entry for `open` says: the contract is
  `open(2)`; status *implemented*; deviations (from compat-deviations.md, §5.2); notes specific to
  AbyssBSD; tests that cover it. The body of the contract stays in FreeBSD's page.
- **Interfaces only AbyssBSD has** (its boot options, its sysctls under `abyss.*` if any, its
  desktop IPC, its kernel-internal API) are documented in full in our volumes.
- **Our reference entries are also emitted as manual pages** (§6.2), so `man` and `apropos` find
  them alongside FreeBSD's. They live under their own manpath, never in `/usr/share/man`, so a
  FreeBSD base update never collides with them.

FreeBSD's section 9 (kernel interfaces) describes FreeBSD's kernel, not ours. It stays installed
because base ships it; Volume 5 is the kernel reference for AbyssBSD, and its generated pages use
section `9abyss` (§6.2) so the two do not shadow each other. *To verify:* how FreeBSD 15.1's `man`
orders a suffixed section against `9` when both match a name.

### 3.2 rustdoc

`cargo doc` stays a developer convenience. It does not serve here: it renders only HTML, it knows
only Rust items (not syscalls, sysctls, ioctls, or device nodes), it cannot be installed as a
searchable set on a system without a browser, and its layout changes with the toolchain. Volume 5's
kernel reference comes from the same `///` comments rustdoc reads (§5.3), so writing for one
writes for both.

### 3.3 The design records in `docs/`

`docs/` records decisions and their reasons, written while deciding. The manuals describe the system
as built, for people who did not take part. They overlap in subject but not in purpose, so the rule
is:

- A manual chapter does not restate a design document's rationale. It describes behavior and links
  to the Design Record volume for the why.
- When a phase lands, its design document's description of the *result* moves (or is rewritten)
  into the right volume, and the design document keeps the decision. Over time Volume 5 absorbs the
  descriptive halves of ARCHITECTURE.md, DRIVER-MODEL.md, VFS.md, and USB.md.
- `docs/` stays GitHub-flavoured Markdown, outside this build. Volume A converts it with the same
  parser (it is close to the manual markup; §4.4).

## 4. Source format

### 4.1 Requirements

- Authored as plain text in this repository, reviewable as a diff.
- Renders well at **80 columns on a VT** (the TUI reader and `man`), on a desktop, and as HTML.
  Nothing may depend on a graphical renderer.
- Structured enough to generate a table of contents, cross-references, and a search index without
  heuristics.
- Parsed by our own code in `lib/` (`no_std` + `alloc`, so the reader can use it anywhere), small
  enough to fuzz.

### 4.2 Choice

**A strict subset of CommonMark with a few block extensions**, called *manual markup* here, in
`.md` files. Contributors already write Markdown in `docs/`, it reads well raw (so `less` on a
source file is acceptable), and a strict subset is small to parse.

Considered and rejected:

| Format | Why not |
|---|---|
| mdoc only | BSD-native and already readable by `man`, but built for reference pages: poor for book-length narrative, cross-volume links, and tables. We *generate* mdoc from our entries instead (§6.2) |
| AsciiDoc, Sphinx/reStructuredText, DocBook | Rich, but their tools are external (Ruby, Python, Java) and their grammars too large to reimplement well. The FreeBSD Handbook's Hugo/AsciiDoc toolchain is out for the same reason |
| Texinfo / info | GPL toolchain ([ADR-0007](adr/0007-license-and-clean-implementation.md)), and not a BSD convention |
| An AmigaGuide-like format of our own | The hypertext model is right (nodes, links, a back stack); a new syntax is not worth it. The node model is kept (§4.3); the syntax is Markdown's |

### 4.3 The model

A **library** contains **volumes**; a volume contains **chapters** (one file each); a chapter
contains **nodes**. Every heading starts a node, and every node has an identifier, stable across
releases, given explicitly: `## Waiting on events {#kqueue-wait}`. A node's address is
`volume/chapter#node`, for example `sysprog/events#kqueue-wait`. Reference entries are nodes too,
with generated identifiers (`ref/syscall#open`).

### 4.4 The subset and the extensions

Kept from CommonMark: ATX headings, paragraphs, emphasis, inline code, fenced code blocks with a
language tag, ordered and unordered lists, block quotes, links, and pipe tables (the GFM table
extension). Dropped: raw HTML, setext headings, indented code blocks, reference-style link
definitions, hard line breaks with trailing spaces, and images (§4.5). The parser rejects what it
does not support, with a file and line, rather than guessing.

Extensions, each chosen so that the raw text still reads correctly:

| Extension | Syntax | Purpose |
|---|---|---|
| Node identifier | `## Title {#id}` | Stable anchors (§4.3) |
| Library link | `[text](doc:sysprog/events#kqueue-wait)`, `[](doc:ref/syscall#open)` (empty text takes the target's title) | Checked at build time; the readers follow them |
| Manual-page link | `[](man:kqueue.2)` | To any installed page, FreeBSD's or ours |
| Admonition | `> **Note:**`, `> **Warning:**`, `> **Compat:**` as the first words of a block quote | Rendered as boxes; *Compat* marks a FreeBSD difference and is indexed |
| Conditional block | a fence `::: arch aarch64` … `:::` | Architecture-specific text; readers show the running architecture's by default |
| Example inclusion | a fence `::: example sysprog/kqueue-timer` … `:::` | Pulls in a program from `manual/examples/` (§8.2), with its build line |
| Generated insertion | a fence `::: generated ref/syscall-table` … `:::` | A table produced by the build (§5) |
| Figure | a fenced `text` block with a caption line `Figure: …` | Diagrams are text, so they render everywhere |

### 4.5 Figures

Figures are **text diagrams** (box drawing, as `docs/` already uses), because every reader can
show them. A figure may additionally name an SVG for the HTML and desktop renderers, but the text
form is mandatory and is what the build checks. This keeps the TUI reader complete rather than a
degraded view.

## 5. Generated reference

### 5.1 Sources

Volume 3 is generated, not written, from the places the facts already live:

| Entries | Source of truth | How it is read |
|---|---|---|
| System calls: name, number, arguments, AbyssBSD status | `lib/abi/src/syscall/generated.rs` (from `tools/syscallgen`) and the kernel's dispatch table | Host tool parses the generated table; status from the dispatch table (implemented, partial, `ENOSYS`) |
| Deviations | `docs/compat-deviations.md` | Its tables gain a key column naming the interface (`syscall:cap_enter`, `sysctl:…`, `device:…`) so each row attaches to its entries |
| ABI structures and constants, with offsets and sizes per architecture | `lib/abi` | The host build prints them from `lib/abi` itself (no source parsing); `abi-check` already proves them against FreeBSD's headers |
| sysctl nodes: name, type, format, description, flags | The running kernel's tree (`sys/sysctl` already keeps descriptions) | A boot test dumps `sysctl -aNdt` equivalent from each architecture into the build, as `devinfo` trees are recorded today |
| ioctls | The dispatcher tables of each device class | A registry next to the dispatch code that names the request, its structure, and its direction |
| Boot options and loader variables | Loader and kernel option parsers | Each option is declared once, with a description, and parsed from that declaration |
| Kernel-internal API | `///` and `//!` comments in `kernel/`, `sys/`, `drivers/`, `lib/` | §5.3 |
| Test coverage per interface | kyua results and `tests/kyua/known-failures.txt` | Which FreeBSD tests exercise the interface and whether they pass |

Generated text is never committed. A writer adds prose to a generated entry with a **supplement**
file (`manual/ref/syscall/open.md`) that the build merges under fixed headings; a supplement for an
interface that no longer exists is a build error.

### 5.2 The entry shape (our autodoc)

Every Volume 3 entry has the same headings, in this order, omitting the empty ones:

```
NAME          open — open or create a file
CONTRACT      open(2), FreeBSD 15.x                  (or "this entry", for AbyssBSD-only interfaces)
SYNOPSIS      generated: C prototype, syscall number, per-arch ABI notes
STATUS        implemented | partial | not implemented (ENOSYS) | AbyssBSD only — since <phase/release>
DEVIATIONS    from compat-deviations.md, with the "until" column
NOTES         supplement prose
EXAMPLE       an included example, if any
TESTS         FreeBSD tests that cover it, and their result here
SEE ALSO      library and manual-page links
```

The fixed shape is the point, as it was with autodocs: a reader who has seen one entry can scan
any other.

### 5.3 Kernel-internal reference (Volume 5)

Extracted from Rust doc comments by a small scanner in the host tool. It does not parse Rust in
general: it tokenizes enough to find `pub` items, their signatures up to the body, and the doc
comments attached to them. The comment conventions are the ones rustdoc already recognizes, made
mandatory for public items in kernel crates:

- The first paragraph is the NAME line.
- `# Errors`, `# Safety`, `# Panics`, `# Locking` (new: which locks the caller holds or may not
  hold), `# Context` (new: which contexts may call it: interrupt, sleepable, fence-signalling as in
  [GRAPHICS.md](GRAPHICS.md)), `# Example`, `# See also`.
- The module's `//!` comment opens its section, including the `Behavior reference:` line that
  ENGINEERING.md §6 already asks for.

Volume 5's reference is marked **internal**: no stability promise, regenerated each build.

## 6. Build and outputs

### 6.1 The tool

A host tool, `tools/manual` (`std`, no crates), driven by `cargo xtask manual …`, over a shared
library crate, `lib/manual` (`no_std` + `alloc`): the parser, the node tree, link resolution, the
search index format (writer and reader), and a renderer-neutral layout of a node (blocks, spans,
links) that each front end draws. The TUI reader and the desktop browser link `lib/manual`; they do
not parse markup themselves.

```
 manual/*.md ──┐
 supplements ──┤                         ┌─▶ bundle  (/usr/share/doc/abyss: source + index)
 generated ────┼─▶ lib/manual: parse ─▶ node tree ─▶ link check ─┼─▶ mdoc   (manpath for man/apropos)
 (syscalls,    │                         └─▶ HTML   (static, offline and hosted)
  sysctls, …)  ┘                         └─▶ text   (plain 80-column, for diffs and mail)
```

Commands: `cargo xtask manual build`, `cargo xtask manual check` (part of `cargo xtask check`),
`cargo xtask manual read` (the TUI reader on the host, for writers).

### 6.2 Outputs

- **The bundle** (§7.1): what the readers use.
- **mdoc pages** for every Volume 3 and Volume 5 entry, and one page per Volume 0–2 and 4 chapter,
  under `/usr/share/doc/abyss/man`, with a `man.d` file adding it to the manpath. `makewhatis` from
  FreeBSD's base indexes them, so `apropos` works with no software of ours. Sections: our entries for
  FreeBSD interfaces use the suffixed sections `2abyss`, `4abyss`, `9abyss`, so `man 2 open` still
  shows FreeBSD's page first and `man 2abyss open` ours. *To verify* on the build server: the
  `man.d` mechanism and the suffix ordering in FreeBSD 15.1's `man(1)`.
- **HTML**: static pages with no scripts required, plus an optional small search script that reads
  the same index. Installed under `/usr/share/doc/abyss/html` (a separate, optional package; §7.2)
  and publishable when there is public hosting (ROADMAP open decision 5).
- **Plain text**: one file per chapter, for review and for the manual's own regression tests.

## 7. Installation, search, and readers

### 7.1 The bundle

```
/usr/share/doc/abyss/
  library.cat          catalog: format version, kernel version/commit, architectures, volumes, chapters, nodes
  library.idx          search index (§7.2)
  <volume>/<chapter>.md   normalized source (generated parts already expanded)
  examples/            the example programs, with a Makefile per example
  man/                 generated mdoc pages
  html/                optional
```

Readers parse the normalized source at display time. Shipping source rather than a pre-rendered
form keeps the bundle small and lets each front end lay text out for its own width. The catalog
gives byte offsets of each node, so opening a node does not parse its whole chapter.

### 7.2 Search

One index serves every reader:

- **Symbols** (exact names of syscalls, sysctls, ioctls, structures, kernel items, boot options)
  map straight to their nodes. Typing `kevent` or `kern.ipc.maxpipekva` goes to the entry.
- **Full text**: an inverted index over lowercased terms, with prefix matching and field weights
  (title > symbol > heading > *Compat* note > body). No stemming at first; it can come later
  without changing the file format's structure.
- Results carry a node address and a snippet range.
- The file is a versioned, little-endian, offset-based layout read in place (`mmap` on AbyssBSD,
  a plain read on the host), with no pointers and no allocation per lookup. `lib/manual` owns the
  format and fuzzes its reader, since an installed index is input like any other.

Packaging: the bundle is a distribution set (`abyss-doc.txz` beside `base.txz`), and `html/` a
second one, so small installs can skip it. When pkgbase-style packages exist, the same split holds.

### 7.3 Readers

**`man` and `apropos`** work from D2 with nothing new (§6.2).

**The terminal reader** (`abyssdoc`, name to decide; §12): a full-screen reader for the VT and for
`ssh`, in Rust over `lib/manual`, using termios and ANSI sequences only.

- Starts at the library contents, or at an address: `abyssdoc sysprog/events`, `abyssdoc kevent`
  (a symbol), `abyssdoc -k "pipe buffer"` (search, like `apropos`).
- Navigation in the AmigaGuide manner: links are selectable (Tab, Enter), *Retrace* (Backspace)
  walks a back stack, *Browse* (`[` `]`) moves to the previous and next node in reading order,
  *Contents* (`c`) and *Index* (`i`).
- `/` searches within the node, `?` searches the library, `g` goes to an address or symbol.
- Architecture blocks show the running architecture, with a key to show all.
- `x` writes the example under the cursor to the current directory with its build line, the modern
  form of copying a listing from the RKM disk.
- Bookmarks and history in `~/.abyssdoc` (read and written defensively; a damaged file is ignored).
- Works on an 80×25 VT with only the base VGA colours, and degrades to `--plain` (a pager-friendly
  text stream) when stdout is not a terminal.

It needs a way to build Rust userland programs for both architectures. `x86_64-unknown-freebsd` is
a built-in target with `std`; `aarch64-unknown-freebsd` has no prebuilt `std` and
[ADR-0003](adr/0003-stable-rust-builtin-targets.md) forbids `-Zbuild-std`. So until M-U decides the
userland Rust target, the reader is built natively on AbyssBSD with FreeBSD's `rust` package (P9
self-hosting), or on FreeBSD/arm64. On the host it runs from day one via `cargo xtask manual read`.

**The desktop browser** (M-D): an Aqua application, written in the desktop's monorepo, over the
same bundle and index (through `lib/manual`'s text output or a C interface to it), with the same
navigation and keys, proportional text, and SVG figures where they exist. Two integrations make
it part of the desktop rather than an app beside it:

- **Help protocol.** Applications ask for help by address (`doc:style/keyboard#focus`) through the
  desktop's IPC; the browser opens the node. F1 or Help in any conforming application does
  this, and Volume 6 makes it a rule, as the Amiga style guide made the Help key one.
- **Launcher search provider.** The launcher queries the index directly, so typing a symbol or
  phrase there offers manual nodes beside applications.

### 7.4 Versions

The catalog records the kernel version and commit it was built for. The readers compare it with
`kern.version` (or the AbyssBSD equivalent) and say so when they differ, since the generated
reference describes one build. Node identifiers are stable across versions; renaming one leaves an
alias in the catalog so old links and bookmarks still resolve.

## 8. Correctness

The manual is held to the code by checks, as the code is held to FreeBSD by tests.

### 8.1 `cargo xtask manual check`

Runs in `cargo xtask check`, so every change answers to it:

- The markup parses; every `doc:` and `man:` link resolves (`man:` against FreeBSD 15.1's page list,
  recorded on the build server like other corpora).
- Every node identifier is unique and every renamed one has an alias.
- **Coverage**: every implemented system call, every sysctl node in the recorded trees, every ioctl
  in a registry, every boot option, and every public item in a kernel crate has an entry, and every
  kernel-crate public item has the headings §5.3 requires. Like `abyss-known.txt` for kyua, gaps are
  listed in a known-gaps file so the check can be turned on before the backlog is cleared, and the
  file can only shrink.
- Every compat-deviations row has a key that names an existing interface.
- Every supplement belongs to an existing entry.

### 8.2 Examples

`manual/examples/<volume>/<name>/` holds complete programs (C against FreeBSD's headers, and Rust
once userland Rust exists), each with a Makefile and an expected-output file. A test tier builds
them on the build server and runs them in QEMU on both architectures, as the kyua cases run, and
compares output. An example that stops building or changes output fails CI, which is the guarantee
the RKM disks never had.

### 8.3 Reviews

Volume 1, 2, and 4 chapters are reviewed like code. A phase's exit criteria include "the manual
describes it": the generated reference is automatic, but the narrative chapter for a new subsystem
is a deliverable of the phase that builds it (§11).

## 9. Licensing

- The manuals are **BSD-2-Clause**, as the code is, with the SPDX line in each source file's front
  matter. Generated text inherits the license of what it is generated from (all BSD-2-Clause, or
  MIT for files derived from MIT sources).
- FreeBSD's manual pages are linked, not copied. Quoting a sentence or a table with attribution is
  fine (they are BSD-licensed); restating a whole page is not, because it would drift.
- Volume 4 describes **what our drivers do**, citing the clean-room specifications in
  `~/Projects/OS/chinese-wall/specs/` by name. It does not republish them; whether a spec may be
  published is decided per spec, not assumed.
- No GPL source is consulted for the manuals any more than for the code
  ([ADR-0007](adr/0007-license-and-clean-implementation.md)).

## 10. Repository layout

```
manual/
  library.cat.src         the catalog source: volumes, chapter order, node aliases
  user/  sysprog/  devices/  hardware/  kernel/  style/     chapters, one .md per chapter
  ref/<kind>/<name>.md    supplements for generated entries
  examples/<volume>/<name>/
  known-gaps.txt
lib/manual/               parser, node tree, index format, layout (no_std + alloc)
tools/manual/             generators, renderers (bundle, mdoc, HTML, text), checks
user/abyssdoc/            the terminal reader (location to match M-U's userland layout)
```

The catalog source is a small line-oriented format parsed by `lib/manual`, since there is no TOML
parser in the tree and one is not worth writing for this.

## 11. Steps

| Step | Contents | Depends on | Exit |
|---|---|---|---|
| **D0** | This design; an ADR when D1 starts | — | Reviewed |
| **D1** | `lib/manual` (parser, links, index) with fuzzing; `tools/manual` with the bundle, text, and mdoc outputs; Volume 3 generated for system calls, ABI structures, and sysctls; compat-deviations keyed; `manual check` in `cargo xtask check` with a known-gaps file; Volume A | Nothing beyond P6 | `cargo xtask manual build` produces the bundle; the check passes |
| **D2** | Installed into the image (`abyss-doc.txz`, the manpath file); `man 2abyss open` and `apropos` work on AbyssBSD in QEMU; Volume 0 (installation guide, supported hardware) and Volume 5's first chapters; the host-side TUI reader | D1; P9's documentation item | A shell case reads an entry with `man` on both architectures |
| **D3** | The terminal reader on AbyssBSD; the examples tier; Volume 1 chapters for the P2–P6 subsystems; HTML output | D2; Rust userland builds (P9 self-hosting or M-U) | `abyssdoc` runs on the VT on both architectures; every example builds and passes |
| **D4** | Volume 2 chapters with each device class (evdev, DRM in M-G); Volume 4 with P8 and P10 | The phases that build them | Per phase |
| **D5** | The desktop browser, help protocol, launcher provider, Volume 6 (all in the desktop's monorepo) | M-D.2 | F1 in an Aqua application opens its help node |

D1 can start alongside P6, since it reads the code and changes none of it. Narrative chapters then
follow the subsystems instead of trailing them by years.

## 12. Open questions

1. **Names.** The set ("AbyssBSD Reference Manuals"?), the reader (`abyssdoc`, `guide`, …), and the
   suffixed manual sections. Avoid Commodore's and Amiga's names; the homage is in the structure.
2. **Manual sections.** Suffixed sections (`2abyss`) versus a separate manpath searched first only
   on request. Depends on what FreeBSD 15.1's `man(1)` does with both (§3.1, §6.2).
3. **Generated reference when the kernel is not running the same build** (a reader on another
   machine, or HTML on the web): show the bundle's version prominently, or ship one bundle per
   release with a version switcher in HTML.
4. **Localization.** Out of scope for now; node identifiers are language-neutral so translations
   could be added per chapter later.
5. **Print.** A PDF per volume would need a typesetter we would have to write. Plain text and HTML
   print adequately; a PDF is deferred until someone needs bound copies.
6. **Where userland Rust lives** (the reader's crate path and its target), settled by M-U.
