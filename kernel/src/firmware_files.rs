// SPDX-License-Identifier: BSD-2-Clause
//! Firmware files for drivers: a whole file read by path from the root file
//! system, in thread context (a driver's own kernel thread, after the root
//! is mounted). FreeBSD's `firmware(9)` hands out images registered by
//! loaded modules; the files here are plain files, put in place by the
//! operator (`/lib/firmware/<vendor>/<name>`), since the images are
//! not ours to ship.

use alloc::vec::Vec;

use abi::errno::Errno;
use sched::Sleepable;

/// The most a firmware file may be: 128 MiB (the largest GPU image is
/// about 64 MiB).
const MAX_BYTES: u64 = 128 << 20;

/// The kernel's reader of firmware files.
pub(crate) struct Loader;

impl drmn::Firmware for Loader {
    fn read(&self, s: &Sleepable, path: &str) -> Result<Vec<u8>, Errno> {
        let root = crate::fs::root();
        let file = crate::fs::namespace().lookup(
            s,
            &vfs::namei::Lookup {
                start: &root,
                root: &root,
                follow: true,
                beneath: false,
                create: false,
                cred: crate::fs::cred(s),
            },
            path.as_bytes(),
        )?;
        let size = file.size(s)?;
        if size > MAX_BYTES {
            return Err(Errno::EFBIG);
        }
        let size = usize::try_from(size).map_err(|_| Errno::EFBIG)?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(size).map_err(|_| Errno::ENOMEM)?;
        buf.resize(size, 0u8);
        let mut done = 0;
        while done < size {
            let mut r = vfs::ReadBuf::new(&mut buf[done..]);
            let n = file.read_at(s, done as u64, &mut r)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }
}

impl panthor_driver::Firmware for Loader {
    fn read(&self, s: &Sleepable, path: &str) -> Result<Vec<u8>, Errno> {
        <Self as drmn::Firmware>::read(self, s, path)
    }
}
