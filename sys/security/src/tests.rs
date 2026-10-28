// SPDX-License-Identifier: BSD-2-Clause
//! Host tests: the set-id rules, the privilege policies, and visibility.

use alloc::sync::Arc;
use core::any::Any;

use abi::errno::Errno;

use crate::{Cred, Ids, Jail, Priv, SetCred, allow};

#[derive(Debug)]
struct TestJail {
    id: i32,
    parent: Option<Arc<dyn Jail>>,
    allow: u32,
    securelevel: i32,
}

impl Jail for TestJail {
    fn id(&self) -> i32 {
        self.id
    }
    fn parent(&self) -> Option<Arc<dyn Jail>> {
        self.parent.clone()
    }
    fn allow(&self) -> u32 {
        self.allow
    }
    fn enforce_statfs(&self) -> i32 {
        2
    }
    fn securelevel(&self) -> i32 {
        self.securelevel
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn jail(id: i32, parent: Option<Arc<dyn Jail>>, allow: u32) -> Arc<dyn Jail> {
    Arc::new(TestJail {
        id,
        parent,
        allow,
        securelevel: -1,
    })
}

fn jailed(c: &Cred, j: &Arc<dyn Jail>) -> Cred {
    let mut c = c.clone();
    c.jail = Some(Arc::clone(j));
    c
}

#[test]
fn setuid_follows_appendix_b() {
    let root = Cred::root();
    let c = root.setuid(1001).unwrap();
    assert_eq!((c.ruid, c.uid, c.svuid), (1001, 1001, 1001));
    assert_eq!(c.setuid(0), Err(Errno::EPERM));
    // An unprivileged setuid to the saved uid moves only the effective.
    let c = root
        .setresuid(Ids {
            real: Some(5),
            effective: Some(5),
            saved: Some(7),
        })
        .unwrap();
    let d = c.setuid(7).unwrap();
    assert_eq!((d.ruid, d.uid, d.svuid), (5, 7, 7));
    let e = c.seteuid(7).unwrap();
    assert_eq!((e.ruid, e.uid, e.svuid), (5, 7, 7));
    assert_eq!(c.seteuid(9), Err(Errno::EPERM));
}

#[test]
fn setreuid_moves_the_saved_uid() {
    let c = Cred::user(5, 5, &[]).setreuid(None, None).unwrap();
    assert_eq!(c.svuid, 5);
    let root = Cred::root();
    let c = root.setreuid(Some(3), Some(4)).unwrap();
    assert_eq!((c.ruid, c.uid, c.svuid), (3, 4, 4));
    // Swapping real and effective is allowed (the saved uid permits it).
    let s = c.setreuid(Some(4), Some(3)).unwrap();
    assert_eq!((s.ruid, s.uid, s.svuid), (4, 3, 3));
}

#[test]
fn groups_are_normalized_and_privileged() {
    let c = Cred::root().setgroups(&[5, 3, 5, 1]).unwrap();
    assert_eq!(c.groups, [1, 3, 5]);
    assert!(c.in_group(3) && c.in_group(0) && !c.in_group(2));
    let u = Cred::user(10, 10, &[]);
    assert_eq!(u.setgroups(&[1]), Err(Errno::EPERM));
    assert_eq!(
        Cred::root().setgroups(&[0; crate::NGROUPS_MAX + 1]),
        Err(Errno::EINVAL)
    );
}

#[test]
fn setcred_needs_its_privilege() {
    let w = SetCred {
        uid: Some(1),
        ruid: Some(2),
        groups: Some([9, 8].into()),
        ..SetCred::default()
    };
    let c = Cred::root().setcred(&w).unwrap();
    assert_eq!(
        (c.uid, c.ruid, c.svuid, &c.groups[..]),
        (1, 2, 0, &[8, 9][..])
    );
    assert_eq!(c.setcred(&w), Err(Errno::EPERM));
}

#[test]
fn root_holds_privileges_unless_a_jail_refuses_them() {
    let root = Cred::root();
    assert_eq!(root.priv_check(Priv::Reboot), Ok(()));
    assert_eq!(
        Cred::user(1, 1, &[]).priv_check(Priv::Reboot),
        Err(Errno::EPERM)
    );
    // Anyone may mlock and read the message buffer by default.
    assert_eq!(Cred::user(1, 1, &[]).priv_check(Priv::VmMlock), Ok(()));
    let j = jail(1, None, allow::SUSER | allow::SET_HOSTNAME);
    let jr = jailed(&root, &j);
    assert_eq!(jr.priv_check(Priv::Reboot), Err(Errno::EPERM));
    assert_eq!(jr.priv_check(Priv::VfsAdmin), Ok(()));
    assert_eq!(jr.priv_check(Priv::VfsSysflags), Err(Errno::EPERM));
    assert_eq!(
        jr.priv_check(Priv::VmMlock),
        Err(Errno::EPERM),
        "allow.mlock is off"
    );
    assert_eq!(jr.priv_check(Priv::VfsGeneration), Err(Errno::EPERM));
    // allow.nosuser: root in it is no superuser.
    let ns = jailed(&root, &jail(2, None, 0));
    assert_eq!(ns.priv_check(Priv::VfsAdmin), Err(Errno::EPERM));
    assert!(!ns.is_root());
    // PRIV_MAXPROC goes by the real uid.
    let c = root.seteuid(5).unwrap();
    assert_eq!(c.priv_check(Priv::Maxproc), Ok(()));
    assert_eq!(c.priv_check(Priv::Reboot), Err(Errno::EPERM));
}

#[test]
fn a_jail_sees_only_itself_and_below() {
    let a = jail(1, None, allow::SUSER);
    let b = jail(2, Some(Arc::clone(&a)), allow::SUSER);
    let host = Cred::root();
    let in_a = jailed(&host, &a);
    let in_b = jailed(&host, &b);
    assert_eq!(host.prison_check(&in_b), Ok(()));
    assert_eq!(in_a.prison_check(&in_b), Ok(()));
    assert_eq!(in_b.prison_check(&in_a), Err(Errno::ESRCH));
    assert_eq!(in_a.prison_check(&host), Err(Errno::ESRCH));
    assert_eq!(in_a.can_see(&in_a), Ok(()));
}

#[test]
fn securelevel_never_falls_below_the_systems() {
    let mut c = Cred::root();
    assert_eq!(c.securelevel(), -1);
    assert_eq!(c.securelevel_gt(0), Ok(()));
    c.jail = Some(Arc::new(TestJail {
        id: 3,
        parent: None,
        allow: 0,
        securelevel: 2,
    }));
    assert_eq!(c.securelevel_gt(1), Err(Errno::EPERM));
    assert_eq!(c.securelevel_ge(2), Err(Errno::EPERM));
    assert_eq!(c.securelevel_ge(3), Ok(()));
}

#[test]
fn signals_need_matching_ids_and_spare_set_id_programs() {
    use abi::signal::{SIGHUP, SIGSEGV};
    let me = Cred::user(5, 5, &[]);
    let other = Cred::user(6, 6, &[]);
    assert_eq!(me.can_signal(&other, false, SIGHUP), Err(Errno::EPERM));
    assert_eq!(Cred::root().can_signal(&other, false, SIGHUP), Ok(()));
    // A set-uid program run by me: its saved uid is the owner's, its real
    // uid mine, so I may signal it, but not with what could break it.
    let setuid = me
        .setresuid(Ids {
            real: None,
            effective: None,
            saved: None,
        })
        .unwrap();
    let mut prog = setuid.clone();
    (prog.uid, prog.svuid) = (0, 0);
    assert_eq!(me.can_signal(&prog, true, SIGHUP), Ok(()));
    assert_eq!(me.can_signal(&prog, true, SIGSEGV), Err(Errno::EPERM));
    assert_eq!(me.can_sched(&prog), Ok(()));
    assert!(!me.xids_subset(&prog));
    assert_eq!(me.can_debug(&prog, true), Err(Errno::EPERM));
    assert_eq!(me.can_debug(&me.clone(), false), Ok(()));
}
