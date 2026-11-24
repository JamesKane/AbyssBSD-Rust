// SPDX-License-Identifier: BSD-2-Clause
//! What the generators that read FreeBSD's source (`syscallgen`, `rgegen`)
//! share: finding the tree, naming its revision, and formatting the output.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::cli::Args;
use crate::util::{self, Result};

/// The FreeBSD tree to read and whether to only check the output:
/// `--src`, else `FREEBSD_SRC`, else `../freebsd-src`; and `--check`.
pub(crate) fn source_args(args: &mut Args) -> Result<(PathBuf, bool)> {
    let src = args
        .take_value("--src")?
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("FREEBSD_SRC").map(PathBuf::from))
        .unwrap_or_else(|| util::root().join("../freebsd-src"));
    let check = args.take_flag("--check");
    Ok((src, check))
}

/// The tree's revision, 12 hex digits, or `unknown`.
pub(crate) fn revision(src: &std::path::Path) -> String {
    util::output(
        Command::new("git")
            .arg("-C")
            .arg(src)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().chars().take(12).collect::<String>())
    .unwrap_or_else(|_| "unknown".into())
}

/// `code` formatted with the workspace's rustfmt configuration, through
/// `build/<name>.rs`.
pub(crate) fn rustfmt(name: &str, code: &str) -> Result<String> {
    let dir = util::root().join("build");
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!("{name}.rs"));
    fs::write(&tmp, code)?;
    util::run(
        Command::new("rustfmt")
            .current_dir(util::root())
            .args(["--edition", "2024", "--config-path", "rustfmt.toml"])
            .arg(&tmp),
    )?;
    Ok(fs::read_to_string(&tmp)?)
}
