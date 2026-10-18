// SPDX-License-Identifier: BSD-2-Clause
//! Running xtask commands on the FreeBSD build server.
//!
//! The working tree (not just commits) is mirrored with rsync, so remote runs
//! test exactly what is on disk locally.

use std::process::Command;

use crate::util::{self, Result};

const DEFAULT_HOST: &str = "192.168.0.31";
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
    )
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    sync()?;
    let quoted: Vec<String> = args.iter().map(|a| quote(a)).collect();
    let script = format!(
        "cd {REMOTE_DIR} && PATH=$HOME/.cargo/bin:$PATH cargo xtask {}",
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
