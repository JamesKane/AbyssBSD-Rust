// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask rgegen`: regenerate `drivers/rge`'s register programs
//! (Realtek's microcode and PHY configuration) from FreeBSD's
//! `sys/dev/rge`.

use std::fs;

use crate::cli::Args;
use crate::codegen;
use crate::util::{self, Result};

const OUTPUT: &str = "drivers/rge/src/generated.rs";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let (src, check) = codegen::source_args(&mut args)?;
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
    let rev = codegen::revision(&src);
    let provenance = format!("freebsd-src sys/dev/rge @ {rev}");
    let formatted = codegen::rustfmt("rgegen", &rgegen::render(&chips, &provenance))?;
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
