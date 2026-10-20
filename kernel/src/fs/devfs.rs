// SPDX-License-Identifier: BSD-2-Clause
//! devfs, as far as the shell needs it: `console`, `null`, `zero`,
//! `random`, `urandom` (a link to `random`, as on FreeBSD), `tty` (the
//! caller's controlling terminal), and the terminals (`ttyu0`).
//!
//! `console` is the console's terminal when its port can be typed at;
//! otherwise it only prints, and reads as end of file. Each device's
//! number is its inode number, as FreeBSD's devfs numbers them; the
//! numbers themselves follow creation order.
//!
//! Behavior reference: sys/fs/devfs/devfs_vnops.c, sys/dev/null/null.c,
//! sys/dev/random/randomdev.c.

use alloc::sync::Arc;

use abi::errno::Errno;
use sched::Sleepable;
use sync::Once;
use vfs::tree::{Builder, Leaf, TreeFs};
use vfs::{CharDevice, Device, Meta, Mount, Uio, Vnode};

/// `/dev/null`: reads end at once, writes vanish.
struct Null;

impl CharDevice for Null {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/zero`: zeros forever; writes vanish.
struct Zero;

static ZEROS: [u8; 4096] = [0; 4096];

impl CharDevice for Zero {
    fn maps_as_zeros(&self) -> bool {
        true
    }

    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        let mut done = 0;
        while uio.resid() > 0 {
            done += uio.copy_out(&ZEROS)?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/random`: the kernel's generator. Writes are accepted and
/// ignored (FreeBSD mixes them in, root only).
struct Random;

impl CharDevice for Random {
    fn read(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        let mut chunk = [0u8; 256];
        let mut done = 0;
        while uio.resid() > 0 {
            let n = uio.resid().min(chunk.len());
            crate::random::fill(&mut chunk[..n]).map_err(|_| Errno::EAGAIN)?;
            done += uio.copy_out(&chunk[..n])?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        Ok(uio.resid())
    }
}

/// `/dev/console`: output to the kernel console.
struct Console;

impl CharDevice for Console {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        Ok(0)
    }

    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: bool) -> Result<usize, Errno> {
        let mut chunk = [0u8; 256];
        let mut done = 0;
        while uio.resid() > 0 {
            let n = uio.copy_in(&mut chunk)?;
            crate::console::write_bytes(&chunk[..n]);
            done += n;
        }
        Ok(done)
    }
}

static CONSOLE: Once<Arc<Vnode>> = Once::new();

/// The boot devfs's tree.
pub(crate) fn build() -> TreeFs {
    let now = super::now();
    let mut b = Builder::new(Meta::root(0o555, now));
    let add = |b: &mut Builder, name: &[u8], perm, ops: Arc<dyn CharDevice>| {
        let rdev = b.next_ino();
        let leaf = Leaf::Device(Device { rdev, ops });
        // A fixed tree of fixed names: nothing to fail.
        let _ = b.leaf(name, Meta::root(perm, now), leaf, Meta::root(0o555, now));
    };
    let console: Arc<dyn CharDevice> = match crate::tty::console() {
        Some(t) => Arc::new(crate::tty::TtyDevice(t)),
        None => Arc::new(Console),
    };
    add(&mut b, b"console", 0o600, console);
    add(&mut b, b"null", 0o666, Arc::new(Null));
    add(&mut b, b"random", 0o644, Arc::new(Random));
    let _ = b.leaf(
        b"urandom",
        Meta::root(0o755, now),
        Leaf::Symlink(b"random".as_slice().into()),
        Meta::root(0o555, now),
    );
    add(&mut b, b"tty", 0o666, Arc::new(crate::tty::ControllingTty));
    for t in &crate::tty::all() {
        add(
            &mut b,
            t.name().as_bytes(),
            0o600,
            Arc::new(crate::tty::TtyDevice(Arc::clone(t))),
        );
    }
    add(&mut b, b"zero", 0o666, Arc::new(Zero));
    b.finish()
}

/// Records the vnodes of devfs mount `m`'s terminals and console, which
/// opening `/dev/tty` and the boot processes' descriptors need.
pub(crate) fn mounted(s: &Sleepable, m: &Arc<Mount>) {
    let Ok(root) = m.root(s) else {
        return;
    };
    for t in &crate::tty::all() {
        if let Ok(v) = vfs::namei::Namespace::child(s, &root, t.name().as_bytes()) {
            t.set_vnode(&v);
        }
    }
    if let Ok(c) = vfs::namei::Namespace::child(s, &root, b"console") {
        let _ = CONSOLE.set(c);
    }
}

/// The console's vnode.
pub(crate) fn console() -> Option<Arc<Vnode>> {
    CONSOLE.get().cloned()
}
