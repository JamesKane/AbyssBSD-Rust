# VFS and file systems

The design for Phase 5 ([ROADMAP.md](ROADMAP.md#phase-5-vfs-and-file-systems-l)): vnodes that
change, the name cache, mounting, the page and buffer caches, write-back, and the file systems
(tmpfs, devfs, UFS2, msdosfs). [ADR-0013](adr/0013-vfs.md) records the decision.

The P4 VFS (`sys/vfs`) serves file systems built once and never changed: the memory disk and devfs.
Its vnodes are immutable `Arc<Vnode>`s with a typed `VnodeKind`, and lookup walks fixed trees. P5
keeps the parts that worked (typed kinds, `Uio`, `namei`'s mount crossing and symlink handling,
FreeBSD's `stat`/`statfs`/`dirent` layouts) and replaces the rest.

Behavior reference: sys/kern/vfs_*.c, sys/ufs/ffs, sys/ufs/ufs, sys/fs/tmpfs, sys/fs/devfs,
sys/fs/msdosfs, sys/vm/vnode_pager.c. As everywhere, FreeBSD is the specification of what userland
sees, not the source of the code ([ADR-0007](adr/0007-license-and-clean-implementation.md)).

## 1. Goals and non-goals

### 1.1 Goals

- **FreeBSD's userland view.** `stat`, `statfs`, `getfsstat`, `getdirentries` (cookies and
  `d_off`), `nmount` options, `errno` choices, timestamps, link counts, `st_blocks`, and
  `st_blksize` as FreeBSD reports them. `mount(8)`, `df(1)`, `ls(1)`, `find(1)`, `tar(1)`, and
  `fsck_ffs(8)` run unmodified and agree with FreeBSD.
- **On-disk compatibility.** A UFS2 or FAT image written by AbyssBSD passes FreeBSD's `fsck_ffs -f`
  or `fsck_msdosfs` with no errors, and FreeBSD mounts it. AbyssBSD mounts images FreeBSD wrote.
- **One cache for file data.** A file's pages live in its vnode's VM object. `read`, `write`, and
  `mmap` share them, so nothing is cached twice and a mapping sees writes at once (FreeBSD's VMIO).
- **Crash behavior FreeBSD users expect**: without soft updates, `fsck` repairs; with them, only
  leaked blocks and inodes are left (background `fsck` territory).
- **Performance parity** ([ADR-0011](adr/0011-performance-parity.md)): the P5 suite within 2% of
  FreeBSD. The name-cache hit path, sequential I/O, and `fsync` get designed for it from the start.
- **File-system logic testable on the host.** On-disk formats and the engines that read and write
  them live in `lib/` crates over a block-device trait, tested against images FreeBSD's tools make
  and check.

### 1.2 Non-goals for Phase 5

- SU+J (Phase 11), quotas, ACLs and extended attributes (Phase 6, with credentials), snapshots, UFS1
  writing, ZFS, NFS, FUSE, nullfs/unionfs.
- Swap and a full pageout daemon (Phase 6+). P5 reclaims clean cached pages under pressure and
  throttles writers. It does not page anonymous memory out.
- Credentials beyond root. The permission checks are written now and run with the process's
  credential (always root until Phase 6), so the paths exist and are tested.

## 2. Principles

1. **Lifetimes in the types.** Memory references (`Arc<Vnode>`) and use references (open files,
   working directories, mappings) are different things. A use reference is a guard type. Dropping
   the last one runs the file system's `inactive`, which frees an unlinked file.
2. **The generic layer owns the protocol.** Locking order, name-cache maintenance, `..` handling,
   mount crossing, `O_EXCL`, permission checks, and timestamps are done once, in `sys/vfs`. A file
   system implements operations on nodes it already holds locked, as FreeBSD's VOPs do. It does not
   reimplement lookup rules.
3. **No surprise blocking.** The page cache is filled and written through pagers that may sleep.
   Nothing holding a spinlock calls into a file system.
4. **Typed states.** A vnode is `Live` or `Doomed` (forcibly unmounted or revoked). A buffer is
   `Clean`, `Dirty`, or `Busy`. A mount is `Mounting`, `Active`, or `Unmounting`. These are enums,
   not flag words.
5. **No locked instruction on a hot path that FreeBSD does not take.** On the gating CPU each one
   costs about 9 ns (P4.7). Lookups that hit the name cache take one reference, on the result.

## 3. Crates

| Crate | Contents | Tested |
|---|---|---|
| `sys/vfs` | Vnodes, the vnode cache, the name cache, `namei`, mounts, the node traits, advisory locks, generic `read`/`write` over the page cache | host (an in-memory test file system), kernel |
| `sys/bufcache` | The metadata buffer cache over `sys/storage`, write-back worklist, dirty throttling | host (a simulated disk), kernel |
| `sys/storage` | `Bio`, disks, GPT partitions, FreeBSD names (exists since P3) | host, kernel |
| `lib/ufs` | UFS2 on-disk layout, and an engine over a `BlockIo` trait: superblock, cylinder groups, inodes, block maps, allocation, directories, truncation, then soft updates | host, against `makefs`/`newfs` images and `fsck_ffs` on the build server |
| `lib/fat` | FAT12/16/32 and VFAT long names, same structure | host, against `newfs_msdos` images and `fsck_msdosfs` |
| `lib/cd9660` | ISO 9660 with Rock Ridge and Joliet, read-only, same structure (§10.7) | host, against hand-made records, `makefs -t cd9660` and `bsdtar --format iso9660` images, and FreeBSD's `ls -aliRnT` of the test volume |
| `sys/vfs/src/nullfs.rs` | nullfs, the loopback layer (§10.5): nothing kernel-specific, so beside tmpfs | host, kernel, user tests |
| `kernel/src/fs/{tmpfs,devfs,ufs,msdosfs,fdescfs,cd9660}` | Glue: the node traits over the engines, pagers, mount options | kernel, user tests |

The loader reads the kernel from UFS2 with `lib/ufs`'s read side (no_std, no allocation in the
block path).

## 4. Vnodes

### 4.1 Identity and the vnode cache

A vnode is the one in-memory object for an inode while anything refers to it. Each mount keeps a
table from inode number to `Weak<Vnode>`. `get(ino)` returns the existing vnode or asks the file
system to load the inode (`vget`), so two lookups of the same file always meet at one vnode, and
one lock and one page cache.

Unused vnodes are kept for reuse (FreeBSD's free list). A vnode with no use references stays alive
while the name cache holds it, or while it sits on a global LRU of recently used vnodes. When the
count of vnodes passes `kern.maxvnodes` (sized as FreeBSD sizes `desiredvnodes`), a reclaimer
(`vnlru`) takes vnodes from the cold end. It purges their name-cache entries, writes back what is
dirty, and drops them, which runs the file system's `reclaim`.

### 4.2 References

- `Arc<Vnode>`: memory. Holding one keeps the vnode valid. It does not keep the file open.
- `VnodeUse`: a use reference (FreeBSD's `v_usecount`), held by open files, working and root
  directories, executables, and mappings. Dropping the last one calls `inactive` with the vnode
  locked. If the link count is 0, the file system frees the inode and its blocks.

### 4.3 States and revocation

The node's file-system data sits behind the vnode lock in a `State` enum: `Live(fs data)` or
`Doomed`. Forced unmount and `revoke(2)` doom vnodes. Operations on a doomed vnode fail as
FreeBSD's dead vnode operations do (`EBADF` for I/O, `ENOENT` for lookup), and its pages are freed.
The kind (`Regular`, `Directory`, `Symlink`, `CharDevice`, `Fifo`, `Socket`) never changes, so code
still cannot ask a file for directory entries.

### 4.4 Attributes

Attributes (`mode`, `uid`, `gid`, `nlink`, size, the four timestamps, flags, generation, and blocks)
are read and written through `getattr`/`setattr`. The generic layer turns `chmod`, `chown`,
`utimensat`, `truncate`, and `chflags` into `setattr` after the permission checks. Timestamps
follow FreeBSD: `vfs.timestamp_precision` defaults to 2 (nanoseconds from the cached clock), and
which operations touch which times matches FreeBSD's UFS.

## 5. Locking

| Lock | Kind | Protects | Order |
|---|---|---|---|
| Per-mount rename lock | sleepable, exclusive | renames that move between directories | first |
| Vnode lock | `SxLock` (shared/exclusive, sleepable) | the node's state and contents | parent before child; two unrelated vnodes by address |
| Per-mount vnode table | spinlock | the inode table | leaf |
| Name cache buckets | see §6 | entries | leaf |
| Buffer | busy flag (sleep on it) | one buffer's contents during I/O | after vnode locks |
| VM object | spinlock (exists) | resident pages | leaf |

- Lookup takes each directory shared. It locks the child after dropping the parent, except for the
  last component of an operation that modifies the parent (create, remove, rename), which keeps the
  parent locked exclusive.
- `..` never locks child then parent: the walk drops the child, then locks the parent, and checks
  that the parent is still live (FreeBSD's `vn_lock` of `..` with `LK_RETRY`).
- A rename within one directory takes only that directory. A rename across directories first takes
  the mount's rename lock, then both directories in address order, and checks that the source is
  not an ancestor of the target. This avoids FreeBSD's retry loop at the cost of serializing
  cross-directory renames per mount, which no P5 benchmark measures.
- The witness gets a class per lock, with levels in the order above.

## 6. Name lookup

### 6.1 The name cache

A hash table keyed by (directory `NodeId`, name) maps to a positive entry (`Arc<Vnode>`) or a
negative one. Every directory also records the entry that names it (its parent and name), which
`getcwd` and `..` use.

- Entries are added by the generic layer after a file system's `lookup`, `create`, `mkdir`,
  `symlink`, and `link`. They are removed on `remove`, `rmdir`, and `rename`, and purged for a
  vnode when it is reclaimed, and for a whole mount on unmount. File systems never touch the cache.
- Negative entries are capped at a share of the cache (FreeBSD's `ncnegfactor` 5), evicted
  oldest-first, and promoted when hit again. A create in a directory removes the negative entry for
  that name before the new vnode is entered.
- A file system can opt out (`MNTK_NOCACHE`-like) when names are not stable.
- Size: `kern.maxvnodes` entries, as FreeBSD's `ncsize`.

### 6.2 The fast path

FreeBSD looks paths up without locks (`cache_fplookup`), and the P5 suite's `stat`/`open` measure
it. The design keeps that path open:

1. **First (P5.1):** each bucket has a spinlock. A path walk holds no vnode locks while every
   component hits, and takes a reference per component.
2. **For parity (P5.9, if measurement asks):** a lockless walk under one read-side section
   covering the whole path. The section is a per-CPU epoch that costs one barrier on entry, not
   per component. Entries are freed after a grace period. The walk takes one reference, on the
   final vnode, and falls back to the locked walk on any miss, `..` across a mount, or a symlink.
   Each directory carries a sequence count that a modification bumps, so the walk detects a
   concurrent change.

### 6.3 `namei`

The P4 `namei` (mount crossing, `..` at mount roots, symlinks with `MAXSYMLINKS`,
`O_RESOLVE_BENEATH`, `path_of`) stays, rebased on the cache. Lookup modes are explicit types:
`Lookup`, `Create`, `Delete`, `Rename`, each returning what the operation needs. For example,
`Create` returns the locked parent and either the existing vnode or a slot for the new name.

## 7. Operations

A file system implements the operations each kind supports, on locked vnodes:

```text
FileSystem: root, statfs, sync(wait), vget(ino), unmount(force), set_options
Node (all): getattr, setattr, inactive, reclaim, access_extra (flags such as schg)
Directory:  lookup, create, mkdir, mknod, symlink, link, remove, rmdir, rename, readdir(cookie)
Regular:    fill(pages), write_back(pages), prepare_write(range), truncate(size), bmap (for clustering)
Symlink:    readlink
```

The generic layer supplies:
- `open` (flags, `O_TRUNC`, `O_EXCL`, `O_CREAT`).
- `read` and `write` through the page cache.
- `getdirentries`, with FreeBSD's `dirent` packing and seek cookies.
- `fsync`, `fdatasync`, and `posix_fallocate`.
- Advisory locks.

FIFOs and sockets in the file system get vnodes whose I/O goes to the pipe and socket code
(sockets come with Phase 6's UNIX domain sockets).

## 8. Caches

### 8.1 File data: the page cache

Each regular-file vnode owns a VM object whose pager is the vnode pager. The object already
exists for `mmap` of memory-disk files since P4; it gains write-back.

- **read**: look up the pages covering the range in the object. Missing ones are filled by the
  file system's `fill`, which turns a run of pages into block reads (and read-ahead, §8.3). The data
  is copied out.
- **write**: `prepare_write` allocates blocks for the range (so `ENOSPC` is reported by `write`, as
  FreeBSD's `ffs_write` does). The data is copied into pages, a page partly written is filled first
  if it was not resident, and the pages are marked dirty. The file size grows as data lands.
- **mmap**: shared mappings map the object's pages. A clean page is mapped read-only, and the first
  write faults and marks it dirty, so dirty tracking works on both architectures without reading
  hardware dirty bits. `msync` and `munmap` hand dirty pages to write-back.
- **Dirty pages** are written by `write_back`, which clusters contiguous dirty pages into large
  writes.
- **Truncation** frees pages past the new end and zeroes the tail of the last page.
- **Pressure**: clean pages of vnodes with no mappings are reclaimed, oldest objects first. The page
  daemon does this from `v_free_target` down, and an allocation at `v_free_reserved` does it
  itself (ARCHITECTURE.md §5.2). Dirty data is throttled (§9.3).

### 8.2 Metadata: the buffer cache

Superblocks, cylinder groups, inode blocks, indirect blocks, FAT sectors, and directory blocks go
through `sys/bufcache`. It holds buffers keyed by (disk, block number, size), each with a typed
state:
- `Clean` (valid, matches the disk).
- `Dirty` (delayed write, with the time it was dirtied).
- `Busy` (owned by one thread, or in flight).

Operations mirror FreeBSD's: `bread`, `getblk` (no read when the block will be overwritten),
`bwrite` (synchronous), `bawrite` (asynchronous), `bdwrite` (delayed), and `brelse`. Soft updates
attach dependency lists to buffers (§10.4).

Directory blocks of UFS are file data, and FreeBSD reads them through the directory vnode's
buffers. AbyssBSD does the same through the buffer cache keyed by the directory's physical
blocks. Directories then get the same write ordering as other metadata, which soft updates needs.

### 8.3 Clustering and read-ahead

Sequential reads are detected per open file (FreeBSD's `f_seqcount`). The page cache then reads
ahead in clusters up to `MAXPHYS` (1 MiB), contiguous on disk as `bmap` reports. Write-back joins
contiguous dirty pages the same way. This is what sequential UFS2 throughput depends on.

## 9. Write-back

### 9.1 The syncer

One kernel thread wakes once a second and walks a wheel of `syncer_maxdelay` slots, as FreeBSD's
syncer does. Dirtied vnodes and buffers are queued `kern.filedelay` (30), `kern.dirdelay` (29), or
`kern.metadelay` (28) seconds ahead. It writes what comes due asynchronously, and every 30 seconds
it syncs each mount's superblock and summary information.

### 9.2 `fsync` and friends

`fsync` writes the vnode's dirty pages and its metadata (inode, indirect blocks, and the directory
entries it depends on, with soft updates), waits for them, then sends a cache flush (`BIO_FLUSH`)
to the disk, as FreeBSD does by default. `fdatasync` skips metadata that only timestamps changed.
`sync(2)` starts writes on every mount and returns. `unmount` and `reboot` write everything, wait,
and flush.

### 9.3 Throttling

The total of dirty pages and dirty buffers has high and low watermarks (FreeBSD's `hidirtybuffers`
and `lodirtybuffers`, and the vnode-pager equivalents). A writer that crosses the high one starts
write-back and sleeps until the total is below the low one. This keeps a large write from filling
memory with dirty data.

## 10. File systems

### 10.1 devfs

devfs becomes dynamic. Nodes appear and disappear as devices are published and withdrawn: disks
and partitions (`nda0`, `nda0p1`, `ada0`, `vtbd0`), the console and terminals, `null`, `zero`,
`random`, `urandom`, `devctl`, and `pci`. Names, modes, and owners match FreeBSD. A devfs instance
mounts at `/dev` at boot, and more can be mounted (jails come later). Opening a disk node gives raw
block access (`read`, `write`, and `DIOCG*` ioctls), which `fsck_ffs` and `newfs` use.

### 10.2 tmpfs

The first writable file system, and the one that proves the VFS. File data lives in anonymous VM
objects, which become swap-backed in Phase 6. Directories are in-memory sorted maps with FreeBSD's
cookie scheme. Mount options are `size`, `inodes`, and `mode`. `/tmp` is a tmpfs mount, as on a
default FreeBSD install with `tmpmfs`. Its `statfs` follows FreeBSD's tmpfs.

### 10.3 UFS2

`lib/ufs` holds the format and an engine over `BlockIo`. A `BlockIo` read hands a closure the bytes
at a sector-aligned offset; write and flush come with stage 2. Each call takes the implementation's
context type: `Sleepable` in the kernel, `()` on the host and in the loader. It is built in three
stages, each validated on the build server.

1. **Read**:
   - Superblock and its checks (magic, sizes, `fs_clean`).
   - Inodes, direct and indirect block maps.
   - Directories, symlinks (short and long), and holes.
   - `statfs` from the summary.
   - The superblock check-hash that FreeBSD verifies (`fs_metackhash`).
2. **Write without soft updates**:
   - Allocation with FreeBSD's policies (cylinder-group preference, `dirpref`, fragment
     reallocation, `blkpref`), so layout and performance resemble FreeBSD's.
   - Block allocation (`balloc`), truncation, and directory entry insert, remove, and compaction.
   - Inode allocation and freeing, and link counts.
   - The cylinder-group and superblock check-hashes.
   - Metadata ordering as FreeBSD's UFS does without soft updates: synchronous writes where order
     matters, everything else delayed. `fs_clean` is cleared on mounting read-write and set on a
     clean unmount.
3. **Soft updates** (P5.7), in `lib/ufs/src/softdep.rs`, for a file system with `FS_DOSOFTDEP`
   (`newfs -U`, FreeBSD 15's `newfs` default). All metadata is written delayed. Each change is
   an item with the waits FreeBSD's rules give it:
   - A block pointer waits for the block's group map and its contents (file data written, or
     the metadata buffer). An allocated inode waits for its group.
   - A direct block's pointer set again before the last one reached the disk (fragments grown
     or moved) replaces that one, as FreeBSD's `allocdirect_merge` does. It goes out in place of
     what the disk holds, and fragments moved away from that the disk never pointed to are
     freed. File data moved with the fragments is written only where they are now.
   - A directory entry waits for the inode it names, written with its new link count. A new
     directory's entry also waits for its first block and its parent's link; a directory's entry
     in a new parent waits for that parent's link.
   - A free waits for the inode (and any indirect block kept) to be written without the
     pointers. A link count on the disk drops only once the entry holding the link is gone there.

   Waits are ordered by stamps taken with the buffer held, and an inode's waits attach when it
   is copied into its block. Before each write the buffer cache asks the tracker (a write hook):
   - What may not go out yet is rolled back in a copy of the buffer, which then stays dirty.
     Inode pointers and size roll back as FreeBSD's `initiate_write_inodeblock_ufs2` does.
     Indirect block pointers roll back too, and so do directory entries (the inode number and
     type). An inode whose group is not written goes out zeroed.
   - Frees and link drops become deferred work, which the syncer does once a second through
     in-core inodes it shares with the vnodes. `Inode::effnlink` is the link count everyone sees
     meanwhile.

   `fsync` writes only the buffers the file needs (its pointers' groups and blocks, its indirect
   blocks, its inode, then the entries naming it). `sync` and unmount repeat until nothing
   waits. Out of space, frees that are due are done and the allocation is tried again.

   Three rules go beyond FreeBSD's, so that a second `fsck` finds nothing either: a pointer to an
   indirect block that is empty on the disk never goes out; partial truncation disconnects the
   kept tree before cutting it; and a failed `balloc` gives back the indirect blocks it made.

   It is tested on the host: random workloads run over a cache that writes buffers back in
   random order and file data late, and crash at random points; FreeBSD's `fsck_ffs -p` must
   preen every crash image. In QEMU, `cargo xtask crash-test` kills the VM mid-workload.

The engine is synchronous over `BlockIo`. In the kernel, `BlockIo` is the buffer cache. On the
host it is an image file. File data goes through the page cache, and the engine only maps file
offsets to disk blocks and allocates them.

### 10.4 msdosfs

`lib/fat` holds FAT12/16/32, VFAT long names (with FreeBSD's short-name generation), and FSInfo,
in the same structure. A file is named by where its directory entry lies, and its number is FreeBSD's
file id for that place (a directory's from its first cluster). A rename moves an entry; an in-use
file's vnode keeps its number, recorded so lookups keep finding it. The kernel mounts the ESP at `/boot/efi` read-write. Names, time zones
(local time, as FreeBSD's msdosfs with no `-L`), and `stat` results match FreeBSD's msdosfs.

### 10.5 nullfs

A second view of a directory (or, `VFCF_FILEMOUNT`, a regular file) of another mount, as FreeBSD's
loopback file system gives it (`sys/vfs/src/nullfs.rs`). Each nullfs vnode stacks on the vnode
below it and holds a use of it (`null_node`'s `null_lowervp`); its number is the lower file's,
and its node passes every operation down:

- **Data**: reads, writes, `fsync`, and the VM object are the lower vnode's, so the layer caches
  nothing and a mapping through it maps the lower file's pages (`null_open` sharing
  `v_object`). A memory-disk file's resident object is keyed by the lowest vnode too.
- **What belongs to the file, not the name**: advisory locks, the writer and text counts
  (`ETXTBSY`), a FIFO's pipe, and a bound socket are the lower vnode's. `Vnode` keeps the lower
  vnode it stacks on (`Node::lower`, asked once as the vnode is made), and `lockf`,
  `add_writer`, `add_text` and the rest go to it; FIFOs open `Vnode::lowest`, and sockets bind
  at `Vnode::unp_vnode`, the lowest unless the mount says `nounixbypass`.
- **Names are not cached here** (`FileSystem::name_cache` false; `null_lookup` enters nothing):
  each lookup asks the lower directory, whose name cache answers. `getcwd` and `vn_fullpath`
  find a layered vnode's name through `Node::vptocnp` (FreeBSD's `VOP_VPTOCNP`), which asks the
  lower vnode's name-cache link.
- **Changes are made below, through the generic layer**: `create`, `link`, `remove`, `rmdir`,
  and `setattr` of a layered node call `vfs::ops` on the lower vnodes, after the generic layer
  checked this layer's rules with the caller's credential (the attributes are the lower ones,
  so the answers are the same; the lower call runs as the superuser). A rename is delegated
  whole before any lock is taken (`ops::rename`), since the lower mount's rename lock may not
  be taken under vnode locks. Two ends on different mounts are `EXDEV` first, the lower mount
  included; a read-only layer is `EROFS`.
- **`stat`** reports the lower attributes with this mount's `st_dev` (`null_getattr`);
  **`statfs`** the lower mount's counts and type number (`nullfs_statfs`). `MNT_LOCAL` is the
  lower mount's.
- **Vnodes nothing uses**: kept while the mount caches (`cache`, the default; FreeBSD's
  `vfs.nullfs.cache_vnodes` and `NULLM_CACHE`), and given up with their last use with `nocache`
  (`Node::recycle_when_unused`, `null_want_recycle`). Either way the vnode reclaimer takes idle
  ones.
- **Removals below**: a mount records the mounts layered on it (`Mount::add_upper`,
  `vfs_register_upper_from_vp`). A file removed or replaced in a mount with layers on it lets
  their idle vnodes of it go, top first (`Mount::notify_unlinked`, FreeBSD's
  `nullfs_unlink_lowervp`), so the lower file is freed as soon as nothing uses it.
- **Mounting and unmounting** (`nullfs_mount`, `nullfs_unmount`): `from` or mount_nullfs's
  `target`, of the covered vnode's type; a nullfs vnode as the covered one over its own lower
  vnode is `EDEADLK`. A busy layer does not unmount unless forced (its files are then doomed),
  and the lower mount does not unmount at all while a layer is on it, forced or not (`dounmount`'s
  `mnt_uppers` check). `MNT_UPDATE` accepts only `export`. It mounts in a jail with
  `allow.mount.nullfs` (`VFCF_JAIL`).

Locks: a layered operation holds this layer's vnode locks, then takes the lower ones (the same
class, parent before child per layer); nothing below calls up. A layered load gets the lower
vnode from its mount, so the `vget` class nests (`dup_ok`), always upper first.

### 10.6 fdescfs

`/dev/fd` as FreeBSD 15.1's fdescfs shows it (`kernel/src/fs/fdescfs.rs`): the root (number 1)
lists the calling process's descriptors, and descriptor `N` is a character device numbered
`N + 3`, mode `0555`, root's, timed at boot (`fdesc_getattr`; its directory entry is numbered
`N + 5`, as `fdesc_readdir` numbers it). One vnode serves every process's `N`. Opening it shares
the descriptor (`dupfdopen`): its node's device answers `dup_fd`, which the open path already
serves for devfs's `/dev/fd/0`–`2`, kept for when no fdescfs is mounted. `linrdlnk` makes the
entries say they are symbolic links (`Attr::kind`) that a lookup does not follow, which
`readlink(2)` reads (`Node::read_link`, `VV_READLINK`): the path of the descriptor's vnode, or
`anon_inode:[unknown]`; opening one still shares the descriptor. `rdlnk` makes them links that
are followed. With `nodup`, a vnode's descriptor looks up as that vnode, so an open is a new
open of the file and a directory's descriptor can be walked through.

The descriptor table is locked before vnodes and open files' offsets, which callers of lookups
and `getdirentries` hold, so fdescfs answers lookups, `readdir`, `readlink`, and `setattr` from
`FileSystem` hooks the generic layer calls without the vnode lock (`lookup_unlocked`,
`readdir_unlocked`, `readlink_unlocked`, `setattr_unlocked`), taking the table with a try-lock
(FreeBSD drops its vnode lock around them likewise; a try checks no order, as FreeBSD's
`LOP_TRYLOCK`). Access is not checked (`checks_access`, FreeBSD's `VOP_NULL`); names can be
neither made nor removed (`fixed_names`: `EROFS` to remove or rename, a lookup's own answer to
make). `setattr` goes to the descriptor's file with that file system's checks. Mounts are never
`MNT_LOCAL`, `statfs` shows only `MNT_IGNORE` of the flags (`FsStats::flags_mask`), and
`MNT_UPDATE` is `EOPNOTSUPP`. It mounts in a jail with `allow.mount.fdescfs`.

### 10.7 cd9660

ISO 9660 volumes, read-only, with Rock Ridge (IEEE P1282 over SUSP: `PX`, `PN`, `SL`, `NM`,
`CL`, `PL`, `RE`, `TF`, `RR`, `CE`, `ST`, found by the root's `SP` and `ER`) and Joliet, as
FreeBSD 15.1's cd9660 reads them. `lib/cd9660` holds the engine over a `BlockIo`; the kernel's
glue (`kernel/src/fs/cd9660.rs`) reads directory records through the disk's buffer cache and
file data through the page cache, with a pager over each file's one extent.

- The volume descriptors from block 16 plus `ssector`; the primary one, and a Joliet
  supplementary one when Rock Ridge is not used (`iso_mountfs`); High Sierra volumes too.
  Options: `norrip` (`-r`), `nojoliet` (`-j`), `gens` (`-g`), `extatt` (`-e`), `brokenjoliet`
  (`-b`), `uid`, `gid`, `mask`, `dirmask`, `ssector`. `kiconv` (`-C`) is not supported.
- A file's number is where its record lies on the volume, a directory's where its data starts
  (`isodirino`); its attributes come from the record its lookup found (a lookup leaves the
  record for the load that follows), or for a number alone from the record at that place.
- Names: Rock Ridge's, or ISO 9660 and Joliet names lower-cased with versions dropped and only
  the last of consecutive versions listed (`isofntrans`, `iso_shipdir`); lookups ignore case
  and version (`isofncmp`). `getdirentries` reports no types (`DT_UNKNOWN`).
- `stat` is `cd9660_getattr` through `vop_stdstat`: masks and the mount's owner applied,
  `st_blocks` the size in whole 512-byte blocks, `st_blksize` the page size, no birth time,
  generation 1, a link's size its target's length when unrecorded.
- Everything read is untrusted: records are checked against their block, System Use entries
  against their record, and an entry too short for its kind gives the record's defaults, as
  FreeBSD's fix of PR 296853 does; a malformed Rock Ridge root fails the mount (`EINVAL`).

It mounts from a disk device, such as a vnode-backed memory disk (`mdconfig -a -t vnode -f
image.iso`, whose requests go through the image file's page cache). The boot tier mounts
`fetch-base`'s test volume (`bsdtar --format iso9660`, Rock Ridge and Joliet) each way FreeBSD's
listings of it were made, and `ls -aliRnT` and `md5 -r` must print what FreeBSD's printed.

## 11. Mounting

- File systems: tmpfs, devfs, UFS2, msdosfs, nullfs, fdescfs, and cd9660 (§10), as `nmount` types
  and in `vfs.conflist`.
- `nmount(2)` takes FreeBSD's option iovec (`fstype`, `fspath`, `from`, `errmsg`, `ro`, `rw`,
  `update`, `noatime`, `nosuid`, `noexec`, `sync`, `async`, and each file system's own). `mount(2)`
  is the old interface, and `unmount(2)` takes `MNT_FORCE`.
- `statfs`, `fstatfs`, and `getfsstat` report FreeBSD's `struct statfs` (checked by abi-check),
  including `f_fsid`, the flags, and the `mntfromname` and `mntonname` strings.
- **Root**: the loader names the root in `vfs.root.mountfrom`: the `/` line of `/etc/fstab` on
  the UFS2 file system it loaded the kernel from (for example `ufs:/dev/gpt/rootfs0`), or else
  that partition as `ufs:/dev/gptid/<uuid>`. The kernel mounts it read-only, as FreeBSD mounts
  root first, waiting up to 3 seconds for the disk (`vfs.mountroot.timeout`), and falls back to
  the memory disk. A disk is found by its name or by the `gpt/<label>` and `gptid/<uuid>` names
  that devfs also shows (FreeBSD's `glabel`). devfs goes on `/dev`, tmpfs on `/tmp`, and the root
  disk's ESP on `/boot/efi`. The shell is `/rescue/sh` as before, now from the disk.
- `mount -u` (`MNT_UPDATE`) moves a UFS2 mount between read-only and read-write. Going
  read-write loads the summary (counted from the groups with soft updates) and marks the file
  system dirty, refusing an unclean one as a read-write mount would. Going read-only writes
  everything and marks it clean. Soft updates are set up when the file system is opened, so a
  root made read-write has them (`MNT_SOFTDEP` shows only while read-write).
- At power-off every file system is unmounted, the root last, forcibly (FreeBSD's
  `vfs_unmountall`), so a read-write UFS2 root is left clean.
- A mount is busy while lookups cross it. `unmount` without `MNT_FORCE` fails with `EBUSY` while
  vnodes are in use. With `MNT_FORCE`, it dooms them.

## 12. Advisory locks

`flock(2)` and `fcntl` `F_GETLK`/`F_SETLK`/`F_SETLKW` (and the OFD variants) are kept per vnode as
FreeBSD's `lf_advlock` keeps them: byte ranges, shared and exclusive, merged and split. `F_SETLKW`
detects deadlock between processes and returns `EDEADLK`. Locks go when the owner closes or exits.
The generic layer implements them for every file system.

## 13. Permissions

`access` checks follow FreeBSD's `vaccess`: owner, group, and other bits, root's bypass except
execute (which needs some execute bit), the sticky bit on directories, and `chflags` flags
(`schg`, `uchg`, and the append-only flags). Every check takes the caller's credential. Until
Phase 6 that is root's, so the checks exist and their tests run as root, plus host tests that
supply other credentials.

## 14. Testing

| Level | What |
|---|---|
| Host, `sys/vfs` | lookup, create, rename, unlink, and cache invalidation against an in-memory test file system; random operation sequences checked against a model |
| Host, `lib/ufs`, `lib/fat` | reading images made on the build server by `makefs`/`newfs`/`newfs_msdos`; random workloads applied to images and checked with `fsck_ffs -f -n` / `fsck_msdosfs -n` on the server (`cargo xtask fs-check`); results compared with FreeBSD mounting the same image |
| Kernel | ktests for the vnode cache, name cache, buffer cache, syncer, and throttling |
| User | C tests (run natively on FreeBSD 15.1 too) for each syscall's semantics and `errno`s |
| Conformance | `pjdfstest` (BSD), driven by a small TAP runner, on tmpfs and UFS2 |
| Stress | FreeBSD's `fsx` (built from the FreeBSD tree on the server, never vendored: it is under the APSL) on tmpfs and UFS2; then `fsck_ffs -f` |
| Crash | for soft updates: kill the VM at random points during a workload, then check the image with `fsck_ffs -p` and `-f -n` |
| Differential | the same workload on FreeBSD and AbyssBSD, and the resulting trees compared with `mtree` |

## 15. Performance

The P5 suite ([PERFORMANCE.md](PERFORMANCE.md) §3) measures the name cache (hits and negative
hits), large directories, create/unlink/rename storms, UFS2 and tmpfs I/O (cold and warm),
`fsync`, extracting `base.txz`, `find /`, and fsx's runtime. The design choices that serve it:

- The lockless lookup path (§6.2).
- One page cache, with clustering and read-ahead (§8).
- FreeBSD's allocation policies, so on-disk layout and seek patterns are comparable.
- No extra locked instructions per operation (§2).
- Directory lookup in large UFS directories: FreeBSD's `dirhash` is on by default, so AbyssBSD
  hashes large directories too.
