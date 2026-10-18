// SPDX-License-Identifier: BSD-2-Clause
//! Files on a firmware-mounted volume (`EFI_FILE_PROTOCOL`, UEFI 2.10 §13.5).

use core::marker::PhantomData;

use crate::raw::{self, RawGuid};
use crate::{Guid, Result, Status, SystemTable};

const FILE_INFO: Guid = Guid::must_parse("09576E92-6D3F-11D2-8E39-00A0C969723B");
/// Longest path [`File::open`] accepts, in UCS-2 units.
const PATH_MAX: usize = 255;

/// How to open a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Read only; the file must exist.
    Read,
    /// Read and write, creating the file if needed.
    Create,
}

/// An open file or directory, closed on drop. Lives no longer than boot
/// services.
#[derive(Debug)]
pub struct File<'st> {
    raw: *mut raw::File,
    _st: PhantomData<&'st SystemTable>,
}

impl File<'_> {
    /// Wraps an open handle.
    ///
    /// # Safety
    ///
    /// `raw` must be an open file handle that nothing else closes.
    pub(crate) unsafe fn from_raw(raw: *mut raw::File) -> Result<Self> {
        if raw.is_null() {
            return Err(Status::NOT_FOUND);
        }
        Ok(Self {
            raw,
            _st: PhantomData,
        })
    }

    /// Opens `path` relative to this directory. `/` and `\` both separate
    /// components.
    pub fn open(&self, path: &str, mode: Mode) -> Result<Self> {
        let mut name = [0u16; PATH_MAX + 1];
        for (n, c) in path.chars().enumerate() {
            let c = if c == '/' { '\\' } else { c };
            let unit = u16::try_from(u32::from(c)).map_err(|_| Status::INVALID_PARAMETER)?;
            *name
                .get_mut(n)
                .filter(|_| n < PATH_MAX)
                .ok_or(Status::INVALID_PARAMETER)? = unit;
        }
        let (open_mode, attrs) = match mode {
            Mode::Read => (raw::file_mode::READ, 0),
            Mode::Create => (
                raw::file_mode::READ | raw::file_mode::WRITE | raw::file_mode::CREATE,
                0,
            ),
        };
        let mut out = core::ptr::null_mut();
        // SAFETY: `self.raw` is open, `name` is NUL-terminated UCS-2.
        unsafe { ((*self.raw).open)(self.raw, &raw mut out, name.as_ptr(), open_mode, attrs) }
            .ok()?;
        // SAFETY: firmware returned a new open handle that we now own.
        unsafe { Self::from_raw(out) }
    }

    /// The file size in bytes.
    pub fn size(&self) -> Result<u64> {
        // The fixed part plus a generous name.
        let mut buf = [0u64; 64];
        let mut len = size_of_val(&buf);
        let guid = RawGuid::new(FILE_INFO);
        // SAFETY: `buf` is writable for `len` bytes and 8-byte aligned.
        unsafe {
            ((*self.raw).get_info)(
                self.raw,
                &raw const guid,
                &raw mut len,
                buf.as_mut_ptr().cast(),
            )
        }
        .ok()?;
        // SAFETY: firmware filled at least the fixed part of EFI_FILE_INFO,
        // which starts at the (aligned) beginning of `buf`.
        let info = unsafe { buf.as_ptr().cast::<raw::FileInfo>().read() };
        Ok(info.file_size)
    }

    /// Reads up to `buf.len()` bytes from the current position.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut len = buf.len();
        // SAFETY: `buf` is writable for `len` bytes.
        unsafe { ((*self.raw).read)(self.raw, &raw mut len, buf.as_mut_ptr().cast()) }.ok()?;
        Ok(len)
    }

    /// Fills `buf` completely, or fails.
    pub fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            match self.read(&mut buf[done..])? {
                0 => return Err(Status::VOLUME_CORRUPTED),
                n => done += n,
            }
        }
        Ok(())
    }

    /// Writes all of `data` at the current position.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < data.len() {
            let mut len = data.len() - done;
            // SAFETY: the slice is readable for `len` bytes.
            unsafe { ((*self.raw).write)(self.raw, &raw mut len, data[done..].as_ptr().cast()) }
                .ok()?;
            if len == 0 {
                return Err(Status::DEVICE_ERROR);
            }
            done += len;
        }
        Ok(())
    }

    /// Deletes the file (which must be open for writing) and closes it.
    pub fn delete(self) -> Result<()> {
        let raw = self.raw;
        core::mem::forget(self);
        // SAFETY: we own the open handle; Delete closes it, so Drop must not
        // run (hence the `forget`).
        unsafe { ((*raw).delete)(raw) }.ok()
    }

    /// Flushes written data to the device.
    pub fn flush(&self) -> Result<()> {
        // SAFETY: the handle is open.
        unsafe { ((*self.raw).flush)(self.raw) }.ok()
    }
}

impl Drop for File<'_> {
    fn drop(&mut self) {
        // SAFETY: we own the open handle and close it exactly once.
        let _ = unsafe { ((*self.raw).close)(self.raw) };
    }
}
