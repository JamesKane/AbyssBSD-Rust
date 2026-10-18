// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask syscallgen`: regenerate lib/abi's syscall tables from
//! FreeBSD's `syscalls.master`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::cli::Args;
use crate::util::{self, Result};

const OUTPUT: &str = "lib/abi/src/syscall/generated.rs";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let src = args
        .take_value("--src")?
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("FREEBSD_SRC").map(PathBuf::from))
        .unwrap_or_else(|| util::root().join("../freebsd-src"));
    let check = args.take_flag("--check");
    args.finish()?;

    let master = src.join("sys/kern/syscalls.master");
    if !master.exists() {
        if check {
            println!("syscallgen: {} not found; skipping check", master.display());
            return Ok(());
        }
        return Err(format!("{} not found (use --src or FREEBSD_SRC)", master.display()).into());
    }
    let text = fs::read_to_string(&master)?;
    let entries = syscallgen::parse(&text).map_err(|e| e.to_string())?;
    let rev = util::output(
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().chars().take(12).collect::<String>())
    .unwrap_or_else(|_| "unknown".into());
    let provenance = format!("freebsd-src sys/kern/syscalls.master @ {rev}");
    let rendered = syscallgen::render(&entries, &provenance).map_err(|e| e.to_string())?;
    let formatted = rustfmt(&rendered)?;

    let out = util::root().join(OUTPUT);
    let live = entries.iter().filter(|e| e.kind.is_live()).count();
    if check {
        let current = fs::read_to_string(&out).unwrap_or_default();
        if current != formatted {
            return Err(format!("{OUTPUT} is stale; run `cargo xtask syscallgen`").into());
        }
        println!(
            "syscallgen: {OUTPUT} is up to date ({} slots, {live} live)",
            entries.len()
        );
    } else {
        fs::write(&out, formatted)?;
        println!(
            "syscallgen: wrote {OUTPUT} ({} slots, {live} live, {rev})",
            entries.len()
        );
    }
    Ok(())
}

fn rustfmt(code: &str) -> Result<String> {
    let dir = util::root().join("build");
    fs::create_dir_all(&dir)?;
    let tmp = dir.join("syscallgen.rs");
    fs::write(&tmp, code)?;
    util::run(
        Command::new("rustfmt")
            .current_dir(util::root())
            .args(["--edition", "2024", "--config-path", "rustfmt.toml"])
            .arg(&tmp),
    )?;
    Ok(fs::read_to_string(&tmp)?)
}
