// SPDX-License-Identifier: BSD-2-Clause
//! A small argument parser: flags, `--key value` options, positionals.

use crate::util::Result;

/// Command-line arguments not yet consumed.
#[derive(Debug)]
pub(crate) struct Args {
    rest: Vec<String>,
}

impl Args {
    pub(crate) fn from_env() -> Self {
        Self {
            rest: std::env::args().skip(1).collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from(v: &[&str]) -> Self {
        Self {
            rest: v.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Removes `flag` if present.
    pub(crate) fn take_flag(&mut self, flag: &str) -> bool {
        match self.rest.iter().position(|a| a == flag) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }

    /// Removes `key value` (or `key=value`) if present.
    pub(crate) fn take_value(&mut self, key: &str) -> Result<Option<String>> {
        let prefix = format!("{key}=");
        if let Some(i) = self.rest.iter().position(|a| a.starts_with(&prefix)) {
            let v = self.rest.remove(i)[prefix.len()..].to_owned();
            return Ok(Some(v));
        }
        match self.rest.iter().position(|a| a == key) {
            Some(i) if i + 1 < self.rest.len() => {
                self.rest.remove(i);
                Ok(Some(self.rest.remove(i)))
            }
            Some(_) => Err(format!("{key} needs a value").into()),
            None => Ok(None),
        }
    }

    /// Removes and returns the first argument that is not an option.
    pub(crate) fn next_positional(&mut self) -> Option<String> {
        let i = self.rest.iter().position(|a| !a.starts_with('-'))?;
        Some(self.rest.remove(i))
    }

    /// Fails if anything was not consumed.
    pub(crate) fn finish(self) -> Result<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(format!("unexpected arguments: {}", self.rest.join(" ")).into())
        }
    }

    pub(crate) fn into_remaining(self) -> Vec<String> {
        self.rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed() {
        let mut a = Args::from(&["run", "--arch", "x86_64", "--release", "--smp=4"]);
        assert_eq!(a.next_positional().as_deref(), Some("run"));
        assert!(a.take_flag("--release"));
        assert!(!a.take_flag("--gdb"));
        assert_eq!(a.take_value("--arch").unwrap().as_deref(), Some("x86_64"));
        assert_eq!(a.take_value("--smp").unwrap().as_deref(), Some("4"));
        a.finish().unwrap();
    }

    #[test]
    fn leftovers_are_errors() {
        let mut a = Args::from(&["--bogus"]);
        assert!(!a.take_flag("--release"));
        assert!(a.finish().is_err());
        let mut b = Args::from(&["--arch"]);
        assert!(b.take_value("--arch").is_err());
    }
}
