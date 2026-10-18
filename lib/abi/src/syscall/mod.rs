// SPDX-License-Identifier: BSD-2-Clause
//! The FreeBSD syscall table.
//!
//! [`generated`] is produced from `sys/kern/syscalls.master` by
//! `cargo xtask syscallgen`; this module defines the types it uses.

mod generated;

pub use generated::{LIVE_COUNT, SLOT_COUNT, SLOTS, SYSCALLS, Sysno};

/// A compatibility generation, for slots only old binaries use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompatLevel {
    /// 4.3BSD interfaces.
    Bsd43,
    /// FreeBSD n.x interfaces.
    FreeBsd(u8),
}

/// What a syscall number refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// A syscall current binaries use.
    Live(Sysno),
    /// An interface kept only for old binaries; AbyssBSD returns `ENOSYS`.
    Compat {
        /// Generation the interface belongs to.
        level: CompatLevel,
        /// Its historical name.
        name: &'static str,
    },
    /// A removed syscall, with its historical name.
    Obsolete(&'static str),
    /// Reserved for local or vendor use.
    Reserved,
    /// A placeholder.
    Unimplemented,
    /// Reserved for syscalls registered at run time (FreeBSD's `lkmnosys`
    /// slots). AbyssBSD has no loadable modules, so these are `ENOSYS`.
    Module,
}

/// One argument of a syscall prototype.
#[derive(Clone, Copy, Debug)]
pub struct ArgInfo {
    /// C type.
    pub ty: &'static str,
    /// Parameter name.
    pub name: &'static str,
    /// SAL / `_Contains_` annotations.
    pub annotations: &'static str,
}

/// A live syscall's prototype and properties.
#[derive(Clone, Copy, Debug)]
pub struct SyscallInfo {
    /// Its number.
    pub sysno: Sysno,
    /// C name.
    pub name: &'static str,
    /// C return type.
    pub ret: &'static str,
    /// Fixed arguments.
    pub args: &'static [ArgInfo],
    /// Whether the prototype is variadic.
    pub variadic: bool,
    /// Allowed in Capsicum capability mode.
    pub cap_enabled: bool,
    /// Never returns.
    pub no_return: bool,
    /// Provided by a loadable module in FreeBSD.
    pub loadable: bool,
}

impl Sysno {
    /// The syscall's description.
    pub fn info(self) -> &'static SyscallInfo {
        &SYSCALLS[self.index()]
    }
}

/// Looks up a raw syscall number.
pub fn lookup(number: u64) -> Option<Slot> {
    usize::try_from(number)
        .ok()
        .and_then(|n| SLOTS.get(n))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_numbers() {
        assert_eq!(Sysno::Exit as u32, 1);
        assert_eq!(Sysno::Read as u32, 3);
        assert_eq!(Sysno::Write as u32, 4);
        assert_eq!(lookup(3), Some(Slot::Live(Sysno::Read)));
        assert_eq!(lookup(u64::MAX), None);
    }

    #[test]
    fn info_matches_variant() {
        for s in Sysno::ALL {
            assert_eq!(s.info().sysno, s);
            assert_eq!(lookup(u64::from(s as u32)), Some(Slot::Live(s)));
        }
        assert!(Sysno::Exit.info().no_return);
        assert!(Sysno::Read.info().cap_enabled);
    }

    #[test]
    fn compat_slots_are_not_live() {
        // 8 was 4.3BSD creat(2).
        assert!(matches!(
            lookup(8),
            Some(Slot::Compat {
                level: CompatLevel::Bsd43,
                ..
            })
        ));
    }
}
