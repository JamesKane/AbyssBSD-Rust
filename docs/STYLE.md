# Desktop Style Guide

- Status: **Draft**. A forward design for the desktop's visual and interaction contract, and the
  seed of Volume 6 (*Desktop Style Guide*) of the [AbyssBSD Reference Manuals](MANUALS.md) (§2).
  Decisions here join the M-D ADR.
- Date: 2026-10-27
- Scope: window anatomy, spatial and tiling rules, keyboard and pointer conventions, actions and
  discoverability, dialogs and undo, error and permission presentation, untrusted-data display,
  stream safety, and the semantic theme token and style contract between rices and applications.
- Inputs: [DESKTOP.md](DESKTOP.md) (§1.2, §4.9, §5.6.4, §7, §8), [TOOLKIT.md](TOOLKIT.md),
  [TERMINAL.md](TERMINAL.md) (A3, §6.3), [FILEMANAGER.md](FILEMANAGER.md) (A3, §4.2, §4.3),
  [CLIPBOARD.md](CLIPBOARD.md) (§4, §5), [MANUALS.md](MANUALS.md) (§1, §2, §7.3),
  [docs/desktop-themes/README.md](desktop-themes/README.md)

The short version: **every native application and Facet on the desktop looks, moves, and responds
like part of one instrument, not fifteen programs glued together with dotfiles.** Curated Linux
desktops pick a terminal here, a GTK file manager there, a TUI system monitor, and half a dozen
chromeless web-browser wrappers, then try to hide the seams with generated color files. The seams
still show in every shortcut, scroll curve, file dialog, and 500 MiB RAM footprint. On AbyssBSD,
cohesion comes from two places: every component is built on [TOOLKIT.md](TOOLKIT.md) (`lib/scene`
and `desk/scene-render`), and every component obeys the interaction and visual rules in this
document. Most of these rules are enforced by the toolkit by construction; the rest are checked by
`sable check` and headless snapshot tests.

## 1. Principles

| # | Principle | Consequence |
|---|---|---|
| S1 | **Windows are the compositor's job** | Applications do not implement document tabs, MDI, or tiled split panes of their own content (TERMINAL.md A3, FILEMANAGER.md A3). Opening a second view beside the first is one compositor action (`window.beside`) (§2.2) |
| S2 | **Native or nothing—no web apps** | No native component embeds a webview, and the desktop has no "web app" wrapper mode. A web browser from `pkg` is a jailed client for reading web pages, not an application runtime (DESKTOP.md §1.3) |
| S3 | **Keyboard-complete and pointer-direct** | Every action is reachable from the keyboard without touching the mouse, and every visible control is operable with the pointer without memorizing chords (§4) |
| S4 | **Undo, don't nag** | Reversible actions happen immediately and offer `Ctrl+Z` plus an undo strip. Confirmation dialogs exist only for irreversible destruction, and the user can turn any confirmation off (DESKTOP.md P5) (§5.1) |
| S5 | **Show the mechanism, not an excuse** | Errors name the path, the operation, and the kernel or config reason (`EACCES`, `ENOSPC`, or file:line), never "Something went wrong." Settings show the KDL path they edit (§5.2, §5.3) |
| S6 | **Stream-safe by default** | Any surface that can hold secrets, private history, authentication prompts, or personal notifications is excluded from capture sinks unless the user opts in (DESKTOP.md §5.6.4, CLIPBOARD.md C6) (§6.2) |
| S7 | **Semantic tokens, never hardcoded visuals** | Applications name roles (`@surface`, `@accent`, `@danger`, `style "button.primary"`), never hex colors, font names, or pixel radii. A rice re-skins the entire desktop and every native application in one frame (§7) |

## 2. Window anatomy and spatial model

### 2.1 One frame, server-side

Every native `xdg_toplevel` uses **server-side decorations** (`xdg-decoration`, DESKTOP.md §4.2.1),
drawn by `sable-comp` from the active rice's window style: border width, active and inactive border
colors or shaders, corner radius, and SDF drop shadow. Applications never draw their own outer
window borders or close/maximize buttons, so tiled and floating windows from every process match
to the pixel.

Inside the compositor's frame, a standard application window has at most three horizontal bands:

```
 ┌─────────────────────────────────────────────────────────────────────────┐
 │ [Sidebar] │  Header bar (36 px): Back/Fwd · Path or Title · Search · ⋯ │
 │ (optional,├─────────────────────────────────────────────────────────────┤
 │  split_h) │                                                             │
 │           │  Content area (flex 1.0)                                    │
 │  Places   │  Virtualized list, grid, canvas, or document surface        │
 │  Volumes  │                                                             │
 │           ├─────────────────────────────────────────────────────────────┤
 │           │  Bottom strip (28 px, only when needed): Jobs / Status / Undo│
 └───────────┴─────────────────────────────────────────────────────────────┘
```

- **Header bar** (`header_bar()`, default `36px` to match the default panel height): holds compact
  navigation (Back/Forward, path bar or view switcher), a filter or search field, and at most three
  top-level action buttons. Everything else belongs in the action palette (`Ctrl+Shift+P`) or context
  menu. There is no traditional `File Edit View Help` menu bar taking up a second row.
- **Sidebar** (`sidebar()`, optional, default `200px` inside a `split_h`): used only when the
  application navigates across distinct roots or categories (`sable-files` places and volumes,
  `sable-settings` domains, `sable-monitor` subsystems). Toggled with `Ctrl+B`.
- **Content area**: takes all remaining space (`flex(1.0)`). Full-screen content apps (`sable-term`,
  `sable-view` when viewing an image or video) omit the header bar and sidebar by default so 100% of
  the window is content, showing an overlay bar only on search or hover.
- **Bottom strip** (`status_bar()` / `undo_strip()`, `28px`): appears when there is an active
  background job, a selection summary, or a recent undoable action, or permanently in inspection
  apps (`sable-edit` cursor/encoding/diagnostics, `sable-files` item count and free space).

### 2.2 Why there are no document tabs or split panes

When every application invents its own tab bar and split-pane manager, the user has to remember
whether `Ctrl+Tab`, `Alt+1`, `Ctrl+W`, or `Ctrl+B %` switches the terminal tab, the editor split,
the file manager pane, or the compositor's workspace.

On Sable:

- **One tiling and tabbing manager**: `sable-comp` (§4.9) already provides `tile`, `stack`,
  `columns`, `float`, and `monocle` layouts, with animated transitions and uniform `Super+*`
  bindings.
- **"Open beside" (`window.beside`)**: every document or browsing application (`sable-term`,
  `sable-files`, `sable-edit`, `sable-view`, `abyssdoc`) exports `<app>.window.new` (`Ctrl+N`) and
  `<app>.window.beside` (`Ctrl+Shift+N` or Shift+Enter on an item). `window.beside` opens a new
  window in the same process (sharing GPU and memory caches, TOOLKIT.md §6.2) and hints `sable-comp`
  to tile it immediately next to the caller.
- **When `tabs()` *is* used**: switching between fixed sub-views of a *single* object inside an
  inspector or dialog (for example, `Permissions | Extended Attributes | ACLs` in `sable-files`'s
  Properties sheet). Never for holding multiple documents, directories, or shells.

### 2.3 Dialogs, sheets, and pickers

- **In-window sheets over separate popup windows**: a modal prompt that belongs to one window
  (Rename, New Folder, Conflict resolution, Properties) renders as a centered overlay sheet inside
  that window (`modal` node in TOOLKIT.md §4.1), dimming the window's content behind it (`@overlay`).
  It never steals focus from other windows on the workspace.
- **System-authenticated or cross-app prompts are compositor Facets**: file choosing (`sable-picker`,
  FILEMANAGER.md §7.3), capability grant requests, and authentication/vault prompts are drawn by
  `sable-comp` as privileged Facets. Because the compositor draws them, a sandboxed application
  cannot spoof them, read their contents, or inject clicks into them.

## 3. Discoverability and actions

### 3.1 Every operation is an action

Following DESKTOP.md §7.6, no button or menu item triggers hidden callback-only logic. Every user
operation in a native application or Facet is a named **action** declared in its `#[app]` or
`#[facet]` manifest (`files.new-folder`, `term.search`, `edit.format`). This gives three things at
once:

1. Any key or controller chord in `~/.config/sable/sable.kdl`'s `bind` block can invoke it.
2. `sablectl action <name>` can script it from the command line or a stream deck.
3. The **in-app action palette** (`Ctrl+Shift+P`) lists every action the focused window supports,
   fuzzy-searchable by title or action ID, with its active keybinding shown on the right. A user
   never needs a menu bar to discover what an application can do.

### 3.2 The Help key (`F1`)

As MANUALS.md §7.3 establishes (following the Amiga User Interface Style Guide), pressing **`F1`**
(or the `Help` key) in any native application or Facet opens the AbyssBSD Reference Manuals browser
(`abyssdoc`) directly at the node for whatever has focus:

- Every widget in `lib/scene` may carry a `.help("volume/chapter#node")` attribute, and every
  `#[app]` declares a root manual node (`help = "user/files"`).
- When `F1` is pressed, `desk/app` walks up from the focused node to the nearest `.help(...)`
  attribute and sends that address over Conduit `doc.Help`.
- In `sable-settings` or any `form::<T>()` view, pressing `F1` on a setting opens the generated
  Volume 3 reference entry for that exact KDL configuration path.

## 4. Keyboard, pointer, and controller conventions

### 4.1 Universal keyboard map

These bindings are implemented once in `desk/app` and `lib/scene` and behave identically in every
native application (with the single exception of `sable-term`, where unmodified `Ctrl+<letter>` belongs
to the pty and terminal actions use `Ctrl+Shift+<letter>`, TERMINAL.md §8):

| Chord | Action | Behavior |
|---|---|---|
| `F1` | `app.help` | Open `abyssdoc` at the focused control's or window's manual node (§3.2) |
| `Ctrl+Shift+P` | `app.palette` | Open the in-window action palette (§3.1) |
| `Ctrl+,` | `app.settings` | Open `sable-settings` focused on this application's config subtree |
| `Ctrl+N` | `<app>.window.new` | New window in the same process |
| `Ctrl+Shift+N` | `<app>.window.beside` | New window tiled beside the current one (unless overridden, e.g. `files.new-folder` in `sable-files`, where `Shift+Enter` opens beside) |
| `Ctrl+W` | `app.close-window` | Close the focused window |
| `Ctrl+Q` | `app.quit` | Close all windows of this application |
| `Ctrl+F` / `/` | `<app>.search` | Focus the search or filter bar |
| `Ctrl+B` | `app.toggle-sidebar` | Show or hide the window's sidebar |
| `Ctrl+Z` | `<app>.undo` | Undo the last reversible operation in the focused text field or job engine |
| `Ctrl+Shift+Z` | `<app>.redo` | Redo |
| `Ctrl+C`, `Ctrl+X`, `Ctrl+V` | `clip.copy`, `clip.cut`, `clip.paste` | Clipboard copy, cut, paste (CLIPBOARD.md §4.1) |
| `Ctrl+Shift+Alt+V` | `clip.paste-plain` | Paste as plain text |
| `Tab` / `Shift+Tab` | — | Move keyboard focus to the next / previous interactive control |
| `Arrow keys`, `Home`, `End`, `PageUp`, `PageDown` | — | Navigate lists, grids, trees, and text |
| `Space` | — | Toggle focused switch/checkbox, or toggle selection in a multi-select list |
| `Enter` | — | Activate focused button or default dialog action; open selected item in lists |
| `Menu` / `Shift+F10` | — | Open the context menu for the focused item |
| `Escape` | — | Close top popover/menu/modal → clear active search/filter → clear selection (in that order) |

### 4.2 Focus rings

- **Keyboard navigation shows the focus ring; pointer clicks do not.** `lib/scene` tracks whether
  the last input event was a key press or a pointer press. Tabbing or arrowing through controls
  draws a high-contrast `@accent` focus ring (`theme.style.focus-ring`). Clicking a button with the
  mouse activates it without leaving a persistent focus box around it.
- **No focus traps.** `Tab` cycles cleanly through the active layer (the modal sheet if one is open,
  otherwise the window). Multi-line `text_area` editors that use `Tab` for indentation release focus on
  `Escape` followed by `Tab`.

### 4.3 Pointer and scroll behavior

- **Middle-click** in any text input pastes the primary selection (when `clipboard.primary` is
  enabled, CLIPBOARD.md §2.2). Middle-click on a folder or link opens it in a window beside
  (`window.beside`).
- **Scroll physics are unified.** Wheel steps, high-resolution touchpad two-finger scrolls
  (`lib/gesture`), and kinetic flick deceleration live in `lib/scene`'s scroll state, parameterized
  by `input.scroll.*` in config. Every application scrolls at the exact same speed and curve.
- **Scrollbars** are thin overlay indicators (`6px` wide, expanding to `10px` on hover) that never
  steal layout width from content or cause reflows when a list crosses the overflow threshold.

## 5. Feedback, undo, errors, and permissions

### 5.1 Undo over confirmation dialogs

Confirmation dialogs train users to click "OK" without reading.

- **Reversible actions never ask.** Moving files to Trash, renaming, changing a setting, closing a
  tab-less viewer window, or clearing a list happens at once. If the action mutated user data, the
  bottom `undo_strip` appears for 8 seconds with an `Undo` button, and `Ctrl+Z` reverses it
  (FILEMANAGER.md §5.1).
- **Destructive, irreversible actions ask once, clearly.** Permanent deletion (`files.delete`),
  formatting a partition, or revoking an active session names the target and the consequence in the
  button label itself: **`Delete 14 files permanently`** (in `.danger()` style), never `Yes` / `OK`.
- **Risky system changes use a revert timer**, as DESKTOP.md §7.5 specifies for display modes:
  apply the change immediately and revert in 15 seconds unless the user confirms.

### 5.2 Honest error messages

When an operation fails, the UI states:

1. **What failed and on which object** (`Cannot copy "report.pdf" to /mnt/usb`).
2. **The human reason and the exact kernel or protocol code** (`No space left on device (ENOSPC)`,
   or `Permission denied (EACCES) — owned by root:wheel, mode 0640`).
3. **The recovery action, if one exists** (for example, FILEMANAGER.md §5.5's offer to clear a
   user-owned `uchg` immutable flag when `funlinkat` returns `EPERM`, or DESKTOP.md §4.7.4's
   one-click re-enable when a user shader exceeds its GPU frame budget).

No error message may hide the underlying `errno`, exit status, or config file and line number behind
an "Advanced…" expander. Developers and administrators are the primary audience.

### 5.3 Config transparency in UI

By DESKTOP.md P3, every persistent setting lives in the KDL configuration tree. In `sable-settings`
and any `form::<T>()` view:

- Hovering a setting or opening its context menu shows its **canonical KDL path** (for example,
  `files.view.hidden`), which layer currently sets it (`Default`, `Rice: hadal`, `User:
  ~/.config/sable/sable.kdl:42`, or `Runtime`), and a one-click **"Copy KDL snippet"** or **"Open in
  editor"**.
- Runtime-modified values (changed with a slider or `sablectl set` without saving) carry a visible
  dot indicator with **Persist** and **Revert** buttons (DESKTOP.md §7.3).

## 6. Untrusted text and stream safety

### 6.1 Rendering untrusted strings (`lib/scene`)

File names from an archive, window titles, terminal output, network SSIDs, controller device names,
and clipboard snippets are untrusted input. `lib/scene`'s `text` node applies FILEMANAGER.md §4.3's
and CLIPBOARD.md §4.3's rules everywhere by default:

- **Invalid UTF-8** (passed via `ByteStr`) renders as `\xNN` badges in the `text.escape` style
  class, never dropping the item or replacing bytes silently during a rename.
- **C0/C1 control characters and DEL** render as `\n`, `\t`, `\x1b` badges.
- **Bidirectional control characters** (U+202A–U+202E, U+2066–U+2069) render as visible code-point
  badges, and every label is laid out as a UAX #9 isolated directional run (`FSI`…`PDI`) so an
  untrusted string cannot flip the layout of surrounding UI text.

### 6.2 Stream-safe UI (`capture_exclude`)

Streamers are a first-class audience (DESKTOP.md §1.1). Every window or Facet node can set
`.capture_exclude(true)`, which clears the capture-sink bits in the compositor's `SceneSnapshot`
visibility mask (DESKTOP.md §4.3, §5.6.4). By default, the toolkit sets this flag on:

- `sable-clip` (clipboard history, paste progress, type-out indicator, CLIPBOARD.md §5.3);
- `sable-notify` toasts and the notification center (unless the user's streaming profile enables them);
- `sable-lock` and `sable-greet`;
- `secret_input` fields and any window or dialog belonging to `sable-vault` / authentication prompts;
- the `grants` and `security` pages of `sable-settings`.

When a window or overlay is excluded from capture while a stream or recording is active, the
window's header bar displays a subtle **"Hidden from stream"** badge so the streamer knows with
certainty that what they are looking at is not going out over RTMP/SRT.

## 7. The semantic theme contract

### 7.1 Required tokens

Every rice under `docs/desktop-themes/rices/` (and any user rice) defines this core token set in
`theme`. Applications and built-in Facets reference **only** these tokens (plus their own documented
namespace tokens, such as `term.ansi.0..15` in TERMINAL.md §8). `sable check` rejects a rice that
omits a required token or an application that references an undeclared one:

| Token | Type | Role |
|---|---|---|
| `@base` | `Color` | Deepest background (window canvas, terminal default background) |
| `@surface` | `Color` | Primary container background (panels, sidebars, cards, headers) |
| `@elevated` | `Color` | Floating or raised elements (popovers, menus, dialogs, hovered rows) |
| `@overlay` | `Color` | Modal backdrop scrim |
| `@border` | `Color` | Subtle hairlines and dividers |
| `@text` | `Color` | Primary foreground text and icons |
| `@text.muted` | `Color` | Secondary labels, timestamps, placeholders, disabled text |
| `@accent` | `Color` | Active focus ring, primary buttons, selected state, active workspace |
| `@ok` | `Color` | Success badges, completed jobs, healthy metrics |
| `@warn` | `Color` | Warnings, battery low, high latency/resource usage |
| `@danger` | `Color` | Destructive actions, errors, active recording/tally indicator |
| `@font.ui` | `Font` | Proportional UI font family (default `Inter`) |
| `@font.mono` | `Font` | Monospace font family for code, paths, sizes, and `sable-term` (default `JetBrains Mono`) |
| `@radius` | `Px` | Base corner radius (`0px` in `esports-zero` and `swiss-grid` up to `14px` in `fluorite`) |
| `@space.xs`, `@space.sm`, `@space.md`, `@space.lg` | `Px` | Standard spacing scale (`4px`, `8px`, `12px`, `20px` by default) |

**Contrast check.** `sable check` computes the OKLCH lightness contrast between `@text` and
`@base`/`@surface`, and warns if a rice drops below readable contrast unless the rice explicitly
marks itself as decorative.

### 7.2 Standard style classes

Every built-in widget in `lib/scene` maps to a named style class in `theme.style.*`, with fallback
defaults derived from the tokens above if a rice does not override the class:

- **Surfaces**: `window`, `panel`, `dock`, `header-bar`, `sidebar`, `status-bar`, `card`, `modal`, `popover`, `osd`
- **Controls**: `button`, `button.primary`, `button.danger`, `button.ghost`, `input`, `switch`, `slider`, `select`, `tab`
- **Data views**: `list.row`, `table.header`, `tree.row`, `badge`, `text.escape`

Each class supports the five state blocks (`hover`, `focus`, `active`, `selected`, `disabled`) and
optional property transitions (`transition "background" (ms)120 curve="ease-out"`). When a rice
sets `animation` durations to `0ms` (as `esports-zero` does for tournament play), all toolkit
transitions snap instantaneously across every application.

## 8. Relationship to other documents

| Document | Here |
|---|---|
| [DESKTOP.md](DESKTOP.md) §1.2, §7.3, §8.3 (principles, rices, styles) | §1, §7: the formal token and style contract |
| [TOOLKIT.md](TOOLKIT.md) (`lib/scene`, `desk/scene-render`, `desk/app`) | The implementation that enforces §2–§7 by default |
| [TERMINAL.md](TERMINAL.md) A3, §6.3, §8 | §2.2 (no tabs/splits), §4.1 (terminal chord exception), §7.1 |
| [FILEMANAGER.md](FILEMANAGER.md) A3, §4.2, §4.3, §5.1 | §2.2 (`window.beside`), §5.1 (undo over confirmation), §6.1 (untrusted names) |
| [CLIPBOARD.md](CLIPBOARD.md) §4, §5.3 | §4.1 (copy/paste chords), §6.2 (stream-safe exclusion) |
| [MANUALS.md](MANUALS.md) §2, §7.3 (Volume 6, Help key) | This document is Volume 6's forward design; §3.2 specifies `F1` |
| [docs/desktop-themes/README.md](desktop-themes/README.md) | §7: the 10 rices implement this token and style contract |
