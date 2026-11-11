// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask perf --platform opi6p --dir DIR [--rev REV]`: the report
//! for a perf run on the Orange Pi 6 Plus (docs/PERFORMANCE.md §4), from
//! the logs `tests/perf/board/perf-board.sh` leaves in `DIR`: one
//! `<side>-<run>.log` per run, each with every suite once per core class.
//! The report, one table per class, goes to `DIR/report.txt`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use super::{Report, Side};
use crate::util::Result;

/// The baseline: the board's FreeBSD kernel without its debugging options
/// (stock 15.1 does not run on the board).
const BASELINE: &str = "FreeBSD 16.0-CURRENT f12c9f4ee6 GENERIC-HMP-IOMMU-NODEBUG";

/// A class's samples: benchmark, side, one value per run.
type Samples = BTreeMap<String, BTreeMap<Side, Vec<f64>>>;

/// One result: class, benchmark, value, unit.
type Sample = (String, String, f64, String);

/// One log's results, and its `kbench env` lines.
fn parse(log: &str) -> Result<(Vec<Sample>, Vec<String>)> {
    let mut results = Vec::new();
    let mut env = Vec::new();
    let mut class: Option<String> = None;
    let mut open = false;
    for line in log.lines() {
        let Some(at) = line.find("kbench ") else {
            continue;
        };
        let words: Vec<&str> = line[at..].split_whitespace().collect();
        match words.as_slice() {
            ["kbench", "env", ..] => env.push(line[at + "kbench env ".len()..].to_owned()),
            ["kbench", "class", name, ..] => class = Some((*name).to_owned()),
            ["kbench", "begin", ..] => open = true,
            ["kbench", "end", ..] => open = false,
            ["kbench", name, value, unit] if open => {
                let class = class.clone().ok_or("a result before any `kbench class`")?;
                let v: f64 = value
                    .parse()
                    .map_err(|_| format!("bad value in `{line}`"))?;
                results.push((class, (*name).to_owned(), v, (*unit).to_owned()));
            }
            _ => {}
        }
    }
    if open {
        return Err("a suite without its `kbench end`".into());
    }
    Ok((results, env))
}

pub(super) fn report(dir: &Path, rev: &str) -> Result<()> {
    let mut by_class: BTreeMap<String, Samples> = BTreeMap::new();
    let mut units = BTreeMap::new();
    let mut runs: BTreeMap<Side, usize> = BTreeMap::new();
    let mut env: BTreeMap<Side, Vec<String>> = BTreeMap::new();
    let mut names: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter_map(|n| n.to_str().map(str::to_owned))
        .filter(|n| n.ends_with(".log"))
        .collect();
    names.sort();
    for name in names {
        let side = if name.starts_with("freebsd-") {
            Side::FreeBsd
        } else if name.starts_with("abyss-") {
            Side::Abyss
        } else {
            continue;
        };
        let text = fs::read_to_string(dir.join(&name))?;
        let (results, lines) = parse(&text).map_err(|e| format!("{name}: {e}"))?;
        *runs.entry(side).or_default() += 1;
        env.entry(side).or_insert(lines);
        for (class, bench, value, unit) in results {
            units.insert(bench.clone(), unit);
            by_class
                .entry(class)
                .or_default()
                .entry(bench)
                .or_default()
                .entry(side)
                .or_default()
                .push(value);
        }
    }
    let (f, a) = (
        runs.get(&Side::FreeBsd).copied().unwrap_or(0),
        runs.get(&Side::Abyss).copied().unwrap_or(0),
    );
    if f == 0 || a == 0 {
        return Err(format!("{}: no complete runs of both sides", dir.display()).into());
    }

    let mut text = format!(
        "perf on the Orange Pi 6 Plus: AbyssBSD {rev} vs {BASELINE}\n\
         {f} FreeBSD runs, {a} AbyssBSD runs; the same 15.1 userland (AbyssBSD's root) on both\n\n"
    );
    for (side, lines) in &env {
        for l in lines {
            let _ = writeln!(text, "{}: {l}", side.name());
        }
    }
    let mut failures = 0;
    // The classes in the order perf-run.sh runs them.
    let order = ["big", "mid", "little", "all"];
    let mut classes: Vec<&String> = by_class.keys().collect();
    classes.sort_by_key(|c| order.iter().position(|o| o == c).unwrap_or(order.len()));
    for class in classes {
        let report = Report::new(&by_class[class], &units);
        failures += report.failures();
        let _ = write!(
            text,
            "\n{}",
            report.render_titled(&format!(
                "core class {class}, AbyssBSD {rev} vs {BASELINE}, {f}/{a} runs"
            ))
        );
    }
    fs::write(dir.join("report.txt"), &text)?;
    print!("{text}");
    eprintln!(
        "xtask: perf: report in {}",
        dir.join("report.txt").display()
    );
    if failures > 0 {
        return Err(format!("{failures} component(s) over the bound, across the classes").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn classes_and_env() {
        let log = "kbench env side=abyss ident=ABYSSBSD\n\
                   kbench class big 0,1,10,11\n\
                   kbench begin p4 cpus=12\n\
                   kbench syscall.getpid 80.5 ns/op\n\
                   kbench end p4\n\
                   kbench class all 0-7\n\
                   kbench begin p4 cpus=12\n\
                   kbench syscall.getpid 90 ns/op\n\
                   kbench end p4\n";
        let (r, env) = parse(log).unwrap();
        assert_eq!(env, ["side=abyss ident=ABYSSBSD"]);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "big");
        assert_eq!(r[0].2, 80.5);
        assert_eq!(r[1].0, "all");
        assert!(parse("kbench class big 0\nkbench begin p4\nkbench x 1 ns\n").is_err());
    }
}
