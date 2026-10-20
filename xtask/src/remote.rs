// SPDX-License-Identifier: BSD-2-Clause
//! Running xtask commands on the FreeBSD build server.
//!
//! The working tree (not just commits) is mirrored with rsync, so remote runs
//! test exactly what is on disk locally.

use std::process::Command;

use crate::util::{self, Result};

/// An ssh alias (`Host abyss-build` in ~/.ssh/config), so an address change is
/// a one-line edit outside the repository.
const DEFAULT_HOST: &str = "abyss-build";
/// Mirror location, relative to the remote home directory.
pub(crate) const REMOTE_DIR: &str = "abyss/src";

pub(crate) fn host() -> String {
    std::env::var("ABYSS_BUILD_HOST").unwrap_or_else(|_| DEFAULT_HOST.into())
}

/// Quotes a word for a POSIX shell.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// An `ssh` command running `script` on the build server.
pub(crate) fn ssh(script: &str) -> Command {
    let mut c = Command::new("ssh");
    c.args(["-o", "BatchMode=yes", &host(), script]);
    c
}

/// Mirrors the working tree to the build server.
pub(crate) fn sync() -> Result<()> {
    let host = host();
    util::run(&mut ssh(&format!("mkdir -p {REMOTE_DIR}")))?;
    let mut src = util::root().into_os_string();
    src.push("/");
    util::run(
        Command::new("rsync")
            .args([
                "-az",
                "--delete",
                "--exclude=/target/",
                "--exclude=/build/",
                "--exclude=/.git/",
            ])
            .arg(src)
            .arg(format!("{host}:{REMOTE_DIR}/")),
    )?;
    // The FreeBSD sources third-party programs build from, beside the
    // mirror as beside this tree (the server has no FreeBSD tree).
    let freebsd = util::root().join("../freebsd-src");
    if !freebsd.exists() {
        return Ok(());
    }
    let mut c = Command::new("rsync");
    c.args(["-az", "--delete", "--relative"]);
    for part in crate::base::FREEBSD_SOURCES {
        let mut p = freebsd.clone().into_os_string();
        p.push("/./");
        p.push(part);
        c.arg(p);
    }
    util::run(c.arg(format!("{host}:{REMOTE_DIR}/../freebsd-src/")))
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    sync()?;
    let quoted: Vec<String> = args.iter().map(|a| quote(a)).collect();
    // The mirror has no .git; tell the remote side what it is running.
    let rev =
        util::output(Command::new("git").args(["describe", "--always", "--dirty", "--abbrev=12"]))
            .map_or_else(|_| "unknown".into(), |r| r.trim().to_owned());
    let script = format!(
        "cd {REMOTE_DIR} && ABYSS_REV={} PATH=$HOME/.cargo/bin:$PATH cargo xtask {}",
        quote(&rev),
        quoted.join(" ")
    );
    eprintln!(
        "xtask: running on {}: cargo xtask {}",
        host(),
        args.join(" ")
    );
    util::run(&mut ssh(&script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
