# File manager

- Status: **Draft**. A forward design for `sable-files`, the file manager, and for the file picker
  behind `fs.Picker` that [DESKTOP.md](DESKTOP.md) §8.4 and §8.9 name. Decisions here join the M-D
  ADR.
- Date: 2026-10-27
- Scope: the file manager's data model, directory listing and change tracking, rendering, file
  operations, trash, thumbnails and previews, search, the picker, process and sandbox structure,
  configuration, testing, and performance. The file systems themselves are the kernel's (VFS.md) and
  are in scope only for what the file manager needs from them (§10).
- Inputs: [DESKTOP.md](DESKTOP.md) (§2.1, §6, §7, §8.3, §8.4, §8.9), [TERMINAL.md](TERMINAL.md)
  (whose structure this document follows), [MULTIUSER.md](MULTIUSER.md) (§10.7, ACLs and
  extended attributes), [ROADMAP.md](ROADMAP.md) (M-D, P6.7, M-Z), [ENGINEERING.md](ENGINEERING.md),
  [PERFORMANCE.md](PERFORMANCE.md), ADR-0007, ADR-0014

The short version: **a file manager whose cost to open a directory is the cost of reading it, and
whose cost to draw one is the size of the window.** A directory is read with large
`getdirentries(2)` calls into a name arena, and its names appear before any file is stat'ed.
Metadata is fetched by a worker pool in inode order, visible rows first. The view is virtualized:
only the rows on screen become scene nodes, so a million-entry directory scrolls like a ten-entry
one. Every file operation is descriptor-relative (`openat`, `fstatat`, `funlinkat`, `renameat2`),
so a tree that changes underneath a copy or a delete cannot redirect it. Data moves with
`copy_file_range(2)`, inside the kernel. Everything that reads file *contents* (thumbnails,
previews, type sniffing, archives) runs in Capsicum capability mode with one rights-limited
descriptor per request. It is built on the desktop's own toolkit (`lib/scene-render`): **no Qt, no
GTK, no GLib or GIO**, and nothing that brings them in.

## 1. Principles

### 1.1 Goals

| # | Principle | Here |
|---|---|---|
| F1 | **Names first, metadata later.** A directory's names cost one sequential read. Its metadata costs one `fstatat` per entry, which on a cold disk is a seek each. The view never waits for the second to show the first | §3.2, §3.3 |
| F2 | **Work scales with what is visible.** Rows, shaped text, icons, and thumbnails exist only for the rows on screen and a margin around them | §4.1 |
| F3 | **Descriptors, not path strings.** A location is a directory descriptor. Walks, copies, and deletes resolve one component at a time relative to a descriptor they already hold, with `O_NOFOLLOW` | §5.2 |
| F4 | **The kernel moves the bytes.** Copies use `copy_file_range(2)`, which keeps holes and lets file systems that can clone blocks (ZFS, M-Z) do so | §5.3 |
| F5 | **Contents are untrusted, names are untrusted.** A file's bytes are parsed only in sandboxed helpers. A file's *name* is bytes, displayed so that it cannot impersonate another (§4.3) | §4.3, §7 |
| F6 | **Nothing half-done is visible.** A copied file appears under its name only when it is complete, and a cancelled job leaves the destination as it found it | §5.3, §5.6 |
| F7 | **Fast to start.** The first window draws within one frame budget of the process's first instruction once warm, and a new window draws in under a frame | §8, §11.2 |

### 1.2 What AbyssBSD adds

| # | Principle | Consequence |
|---|---|---|
| A1 | **AbyssBSD-native** (DESKTOP.md P6) | kqueue `EVFILT_VNODE` for directory changes, `F_KINFO` for a descriptor's current path, `fstatfs` for volumes, `funlinkat` and `renameat2` for race-free deletes and no-clobber moves, `chflags` `UF_HIDDEN` alongside dotfiles, FreeBSD's extended attributes and NFSv4 ACLs in properties, the `storage` provider (DESKTOP.md §6.8) for removable media |
| A2 | **No foreign toolkit** | No Qt, GTK, GLib, GIO, gvfs, GdkPixbuf, shared-mime-info, udisks, Tracker, or D-Bus. The UI is `lib/scene-render` (DESKTOP.md §8.9); the foreign code is DESKTOP.md §2.1's inventory plus two base libraries (§2.4) |
| A3 | **One canonical way** (DESKTOP.md P4) | No tabs and no split panes: windows are the compositor's job, and a second window beside the first is one action (§6.3). One configuration tree: bookmarks, per-folder views, and file associations are config, not side files (§9) |
| A4 | **FreeBSD's tools are the behavioral reference** | What a copy preserves and how a move crosses file systems follow FreeBSD's `cp -Rp` and `mv(1)` (BSD-licensed, consultable), and the tests compare against them (§11.1) |
| A5 | **Measured** | A `desk.files` component under ADR-0014, against Thunar on sway on FreeBSD 15.1 (§11.2) |

### 1.3 Non-goals

- Tabs, split panes, and dual-pane modes (A3).
- A virtual file system layer (gvfs, KIO). `sftp://`, `smb://`, and similar locations are reached by
  mounting them (NFS, or `fusefs` with `sshfs` from packages), after which they are directories like
  any other. The file manager does not speak network protocols.
- A desktop with icons on it. The wallpaper is the compositor's.
- A search index or indexing daemon. Recursive name search is a parallel walk (§6.4). Content
  search is §13 question 6.
- Recent-files tracking. It is a record of what the user did, kept for nobody in particular (P5).
- PDF and office-document thumbnails in the first release (§13 question 3).
- Running on anything but AbyssBSD and FreeBSD (DESKTOP.md §1.3).

## 2. Context

### 2.1 Where it sits

`sable-files` is an ordinary Wayland application (DESKTOP.md §8.9): `xdg_toplevel` windows rendered
client-side with Vulkan by `lib/scene-render`. It is also a **provider**: it serves `fs.Browse`,
which the picker Facet uses (§7.3).

```
              keyboard, pointer, DnD (Wayland)                       frames (Vulkan WSI)
                           │                                                ▲
 ┌─────────────────────────▼────────────────────────────────────────────────┴──────────┐
 │ sable-files (the user's authority over files)                                       │
 │                                                                                     │
 │  UI thread ─ view model ─ virtual list ─ lib/scene-render ─────────▶ render thread  │
 │      │  ▲                                                                           │
 │      │  │ listings, diffs                                                           │
 │      ▼  │                                                                           │
 │  list pool: getdirentries ─▶ name arena ─▶ fstatat (inode order) ─▶ entry table     │
 │      ▲ EVFILT_VNODE on open directories                                             │
 │  job engine: walk ─▶ copy_file_range / renameat2 / funlinkat ─▶ journal             │
 │                                                                                     │
 │  fs.Browse server ◀──── Conduit ───── sable-picker (Facet, capability mode)          │
 └────┬───────────────────────────────────────────────────────────────▲────────────────┘
      │ one rights-limited fd per request                             │ RGBA in a sealed memfd,
      ▼                                                               │ text, type, entry lists
   sable-files-peek × N (capability mode): libpng, libjpeg-turbo, lib/svg, libavcodec,
                                           libarchive, libmagic, HarfBuzz
```

### 2.2 Crates

| Crate | Kind | Contents |
|---|---|---|
| `lib/fscore` | `no_std` + `alloc`, `#![forbid(unsafe_code)]` | The name arena, entry table, sort keys and natural comparison, listing diffs, filters, selection, the job planner and its conflict state machine, the trash-info and `text/uri-list` codecs, the thumbnail cache's key (MD5, in-house). Host-tested and fuzzed |
| `lib/mediatype` | `no_std`, a checked-in table | Media types by extension, their generic icon names, and their "kind" (image, video, text, archive, …). Written here from the IANA registry's type names, not derived from shared-mime-info, whose database is GPL |
| `desk/files-view` | `std`, `#![forbid(unsafe_code)]` | The browsing view as `Node` trees: the list, grid, and column views, the path bar, the sidebar. Shared by `sable-files` (client-side) and the picker (compositor-side, §7.3) |
| `desk/files` | `std` | `sable-files`: windows, listing pool, job engine, `fs.Browse`, configuration. Its `unsafe` is the FFI for `getdirentries` packing and `copy_file_range`, in one module |
| `desk/files-peek` | `std` | `sable-files-peek`: the sandboxed content helper. All FFI to the decoders lives here |
| `desk/facets/picker` | Facet | `sable-picker`: the file chooser other components and applications get through `fs.Picker` |

`lib/fscore` knows nothing of system calls, so listings, diffs, sorting, conflict handling, and job
plans all run in host tests over synthetic directories, and its planner is fuzzed with random trees
and random concurrent changes.

### 2.3 What GTK and Qt file managers lean on, and what replaces it

| Need | GTK and Qt stacks | Here |
|---|---|---|
| Widgets, layout, text | GTK, Qt Widgets | `lib/scene-render`, `lib/flexgrid`, HarfBuzz, `lib/raster` (DESKTOP.md §2.1) |
| File I/O and monitoring | GIO `GFile`, `GFileMonitor`, inotify, KDirWatch | System calls directly; kqueue `EVFILT_VNODE` (§3.5) |
| Remote locations | gvfs, KIO | Mounts (§1.3) |
| Media types | shared-mime-info (GPL data), QMimeDatabase | `lib/mediatype` and base's `libmagic` in the helper (§7.2) |
| Thumbnails | GdkPixbuf loaders, tumbler over D-Bus, KIO thumbnailers | `sable-files-peek` (§7.2) writing the freedesktop.org thumbnail cache |
| Volumes | udisks over D-Bus | The `storage` provider in `sable-sysd` (DESKTOP.md §6.8), `getfsstat(2)` |
| Trash | GIO's trash backend | The freedesktop.org Trash specification, implemented in `lib/fscore` and `desk/files` (§5.5) |
| "Show in folder" from other apps | `org.freedesktop.FileManager1` over D-Bus | The `files.show` action (§9); the D-Bus name only through DESKTOP.md §8.8's compatibility bridge |

### 2.4 Foreign code

DESKTOP.md §2.1's rule applies. `sable-files` itself links only base's `libc`. `sable-files-peek`
links the inventory's libpng, libjpeg-turbo, libavcodec, libavformat, and libavutil, HarfBuzz for
font previews, and two **base libraries** this document adds to DESKTOP.md §2.1's list:

- **`libarchive`**, FreeBSD's `tar(1)` library (BSD-2-Clause, its compressors from base), to list,
  extract, and create archives.
- **`libmagic`**, FreeBSD's `file(1)` library (BSD-2-Clause), to name the type of files with no
  known extension.

Both parse untrusted data, both are continuously fuzzed upstream, and both run only in the helper.
Video thumbnails need ffmpeg's *decoders*, where the inventory configured ffmpeg for encoding and
muxing (§13 question 4).

## 3. Listing

### 3.1 The entry model

A directory's state is three stores, all in `lib/fscore`:

| Store | Holds | Size |
|---|---|---|
| **Name arena** | Every name as bytes, back to back | The sum of the names |
| **Entry table** | One fixed-size `Entry` per name | 48 bytes an entry |
| **Order** | A permutation of entry indices in the current sort, and the filtered subset | 4 bytes an entry |

```rust
/// A name's place in the arena. Names are at most NAME_MAX (255) bytes.
#[derive(Clone, Copy)]
pub struct NameRef { off: u32, len: u8 }

pub struct Entry {
    name: NameRef,
    ino: Ino,               // d_fileno
    kind: Kind,             // from d_type, confirmed by stat
    meta: Meta,             // Pending, or the fields of the stat below
    flags: EntryFlags,      // HIDDEN, SYMLINK_BROKEN, NAME_NOT_UTF8, …
}

pub enum Meta {
    Pending,
    Known { size: u64, blocks: u64, mtime: Timespec, birthtime: Timespec, mode: Mode,
            uid: Uid, gid: Gid, fflags: FileFlags, nlink: u32 },
    Failed(Errno),          // EACCES on a directory without search permission, a raced unlink
}
```

A million-entry directory with typical names costs about 48 MiB of entries, 20 MiB of names, and
4 MiB of order: some 72 MiB, against several hundred for a toolkit that makes an object per file.
Names stay bytes. They are decoded to text only for display and matching (§4.3), and every operation
uses the original bytes, so a name that is not valid UTF-8 can be opened, renamed, copied, and
deleted.

### 3.2 Reading names

Opening a directory:

1. `openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`, or `open(path)` from the
   path bar, then `fstat` and `fstatfs` on the descriptor (its device, file system type, and mount
   point).
2. `getdirentries(2)` in **256 KiB reads** (FreeBSD's `readdir(3)` reads a page at a time),
   unpacking `struct dirent` records straight into the arena and the table. `.` and `..` are skipped.
3. Entries whose `d_type` is `DT_UNKNOWN` (some file systems) are marked for stat before they can
   show a kind-specific icon. UFS, tmpfs, and msdosfs supply `d_type`.
4. The names are sorted (§3.4) and the view shown.

If reading takes longer than one frame (a huge directory on a slow disk), the view appears with the
names read so far and later batches merge into place, with the scroll position anchored to the
first visible entry so nothing the user is looking at moves.

### 3.3 Reading metadata

Every entry needs an `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)` for its size, times, and mode,
and a symbolic link needs a second without the flag for its target's kind. The **list pool** does
these on `files.list.threads` workers (default: the core count, at most 8), from a queue with two
priorities:

- **Visible rows first.** When the user scrolls, the rows that come into view move to the front.
- **Then the rest in inode order.** On UFS, inodes live in cylinder-group tables, so stat'ing by
  ascending `d_fileno` reads those tables close to sequentially. On a rotating disk a cold
  directory of 100 000 files is seek-bound in name order and much less so in inode order. On flash
  the order costs nothing.

Rows whose metadata is still pending show their name and a generic icon, and fill in when it
arrives. Sorting by name needs no metadata. Sorting by size, date, or type puts pending entries
last in a stable block and re-sorts in batches as metadata arrives, at most every 100 ms, with the
same scroll anchoring as §3.2.

**Folder item counts** ("12 items") cost a directory read per subfolder. They are off by default
(`files.view.count-items`) and, when on, are computed for visible rows only.

### 3.4 Sorting

Sorting compares precomputed **sort keys**, so comparisons are `memcmp`:

| `files.sort.collation` | Key |
|---|---|
| `"natural"` (default) | Unicode simple case folding (from `lib/unicode`, TERMINAL.md §2.2), with each run of ASCII digits encoded as its length and then its digits, so `file2` sorts before `file10`. Invalid UTF-8 bytes keep their byte value |
| `"locale"` | libc's `strxfrm(3)` in the session's `LC_COLLATE` |
| `"bytes"` | The name itself, as `ls -f \| sort` would order it under `LC_ALL=C` |

Keys are built and sorted on the list pool in parallel (sorted runs per worker, then one merge).
Folders sort before files unless `files.sort.folders-first` is off. Changing the sort column
re-sorts the order array, never the entries.

### 3.5 Tracking changes

FreeBSD has no per-entry change notification, and this design does not propose one (it would be a
deviation with no FreeBSD program to use it). What kqueue offers is enough:

- Each open directory's descriptor is registered with `EVFILT_VNODE` for `NOTE_WRITE | NOTE_EXTEND |
  NOTE_LINK | NOTE_ATTRIB | NOTE_RENAME | NOTE_DELETE | NOTE_REVOKE`.
- **`NOTE_WRITE`, `NOTE_EXTEND`, `NOTE_LINK`** (an entry was added, removed, or renamed) start a
  **rescan** after 30 ms. While events keep coming, rescans happen at most every 250 ms, and for
  directories over 100 000 entries at most once a second, so a build churning its output directory
  costs a bounded share of one core. A rescan rereads the names, diffs them against the table by
  `(ino, name)` with a hash set in `lib/fscore`, and stats only what is new.
- **`NOTE_RENAME`** on the directory itself: its new path comes from `fcntl(F_KINFO)` and the title
  and path bar follow it.
- **`NOTE_DELETE`**, **`NOTE_REVOKE`** (removed, or its file system force-unmounted): the window moves
  to the nearest ancestor that still exists, and says why.
- **Children's attributes** (a download growing, a file touched) change no directory, so kqueue
  cannot report them without a descriptor per file. Instead, visible rows are re-stat'ed when the
  window gains focus and every `files.refresh` (default 2 s) while it is visible: at most a screen of
  `fstatat` calls.

The file manager's own operations update the table directly when they complete, so the user never
waits for a rescan to see their own change.

## 4. The view

### 4.1 The virtual list

DESKTOP.md §8.3's scene model builds a node per element, which is the right model for panels and
the wrong one for a million files. This document needs a **`List` node** in that model (§13
question 1): a container that knows its item count and row height (or a grid's cell size) and asks
the application for nodes only for the indices in its viewport plus a margin of one screen. Scrolling
recycles row nodes by key. The launcher's results and the settings GUI's long lists need the same
node.

On that node, each view builds rows from the entry table:

| View | Layout |
|---|---|
| **List** (default) | One row per entry with configurable columns: name, size, modified, type, and optionally permissions, owner, and birth time |
| **Grid** | Icons or thumbnails with names under them, wrapping to the window's width |
| **Columns** | A Miller-column view, one column per level of the current path, each a virtual list over its own directory |

Text is shaped once per row and cached by `(entry, column width bucket)`. A name too long for its
column is truncated in the middle, keeping its extension (`a-very-long-na…ume.tar.zst`). Icons come
from the icon theme through `lib/scene-render`'s `Icon` node, and a scroll frame makes no
allocations: rows, shaped runs, and icons are reused from caches whose size depends on the window.

### 4.2 Interaction

- **Keyboard first.** Arrow keys, Home and End, Page Up and Down, Enter to open, Backspace or
  Alt+Up for the parent, Alt+Left and Right for history, F2 to rename, Delete to trash. Typing
  starts the filter (§6.4). Every one of these is an action (§9), so all are rebindable.
- **Selection** is a set of entry indices with an anchor, so it survives re-sorting and rescans
  (an entry that disappears leaves the selection). Rubber-band selection works in the grid.
- **Drag and drop** offers `text/uri-list` through `wl_data_device`, to and from other windows and
  applications. Dropping onto a folder copies or moves by the usual modifier rules (same file
  system: move; otherwise copy; Shift forces move, Ctrl forces copy).
- **Clipboard.** Copy and Cut offer `text/uri-list` (and plain text paths). Paste into a folder
  starts a job. Cut is recorded in the offer so that a paste moves.
- **Opening** a file invokes its type's default application through `sabled`'s `apps.open`
  (§5.7). Opening an **executable** opens it with its type's application like any other file: a
  shell script opens in the editor. Running it is a separate action (`files.run`), in the context
  menu. A `.desktop` file outside the application directories is text, so a download cannot pose
  as a launcher.

### 4.3 Names are untrusted

A name is up to 255 arbitrary bytes other than `/` and NUL, chosen by whoever made the file. Display
rules, all in `lib/fscore`:

- **Invalid UTF-8** bytes are shown as `\xNN` in the `files.name.escape` style, and the entry is
  flagged so renames start from the original bytes.
- **Control characters** (C0, DEL, C1) are shown as escapes (`\n`, `\t`, `\x1b`), never interpreted.
- **Bidirectional controls** (U+202A–U+202E, U+2066–U+2069) are shown as visible marks, and each
  name is laid out as an isolated run. `invoice‮fdp.exe` cannot pass for a PDF.
- **Leading and trailing spaces**, and names made only of invisible characters, are shown with
  visible space marks.
- **Paths dropped into a terminal** are shell-quoted by the terminal's paste handling, and the
  `text/uri-list` offer percent-encodes, so no name reaches a shell as syntax.

## 5. Operations

### 5.1 Jobs

Every operation that touches more than one name, or might take more than a frame, is a **job** in
the job engine: copy, move, trash, restore, delete, extract, compress, link, and recursive
permission or flag changes. A job is a typestate:

```
Planned ──▶ Running ⇄ Paused
               │
               ├──▶ Waiting(Conflict | Error) ──▶ Running
               ▼
        Done | Cancelled | Failed
```

- **Walk and work overlap.** A job starts at once. Its walk runs ahead of the work and refines the
  totals (files, bytes) as it goes, so copying a million files does not wait for a million stats
  before the first byte moves.
- **Devices are queued, not thrashed.** Jobs whose sources or destinations share a device
  (`st_dev`) run one at a time by default. Jobs on different devices run in parallel. Within a job,
  large files are copied one at a time and small files by `files.jobs.small-file-threads` workers
  (default 4), since small-file copies are bound by metadata, not bandwidth.
- **Progress** (files, bytes, rate, time left) is shown in the window's job strip and, for a job
  still running after its window closes, in a notification through `notify.Post`.
- **Undo** reverses the last reversible job: a rename, move, trash, new folder, or copy (whose
  undo trashes the copies). The undo list lives in memory and ends with the process. Permanent
  deletion is not reversible and is the one action that asks first (`files.confirm.delete`, on by
  default, P5 lets the user turn it off).

### 5.2 Descriptor-relative walks

A walk holds a descriptor for every directory on its current path and, unlike `fts(3)`, never
changes directory or builds a path string to reach anything:

- Each child is opened with `openat(dirfd, name, O_NOFOLLOW | …)` and checked with `fstat` against
  the kind its directory entry claimed. A directory swapped for a symbolic link between the read
  and the open makes the open fail (`ELOOP`) instead of leading the walk somewhere else.
- **Symbolic links are copied as links**, not followed, as `cp -R` does by default (`-P`).
- Walks **stay on one file system** unless the job says otherwise (copy and delete cross mount points
  inside the source tree as `cp -R` and `rm -r` do; recursive search and size counts do not, §6.4).
- **Hard links** within a copied tree are copied as separate files, as `cp -R` does (§13 question 7).

### 5.3 Copying a file

1. Open the source with `O_RDONLY | O_NOFOLLOW` and `fstat` it.
2. Create **`.<name>.sable-part.<job>`** in the destination directory with `O_CREAT | O_EXCL |
   O_WRONLY`, mode 0600.
3. Move the data with **`copy_file_range(2)`** in calls of at most 64 MiB, so progress and
   cancellation stay responsive. FreeBSD's implementation keeps holes, and on ZFS with block cloning
   enabled it shares blocks instead of copying them (M-Z), with no special case here. A file system
   that returns `EINVAL` or `EXDEV` falls back to `read` and `write` through a 1 MiB buffer.
4. Apply metadata to the open descriptor, as `cp -p` does: `fchmod`, `futimens` (access and
   modification times), file flags with `fchflags` (user flags; system flags only as root), extended
   attributes (`extattr_list_fd`, `extattr_get_fd`, `extattr_set_fd`; the `user` namespace, and
   `system` only as root), and the ACL (`acl_get_fd_np` and `acl_set_fd_np`, NFSv4 or POSIX.1e as
   the source has). Ownership is copied only when `files.copy.preserve` includes `owner` and the
   process may set it. When it can't, set-user-ID and set-group-ID bits are cleared, as `cp -p` does.
   A destination file system that doesn't support a kind of metadata (`EOPNOTSUPP`, msdosfs and
   ACLs) is noted once per job, not failed.
5. **`renameat2(dst_dir, part, dst_dir, name, AT_RENAME_NOREPLACE)`**. The file appears under its
   name only now, complete. If the name was taken in the meantime, the conflict policy decides
   (§5.4).

Directories are created as `.<name>.sable-part.<job>` the same way when the destination name does
not exist, filled, given their metadata last (so a read-only source directory can still be filled),
and renamed into place. When the destination directory exists and the policy is to merge, the copy
goes into it file by file.

### 5.4 Conflicts

| Conflict | Choices | Default when the user has not chosen |
|---|---|---|
| File over file | Replace, Skip, Keep both (`name (2).ext`), Replace if newer | Ask |
| Directory over directory | Merge, Skip, Keep both | Ask |
| File over directory, or the reverse | Skip, Keep both | Ask |

Each answer can apply to the rest of the job. **Replace** is `renameat` of the finished part file
over the old name, so the old file is replaced atomically, never deleted first. There is no
"replace directory": removing a whole existing tree is a delete, and a delete is its own action.
A job can be started with a policy (`files.conflict "keep-both"`) so it never stops to ask.

### 5.5 Trash and delete

**Trash** follows the freedesktop.org Trash specification, so it is shared with other applications
the user runs:

- In the home file system, `$XDG_DATA_HOME/Trash/files/` and `info/<name>.trashinfo` (`Path=` and
  `DeletionDate=`). Elsewhere, at the volume's mount point (`f_mntonname` from `fstatfs`):
  `.Trash/<uid>/` if the administrator made a sticky `.Trash`, otherwise `.Trash-<uid>/`.
- Trashing is `renameat2(… AT_RENAME_NOREPLACE)` into the trash on the **same file system**,
  after the info file is created with `O_EXCL`, which reserves the name. It is instant whatever the
  size.
- A volume with no usable trash (read-only, or the user can't create one) is told plainly: the
  choices are delete, or copy to the home trash, which the specification allows and which can take
  a long time for a large file. The file manager does not pick copying silently.
- The trash view lists every trash directory on mounted volumes, restores to the original path
  (with the conflict rules of §5.4), and empties with a delete job.

**Delete** removes a tree bottom-up. Each entry is opened with `O_PATH | O_NOFOLLOW`, checked, and
removed with **`funlinkat(dirfd, name, fd, 0)`**, which unlinks only if the name still refers to
the file just checked. Directories go with `unlinkat(AT_REMOVEDIR)`. A file with an immutable or
append-only flag stops the job with the flag named, and offers to clear it when the flag is a user
flag (`uchg`, `uappnd`) the user may clear. `rm` does not offer that. A GUI that shows the flag
might as well.

### 5.6 Interrupted jobs

Each running job keeps a small **journal** in `$XDG_STATE_HOME/sable/files/jobs/<job>`: the
operation, the source and destination roots, and the part names it has created. When `sable-files`
starts and finds a journal from a process that is gone, it offers to resume the job (the walk skips
destinations already in place) or to clean up its part files. Part files are ordinary hidden files,
so nothing is lost if the user deletes the journal instead. A journal is state the job engine owns,
not configuration (DESKTOP.md §7.6).

### 5.7 Opening files

`apps.open` in `sabled` maps a media type to an application from the user's `apps.default` keys (§9)
or, failing that, from the `MimeType=` lines of the `.desktop` entries the `apps.Catalog` provider
already scans (DESKTOP.md §8.6). The file manager sends the path and its type. How a **jailed**
application sees a file outside its nullfs view is §13 question 5.

## 6. Places, volumes, search

### 6.1 The sidebar

- **Places** are the `files.places` list in config (§9): Home and the XDG user directories that
  exist (Documents, Downloads, Pictures, Music, Videos), and whatever the user pins.
  Pinning writes the list. There is no separate bookmarks file.
- **Volumes** are the mounted file systems from `getfsstat(2)`, minus `devfs`, `procfs`, `fdescfs`,
  and nullfs mounts under a jail's root, each with its free space (`fstatfs`).
- **Removable media** comes from the `storage` provider (DESKTOP.md §6.8) through a `storage.Removable`
  grant: unmounted partitions appear, and mount, unmount, and eject are its methods. The file
  manager never runs `mount` itself. An unmount that fails with `EBUSY` names the processes holding
  files there when the provider can tell (`fstat(1)`'s information, from `kern.file`).
- **Trash** (§5.5).

### 6.2 Hidden files

An entry is hidden when its name starts with `.` **or** it has FreeBSD's `UF_HIDDEN` flag
(`chflags hidden`), which exists for exactly this. `files.view.hidden` shows them, and Ctrl+H toggles
it for the window.

### 6.3 Windows

One `sable-files` process serves every window, as `sable-term` does (TERMINAL.md §7.3): windows
share the listing cache, the icon and thumbnail textures, and the Vulkan device, so a new window
draws at once. `files.window.beside` opens the selected folder in a new window and asks the compositor
to place it next to this one, which is the dual-pane workflow without a dual-pane mode (A3). Dragging
between the two is a copy or move job like any other.

### 6.4 Filtering and search

- **Filter.** Typing in a window filters its current directory: a subsequence match over sort keys,
  with matched characters highlighted, recomputed on each keystroke over the entry table. For
  100 000 entries that is well under a millisecond. Escape clears it.
- **Search.** Ctrl+F searches the tree below the current folder: a **parallel walk** on the list
  pool, work-stealing over directory descriptors, matching names by substring or `fnmatch(3)`
  pattern, streaming results into a results view (a virtual list like any other). It skips hidden
  entries unless they are shown, does not cross mount points or enter `.zfs`, and stops when the
  view closes. On a warm cache it visits several hundred thousand entries a second on the reference
  machines' NVMe disks, which is why there is no index (§1.3).

## 7. Processes and the sandbox

### 7.1 Who holds what

A file manager is the user's hand on their files: copying, moving, and deleting anything the user
may is its purpose. Putting it in capability mode with a descriptor for `/` and full rights would be
theater, the same theater DESKTOP.md §6.5 declines to perform. What *can* be contained is the part
that parses hostile data:

| Process | Holds | Sandbox | Parses |
|---|---|---|---|
| `sable-files` | The user's file authority, its Wayland connection, a render node, Conduit channels from `sabled` | None | Names, `struct stat`, trash-info files, `text/uri-list` offers: small, bounded formats in safe Rust, fuzzed |
| `sable-files-peek` × N | One request's descriptors, a reply memfd | Capability mode from its first request | File contents: images, video, fonts, text, archives, magic |
| `sable-picker` | A scene channel, an attenuated `fs.Browse` channel | Capability mode (a Facet) | Listings and thumbnails from `fs.Browse`, already decoded |

### 7.2 The content helper

`sable-files` starts `files.peek.helpers` helpers (default 2, more while thumbnailing a large
folder) with `pdfork`, each over a socket pair, and each enters capability mode before reading its
first request. A request carries:

- the file, opened by `sable-files` with `O_RDONLY | O_NOFOLLOW` and narrowed to `CAP_READ |
  CAP_SEEK | CAP_FSTAT | CAP_MMAP_R`;
- the operation: `thumbnail(size)`, `preview(kind, limits)`, `sniff`, `list-archive`;
- for an extraction, a destination directory descriptor with only the rights creation needs
  (`CAP_LOOKUP`, `CAP_CREATE`, `CAP_WRITE`, `CAP_MKDIRAT`, `CAP_SYMLINKAT`, `CAP_FCHMOD`,
  `CAP_FUTIMES`).

Replies (RGBA pixels, sanitized text, entry lists) come back in a sealed memfd, mapped read-only by
`sable-files`. Each request has a time and memory budget (`files.peek.budget`, default 2 s and
256 MiB with `setrlimit`). A helper that crashes or exceeds its budget is `pdkill`ed and replaced,
and the file is marked in the thumbnail cache's `fail/` directory so it is not retried until it
changes.

**Archives extract safely by construction.** In capability mode every lookup is relative to a held
descriptor and cannot climb above it, so an archive entry named `../../.profile`, an absolute path,
or a symbolic link pointing out of the destination fails with `ENOTCAPABLE` instead of writing
outside the folder the user chose. libarchive's own checks stay on as a second layer.

**Thumbnails** use the freedesktop.org thumbnail cache (`$XDG_CACHE_HOME/thumbnails/{normal,large,
x-large,xx-large}/<md5 of the URI>.png`, with `Thumb::URI` and `Thumb::MTime`), so they are shared
with other applications. Thumbnails in that cache were written by other programs, so they are
untrusted too: `sable-files` opens the cached PNG and sends it to a helper to decode, like any image.

| Kind | Decoded by |
|---|---|
| PNG, JPEG | libpng, libjpeg-turbo |
| SVG | `lib/svg` (the icon subset; richer SVGs get a generic icon) |
| Video | libavformat and libavcodec: one frame at 10% of the duration (§13 question 4) |
| Fonts | HarfBuzz and `lib/raster`: a sample line |
| Text | The first 64 KiB, decoded as UTF-8 with the rules of §4.3, for previews only |
| Archives | libarchive: the entry list, for previews and browsing into the archive read-only |

### 7.3 The picker

DESKTOP.md §8.4 has sandboxed components ask an `fs.Picker` provider for files: it shows a chooser
and returns one rights-limited descriptor. This document supplies both halves:

- **`fs.Browse`** is served by `sable-files`. `sabled` starts it windowless on the first picker
  request if it isn't running. A browse channel lists directories by opaque handles (never paths
  the peer can forge), returns decoded thumbnails, and on confirmation opens the chosen file or
  creates the chosen name with the rights the *requesting* component's grant allows, and sends that
  descriptor to the requester through the broker.
- **`sable-picker`** is a Facet: the chooser is drawn by the compositor from `desk/files-view`'s
  `Node` trees, the same code as `sable-files`'s windows. Because the compositor draws it, a
  requesting application cannot draw over it, read it, or forge its buttons. Its channel is
  attenuated (DESKTOP.md §6.4): it can list and confirm a choice, and it cannot rename, delete, or
  open anything for itself.

Native applications on `lib/scene-render` use `fs.Picker` for Open and Save dialogs, and a
capability-mode application gets exactly the files the user picked. GTK and Qt applications from
packages keep their own dialogs (compatibility, DESKTOP.md §7.6).

## 8. Startup

The process is small and starts fast because it does little before its first frame:

1. Connect to Wayland, create the Vulkan device, load the pipeline cache (DESKTOP.md §4.7.2).
2. Open the starting folder, read its names (§3.2), and draw. Metadata and icons fill in after.
3. Start the list pool, the helpers, and the `fs.Browse` server.

Icons for the common types are rasterized into the atlas at first use and kept for the process's
life. The `files.start` benchmark (§11.2) holds the first window to one frame budget once the
binary and the icon theme are in the page cache.

## 9. Configuration

Configuration is the `files` subtree of the one tree (DESKTOP.md §7), plus `apps.default` for
associations, which belong to `apps.Catalog` because the launcher and every other component use the
same ones. Per-folder view settings are **rules in config**, not hidden per-directory state: "Remember
this view for this folder" writes a `view` node.

```kdl
files {
    view {
        mode "list"                         // "list" | "grid" | "columns"
        columns "name" "size" "modified" "type"
        hidden #false
        count-items #false
        thumbnails "local"                  // "all" | "local" (not on network mounts) | "off"
    }
    view "~/Pictures" { mode "grid" ; sort "modified" order="descending" }
    view "~/src/**"   { hidden #true }

    sort "name" order="ascending" collation="natural" folders-first=#true
    places "~" "~/Documents" "~/Downloads" "~/Pictures" "~/src/AbyssBSD-Rust"

    copy preserve="mode" "times" "flags" "extattr" "acl"
    conflict "ask"
    confirm delete=#true
    refresh (s)2

    list threads=8
    jobs small-file-threads=4
    peek helpers=2 budget=(s)2 memory=(MiB)256
}

apps {
    default "image/*"          "org.example.Viewer"
    default "text/plain"       "org.example.Editor"
    default "inode/directory"  "sable-files"
}

bind {
    key "Super+E"        action="apps.launch" app="sable-files"
    key "Ctrl+H"         action="files.toggle-hidden" app="sable-files"
    key "Ctrl+L"         action="files.location"      app="sable-files"
    key "Ctrl+Shift+N"   action="files.new-folder"    app="sable-files"
    key "Shift+Delete"   action="files.delete"        app="sable-files"
    key "Ctrl+Alt+T"     action="files.terminal-here" app="sable-files"
}
```

- The schema is `#[derive(Schema)]` in `desk/files`, so the settings GUI, the manual page, and the
  language server come from it (DESKTOP.md §7.4). `view` rules match by path glob, the most
  specific pattern wins, and two rules with the same pattern are an error in one layer, as for any
  key (DESKTOP.md §7.3).
- Exported actions: `files.open`, `files.run`, `files.rename`, `files.copy`, `files.cut`,
  `files.paste`, `files.trash`, `files.delete`, `files.undo`, `files.new-folder`, `files.location`,
  `files.up`, `files.back`, `files.forward`, `files.toggle-hidden`, `files.search`, `files.window.new`,
  `files.window.beside`, `files.terminal-here` (launches `sable-term` with the folder as its working
  directory), and **`files.show path=<p>`**, which other components invoke to reveal a file.
  Application-scoped bindings carry `app=`, as TERMINAL.md §12 question 6 assumes.
- Colors, spacing, and row styles are theme tokens and styles (`files.row`, `files.row:selected`,
  `files.name.escape`), so a rice themes the file manager with everything else.
- **Reaching `sabled`**: config, theme, `apps.open`, `notify.Post`, and `storage.Removable` come
  over channels granted at launch. A `sable-files` started from a terminal uses defaults, opens
  files only through `files.run`, and shows no removable media.

## 10. Kernel and platform requirements

| Need | Used for | Status |
|---|---|---|
| `getdirentries`, `fstatat`, `openat` with `O_DIRECTORY`, `O_NOFOLLOW`, `O_PATH`, `O_RESOLVE_BENEATH` | Listing, walks | P5, done |
| `fcntl(F_KINFO)` | A directory's current path after a rename | P5, done |
| `copy_file_range`, `funlinkat`, `chflagsat`/`fchflags`, `utimensat`/`futimens`, `getfsstat`, `fstatfs` | Copies, deletes, metadata, volumes | P5, done |
| **`renameat2`** with `AT_RENAME_NOREPLACE` | No-clobber renames, part files into place, trash | **Missing**. FreeBSD 15.1 ABI (syscall 602 in 15.1's table), not yet in `kernel/src/syscall` |
| kqueue `EVFILT_VNODE` on directories (`NOTE_WRITE`, `LINK`, `RENAME`, `DELETE`, `REVOKE`) | Change tracking | P6.4, done; the directory notes to check against the tests in §11.1 |
| Extended attributes and ACLs (`extattr_*_fd`, `__acl_*_fd`) | Copies, properties | P6.7, planned (MULTIUSER.md §10.7). `EOPNOTSUPP` until then, which §5.3 tolerates |
| Capsicum: capability mode, `cap_rights_limit`, process-descriptor rights | Helpers, picker | P6.7, planned |
| `pdfork`, `pdkill`, `SOCK_SEQPACKET`, `SCM_RIGHTS`, `memfd_create` with seals | Helpers | Done (P4.5, P6.3, P6.5) |
| `storage` provider, devd | Removable media | DESKTOP.md §6.8, M-D.6 |
| ZFS block cloning through `copy_file_range` | Instant copies | M-Z; nothing here changes |

## 11. Testing and performance

### 11.1 Testing

- **Host, `lib/fscore`**: sort-key and natural-order tests (including invalid UTF-8 and digit runs
  past 64 bits), listing diffs, filter matching, trash-info and `text/uri-list` round trips, and the
  name display rules of §4.3 against a corpus of hostile names.
- **The planner** is fuzzed against a simulated file system that changes during the job (entries
  removed, replaced by symbolic links, renamed, made immutable). Invariants: no write outside the
  destination, no delete outside the source, no visible partial file, and every conflict reaches a
  decision.
- **Differential**: copies and moves of generated trees (sparse files, symbolic links, special
  permissions, file flags, extended attributes, ACLs, names with every byte value) on UFS2 and
  tmpfs, compared with what `cp -Rp` and `mv` produce on FreeBSD 15.1 on `abyss-build`, by `mtree -c`
  of both results. Intentional differences (part files, conflict handling) are listed in the test.
- **Change tracking**: a test program creates, renames, and deletes entries while a headless
  `sable-files` watches, and checks the table converges to `ls -f`'s view within the rescan bound.
- **Helpers**: each decoder path fuzzed through the helper's request decoder; archive extraction
  fuzzed with traversal, absolute-path, and link-escape entries, checking nothing appears outside
  the destination.
- **Headless**: `sable-files` and `sable-picker` against the compositor's headless backend on
  lavapipe (DESKTOP.md §10.1), pixel-compared: list, grid, and column views, hostile names, scale
  1, 1.25, and 2.

### 11.2 Performance

The file manager is a component under ADR-0014: **`desk.files`**, measured against **Thunar on sway
on FreeBSD 15.1 with `drm-kmod`, on the same machine**. Thunar is the baseline because it is the
most widely used lightweight file manager in FreeBSD's packages, and it runs natively on Wayland.
The gate is the geometric mean no more than 2% worse. The aim is to be several times faster at
opening large directories and never slower at moving data than `cp`.

| Benchmark | Measures |
|---|---|
| `files.open` | Navigation to first frame with names, for directories of 100, 10 000, and 1 000 000 entries, warm and cold cache |
| `files.stat` | Navigation to every visible row's metadata shown, same directories |
| `files.sort` | Re-sort of the 1 000 000-entry directory by size |
| `files.scroll` | p99 frame time while flinging through the 1 000 000-entry directory |
| `files.thumbs` | Time to thumbnail 1 000 JPEGs in a grid view, cold cache |
| `files.copy` | One 10 GiB file, and a tree of 100 000 small files, same disk and across disks |
| `files.delete` | Deleting the 100 000-file tree, permanently and to the trash |
| `files.watch` | Latency from another process's `creat` to the entry on screen |
| `files.start` | Launch to first frame, for the first window and for another window |
| `files.rss` | Resident memory with the 1 000 000-entry directory open |

**Diagnostics**, not gated:

- **`files.fs-ratio`**: the time to list and stat a directory, over the time a bare program doing
  only `getdirentries` and `fstatat` takes on it. Above 0.9 means the file manager costs less than
  a tenth of what the file system does.
- **`files.cp-ratio`**: `files.copy` against `cp -Rp` of the same tree on the same kernel.
- **`sable-files` on FreeBSD 15.1**, against Thunar on the same kernel and compositor, isolating the
  file manager from AbyssBSD's VFS.

## 12. Relationship to other documents

| Document | Here |
|---|---|
| DESKTOP.md §8.9 (applications, the file picker) | This document |
| DESKTOP.md §8.4 (`fs.Picker`) | §7.3 |
| DESKTOP.md §8.3 (the scene model) | §4.1 asks for a `List` node |
| DESKTOP.md §2.1 (dependencies) | §2.4 adds base's `libarchive` and `libmagic`, and ffmpeg's decoders |
| DESKTOP.md §6.8 (`storage` provider) | §6.1 |
| TERMINAL.md §7.3 (one process, several windows) | §6.3 |
| MULTIUSER.md §10.7 (ACLs and extended attributes) | §5.3, §10 |
| ROADMAP M-Z (OpenZFS) | §5.3: block cloning through `copy_file_range` |

## 13. Open questions

1. **A `List` node in the scene model** (§4.1). Proposed: add it to DESKTOP.md §8.3, with fixed and
   measured row heights and a grid mode, for this, the launcher, and settings. **Settled** by
   [TOOLKIT.md](TOOLKIT.md) §4.2 and DESKTOP.md §8.3, which do so.
2. **The picker in M-D.3.** DESKTOP.md's M-D.3 has the settings GUI and Facets that may need files
   (wallpapers, rice archives). Proposed: `fs.Browse` and `sable-picker` (F.3) move ahead of the
   rest of `sable-files`, into M-D.3.
3. **PDF and document thumbnails.** Poppler is GPL and MuPDF AGPL, both excluded. PDFium is
   BSD-licensed but vendors a deep tree (FreeType, OpenJPEG, Little CMS, …) that fails DESKTOP.md
   §2.1's shallow-closure rule. Proposed: none in the first release; a `lib/pdf` subset that renders
   the first page's embedded thumbnail or image is a possible later step.
4. **ffmpeg's decoders.** Video thumbnails need ffmpeg's native decoders in the sandboxed helper,
   where DESKTOP.md §12 question 4 configured it for encoding. Proposed: enable the native decoders
   for common formats in the same LGPL build, used only by `sable-files-peek`.
5. **Opening files in jailed applications.** A jail's nullfs view is fixed when the jail starts
   (DESKTOP.md §6.7). Pass a descriptor and a `/dev/fd` path, mount the file's directory into the
   view on demand, or copy into the jail's home. Proposed: a descriptor where the application
   accepts one, otherwise a read-only nullfs mount of the file's directory, both decided by
   `sabled`'s launcher, not the file manager.
6. **Content search.** GUI-first (DESKTOP.md P1) argues for searching inside files. A parallel `grep`
   over the walk in the helpers is simple, and without an index it is as fast as the disk. Proposed:
   after the first release.
7. **Hard links in copied trees.** `cp -R` copies each link separately, and `tar` keeps them. A file
   manager user copying a backup tree would expect `tar`'s behavior. Proposed: follow `cp`, and add
   `files.copy.preserve "hardlinks"` (off by default).
8. **Network mounts and thumbnails.** On an NFS or FUSE mount, stat and thumbnails are slow and cost
   the server. Proposed: `thumbnails "local"` by default (§9), and a lower stat concurrency for
   network file systems (from `f_fstypename`).

## 14. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **F.1** | `lib/fscore` (arena, table, sort keys, diffs, filters, planner, codecs) and `lib/mediatype` | Nothing (host only) | Host tests pass, and the planner fuzzer runs clean for 24 hours against the changing-tree simulator |
| **F.2** | `sable-files`: listing pool, change tracking, list and grid views on a `List` node, navigation, rename, new folder, copy, move, trash, delete, the job engine and journal | M-D.2, M-D.3 (toolkit, text), `renameat2` | The differential tests match `cp -Rp` and `mv` on UFS2 and tmpfs in QEMU, and `files.open` and `files.scroll` hold their bounds on lavapipe |
| **F.3** | `sable-files-peek` (thumbnails, previews, archives), `fs.Browse`, `sable-picker` | P6.7 Capsicum | A capability-mode test Facet opens a file through `sable-picker` and gets only that file; the helper fuzzers run clean |
| **F.4** | Column view, search, the trash view across volumes, removable media through `storage`, properties (flags, extended attributes, ACLs), undo | M-D.6 (`storage`), P6.7 ACLs | `desk.files` within the gate against Thunar, on QEMU's `virtio-gpu` first and then the reference machines |
