// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask rgegen`: regenerate `drivers/rge`'s register programs
//! (Realtek's microcode and PHY configuration) from FreeBSD's
//! `sys/dev/rge`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::cli::Args;
use crate::util::{self, Result};

const OUTPUT: &str = "drivers/rge/src/generated.rs";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let src = args
        .take_value("--src")?
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("FREEBSD_SRC").map(PathBuf::from))
        .unwrap_or_else(|| util::root().join("../freebsd-src"));
    let check = args.take_flag("--check");
    args.finish()?;

    let dir = src.join("sys/dev/rge");
    if !dir.exists() {
        if check {
            println!("rgegen: {} not found; skipping check", dir.display());
            return Ok(());
        }
        return Err(format!("{} not found (use --src or FREEBSD_SRC)", dir.display()).into());
    }
    let read = |f: &str| fs::read_to_string(dir.join(f));
    let chips = rgegen::chips(
        &read("if_rgereg.h")?,
        &read("if_rge_microcode.h")?,
        &read("if_rge_hw.c")?,
    )
    .map_err(|e| e.to_string())?;
    let rev = util::output(
        Command::new("git")
            .arg("-C")
            .arg(&src)
            .args(["rev-parse", "HEAD"]),
    )
    .map(|s| s.trim().chars().take(12).collect::<String>())
    .unwrap_or_else(|_| "unknown".into());
    let provenance = format!("freebsd-src sys/dev/rge @ {rev}");
    let formatted = rustfmt(&rgegen::render(&chips, &provenance))?;
    let summary: Vec<String> = chips
        .iter()
        .map(|c| {
            format!(
                "{}: {} MAC, {} PHY microcode, {} configuration steps",
                c.name,
                c.mac_bps.len(),
                c.phy_mcu.len(),
                c.phy_config.len()
            )
        })
        .collect();

    let out = util::root().join(OUTPUT);
    if check {
        let current = fs::read_to_string(&out).unwrap_or_default();
        if current != formatted {
            return Err(format!("{OUTPUT} is stale; run `cargo xtask rgegen`").into());
        }
        println!("rgegen: {OUTPUT} is up to date ({})", summary.join("; "));
    } else {
        fs::create_dir_all(out.parent().unwrap_or(&out))?;
        fs::write(&out, formatted)?;
        println!("rgegen: wrote {OUTPUT} ({}; {rev})", summary.join("; "));
    }
    Ok(())
}

fn rustfmt(code: &str) -> Result<String> {
    let dir = util::root().join("build");
    fs::create_dir_all(&dir)?;
    let tmp = dir.join("rgegen.rs");
    fs::write(&tmp, code)?;
    util::run(
        Command::new("rustfmt")
            .current_dir(util::root())
            .args(["--edition", "2024", "--config-path", "rustfmt.toml"])
            .arg(&tmp),
    )?;
    Ok(fs::read_to_string(&tmp)?)
}
