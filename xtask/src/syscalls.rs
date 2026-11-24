// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask syscallgen`: regenerate lib/abi's syscall tables from
//! FreeBSD's `syscalls.master`.

use std::fs;

use crate::cli::Args;
use crate::codegen;
use crate::util::{self, Result};

const OUTPUT: &str = "lib/abi/src/syscall/generated.rs";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let (src, check) = codegen::source_args(&mut args)?;
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
    let rev = codegen::revision(&src);
    let provenance = format!("freebsd-src sys/kern/syscalls.master @ {rev}");
    let rendered = syscallgen::render(&entries, &provenance).map_err(|e| e.to_string())?;
    let formatted = codegen::rustfmt("syscallgen", &rendered)?;

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
