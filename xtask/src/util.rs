// SPDX-License-Identifier: BSD-2-Clause
//! Errors and process helpers.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// An error with a human-readable message.
#[derive(Debug)]
pub(crate) struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(e.to_string())
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// The workspace root (the parent of `xtask/`).
pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

/// Runs a command to completion, failing on a non-zero exit.
pub(crate) fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} failed ({status})", describe(cmd)).into())
    }
}

/// Runs a command and returns its standard output.
pub(crate) fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{} failed ({}):\n{stderr}", describe(cmd), out.status).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `cargo`, as invoked by the outer `cargo xtask`.
pub(crate) fn cargo() -> Command {
    let mut c = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    c.current_dir(root());
    c
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}
