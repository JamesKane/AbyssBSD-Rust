// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask acpidump`: capture a virtual machine's ACPI tables.
//!
//! Boots the kernel with `abyss.acpidump`, which prints every table (the
//! XSDT's, plus the DSDT) hex-encoded on the console, and writes each to
//! `<out>/<signature>.dat` (`ssdt.dat`, `ssdt2.dat`, … for repeats). The
//! files feed the host tests of `lib/acpi-tables` and `lib/aml`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::qemu::{Outcome, Vm, Vmm};
use crate::util::Result;

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let vmm = Vmm::parse(&args.take_value("--vmm")?.unwrap_or_else(|| "qemu".into()))?;
    let machine = args.take_value("--machine")?;
    let out = PathBuf::from(args.take_value("--out")?.ok_or("--out DIR is required")?);
    args.finish()?;
    if !vmm.available(arch) {
        return Err(format!("{vmm:?} cannot run {arch} guests on this host").into());
    }
    let opts = build::Options {
        release: false,
        ktest: false,
    };
    let disk = image::build(arch, opts, "abyss.acpidump=1")?;
    let vm = Vm {
        machine,
        ..Vm::new(vmm, arch, disk.clone())
    };
    let log = disk.with_file_name("acpidump.log");
    let done = |line: &str| line.contains("acpidump: done").then_some(Outcome::Passed);
    let run = vm.run_until(Duration::from_secs(300), &log, false, &done)?;
    if run.outcome != Outcome::Passed {
        return Err(format!(
            "no complete dump ({:?}); see {}",
            run.outcome,
            log.display()
        )
        .into());
    }
    let tables = decode(&fs::read_to_string(&log)?)?;
    fs::create_dir_all(&out)?;
    for (name, bytes) in &tables {
        let path = out.join(format!("{name}.dat"));
        fs::write(&path, bytes)?;
        println!("{} ({} bytes)", path.display(), bytes.len());
    }
    Ok(())
}

/// Decodes the console's `acpidump:` lines into (file name, bytes).
fn decode(log: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let mut tables = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut cur: Option<(String, usize, Vec<u8>)> = None;
    for line in log.lines() {
        let Some(at) = line.find("acpidump: ") else {
            continue;
        };
        let rest = &line[at + "acpidump: ".len()..];
        if let Some(head) = rest.strip_prefix("table ") {
            let mut words = head.split_whitespace();
            let sig = words.next().ok_or("table line without a signature")?;
            let len: usize = words
                .next()
                .and_then(|l| l.parse().ok())
                .ok_or("table line without a length")?;
            cur = Some((sig.to_ascii_lowercase(), len, Vec::with_capacity(len)));
        } else if rest == "end" {
            let (sig, len, bytes) = cur.take().ok_or("end without a table")?;
            if bytes.len() != len {
                return Err(format!("{sig}: {} bytes, expected {len}", bytes.len()).into());
            }
            let n = seen.entry(sig.clone()).or_insert(0);
            *n += 1;
            let name = if *n == 1 { sig } else { format!("{sig}{n}") };
            tables.push((name, bytes));
        } else if let Some((_, _, bytes)) = cur.as_mut() {
            let hex = rest.trim();
            if hex.len() % 2 != 0 {
                return Err(format!("odd hex line `{hex}`").into());
            }
            for i in (0..hex.len()).step_by(2) {
                let b = u8::from_str_radix(&hex[i..i + 2], 16)
                    .map_err(|_| format!("bad hex `{hex}`"))?;
                bytes.push(b);
            }
        }
    }
    if tables.is_empty() {
        return Err("no tables in the log".into());
    }
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_repeats_and_checks_lengths() {
        let log = "boot\nacpidump: table SSDT 3\n[ 1.0] acpidump: 0102\nacpidump: 03\n\
                   acpidump: end\nacpidump: table SSDT 1\nacpidump: ff\nacpidump: end\n\
                   acpidump: table DSDT 2\nacpidump: aabb\nacpidump: end\nacpidump: done\n";
        let t = decode(log).unwrap();
        let names: Vec<&str> = t.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["ssdt", "ssdt2", "dsdt"]);
        assert_eq!(t[0].1, [1, 2, 3]);
        assert!(decode("acpidump: table APIC 4\nacpidump: 01\nacpidump: end\n").is_err());
    }
}
