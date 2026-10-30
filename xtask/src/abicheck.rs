// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask abi-check`: verify lib/abi's structure layouts against real
//! FreeBSD headers (docs/COMPATIBILITY.md §2.2).
//!
//! The Rust layouts become C11 `_Static_assert`s, compiled with
//! `-fsyntax-only` on the build server: natively for amd64, and with
//! `--target` plus an arm64 sysroot from `base.txz` for aarch64. Nothing is
//! executed, so cross-checking needs no arm64 machine.

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::process::Stdio;

use abi::layout::{Arches, CONSTANTS, LAYOUTS};

use crate::arch::Arch;
use crate::cli::Args;
use crate::remote;
use crate::util::{self, Result};

const RELEASE: &str = "15.1-RELEASE";

/// Macros defined before the headers: what libc itself defines to see
/// the structures the system calls take but the public headers hide
/// (`struct acl` behind `_ACL_PRIVATE`).
const DEFINES: &[&str] = &["_ACL_PRIVATE"];

/// Types `LAYOUTS` checks only as a part of another structure it checks:
/// the Rust type and that structure's C type.
const PARTS: &[(&str, &str)] = &[
    // `mc_fpstate`, an array in C.
    ("FpState", "mcontext_t"),
    // An anonymous structure in C.
    ("DbregPair", "struct dbreg"),
    // The padding of `kf_un`'s members.
    ("KfPad", "struct kinfo_file"),
];

/// Types whose C declaration is the kernel's own, in no header, so
/// `abi-check` cannot compare them: the Rust type and where FreeBSD
/// declares it. Each pins its size at compile time instead.
const PRIVATE: &[(&str, &str)] = &[("ModuleStatV2", "sys/kern/kern_module.c")];

/// The boundary structures `abi-check` would not check
/// (docs/MULTIUSER.md §13), from the workspace's Rust sources (path and
/// text): a `#[repr(C)]` type in lib/abi that no layout names, or one
/// outside lib/abi copied to or from user memory (`Plain`), which belongs
/// in lib/abi with a layout.
pub(crate) fn unchecked(sources: &[(String, String)]) -> Vec<String> {
    let checked: std::collections::HashSet<&str> = LAYOUTS
        .iter()
        .flat_map(|l| {
            l.rust_type
                .split(|c: char| !c.is_alphanumeric() && c != '_')
        })
        .filter(|w| !w.is_empty())
        .collect();
    let c_types: std::collections::HashSet<&str> = LAYOUTS.iter().map(|l| l.c_type).collect();
    let mut out = Vec::new();
    for (path, text) in sources {
        let in_abi = path.starts_with("lib/abi/");
        for name in repr_c_types(text) {
            if in_abi {
                let part = PARTS.iter().find(|(t, _)| *t == name);
                match part {
                    Some((_, parent)) if c_types.contains(parent) => {}
                    Some((_, parent)) => {
                        out.push(format!("{path}: {name}'s parent `{parent}` has no layout"));
                    }
                    None if checked.contains(name) => {}
                    None if PRIVATE.iter().any(|(t, _)| *t == name) => {}
                    None => out.push(format!(
                        "{path}: `{name}` is #[repr(C)] but no abi::layout::LAYOUTS entry checks it"
                    )),
                }
            } else if text.contains(&format!("Plain for {name} ")) {
                out.push(format!(
                    "{path}: `{name}` crosses the user boundary; it belongs in lib/abi with a layout"
                ));
            }
        }
    }
    out
}

/// The names of the `#[repr(C)]` structures and unions in `text`.
fn repr_c_types(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(l) = lines.next() {
        if !l.starts_with("#[repr(C") {
            continue;
        }
        // Past the other attributes and comments to the item.
        for l in lines.by_ref() {
            if l.starts_with("#[") || l.starts_with("//") {
                continue;
            }
            let item = l
                .strip_prefix("pub(crate) ")
                .or_else(|| l.strip_prefix("pub "))
                .unwrap_or(l);
            if let Some(rest) = item
                .strip_prefix("struct ")
                .or_else(|| item.strip_prefix("union "))
            {
                let end = rest
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(rest.len());
                out.push(&rest[..end]);
            }
            break;
        }
    }
    out
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    args.finish()?;
    let root = util::root();
    let mut sources = Vec::new();
    for dir in ["lib", "sys", "kernel", "drivers"] {
        collect_sources(&root, &root.join(dir), &mut sources)?;
    }
    let missing = unchecked(&sources);
    if !missing.is_empty() {
        return Err(format!("abi-check: unchecked types:\n  {}", missing.join("\n  ")).into());
    }
    let dir = util::root().join("build/abi-check");
    fs::create_dir_all(&dir)?;
    for arch in arches {
        let c = generate(arch);
        let local = dir.join(format!("{arch}.c"));
        fs::write(&local, &c)?;
        let asserts = c.matches("_Static_assert").count();
        compile_remote(arch, &c)?;
        println!("abi-check: {arch}: {asserts} assertions hold against FreeBSD {RELEASE} headers");
    }
    Ok(())
}

fn applies(arches: Arches, arch: Arch) -> bool {
    match arches {
        Arches::All => true,
        Arches::Amd64 => arch == Arch::X86_64,
        Arches::Arm64 => arch == Arch::Aarch64,
    }
}

/// The Rust sources under `dir`, by their path from `root`.
fn collect_sources(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    for e in fs::read_dir(dir)? {
        let path = e?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                collect_sources(root, &path, out)?;
            }
        } else if path.extension().is_some_and(|x| x == "rs") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            out.push((rel, fs::read_to_string(&path)?));
        }
    }
    Ok(())
}

/// The C translation unit asserting every layout for `arch`.
fn generate(arch: Arch) -> String {
    let layouts: Vec<_> = LAYOUTS.iter().filter(|l| applies(l.arches, arch)).collect();
    let mut headers: Vec<&str> = Vec::new();
    for h in layouts
        .iter()
        .flat_map(|l| l.headers.iter())
        .chain(CONSTANTS.iter().flat_map(|g| g.iter().map(|k| &k.header)))
        .chain(&["stddef.h"])
    {
        if !headers.contains(h) {
            headers.push(h);
        }
    }
    let mut c = String::new();
    // `String` as a `fmt::Write` sink cannot fail.
    let _ = writeln!(c, "/* Generated by `cargo xtask abi-check` for {arch}. */");
    for d in DEFINES {
        let _ = writeln!(c, "#define {d}");
    }
    for h in headers {
        let _ = writeln!(c, "#include <{h}>");
    }
    for l in layouts {
        let t = l.c_type;
        let _ = writeln!(c, "\n/* {t} */");
        let _ = writeln!(
            c,
            "_Static_assert(sizeof({t}) == {}, \"sizeof({t})\");",
            l.size
        );
        let _ = writeln!(
            c,
            "_Static_assert(_Alignof({t}) == {}, \"_Alignof({t})\");",
            l.align
        );
        for f in l.fields {
            let n = f.c_name;
            let _ = writeln!(
                c,
                "_Static_assert(offsetof({t}, {n}) == {}, \"offsetof({t}, {n})\");",
                f.offset
            );
            let _ = writeln!(
                c,
                "_Static_assert(sizeof((({t} *)0)->{n}) == {}, \"sizeof({t}.{n})\");",
                f.size
            );
        }
    }
    let _ = writeln!(c, "\n/* Constants. */");
    for k in CONSTANTS
        .iter()
        .flat_map(|g| g.iter())
        .filter(|k| applies(k.arches, arch))
    {
        let n = k.c_name;
        let _ = writeln!(c, "_Static_assert({n} == {}, \"{n}\");", k.value);
    }
    c
}

/// The remote sysroot used to cross-check `arch`, if it needs one.
fn sysroot(arch: Arch) -> Option<String> {
    (arch != Arch::X86_64).then(|| format!("abyss/sysroot/{RELEASE}-{}", arch.freebsd_name()))
}

fn compile_remote(arch: Arch, source: &str) -> Result<()> {
    let mut flags = format!(
        "--target={} -std=c11 -fsyntax-only -Werror",
        arch.clang_target()
    );
    if let Some(root) = sysroot(arch) {
        // base.txz carries the headers; fetch it once per release.
        let url = format!(
            "https://download.freebsd.org/releases/{}/{}/{RELEASE}/base.txz",
            match arch {
                Arch::X86_64 => "amd64",
                Arch::Aarch64 => "arm64",
            },
            arch.freebsd_name()
        );
        // Extract beside the final path and rename on success, so an
        // interrupted download never leaves a half-populated sysroot.
        let setup = format!(
            "test -d {root} || {{ rm -rf {root}.tmp && mkdir -p {root}.tmp && \
             fetch -q -o - {url} | tar -xf - -C {root}.tmp ./usr/include && \
             mv {root}.tmp {root}; }}"
        );
        util::run(&mut remote::ssh(&setup))?;
        let _ = write!(flags, " --sysroot=$HOME/{root}");
    }
    let script = format!("cc {flags} -x c -");
    let mut child = remote::ssh(&script)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run ssh: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("ssh stdin not captured")?
        .write_all(source.as_bytes())?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(
            format!("{arch}: layouts differ from FreeBSD headers (see compiler errors above)")
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_assertions() {
        let c = generate(Arch::X86_64);
        assert!(c.contains("#include <sys/stat.h>"));
        assert!(c.contains("_Static_assert(sizeof(struct stat) == 224,"));
        assert!(c.contains("offsetof(struct sigaction, sa_mask) == 12"));
        assert_eq!(c.matches("#include <sys/types.h>").count(), 1);
        assert!(c.contains("#include <sys/sysctl.h>"));
        assert!(c.contains("_Static_assert(KERN_OSRELDATE == 24, \"KERN_OSRELDATE\");"));
        assert!(c.contains("_Static_assert(CTLFLAG_RD == 2147483648,"));
    }

    #[test]
    fn unchecked_types() {
        let abi = "#[repr(C)]\n#[derive(Clone)]\n/// x\npub struct Stat {}\n\
                   #[repr(C, align(16))]\npub struct FpState(pub [u8; 512]);\n\
                   #[repr(C)]\npub union Nope { a: u32 }\n";
        let kernel = "#[repr(C)]\nstruct Arg { a: u32 }\nunsafe impl Plain for Arg {}\n\
                      #[repr(C)]\nstruct Inode { a: u32 }\n";
        let v = unchecked(&[
            ("lib/abi/src/x.rs".into(), abi.into()),
            ("kernel/src/y.rs".into(), kernel.into()),
        ]);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].contains("`Nope`"));
        assert!(v[1].contains("`Arg` crosses"));
    }
}
