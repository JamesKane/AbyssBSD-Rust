// SPDX-License-Identifier: BSD-2-Clause
//! In-image test harness for code that only runs inside firmware or the
//! kernel.
//!
//! [`ktest!`] registers a test function in a dedicated linker section, and
//! [`run_all`] walks that section. Results go to a text sink (the console) in
//! a line protocol that `cargo xtask test` parses from the serial log.
//!
//! Tests fail by panicking. Images are built with `panic = "abort"`, so the
//! first failure ends the run; the image's panic handler reports it with
//! [`report_panic`].
//!
//! Registration differs by object format:
//! - PE/COFF (UEFI): the linker sorts `.ktest$a`, `.ktest$m`, `.ktest$z`
//!   into one `.ktest` section, so markers in `$a` and `$z` bracket the
//!   entries.
//! - ELF (kernel, host): the linker defines `__start_ktest` and
//!   `__stop_ktest` around the `ktest` section.

#![no_std]

use core::fmt::{self, Write};

/// Printed before the first test.
pub const BEGIN_MARKER: &str = "ABYSS-TEST-BEGIN";
/// Printed when every test passed.
pub const PASS_MARKER: &str = "ABYSS-TEST-RESULT: PASS";
/// Printed when a test failed or the image panicked.
pub const FAIL_MARKER: &str = "ABYSS-TEST-RESULT: FAIL";

/// One registered test.
#[derive(Debug)]
pub struct Test {
    /// Fully qualified name, `module::function`.
    pub name: &'static str,
    /// The test body. It fails by panicking.
    pub run: fn(),
}

/// Registers a test function.
///
/// ```ignore
/// ktest::ktest! {
///     fn crc_check_value() {
///         assert_eq!(crc::crc32(b"123456789"), 0xCBF4_3926);
///     }
/// }
/// ```
#[macro_export]
macro_rules! ktest {
    ($(#[$attr:meta])* fn $name:ident() $body:block) => {
        $(#[$attr])*
        fn $name() $body

        const _: () = {
            #[used]
            #[cfg_attr(target_os = "uefi", unsafe(link_section = ".ktest$m"))]
            #[cfg_attr(not(target_os = "uefi"), unsafe(link_section = "ktest"))]
            static ENTRY: ::core::option::Option<&$crate::Test> = ::core::option::Option::Some(
                &$crate::Test { name: concat!(module_path!(), "::", stringify!($name)), run: $name },
            );
        };
    };
}

#[cfg(target_os = "uefi")]
mod section {
    use super::Test;

    #[used]
    #[unsafe(link_section = ".ktest$a")]
    static START: Option<&Test> = None;

    #[used]
    #[unsafe(link_section = ".ktest$z")]
    static END: Option<&Test> = None;

    pub(crate) fn bounds() -> (*const Option<&'static Test>, *const Option<&'static Test>) {
        // The entries lie strictly between the two markers.
        (
            core::ptr::addr_of!(START).wrapping_add(1),
            core::ptr::addr_of!(END),
        )
    }
}

#[cfg(not(target_os = "uefi"))]
mod section {
    use super::Test;

    // Keeps the section (and so the linker-defined bounds) in existence even
    // when an image registers no tests.
    #[used]
    #[unsafe(link_section = "ktest")]
    static EMPTY: Option<&Test> = None;

    // Declared as bytes: only the addresses are used, and the real element
    // type is not FFI-safe.
    unsafe extern "C" {
        static __start_ktest: u8;
        static __stop_ktest: u8;
    }

    pub(crate) fn bounds() -> (*const Option<&'static Test>, *const Option<&'static Test>) {
        let _ = &EMPTY;
        (
            core::ptr::addr_of!(__start_ktest).cast(),
            core::ptr::addr_of!(__stop_ktest).cast(),
        )
    }
}

/// Every registered test, in link order.
pub fn tests() -> impl Iterator<Item = &'static Test> {
    let (start, end) = section::bounds();
    let len = (end as usize).saturating_sub(start as usize) / size_of::<Option<&Test>>();
    // SAFETY: `start..end` is the section the linker built from statics of
    // type `Option<&'static Test>` (see `ktest!` and the markers above), so
    // it holds `len` initialised values of that type, suitably aligned.
    // Padding the linker may insert is zero, which is `None`.
    let slots = unsafe { core::slice::from_raw_parts(start, len) };
    slots.iter().filter_map(|t| *t)
}

/// Totals from a completed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Tests that ran to completion.
    pub passed: usize,
}

/// Runs every registered test, reporting to `out`, then prints the pass
/// verdict.
///
/// Returns only if all tests pass; a failing test panics into the image's
/// panic handler.
pub fn run_all(out: &mut impl Write) -> Result<Summary, fmt::Error> {
    let summary = run_tests(out)?;
    report_pass(out)?;
    Ok(summary)
}

/// Runs every registered test without printing a verdict, for images that
/// have more checks to make (or another stage to boot) afterwards.
pub fn run_tests(out: &mut impl Write) -> Result<Summary, fmt::Error> {
    run_tests_timed(out, None)
}

/// Like [`run_tests`], also printing each test's duration from `clock`
/// (milliseconds).
pub fn run_tests_timed(
    out: &mut impl Write,
    clock: Option<&dyn Fn() -> u64>,
) -> Result<Summary, fmt::Error> {
    let total = tests().count();
    writeln!(out, "{BEGIN_MARKER} ({total} tests)")?;
    let mut passed = 0;
    for t in tests() {
        write!(out, "ktest: {} ... ", t.name)?;
        let start = clock.map(|c| c());
        (t.run)();
        match (clock, start) {
            (Some(c), Some(s)) => writeln!(out, "ok ({} ms)", c().saturating_sub(s))?,
            _ => writeln!(out, "ok")?,
        }
        passed += 1;
    }
    writeln!(out, "ktest: {passed} passed")?;
    Ok(Summary { passed })
}

/// Prints the pass verdict.
pub fn report_pass(out: &mut impl Write) -> fmt::Result {
    writeln!(out, "{PASS_MARKER}")
}

/// Reports a panic in the line protocol. Call from the image's panic handler.
pub fn report_panic(out: &mut impl Write, info: &core::panic::PanicInfo<'_>) {
    let _ = writeln!(out, "\nPANIC: {info}");
    let _ = writeln!(out, "{FAIL_MARKER}");
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::String;

    crate::ktest! {
        fn registered_on_host() {
            assert!(crate::tests().count() >= 1);
        }
    }

    #[test]
    fn host_elf_registration_is_found() {
        let names: std::vec::Vec<_> = super::tests().map(|t| t.name).collect();
        assert_eq!(names, ["ktest::tests::registered_on_host"]);
    }

    #[test]
    fn run_all_reports_protocol() {
        let mut out = String::new();
        let summary = super::run_all(&mut out).unwrap();
        assert_eq!(summary.passed, 1);
        assert!(out.starts_with("ABYSS-TEST-BEGIN (1 tests)\n"));
        assert!(out.contains("ktest: ktest::tests::registered_on_host ... ok\n"));
        assert!(out.ends_with("ABYSS-TEST-RESULT: PASS\n"));
    }
}
