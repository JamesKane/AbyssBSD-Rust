# Toolkit

- Status: **Draft**. A forward design for the desktop's UI foundation: the scene model (`lib/scene`),
  layout (`lib/flexgrid`), the shared Vulkan renderer (`desk/scene-render`), and the application and
  Facet SDKs (`desk/app`, `desk/facet`) that [DESKTOP.md](DESKTOP.md) §8.1–§8.9,
  [TERMINAL.md](TERMINAL.md) §2.1, and [FILEMANAGER.md](FILEMANAGER.md) §2.2 build on. Decisions
  here join the M-D ADR.
- Date: 2026-10-27
- Scope: the developer-facing API, the `Node` and widget vocabulary, state and event flow, layout
  and virtualization, client-side and compositor-side rendering, custom GPU passes, automatic
  settings forms, sandboxing, testing, and performance. Visual and interaction conventions live in
  [STYLE.md](STYLE.md).
- Inputs: [DESKTOP.md](DESKTOP.md) (§2.1, §4.7, §6, §7, §8), [STYLE.md](STYLE.md),
  [TERMINAL.md](TERMINAL.md) (§5.4, §7.3), [FILEMANAGER.md](FILEMANAGER.md) (§4.1, §7.3, §13
  question 1), [CLIPBOARD.md](CLIPBOARD.md) (§4, §8.1), [MANUALS.md](MANUALS.md) (§7.3),
  [ENGINEERING.md](ENGINEERING.md), [PERFORMANCE.md](PERFORMANCE.md), ADR-0014

The short version: **one declarative UI model for shell Facets and desktop applications, designed so
an author writes state, `update`, and `view`, and zero plumbing.** Vulkan is how we render, and it
is also our example of how *not* to design an API: pages of ceremonial structures, manual lifetime
wiring, and no defaults for the common case. In this toolkit, opening a window, connecting to
Wayland, allocating dma-buf swapchains, shaping text, scaling for DPI, handling IME and clipboard
pipes, loading config and themes, and entering Capsicum capability mode happen before the developer's
`init` runs. A view is a pure function `fn view(&self) -> Node`. The toolkit diffs successive trees
by key, lays out changed subtrees with `lib/flexgrid`, and either sends a compact patch over Conduit
(for a Facet) or draws it in-process with Vulkan (for an application). A custom GPU pass—like
`sable-term`'s cell grid—is a single trait implementation that receives a ready command buffer and
target image, never raw Vulkan setup.

## 1. Principles

### 1.1 The anti-Vulkan rule

Vulkan is fast because its execution model matches modern GPUs. Its API is painful because it
confuses explicitness on the hot path with ceremony everywhere else: fifteen chained structs with
`sType` and `pNext` just to say what every program says the same way. Traditional GUI toolkits
(GTK, Qt, Win32, and many Rust toolkits) repeat that mistake in another form: deep widget-object
hierarchies, manual signal connection and disconnection, interior-mutability traps, and hundreds of
lines of initialization before a window shows a list.

| # | Principle | Consequence |
|---|---|---|
| T1 | **Zero ceremony for the 95% case** | A complete application starts with a state struct, a message enum, `init`, `update`, `view`, and `sable_app::main!`. No Wayland, Vulkan, swapchain, font, clipboard, or config boilerplate ever appears in application code (§3) |
| T2 | **One UI model, two hosts** | Facets (rendered by the compositor) and applications (rendered client-side) use the exact same `Node` builders, widget vocabulary, layout engine, and style system. Moving a view between them changes the entry macro, not the view (§2.1) |
| T3 | **Views are values, not retained objects** | Widgets are not long-lived mutable structs with callbacks attached. `view(&self) -> Node` returns a lightweight description of what should be on screen now. The runtime owns the retained state (focus, scroll offsets, text cursors, animations, glyph caches) keyed by node ID (§3.3) |
| T4 | **Work scales with what changed and what is visible** | Unchanged subtrees are skipped in diffing, layout, and rendering. Virtualized containers (`list`, `grid_view`, `table`, `tree`) build nodes only for the viewport plus a one-screen margin ([FILEMANAGER.md](FILEMANAGER.md) F2) (§4.2) |
| T5 | **Progressive disclosure for custom GPU work** | When an application needs its own shader or render pass (`sable-term`'s cell pass, a video viewport, a waveform), it implements one `Pass` trait and receives a pre-recorded command buffer, a target view, and a damage scissor. It never creates a `VkInstance`, `VkDevice`, or swapchain (§6.3) |
| T6 | **Conventions enforced by construction** | Keyboard navigation, clipboard and primary selection rules ([CLIPBOARD.md](CLIPBOARD.md) §4), untrusted-text sanitization, `F1` help routing ([MANUALS.md](MANUALS.md) §7.3), and theme reactivity are built into the standard nodes, so an application cannot forget to implement them ([STYLE.md](STYLE.md)) |
| T7 | **Safe Rust at the application boundary** | `lib/scene`, `lib/flexgrid`, `lib/textedit`, `desk/app`, and `desk/facet` are `#![forbid(unsafe_code)]`. All `unsafe` Vulkan and FFI calls sit inside `desk/scene-render` |

### 1.2 Non-goals

- **A general-purpose cross-platform GUI toolkit.** It targets AbyssBSD and FreeBSD on Wayland and
  Vulkan (DESKTOP.md §1.3). Not having to abstract over X11, Windows, macOS, OpenGL, Direct3D, or
  system-native menu bars deletes three-quarters of what makes cross-platform toolkits large.
- **Webviews or HTML/CSS layout.** No component embeds a browser engine, and CSS's cascade and
  selector complexity are deliberately absent (DESKTOP.md §8.3, [STYLE.md](STYLE.md) S2).
- **In-process dynamic plugins or scripting.** Views are compiled Rust. Rices customize appearance
  through KDL tokens, styles, and SSL shaders (DESKTOP.md §7.3).
- **Exposing raw Vulkan to ordinary views.** Standard applications never import `lib/vk`.

## 2. Architecture and crates

### 2.1 One pipeline, two hosts

```
  Application or Facet code:   State + update(Msg, &mut Cx) + view(&self) -> Node
                                                      │
                                         lib/scene: key-based diff
                                                      │
                        ┌─────────────────────────────┴─────────────────────────────┐
                        ▼ (Facet: over Conduit `scene.Surface`)                     ▼ (App: in-process)
         ┌─────────────────────────────┐                             ┌─────────────────────────────┐
         │         sable-comp          │                             │      desk/scene-render      │
         │  lib/scene style + state    │                             │  lib/scene style + state    │
         │  lib/flexgrid layout        │                             │  lib/flexgrid layout        │
         │  HarfBuzz + lib/raster      │                             │  HarfBuzz + lib/raster      │
         │  Vulkan render graph → KMS  │                             │  Vulkan render graph → WSI  │
         └─────────────────────────────┘                             └─────────────────────────────┘
```

Because both paths run the same `lib/scene`, `lib/flexgrid`, `lib/textedit`, `lib/textlayout`, and
`lib/raster` code, and `sable-comp`'s Facet pass is `desk/scene-render`, a button, a text field, or
a virtual list looks and behaves identically in a panel popup, in the compositor-drawn file picker
(`sable-picker`), and in a standalone application window (`sable-files`).

### 2.2 Crate split

Following DESKTOP.md §2.1, pure logic lives in `lib/` (`no_std` + `alloc`, no foreign dependencies,
host-tested and fuzzed), and crates that touch the OS, Vulkan, or the foreign inventory (HarfBuzz,
xkbcommon, libpng, libjpeg-turbo) live in `desk/`:

| Crate | Kind | `unsafe` | Contents |
|---|---|---|---|
| `lib/scene` | `no_std` + `alloc` | forbidden | `Node`, widget builders, `Key`, view diffing (`Patch` codec), retained widget state (focus, hover, scroll, text edit state), style resolution (`class → state → inline`), animation interpolation, hit-testing |
| `lib/flexgrid` | `no_std` + `alloc` | forbidden | Flexbox and CSS-grid subset layout, intrinsic sizing, virtualized viewport slicing |
| `lib/textedit` | `no_std` + `alloc` | forbidden | Cursor movement, grapheme-aware selection, undo/redo history, and paste normalization (CLIPBOARD.md §4.3, §8.1) |
| `lib/textlayout` | `no_std` + `alloc` | forbidden | UAX #14 line breaking, UAX #9 bidi resolution, paragraph layout and hit-testing |
| `lib/raster` | `no_std` + `alloc` | forbidden | Area-coverage outline rasterizer, path tessellation, analytic box-drawing generator |
| `lib/svg` | `no_std` + `alloc` | forbidden | Icon-subset SVG parser and flattener |
| `desk/scene-render` | `std` | Vulkan & FFI modules | The Vulkan scene renderer shared by `sable-comp` and client apps: dma-buf/shm buffer management, glyph and icon atlases, HarfBuzz shaping, image decoding, instanced SDF/quad/blur/shader passes, damage tracking (`lib/scene-render` in DESKTOP.md §8.9 refers to this crate) |
| `desk/app` | `std` | forbidden | `sable_app`: the application SDK (Wayland client, multi-window lifecycle, Conduit channels, action registry, `form::<T>()` schema UI generator, headless test runner) |
| `desk/facet` | `std` | forbidden | `sable_facet`: the Facet SDK (DESKTOP.md §8.4), re-exporting `lib/scene`'s builders |

An application links `desk/app` (which pulls in `lib/scene` and `desk/scene-render`). It imports one
prelude: `use sable_app::prelude::*;`.

## 3. The developer API

### 3.1 A complete application

Here is what an application author writes—in full—to build a searchable, keyboard-navigable log
viewer with live config, an exported action, and a settings page:

```rust
use sable_app::prelude::*;

#[derive(Schema, Clone, PartialEq)]
#[schema(path = "logview")]
pub struct Config {
    #[schema(default = "true")]
    pub timestamps: bool,
    #[schema(default = "10000", range = 100..=1_000_000)]
    pub max_lines: u32,
}

pub enum Msg {
    QueryChanged(String),
    Select(usize),
    CopySelected,
    Clear,
}

#[app(
    id = "sable-logview",
    title = "Log Viewer",
    config = Config,
    sandbox = "capability",
    actions(
        ("logview.copy",  Msg::CopySelected, "Copy selected line"),
        ("logview.clear", Msg::Clear,        "Clear log buffer"),
    ),
)]
pub struct LogView {
    lines: Vec<String>,
    filtered: Vec<usize>,
    query: String,
    selected: Option<usize>,
}

impl App for LogView {
    type Config = Config;
    type Msg = Msg;

    fn init(cx: &mut Init<Self>) -> Self {
        Self { lines: Vec::new(), filtered: Vec::new(), query: String::new(), selected: None }
    }

    fn update(&mut self, msg: Msg, cx: &mut Cx<Self>) {
        match msg {
            Msg::QueryChanged(q) => {
                self.query = q;
                self.filtered = (0..self.lines.len())
                    .filter(|&i| self.lines[i].contains(&self.query))
                    .collect();
            }
            Msg::Select(i) => self.selected = Some(i),
            Msg::CopySelected => {
                if let Some(i) = self.selected.and_then(|s| self.filtered.get(s)) {
                    cx.clipboard().set_text(&self.lines[*i]);
                }
            }
            Msg::Clear => {
                self.lines.clear();
                self.filtered.clear();
            }
        }
    }

    fn view(&self, cx: &ViewCx<Self>) -> Node {
        col()
            .child(
                header_bar()
                    .start(search_input(&self.query).placeholder("Filter…").on_input(Msg::QueryChanged))
                    .end(button("Clear").on_press(Msg::Clear)),
            )
            .child(
                list(self.filtered.len(), Px(24), |row_idx| {
                    let line = &self.lines[self.filtered[row_idx]];
                    row()
                        .key(row_idx)
                        .class("list.row")
                        .state_if(self.selected == Some(row_idx), "selected")
                        .on_activate(Msg::Select(row_idx))
                        .child(text(line).mono())
                })
                .flex(1.0),
            )
            .child(status_bar().start(text(format!("{} lines", self.filtered.len()))))
    }
}

sable_app::main!(LogView);
```

### 3.2 What `sable_app::main!` does behind the scenes

Before `LogView::init` is called, the SDK has already:

1. Embedded the `Config` schema (`.note.sable.schema`) and the app manifest (`.note.sable.manifest`,
   with its action signatures and requested grants) into the binary at compile time, so `sabled`,
   `sablectl`, and `sable-settings` know its config keys and actions without running it.
2. Started the `sable-rt` kqueue executor.
3. Connected to Wayland, bound the baseline protocols (DESKTOP.md §4.2.1), opened the GPU render
   node, created the Vulkan device and pipeline cache, and mapped the shared glyph and icon caches.
4. Connected its Conduit channels from `sabled` (or fallen back to built-in defaults if launched
   standalone from a shell, as TERMINAL.md §8 specifies), receiving the initial `Config` and active
   theme tokens.
5. If `sandbox = "capability"` is set on `#[app]`, called `cap_enter(2)` after opening the render
   node and fonts, so `init`, `update`, and `view` execute in Capsicum capability mode from the
   first line.
6. Created the initial `xdg_toplevel` window, negotiated server-side decorations and fractional
   scale, and scheduled the first frame.

When the user edits `~/.config/sable/sable.kdl` or switches rices:

- Theme token and style changes trigger a re-render automatically; `LogView` is not even woken
  unless its `view` reads `cx.config()`.
- Changes to `logview.*` validate against `Config`'s schema, update `cx.config()`, and call a view
  pass for the next frame.

### 3.3 Why views are values (`Node`), and where widget state lives

In immediate-mode GUIs (Dear ImGui), there is no retained tree, which makes layout, accessibility,
and animations awkward. In classic retained-mode GUIs (GTK, Qt), the developer constructs and mutates
a graph of heap-allocated widget objects, which requires shared mutable state and callback lifetimes.

`lib/scene` splits the two:

- **The developer's view is a value.** `view(&self) -> Node` builds a flat arena of `NodeData`
  structs for the current state. Allocation goes into a bump arena reset every frame (or two
  arenas swapped between frames), so building a view of 500 visible nodes takes microseconds and zero
  individual heap allocations.
- **The toolkit's scene store is retained.** `lib/scene::Store` diffs the new `Node` arena against
  the previous one by **`Key`** (an explicit `.key(k)` on dynamic items, or an automatic structural
  hash of `(parent_key, child_index, node_kind)`).
- **Transient widget state belongs to the `Store`, keyed by `Key`.** An `input(&self.query)` node
  does not make the developer store the cursor byte offset, selection anchor, scroll offset, IME
  preedit string, or undo ring in `LogView`. The `Store` keeps a `TextEditState` (`lib/textedit`)
  for that node's `Key` as long as the key stays in the tree, and fires `on_input(String)` only when
  the text changes.
- **Scroll offsets work the same way.** A `scroll(...)` or `list(...)` node keeps its pixel offset
  and kinetic-fling velocity in the `Store` by default, while also offering `.scroll_offset(px)` and
  `.on_scroll(Msg)` when the application wants to drive or observe it.

### 3.4 Multi-window applications

Both `sable-term` (TERMINAL.md §7.3) and `sable-files` (FILEMANAGER.md §6.3) serve all of their
windows from one process so they share the Vulkan device, glyph atlases, and caches. `desk/app`
supports single-window and multi-window apps with the same trait:

- **Single-window** (default): implements `fn view(&self, cx: &ViewCx<Self>) -> Node`.
- **Multi-window**: implements `fn view_window(&self, win: WindowId, cx: &ViewCx<Self>) -> Node` and
  spawns or closes windows through `cx.open_window(WindowSpec::default().beside(win))` and
  `cx.close_window(win)`. When `sabled` invokes `apps.launch` for an already-running instance, the
  SDK delivers a `WindowRequested` event instead of starting a second process.

### 3.5 Asynchronous work and subscriptions

The UI thread never blocks. `Cx<Self>` provides three non-blocking primitives on `sable-rt`:

```rust
// 1. One-shot kqueue / Conduit future (e.g., opening a file through the compositor-drawn picker):
cx.spawn(async move |proxy| {
    let fd = proxy.picker().open_file(FileFilter::images()).await?;
    Ok(Msg::FileOpened(fd))
});

// 2. Background CPU/blocking work on the worker pool (e.g., decoding, stat walks):
cx.compute(move || heavy_parse(bytes), Msg::Parsed);

// 3. Subscriptions to Conduit events, kqueue fds (EVFILT_VNODE, EVFILT_READ, EVFILT_PROCDESC), or timers:
cx.subscribe(stream, Msg::ItemArrived);
cx.every(Duration::from_secs(1), Msg::Tick);
```

Every completion delivers a typed `Msg` to `update` on the UI thread, which coalesces bursty
completions within the same frame so `view` runs at most once per vblank.

## 4. The Node and widget vocabulary

### 4.1 Primitives and controls

DESKTOP.md §8.3 lists the ten low-level scene primitives (`Box`, `Text`, `Icon`, `Image`, `Shape`,
`Slot`, `Surface`, `Canvas`, `Effect`, `Input`). On top of those primitives, `lib/scene` provides
ergonomic builder functions that pre-configure layout, style classes, keyboard navigation, and ARIA-like
role metadata:

| Category | Builders | Notes |
|---|---|---|
| **Layout** | `row()`, `col()`, `grid(cols)`, `stack()`, `spacer()`, `divider()` | Built on `lib/flexgrid`. Gap, padding, alignment, and flex grow/shrink are chainable methods (`.gap(Px(8))`, `.pad(Px(12))`, `.flex(1.0)`) |
| **Panes & Scroll** | `scroll(child)`, `split_h(a, b)`, `split_v(a, b)` | `split_*` provides a draggable divider for sidebar + main views (e.g., `sable-files`'s places sidebar). Document tabs and multi-document splits remain the compositor's job ([STYLE.md](STYLE.md) S1) |
| **Virtualized** | `list(len, row_h, f)`, `list_var(len, est_h, f)`, `grid_view(len, cell_sz, f)`, `table(cols, len, f)`, `tree(nodes, f)` | §4.2. Settles FILEMANAGER.md §13 question 1 |
| **Chrome** | `header_bar()`, `status_bar()`, `toolbar()`, `sidebar()`, `tabs(items, active)` | `tabs` is for switching category views inside an inspector or settings pane, never for document windows |
| **Text & Media** | `text(s)`, `rich_text(spans)`, `code_block(s)`, `icon(name)`, `image(src)`, `avatar(src)` | `text` sanitizes control and bidi characters automatically unless `.raw()` is requested; `.mono()` selects `@font.mono` |
| **Buttons** | `button(label)`, `icon_button(icon)`, `toggle(label, on)`, `segmented(options, active)` | Style variants: `.primary()`, `.danger()`, `.ghost()` |
| **Inputs** | `input(val)`, `search_input(val)`, `secret_input(val)`, `text_area(val)`, `number_input(val)`, `slider(val, range)`, `checkbox(label, on)`, `switch(on)`, `select(options, active)`, `color_picker(color)`, `key_capture(bind)` | Every text field uses `lib/textedit` and honors CLIPBOARD.md §4.3 (including disabling copy/primary in `secret_input`) |
| **Feedback** | `progress_bar(ratio)`, `progress_ring(ratio)`, `badge(label)`, `banner(kind, msg)`, `empty_state(icon, title, hint)`, `undo_strip(msg, on_undo)` | — |
| **Overlays** | `tooltip(text)`, `popover(content)`, `menu(items)`, `context_menu(items)`, `modal(title, body)`, `confirm_dialog(spec)` | Rendered in the window's overlay layer (or as compositor popups for Facets), dismissed on `Escape` or outside click |
| **Escape hatches** | `canvas(id, pass)`, `effect(shader, child)`, `surface_view(handle)`, `slot(name)` | §6.3, DESKTOP.md §8.3 |

### 4.2 Virtualized containers (`List`, `Grid`, `Table`, `Tree`)

FILEMANAGER.md §4.1 and §13 question 1 ask for a virtualized `List` node in the scene model so a
million-entry directory builds nodes only for what is on screen. `lib/scene` includes `List` as a
first-class scene primitive:

```rust
list(1_000_000, Px(28), |index| {
    let entry = &self.entries[index];
    row().key(entry.ino)
        .child(icon(entry.icon))
        .child(text(entry.display_name()))
})
```

How it works in both hosts:

1. **In an Application (`desk/scene-render`)**: `lib/flexgrid` knows the container's viewport height
   and scroll offset from the `Store`. It invokes the closure **only** for the visible index range
   `first_visible .. last_visible` plus a margin of one viewport above and below (for smooth kinetic
   scrolling). For a 1080p window with 28 px rows, that is ~115 closure calls whether the list has
   200 items or 2,000,000.
2. **In a Facet (`sable-comp`)**: the Facet SDK sends the total item count, row height, and the
   slice of nodes for the current viewport window. As the user scrolls in the compositor, the
   compositor scrolls the already-cached margin immediately at 240 Hz and sends a `ViewportChanged`
   event so the Facet streams the next margin slice asynchronously.
3. **Row recycling**: `desk/scene-render` keys shaped text runs and icon lookups by `(RowKey,
   ColumnWidthBucket)`, so scrolling through already-visited or uniform rows allocates nothing.
4. **`table` and `tree`** build on the same virtualized primitive: `table` adds sticky column
   headers with sort indicators and column-resize handles; `tree` flattens expanded nodes into a
   visible-row index with indentation levels and expand/collapse chevrons.

### 4.3 Automatic forms from `#[derive(Schema)]`

Because every configuration struct derives `Schema` (DESKTOP.md §7.4), `desk/app` provides
**`form::<T>(&value, Msg::Changed)`**, which generates a complete, themed settings view from `T`'s
schema metadata:

| Rust type / attribute | Generated control |
|---|---|
| `bool` | `switch` with the doc comment as label and description |
| `u32`, `f32`, `Px`, `Ms`, `Percent` with `range = a..=b` | `slider` + `number_input` with unit suffix |
| `enum` | `segmented` (≤ 4 variants) or `select` (> 4 variants) |
| `Color` | `color_picker` with OKLCH/hex input and theme-token references |
| `KeyBind` / `PadBind` | `key_capture` widget |
| `OutputRef`, `DeviceMatch`, `StyleRef` | Live dropdown populated from `sabled`'s current state |
| `Vec<T>` / `OrderedMap<K, V>` | Reorderable list with Add / Remove buttons |

`sable-settings` uses this to render the entire desktop's settings UI from the `.note.sable.schema`
sections of installed components, and any application can embed `form::<Config>()` for its own
preferences or inspector panes with zero hand-written form code.

## 5. Layout, styling, and text

### 5.1 Layout (`lib/flexgrid`)

`lib/flexgrid` implements two layout modes that cover every desktop UI layout without CSS's legacy
float, inline, or table engines:

- **Flexbox** (`row`, `col`): main-axis and cross-axis alignment, `gap`, `flex` grow/shrink, `wrap`
  (used by flow grids and tag lists), and `min`/`max`/`fixed` sizes in logical `Px` or percentages.
- **Grid** (`grid`): fixed (`Px`), fractional (`Fr`), and `Auto` tracks, with explicit or automatic
  cell placement and `gap`.

Layout is **incremental**: each node carries a dirty bit set when its style, text, or children
change, and a cached `(SizeConstraints → Size)` measurement. A keystroke in a search box re-lays out
the search box and the virtual list's visible slice, not the rest of the window.

### 5.2 Style resolution (`lib/scene`)

As DESKTOP.md §8.3 specifies, styles come from the active theme's `theme.style.*` definitions and
resolve in strict order:

```
  1. Class default        (e.g. `style "button"`)
  2. Active state blocks  (in fixed order: `disabled`, `selected`, `active`, `focus`, `hover`)
  3. Inline overrides     (explicit `.pad(...)` or `.bg(...)` in the view, used sparingly)
```

There are no descendant selectors (`div > ul li:nth-child(2)`), no `!important`, and no DOM walks.
Resolving a node's style is an $O(1)$ table lookup plus merging at most five state masks. Every
color, radius, font, and spacing value references a theme token (`@surface`, `@accent`, `@radius`),
resolved in linear `R16G16B16A16_SFLOAT` space for blending.

### 5.3 Text shaping and rendering

Text in `desk/scene-render` follows the exact path DESKTOP.md §4.7.3 defines for the compositor:

1. **Sanitization**: control characters and bidirectional overrides are replaced with visible escape
   badges (`\xNN`, `U+202E`) unless the node opted into raw rendering ([FILEMANAGER.md](FILEMANAGER.md) §4.3).
2. **Line breaking and bidi**: `lib/textlayout` splits paragraphs at UAX #14 break opportunities and
   resolves UAX #9 embedding levels.
3. **Shaping**: HarfBuzz shapes each run and supplies glyph outlines (`hb-draw`) and color glyph
   paint lists (`hb-paint`). Shaped runs are cached by `(hash(text), FontId, SizePx, Features)`.
4. **Rasterization**: `lib/raster` rasterizes coverage glyphs into the process's `R8_UNORM` GPU
   atlas (and color glyphs into `R8G8B8A8_SRGB`), quantized to 4 horizontal subpixel positions for
   proportional text (whole pixels for monospace).

## 6. Client-side Vulkan rendering (`desk/scene-render`)

### 6.1 How applications render without touching Vulkan

`desk/scene-render` wraps `lib/vk` (generated from `vk.xml`, DESKTOP.md §2.1) behind a renderer that
manages the entire GPU lifecycle:

- **Buffer pool, not `VkSwapchainKHR` surprises**: the renderer allocates a 3-buffer pool of
  `VK_EXT_external_memory_dma_buf` images with the format and DRM modifier chosen from the
  compositor's `zwp_linux_dmabuf_v1` feedback (so a fullscreen app automatically picks a
  direct-scanout modifier). On lavapipe (`gop-kms`), it uses host-visible dma-bufs or `wl_shm`
  transparently.
- **Explicit sync**: each frame waits on the compositor's release point and signals its acquire
  point through `wp_linux_drm_syncobj_v1` timeline semaphores (DESKTOP.md §4.4).
- **Instanced uber-batching**: rounded rectangles, borders, analytic SDF drop shadows, icons, and
  atlas glyphs are packed into a single storage buffer of instance records and drawn in a handful of
  instanced calls per frame.
- **Buffer-age damage**: `lib/scene` computes the bounding union of nodes that moved, resized, or
  changed visual state since frame $n$, intersects it with each buffer's age, sets the Vulkan render
  pass scissor, and reports the damage rectangles via `wl_surface.damage_buffer`. An idle window
  draws **zero frames** and consumes **zero GPU time**—cursor blink in a focused text field damages
  only the 2 × 20 px caret rectangle.

### 6.2 Shared caches across windows and processes

- **Within a multi-window process**: one `VkDevice`, one pipeline cache, one glyph atlas, and one
  icon atlas serve every window. Opening a second window (`files.window.beside`, `term.window.new`)
  allocates only that window's 3 dma-buf swap images and draws on the next vblank.
- **Across processes**: compiled SPIR-V shaders are embedded in `desk/scene-render` at build time by
  `tools/shadec`. The Vulkan pipeline cache is persisted in `$XDG_CACHE_HOME/sable/vk-cache/` keyed
  by `(device_uuid, driver_version)`. Theme icons pre-rasterized at the session's scale by `sabled`
  are shared via a read-only sealed memfd handed to every launched application, so common icons
  (`folder`, `file`, `search`, `close`) are uploaded without parsing SVG in each process.

### 6.3 Custom GPU passes (`canvas`)

When an application needs custom GPU rendering—such as `sable-term`'s single-pass cell grid
(TERMINAL.md §5.4), `sable-view`'s zero-copy video plane, or an audio spectrum in `sable-patchbay`—it
places a `canvas(id, pass)` node in its `view()` and implements `CustomPass`:

```rust
pub trait CustomPass: Send + 'static {
    /// Called once when the device is created or recreated after VK_ERROR_DEVICE_LOST.
    fn init(&mut self, gpu: &mut GpuInitCx) -> Result<(), GpuError>;

    /// Called during the frame's render pass for this canvas node's rectangle.
    fn record(&mut self, frame: &mut GpuFrameCx);
}
```

`GpuInitCx` lets the pass compile an SSL shader (checked in at build time) into a pipeline and
allocate typed storage buffers or textures (`GpuBuffer<GpuCell>`, `GpuAtlas`) without filling in raw
Vulkan descriptor-set layouts or memory-type bitmasks. `GpuFrameCx` hands it the command buffer, its
clipped `Rect` in physical pixels, the damage scissor, and the frame's uniform time. If the GPU
resets (`VK_ERROR_DEVICE_LOST`, DESKTOP.md §4.8), `desk/scene-render` rebuilds the device and calls
`init` again automatically.

## 7. Desktop integration out of the box

Every `sable_app` application gets these integrations with zero code from the author:

| Integration | How it works automatically |
|---|---|
| **Configuration & live reload** | `#[app(config = T)]` subscribes to the application's KDL subtree and updates `cx.config()` live (DESKTOP.md §7.5) |
| **Actions & keybindings** | `#[app(actions(...))]` registers actions in the manifest. Keys bound in `bind` (including `app=` scoped bindings) dispatch straight to `update` |
| **In-app action palette** | Pressing `Ctrl+Shift+P` opens a built-in overlay listing all of the application's actions and their current bindings ([STYLE.md](STYLE.md) §4.2) |
| **Context-sensitive help** | Pressing `F1` sends the focused node's `.help("volume/chapter#node")` address (or the app's root manual node) over Conduit `doc.Help` to open `abyssdoc` (MANUALS.md §7.3) |
| **Clipboard & primary selection** | All `input` and selectable `text` nodes implement CLIPBOARD.md §4 (`Ctrl+C`/`V`/`X`, middle-click primary paste, async large-paste progress, secret-field protection) |
| **IME (`zwp_text_input_v3`)** | Focused text fields report their cursor rectangle and render preedit spans with composition underlines |
| **File picking** | `cx.picker().open_file(...)` and `save_file(...)` invoke `fs.Picker` (`sable-picker`) and return a rights-limited `OwnedFd` (FILEMANAGER.md §7.3), working inside capability mode |
| **Opening URLs and files** | `cx.open_uri(uri)` and `cx.open_file(path)` call `sabled`'s `apps.open` after showing the target URI on hover (TERMINAL.md §6.3) |
| **Notifications** | `cx.notify(Notification::new(...))` posts through `notify.Post` |

## 8. Processes and the sandbox

As with `sable-term` (TERMINAL.md §7) and `sable-files` (FILEMANAGER.md §7), the toolkit makes the
least-privilege structure the easiest one to write:

- **`sandbox = "capability"` (the default for `#[app]`)**: `sable_app::main!` opens its Wayland
  socket, DRM render node, font memfds, and Conduit channels, then calls `cap_enter(2)` before
  `App::init`. The application cannot open files by path or make network connections; it accesses
  user files solely through descriptors returned by `cx.picker()` or passed at launch by `sabled`.
- **Privilege-separated helpers**: when an application needs an unsandboxed authority alongside a
  sandboxed parser (`sable-term` + `sable-term-spawn`, `sable-files` + `sable-files-peek`),
  `sable_app::spawn_helper::<H>()` `pdfork`s the helper binary over a `SOCK_SEQPACKET` pair with
  Conduit-generated message codecs and `SCM_RIGHTS` descriptor narrowing in one call.

## 9. Testing and performance

### 9.1 Testing (`sable_app::test`)

Because `view` is a pure function and `lib/scene` is `no_std`, applications are tested at three
levels without a display server:

1. **State & model tests (host, microseconds)**: instantiate `App` with `TestCx`, feed `Msg` values
   or simulated key/pointer actions (`cx.press_key("Ctrl+F")`, `cx.type_text("error")`), and assert
   on the state or query the returned `Node` tree by key, class, or text (`assert!(tree.has_text("3 lines"))`).
2. **Layout & property fuzzing (host)**: `lib/flexgrid` and `lib/scene`'s diffing and hit-testing
   are fuzzed with random trees, resizes, and scroll jumps, checking that no node overflows its clip
   without a scroll container and that every visible interactive node is reachable by `Tab` traversal.
3. **Pixel snapshot tests (headless lavapipe)**: `sable_app::test::render(&app, Size::new(800, 600),
   &HADAL_RICE)` renders the view through `desk/scene-render` on lavapipe at scales `1.0`, `1.25`,
   and `2.0`, and compares the output image against golden PNGs. Every built-in widget is snapshot-tested
   across the shipped rices.

### 9.2 Performance (`desk.toolkit`)

Under ADR-0014, `desk.toolkit` is measured against **GTK 4 (Vulkan renderer) on sway on FreeBSD 15.1**:

| Benchmark | Measures | Target / Gate |
|---|---|---|
| `toolkit.start` | Process start to first presented frame for a warm window (shared pipeline cache and icon memfd) | ≤ 8 ms on reference hardware, and faster than GTK 4 |
| `toolkit.window-new` | Action to first presented frame for a second window in the same process | ≤ 1 frame budget (4.1 ms at 240 Hz) |
| `toolkit.list-scroll` | CPU + GPU frame time (p99) while flinging a 1,000,000-row virtual list at 240 Hz | ≤ 1.5 ms per frame, zero heap allocations per scrolled frame |
| `toolkit.diff-layout` | Time to build, diff, and lay out a complex 500-node application view after a state update | ≤ 250 µs on both reference machines |
| `toolkit.idle-cpu` | CPU wakeups over 10 seconds for a visible, unfocused window with no animations | **0 wakeups** |
| `toolkit.rss` | Resident memory of a minimal window (`LogView` with 1,000 lines) | ≤ 18 MiB (including Vulkan driver mappings); ≤ 3 MiB for a Facet (DESKTOP.md §8.5) |

## 10. Relationship to other documents

| Document | Here |
|---|---|
| [DESKTOP.md](DESKTOP.md) §8.1–§8.5 (Facets, scene model, SDK) | §2.1, §3.3, §4: the shared `lib/scene` and `lib/flexgrid` crates |
| [DESKTOP.md](DESKTOP.md) §8.9 (`lib/scene-render`, minimal toolkit) | This document (`desk/scene-render` and `desk/app`) |
| [STYLE.md](STYLE.md) (Desktop Style Guide, Volume 6) | §4.1, §7: the rules and conventions the toolkit enforces by default |
| [TERMINAL.md](TERMINAL.md) §2.1, §5.4 (`sable-term` chrome and cell pass) | §6.3 (`CustomPass`) |
| [FILEMANAGER.md](FILEMANAGER.md) §4.1, §13 Q1 (virtual `List` node) | §4.2: settles Q1 by adding `List` (and `Grid`, `Table`, `Tree`) to `lib/scene` |
| [CLIPBOARD.md](CLIPBOARD.md) §4, §8.1 (`lib/textedit` and paste rules) | §2.2, §4.1 |
| [MANUALS.md](MANUALS.md) §7.3 (Help protocol and Volume 6) | §7 (`F1` routing over `doc.Help`) |

## 11. Open questions

1. **Crate naming: `lib/scene-render` vs `desk/scene-render`.** DESKTOP.md §8.9, TERMINAL.md,
   FILEMANAGER.md, and CLIPBOARD.md refer to `lib/scene-render`, while DESKTOP.md §2.1 states that
   `lib/` holds only pure `no_std` crates with no foreign dependencies and that crates wrapping
   foreign libraries (HarfBuzz, Vulkan loader) live in `desk/`. Proposed: pure scene/layout crates
   live in `lib/scene` and `lib/flexgrid`, the Vulkan renderer lives at `desk/scene-render` (with
   Cargo package name `sable-scene-render`), and earlier references to `lib/scene-render` mean the
   `lib/scene` + `desk/scene-render` pair.
2. **Rich text editing.** `lib/textedit` handles plain and monospace text fields. Do any first-release
   native applications need a rich-text (formatted spans) *editor*, or only rich-text *display*
   (`rich_text` for manuals, markdown preview, notifications)? Proposed: display only in the first
   release; `sable-edit` edits plain text and source code.
3. **Accessibility protocol (`accesskit` / AT-SPI2).** AT-SPI2 is deeply tied to D-Bus. Because
   `lib/scene` already carries semantic roles, labels, values, and focus states on every `Node`, the
   compositor has the complete accessibility tree of every Facet, and `desk/app` has it for every
   native window. Proposed: expose a native Conduit `a11y.Tree` interface from `sable-comp` first,
   and bridge to AT-SPI2 in `sable-compat-freedesktop` (DESKTOP.md §8.8) if external screen readers
   are used.

## 12. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **K.1** | `lib/scene` (nodes, builders, key diffing, style resolution, hit-testing), `lib/flexgrid` (flex, grid, virtual list math), and `lib/textedit` | Nothing (host only) | Host unit tests and layout/diff fuzzers run clean for 24 hours; 500-node diff + layout under 250 µs |
| **K.2** | `desk/scene-render`: Vulkan dma-buf pool, syncobj, HarfBuzz + `lib/textlayout` + `lib/raster` text, `lib/svg` icons, instanced quad/SDF/blur passes, `CustomPass`, headless lavapipe runner | M-D.2, M-D.3 | Snapshot tests for every widget pass on lavapipe at scales 1.0, 1.25, and 2.0 across all 10 rices |
| **K.3** | `desk/facet` and `desk/app`: `#[facet]` and `#[app]` macros, `form::<T>()` schema UI generator, multi-window support, Conduit integration (`config`, `fs.Picker`, `doc.Help`), capability-mode startup | M-D.1, M-D.3, P6.7 Capsicum | `LogView`-style reference app and `sable-settings` run in QEMU; `desk.toolkit` meets its bounds against GTK 4 |
