// SPDX-License-Identifier: BSD-2-Clause
//! Repository rules that the compiler cannot check (docs/ENGINEERING.md):
//! no dependencies from outside the workspace (ADR-0002), an SPDX
//! header on every source file (ADR-0007): BSD-2-Clause, or MIT with the
//! original copyright notice for a file derived from MIT-licensed source,
//! and no vendor in the generic ACPI crates (ADR-0020).

use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::Args;
use crate::util::{self, Result};

const SPDX: &str = "// SPDX-License-Identifier: BSD-2-Clause";
const SPDX_MIT: &str = "// SPDX-License-Identifier: MIT";
const SKIP_DIRS: &[&str] = &["target", "build", ".git", ".cargo"];

pub(crate) fn cmd(args: Args) -> Result<()> {
    args.finish()?;
    check()
}

/// Whether `text` starts with an accepted SPDX header.
fn spdx_ok(text: &str) -> bool {
    let mut lines = text.lines();
    match lines.next() {
        Some(SPDX) => true,
        Some(SPDX_MIT) => lines.next().is_some_and(|l| l.starts_with("// Copyright")),
        _ => false,
    }
}

/// Runs every policy check, reporting all violations at once.
pub(crate) fn check() -> Result<()> {
    let root = util::root();
    let mut violations = Vec::new();
    for manifest in files(&root, "Cargo.toml") {
        let text = fs::read_to_string(&manifest)?;
        for v in dependency_violations(&text) {
            violations.push(format!("{}: {v}", rel(&root, &manifest)));
        }
    }
    let mut sources = Vec::new();
    for src in files(&root, ".rs") {
        let text = fs::read_to_string(&src)?;
        if !spdx_ok(&text) {
            violations.push(format!(
                "{}: first line must be `{SPDX}` (or `{SPDX_MIT}` followed by a `// Copyright` line)",
                rel(&root, &src)
            ));
        }
        sources.push((rel(&root, &src), text));
    }
    violations.extend(crate::abicheck::unchecked(&sources));
    violations.extend(vendor_violations(&sources));
    if violations.is_empty() {
        println!("policy: ok");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("policy: {v}");
        }
        Err(format!("{} policy violation(s)", violations.len()).into())
    }
}

/// The crates that must name no vendor (ADR-0020): platform knowledge
/// lives in `platform/<soc>`.
const VENDOR_FREE: &[&str] = &[
    "sys/acpi/src/",
    "lib/devtree/src/",
    "lib/acpi-platform/src/",
];

/// Vendor names and IDs those crates' code may not contain.
const VENDOR_NAMES: &[&str] = &["CIX", "cix", "Sky1", "sky1", "QCOM", "qcom", "Qualcomm"];

/// Vendor names in the identifiers and string literals of the vendor-free
/// crates' non-test code. Comments may name a board as an example, and
/// tests name their corpus machines: test files (`tests.rs`,
/// `*_corpus.rs`) and everything after a file's `#[cfg(test)]` module
/// are skipped.
fn vendor_violations(sources: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, text) in sources {
        if !VENDOR_FREE.iter().any(|d| path.starts_with(d))
            || path.ends_with("/tests.rs")
            || path.ends_with("_corpus.rs")
        {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if line.trim_start() == "#[cfg(test)]" && is_test_module(text, n) {
                break;
            }
            let code = line.split("//").next().unwrap_or("");
            if let Some(v) = VENDOR_NAMES.iter().find(|v| code.contains(*v)) {
                out.push(format!(
                    "{path}:{}: `{v}` in a vendor-free crate (ADR-0020); it belongs in platform/<soc>",
                    n + 1
                ));
            }
        }
    }
    out
}

/// Whether the `#[cfg(test)]` at line `n` opens an inline test module.
fn is_test_module(text: &str, n: usize) -> bool {
    text.lines()
        .nth(n + 1)
        .is_some_and(|l| l.trim_start().starts_with("mod ") && l.trim_end().ends_with('{'))
}

/// Dependencies that are not `path` dependencies inside the workspace.
fn dependency_violations(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    // `[dependencies]`-style table, or `[dependencies.name]` subtable.
    let mut table: Option<(String, bool)> = None;
    let mut subtable_has_path = false;
    let flush = |table: &Option<(String, bool)>, has_path: bool, out: &mut Vec<String>| {
        if let Some((name, true)) = table
            && !has_path
        {
            out.push(format!("[{name}] is not a path dependency"));
        }
    };
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            flush(&table, subtable_has_path, &mut out);
            let name = line.trim_matches(['[', ']']).to_owned();
            let parts: Vec<&str> = name.split('.').collect();
            let dep_pos = parts.iter().position(|p| p.ends_with("dependencies"));
            table = dep_pos.map(|i| (name.clone(), i + 1 < parts.len()));
            subtable_has_path = false;
            continue;
        }
        let Some((_, is_subtable)) = &table else {
            continue;
        };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if *is_subtable {
            if key == "path" {
                subtable_has_path = true;
            }
        } else if !(value.contains("path") || key.ends_with(".workspace") && value == "true") {
            out.push(format!("dependency `{key}` is not a path dependency"));
        }
    }
    flush(&table, subtable_has_path, &mut out);
    out
}

fn files(root: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push(path);
                }
            } else if name.ends_with(suffix) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

pub(crate) fn cmd_unsafe_report(args: Args) -> Result<()> {
    args.finish()?;
    let root = util::root();
    let mut rows: Vec<(String, usize, usize)> = Vec::new();
    for manifest in files(&root, "Cargo.toml") {
        let dir = manifest.parent().unwrap_or(&root);
        if dir == root {
            continue;
        }
        let (mut unsafe_count, mut lines) = (0, 0);
        for src in files(dir, ".rs") {
            let text = fs::read_to_string(&src)?;
            for line in text.lines() {
                let code = line.trim();
                if code.is_empty() || code.starts_with("//") {
                    continue;
                }
                lines += 1;
                unsafe_count += code.matches("unsafe {").count()
                    + code.matches("unsafe fn").count()
                    + code.matches("unsafe impl").count()
                    + code.matches("unsafe extern").count();
            }
        }
        rows.push((rel(&root, dir), unsafe_count, lines));
    }
    println!("{:<24} {:>7} {:>7}", "crate", "unsafe", "lines");
    let (mut total_unsafe, mut total_lines) = (0, 0);
    for (name, u, l) in &rows {
        println!("{name:<24} {u:>7} {l:>7}");
        total_unsafe += u;
        total_lines += l;
    }
    println!("{:<24} {total_unsafe:>7} {total_lines:>7}", "total");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_names() {
        let src = |path: &str, text: &str| (path.to_string(), text.to_string());
        let sources = [
            // Comments may name a board.
            src("sys/acpi/src/a.rs", "// the Sky1's PMMX\nfn f() {}\n"),
            // Code may not.
            src("sys/acpi/src/b.rs", "fn f() { let _ = \"CIXA1019\"; }\n"),
            // Nor a vendor-free lib.
            src("lib/devtree/src/c.rs", "const N: &str = \"cix,hw\";\n"),
            // Tests and inline test modules may.
            src("sys/acpi/src/tests.rs", "const M: &str = \"opi6p cix\";\n"),
            src("sys/acpi/src/x_corpus.rs", "const M: &str = \"CIX\";\n"),
            src(
                "lib/devtree/src/d.rs",
                "fn f() {}\n#[cfg(test)]\nmod tests {\n    const C: &str = \"cix\";\n}\n",
            ),
            // Platform crates are where vendors live.
            src(
                "platform/cix-sky1/src/lib.rs",
                "pub const NAME: &str = \"cix-sky1\";\n",
            ),
        ];
        let v = vendor_violations(&sources);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].starts_with("sys/acpi/src/b.rs:1:"));
        assert!(v[1].starts_with("lib/devtree/src/c.rs:1:"));
    }

    #[test]
    fn spdx_headers() {
        assert!(spdx_ok("// SPDX-License-Identifier: BSD-2-Clause\n//! x"));
        assert!(spdx_ok(
            "// SPDX-License-Identifier: MIT\n// Copyright (c) 2016 A\n"
        ));
        assert!(!spdx_ok("// SPDX-License-Identifier: MIT\n//! no notice"));
        assert!(!spdx_ok("// SPDX-License-Identifier: GPL-2.0\n"));
    }

    #[test]
    fn accepts_path_dependencies() {
        let m = "[package]\nname = \"x\"\n\n[dependencies]\ncrc = { path = \"../crc\" }\n\
                 [dependencies.gpt]\npath = \"../gpt\"\n[lints]\nworkspace = true\n";
        assert!(dependency_violations(m).is_empty());
    }

    #[test]
    fn rejects_registry_dependencies() {
        let m = "[dependencies]\nserde = \"1\"\n[dev-dependencies.rand]\nversion = \"0.8\"\n\
                 [target.'cfg(unix)'.build-dependencies]\ncc = { version = \"1\" }\n";
        let v = dependency_violations(m);
        assert_eq!(v.len(), 3, "{v:?}");
    }
}
