// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask crash-test`: soft updates against real crashes (P5.7).
//!
//! Each round boots the shell image in QEMU with a UFS2 disk made by
//! FreeBSD's `newfs -U`, mounts it from `/rescue`, starts a workload of
//! creates, writes, links, renames, `fsync`s, and removals on it, and kills
//! QEMU at a random moment. FreeBSD's `fsck_ffs -p` must then repair the
//! image alone (preening a file system with soft updates fails on anything
//! soft updates should not leave), again until `fsck_ffs -f -n` finds
//! nothing. Every other round boots on the crashed image as it is, so
//! AbyssBSD mounts an unclean file system with soft updates (as FreeBSD
//! does, leaving fsck for later); the others boot on the repaired one.
//!
//! The fsck runs on the build server (or here, on FreeBSD).

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::arch::Arch;
use crate::cli::Args;
use crate::qemu::{Vm, Vmm};
use crate::util::{self, Result};
use crate::{build, image, remote};

/// The workload, at `/usr/tests/crash/work.sh` on the memory disk: three
/// loops over small trees, so names, blocks, and inodes are reused while
/// their old state is still on the way to the disk.
const WORKLOAD: &str = r#"for d in /dev/vtbd[0-9]; do mount -t ufs $d /mnt 2>/dev/null && break; done
[ -n "$(mount | sed -n '\, on /mnt ,p')" ] || { echo crash-test-no-mount; exit 1; }
cd /mnt || exit 1
mkdir -p w && cd w || exit 1
(
  i=0
  while :; do
    i=$((i+1)); d=d$((i%7))
    mkdir -p $d/s$((i%3))
    echo $i > $d/f$((i%13))
    dd if=/rescue/rescue of=$d/b$((i%5)) bs=8k count=$((i%23)) 2>/dev/null
    mv $d/f$((i%13)) $d/s$((i%3))/g$((i%11)) 2>/dev/null
    ln $d/s$((i%3))/g$((i%11)) h$((i%9)) 2>/dev/null
    rm -f $d/b$(((i+2)%5)) h$(((i+4)%9))
    [ $((i%19)) -eq 0 ] && rm -rf d$(((i+3)%7))
    [ $((i%29)) -eq 0 ] && mv d$((i%7)) d$(((i+1)%7))/m$((i%2)) 2>/dev/null
  done
) > /dev/null 2>&1 &
(
  j=0
  while :; do
    j=$((j+1))
    dd if=/rescue/rescue of=big$((j%3)) bs=64k count=$((j%40)) conv=fsync 2>/dev/null
    dd if=/dev/zero of=big$(((j+1)%3)) bs=4k count=1 seek=$((j%300)) conv=notrunc 2>/dev/null
    : > big$(((j+2)%3))
    mkdir -p t$((j%4)) && echo $j > t$((j%4))/x
    rm -rf t$(((j+2)%4))
  done
) > /dev/null 2>&1 &
(
  k=0
  while :; do
    k=$((k+1)); mkdir -p r
    for n in 1 2 3 4 5 6 7 8; do echo $k > r/n$n; done
    mv r r$((k%2)) 2>/dev/null || rm -rf r
    rm -rf r$(((k+1)%2))
  done
) > /dev/null 2>&1 &
echo crash-test-started
"#;

/// `cargo xtask crash-test [--rounds N] [--arch A] [--seed S] [--debug]`.
pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let rounds: u32 = args
        .take_value("--rounds")?
        .map_or(Ok(10), |v| v.parse())
        .map_err(|_| "--rounds wants a number")?;
    let arch = args
        .take_value("--arch")?
        .map_or(Ok(Arch::X86_64), |a| Arch::parse(&a))?;
    let mut seed: u64 = args
        .take_value("--seed")?
        .map_or(Ok(1), |v| v.parse())
        .map_err(|_| "--seed wants a number")?;
    let release = !args.take_flag("--debug");
    args.finish()?;
    let script = util::root().join("build").join("crash-work.sh");
    std::fs::create_dir_all(script.parent().ok_or("no build directory")?)?;
    std::fs::write(&script, WORKLOAD)?;
    let disk = image::build_with(
        arch,
        build::Options {
            release,
            ktest: false,
        },
        "abyss.verifier=1",
        &[("usr/tests/crash/work.sh", &script)],
    )?;
    let dir = disk.parent().ok_or("no build directory")?.to_path_buf();
    let boot = dir.join("disk-crash.img");
    let crash = dir.join("crash-ufs.img");
    let checked = dir.join("crash-ufs-checked.img");
    newfs_soft(&crash)?;
    let mut kinds: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for round in 1..=rounds {
        std::fs::copy(&disk, &boot)?;
        // Up to 15 seconds of workload, decided here.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let after = Duration::from_millis(500 + seed % 15_000);
        let log = dir.join(format!("crash-{round}.log"));
        let vm = Vm {
            smp: 4,
            extra_disk: Some(crash.clone()),
            ..Vm::new(Vmm::Qemu, arch, boot.clone())
        };
        let mut session = crate::expect::Session::start(vm, &log)?;
        let started = (|| -> Result<()> {
            session.expect("# ", Duration::from_secs(240))?;
            session.send("sh /usr/tests/crash/work.sh\n")?;
            session.expect("crash-test-started", Duration::from_secs(30))
        })();
        if let Err(e) = started {
            session.kill();
            return Err(format!("round {round}: {e}; log in {}", log.display()).into());
        }
        std::thread::sleep(after);
        session.kill();
        // A boot on the crashed image mounted it unclean, as FreeBSD does.
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if round > 1 && round % 2 == 1 && !text.contains("was not properly dismounted") {
            return Err(
                format!("round {round}: no unclean mount; log in {}", log.display()).into(),
            );
        }
        // Check a copy; the next round boots on it or on the crashed image.
        std::fs::copy(&crash, &checked)?;
        let found = preen(&checked).map_err(|e| {
            format!(
                "round {round} ({after:?} in): {e}; image in {}",
                crash.display()
            )
        })?;
        for k in &found {
            *kinds.entry(k.clone()).or_default() += 1;
        }
        println!(
            "crash-test round {round}: killed {:.1} s in, preened ({} kinds of repair){}",
            after.as_secs_f64(),
            found.len(),
            if round % 2 == 0 {
                "; next boot unclean"
            } else {
                ""
            }
        );
        if round % 2 == 1 {
            std::fs::copy(&checked, &crash)?;
        }
    }
    println!("crash-test: {rounds} rounds preened; fsck repaired:");
    for (k, n) in &kinds {
        println!("  {n:4} {k}");
    }
    Ok(())
}

/// Makes `img` a 64 MiB UFS2 file system with soft updates (`newfs -U`),
/// here on FreeBSD, else on the build server.
fn newfs_soft(img: &Path) -> Result<()> {
    if cfg!(target_os = "freebsd") {
        std::fs::File::create(img)?.set_len(64 << 20)?;
        return util::run(
            Command::new("newfs")
                .arg("-U")
                .arg(img)
                .stdout(std::process::Stdio::null()),
        );
    }
    let remote_img = "abyss/crash-ufs.img";
    util::run(&mut remote::ssh(&format!(
        "rm -f {remote_img} && truncate -s 64m {remote_img} && newfs -U {remote_img} > /dev/null"
    )))?;
    fetch(remote_img, img)
}

fn fetch(remote_path: &str, local: &Path) -> Result<()> {
    util::run(
        Command::new("scp")
            .arg("-q")
            .arg(format!("{}:{remote_path}", remote::host()))
            .arg(local),
    )
}

fn send(local: &Path, remote_path: &str) -> Result<()> {
    util::run(
        Command::new("scp")
            .arg("-q")
            .arg(local)
            .arg(format!("{}:{remote_path}", remote::host())),
    )
}

/// Runs `script` (with the image's memory disk as `$dev`) where FreeBSD's
/// tools are; returns its output and whether it succeeded.
fn on_freebsd(img: &Path, script: &str) -> Result<(bool, String)> {
    let wrapped = |path: &str| {
        format!(
            "u=$(doas -n mdconfig -a -t vnode -f {path}) || exit 99; dev=/dev/$u; \
             ( {script} ); r=$?; doas -n mdconfig -d -u $u; exit $r"
        )
    };
    let out = if cfg!(target_os = "freebsd") {
        Command::new("sh")
            .arg("-c")
            .arg(wrapped(&img.display().to_string()))
            .output()?
    } else {
        remote::ssh(&wrapped("abyss/crash-check.img")).output()?
    };
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Ok((out.status.success(), text))
}

/// `fsck_ffs -p` until `fsck_ffs -f -n` is clean (a directory cleared
/// leaves what was in it to the next run); each preen must succeed.
/// Returns the kinds of repair preening made; the image ends repaired.
fn preen(img: &Path) -> std::result::Result<Vec<String>, String> {
    let remote_img = "abyss/crash-check.img";
    if !cfg!(target_os = "freebsd") {
        send(img, remote_img).map_err(|e| e.to_string())?;
    }
    let mut kinds = std::collections::BTreeSet::new();
    let mut log = String::new();
    for _ in 0..4 {
        let (ok, text) =
            on_freebsd(img, "doas -n fsck_ffs -p -f $dev").map_err(|e| e.to_string())?;
        log += &text;
        if !ok {
            return Err(format!("fsck_ffs -p failed:\n{log}"));
        }
        kinds.extend(repairs(&text));
        let (_, listing) =
            on_freebsd(img, "doas -n fsck_ffs -f -n $dev").map_err(|e| e.to_string())?;
        if !listing
            .lines()
            .any(|l| l.ends_with("? no") && !l.starts_with("CONTINUE"))
        {
            if !cfg!(target_os = "freebsd") {
                fetch(remote_img, img).map_err(|e| e.to_string())?;
            }
            return Ok(kinds.into_iter().collect());
        }
        log += &listing;
    }
    Err(format!("preening never finished:\n{log}"))
}

/// What a preen's output says it repaired: "/dev/mdN: WHAT I=..", maybe
/// details on later lines, then "(DONE)".
fn repairs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut what = String::new();
    for l in text.lines() {
        let l = l.split_once(": ").map_or(l, |(_, r)| r);
        let mut words: Vec<&str> = l.split_whitespace().collect();
        if words.first() == Some(&"INODE") {
            words.drain(..2.min(words.len()));
        }
        let lead: Vec<&str> = words
            .iter()
            .copied()
            .take_while(|t| {
                !t.contains('=')
                    && !t.starts_with(['(', '/'])
                    && !t.starts_with(|c: char| c.is_ascii_digit())
            })
            .collect();
        if !lead.is_empty() {
            what = lead.join(" ");
        }
        if let Some((_, done)) = l.rsplit_once('(') {
            let done = done.trim_end_matches(')');
            if !done.is_empty() && done.chars().all(|c| c.is_ascii_uppercase()) {
                out.push(format!("{what} ({done})"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_are_named() {
        let text = "/dev/md0: LINK COUNT DIR I=3  OWNER=root MODE=40775\n\
                    /dev/md0: SIZE=512 MTIME=Oct  9 03:53 2025  COUNT 6 SHOULD BE 5 (ADJUSTED)\n\
                    /dev/md0: UNREF FILE I=7  OWNER=x MODE=100644\n\
                    /dev/md0: SIZE=0 MTIME=Oct  9 03:53 2025 \n (CLEARED)\n\
                    /dev/md0: INODE 13: FILE SIZE 556333 BEYOND END OF ALLOCATED FILE, SIZE SHOULD BE 0 (ADJUSTED)\n\
                    /dev/md0: FREE BLK COUNT(S) WRONG IN SUPERBLK (SALVAGED)\n\
                    /dev/md0: 506 files, 1003 used, 14748 free (20 frags, 1841 blocks, 0.1% fragmentation)\n";
        assert_eq!(
            repairs(text),
            [
                "LINK COUNT DIR (ADJUSTED)",
                "UNREF FILE (CLEARED)",
                "FILE SIZE (ADJUSTED)",
                "FREE BLK COUNT(S) WRONG IN SUPERBLK (SALVAGED)",
            ]
        );
    }
}
