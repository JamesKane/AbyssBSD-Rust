// SPDX-License-Identifier: BSD-2-Clause
//! Privileges (FreeBSD's `PRIV_*`, sys/sys/priv.h), with FreeBSD's numbers:
//! those of the subsystems AbyssBSD has, and the few network ones the jail
//! policy names. Each is what one superuser override in the kernel asks
//! for; [`crate::Cred::priv_check`] grants or refuses it.

/// One privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Priv {
    /// `PRIV_ACCT`: Manage process accounting.
    Acct = 2,
    /// `PRIV_MAXFILES`: Exceed system open files limit.
    Maxfiles = 3,
    /// `PRIV_MAXPROC`: Exceed system processes limit.
    Maxproc = 4,
    /// `PRIV_KTRACE`: Set/clear KTRFAC_ROOT on ktrace.
    Ktrace = 5,
    /// `PRIV_SETDUMPER`: Configure dump device.
    Setdumper = 6,
    /// `PRIV_REBOOT`: Can reboot system.
    Reboot = 8,
    /// `PRIV_SWAPON`: Can swapon().
    Swapon = 9,
    /// `PRIV_SWAPOFF`: Can swapoff().
    Swapoff = 10,
    /// `PRIV_MSGBUF`: Can read kernel message buffer.
    Msgbuf = 11,
    /// `PRIV_IO`: Can perform low-level I/O.
    Io = 12,
    /// `PRIV_KEYBOARD`: Reprogram keyboard.
    Keyboard = 13,
    /// `PRIV_DRIVER`: Low-level driver privilege.
    Driver = 14,
    /// `PRIV_ADJTIME`: Set time adjustment.
    Adjtime = 15,
    /// `PRIV_NTP_ADJTIME`: Set NTP time adjustment.
    NtpAdjtime = 16,
    /// `PRIV_CLOCK_SETTIME`: Can call clock_settime.
    ClockSettime = 17,
    /// `PRIV_SETTIMEOFDAY`: Can call settimeofday.
    Settimeofday = 18,
    /// `PRIV_AUDIT_CONTROL`: Can configure audit.
    AuditControl = 40,
    /// `PRIV_AUDIT_FAILSTOP`: Can run during audit fail stop.
    AuditFailstop = 41,
    /// `PRIV_AUDIT_GETAUDIT`: Can get proc audit properties.
    AuditGetaudit = 42,
    /// `PRIV_AUDIT_SETAUDIT`: Can set proc audit properties.
    AuditSetaudit = 43,
    /// `PRIV_AUDIT_SUBMIT`: Can submit an audit record.
    AuditSubmit = 44,
    /// `PRIV_CRED_SETUID`: setuid.
    CredSetuid = 50,
    /// `PRIV_CRED_SETEUID`: seteuid to !ruid and !svuid.
    CredSeteuid = 51,
    /// `PRIV_CRED_SETGID`: setgid.
    CredSetgid = 52,
    /// `PRIV_CRED_SETEGID`: setgid to !rgid and !svgid.
    CredSetegid = 53,
    /// `PRIV_CRED_SETGROUPS`: Set process additional groups.
    CredSetgroups = 54,
    /// `PRIV_CRED_SETREUID`: setreuid.
    CredSetreuid = 55,
    /// `PRIV_CRED_SETREGID`: setregid.
    CredSetregid = 56,
    /// `PRIV_CRED_SETRESUID`: setresuid.
    CredSetresuid = 57,
    /// `PRIV_CRED_SETRESGID`: setresgid.
    CredSetresgid = 58,
    /// `PRIV_SEEOTHERGIDS`: Exempt bsd.seeothergids.
    Seeothergids = 59,
    /// `PRIV_SEEOTHERUIDS`: Exempt bsd.seeotheruids.
    Seeotheruids = 60,
    /// `PRIV_SEEJAILPROC`: Exempt from bsd.see_jail_proc.
    Seejailproc = 61,
    /// `PRIV_CRED_SETCRED`: setcred.
    CredSetcred = 62,
    /// `PRIV_DEBUG_DIFFCRED`: Exempt debugging other users.
    DebugDiffcred = 80,
    /// `PRIV_DEBUG_SUGID`: Exempt debugging setuid proc.
    DebugSugid = 81,
    /// `PRIV_DEBUG_UNPRIV`: Exempt unprivileged debug limit.
    DebugUnpriv = 82,
    /// `PRIV_DEBUG_DENIED`: Exempt P2_NOTRACE.
    DebugDenied = 83,
    /// `PRIV_DEBUG_DIFFJAIL`: Exempt debugging other jails.
    DebugDiffjail = 84,
    /// `PRIV_FIRMWARE_LOAD`: Can load firmware.
    FirmwareLoad = 100,
    /// `PRIV_JAIL_ATTACH`: Attach to a jail.
    JailAttach = 110,
    /// `PRIV_JAIL_SET`: Set jail parameters.
    JailSet = 111,
    /// `PRIV_JAIL_REMOVE`: Remove a jail.
    JailRemove = 112,
    /// `PRIV_KENV_SET`: Set kernel env. variables.
    KenvSet = 120,
    /// `PRIV_KENV_UNSET`: Unset kernel env. variables.
    KenvUnset = 121,
    /// `PRIV_KLD_LOAD`: Load a kernel module.
    KldLoad = 130,
    /// `PRIV_KLD_UNLOAD`: Unload a kernel module.
    KldUnload = 131,
    /// `PRIV_PROC_LIMIT`: Exceed user process limit.
    ProcLimit = 160,
    /// `PRIV_PROC_SETLOGIN`: Can call setlogin.
    ProcSetlogin = 161,
    /// `PRIV_PROC_SETRLIMIT`: Can raise resources limits.
    ProcSetrlimit = 162,
    /// `PRIV_PROC_SETLOGINCLASS`: Can call setloginclass(2).
    ProcSetloginclass = 163,
    /// `PRIV_IPC_READ`: Can override IPC read perm.
    IpcRead = 170,
    /// `PRIV_IPC_WRITE`: Can override IPC write perm.
    IpcWrite = 171,
    /// `PRIV_IPC_ADMIN`: Can override IPC owner-only perm.
    IpcAdmin = 172,
    /// `PRIV_IPC_MSGSIZE`: Exempt IPC message queue limit.
    IpcMsgsize = 173,
    /// `PRIV_MQ_ADMIN`: Can override msgq owner-only perm.
    MqAdmin = 180,
    /// `PRIV_SCHED_DIFFCRED`: Exempt scheduling other users.
    SchedDiffcred = 200,
    /// `PRIV_SCHED_SETPRIORITY`: Can set lower nice value for proc.
    SchedSetpriority = 201,
    /// `PRIV_SCHED_RTPRIO`: Can set real time scheduling.
    SchedRtprio = 202,
    /// `PRIV_SCHED_SETPOLICY`: Can set scheduler policy.
    SchedSetpolicy = 203,
    /// `PRIV_SCHED_SET`: Can set thread scheduler.
    SchedSet = 204,
    /// `PRIV_SCHED_SETPARAM`: Can set thread scheduler params.
    SchedSetparam = 205,
    /// `PRIV_SCHED_CPUSET`: Can manipulate cpusets.
    SchedCpuset = 206,
    /// `PRIV_SCHED_CPUSET_INTR`: Can adjust IRQ to CPU binding.
    SchedCpusetIntr = 207,
    /// `PRIV_SCHED_IDPRIO`: Can set idle time scheduling.
    SchedIdprio = 208,
    /// `PRIV_SCHED_DIFFJAIL`: Exempt scheduling other jails.
    SchedDiffjail = 209,
    /// `PRIV_SEM_WRITE`: Can override sem write perm.
    SemWrite = 220,
    /// `PRIV_SIGNAL_DIFFCRED`: Exempt signalling other users.
    SignalDiffcred = 230,
    /// `PRIV_SIGNAL_SUGID`: Non-conserv signal setuid proc.
    SignalSugid = 231,
    /// `PRIV_SIGNAL_DIFFJAIL`: Exempt signalling other jails.
    SignalDiffjail = 232,
    /// `PRIV_SYSCTL_DEBUG`: Can invoke sysctl.debug.
    SysctlDebug = 240,
    /// `PRIV_SYSCTL_WRITE`: Can write sysctls.
    SysctlWrite = 241,
    /// `PRIV_SYSCTL_WRITEJAIL`: Can write sysctls, jail permitted.
    SysctlWritejail = 242,
    /// `PRIV_SYSCTL_MEMLOCK`: Large requests are not serialized.
    SysctlMemlock = 243,
    /// `PRIV_TTY_CONSOLE`: Set console to tty.
    TtyConsole = 250,
    /// `PRIV_TTY_DRAINWAIT`: Set tty drain wait time.
    TtyDrainwait = 251,
    /// `PRIV_TTY_DTRWAIT`: Set DTR wait on tty.
    TtyDtrwait = 252,
    /// `PRIV_TTY_EXCLUSIVE`: Override tty exclusive flag.
    TtyExclusive = 253,
    /// `PRIV_TTY_STI`: Simulate input on another tty.
    TtySti = 255,
    /// `PRIV_TTY_SETA`: Set tty termios structure.
    TtySeta = 256,
    /// `PRIV_UFS_EXTATTRCTL`: Can configure EAs on UFS1.
    UfsExtattrctl = 270,
    /// `PRIV_UFS_QUOTAOFF`: quotaoff().
    UfsQuotaoff = 271,
    /// `PRIV_UFS_QUOTAON`: quotaon().
    UfsQuotaon = 272,
    /// `PRIV_UFS_SETUSE`: setuse().
    UfsSetuse = 273,
    /// `PRIV_VFS_READ`: Override vnode DAC read perm.
    VfsRead = 310,
    /// `PRIV_VFS_WRITE`: Override vnode DAC write perm.
    VfsWrite = 311,
    /// `PRIV_VFS_ADMIN`: Override vnode DAC admin perm.
    VfsAdmin = 312,
    /// `PRIV_VFS_EXEC`: Override vnode DAC exec perm.
    VfsExec = 313,
    /// `PRIV_VFS_LOOKUP`: Override vnode DAC lookup perm.
    VfsLookup = 314,
    /// `PRIV_VFS_BLOCKRESERVE`: Can use free block reserve.
    VfsBlockreserve = 315,
    /// `PRIV_VFS_CHFLAGS_DEV`: Can chflags() a device node.
    VfsChflagsDev = 316,
    /// `PRIV_VFS_CHOWN`: Can set user; group to non-member.
    VfsChown = 317,
    /// `PRIV_VFS_CHROOT`: chroot().
    VfsChroot = 318,
    /// `PRIV_VFS_RETAINSUGID`: Can retain sugid bits on change.
    VfsRetainsugid = 319,
    /// `PRIV_VFS_EXCEEDQUOTA`: Exempt from quota restrictions.
    VfsExceedquota = 320,
    /// `PRIV_VFS_EXTATTR_SYSTEM`: Operate on system EA namespace.
    VfsExtattrSystem = 321,
    /// `PRIV_VFS_FCHROOT`: fchroot().
    VfsFchroot = 322,
    /// `PRIV_VFS_FHOPEN`: Can fhopen().
    VfsFhopen = 323,
    /// `PRIV_VFS_FHSTAT`: Can fhstat().
    VfsFhstat = 324,
    /// `PRIV_VFS_FHSTATFS`: Can fhstatfs().
    VfsFhstatfs = 325,
    /// `PRIV_VFS_GENERATION`: stat() returns generation number.
    VfsGeneration = 326,
    /// `PRIV_VFS_GETFH`: Can retrieve file handles.
    VfsGetfh = 327,
    /// `PRIV_VFS_GETQUOTA`: getquota().
    VfsGetquota = 328,
    /// `PRIV_VFS_LINK`: bsd.hardlink_check_uid.
    VfsLink = 329,
    /// `PRIV_VFS_MKNOD_BAD`: Was: mknod() can mark bad inodes.
    VfsMknodBad = 330,
    /// `PRIV_VFS_MKNOD_DEV`: Can mknod() to create dev nodes.
    VfsMknodDev = 331,
    /// `PRIV_VFS_MKNOD_WHT`: Can mknod() to create whiteout.
    VfsMknodWht = 332,
    /// `PRIV_VFS_MOUNT`: Can mount().
    VfsMount = 333,
    /// `PRIV_VFS_MOUNT_OWNER`: Can manage other users' file systems.
    VfsMountOwner = 334,
    /// `PRIV_VFS_MOUNT_EXPORTED`: Can set MNT_EXPORTED on mount.
    VfsMountExported = 335,
    /// `PRIV_VFS_MOUNT_PERM`: Override dev node perms at mount.
    VfsMountPerm = 336,
    /// `PRIV_VFS_MOUNT_SUIDDIR`: Can set MNT_SUIDDIR on mount.
    VfsMountSuiddir = 337,
    /// `PRIV_VFS_MOUNT_NONUSER`: Can perform a non-user mount.
    VfsMountNonuser = 338,
    /// `PRIV_VFS_SETGID`: Can setgid if not in group.
    VfsSetgid = 339,
    /// `PRIV_VFS_SETQUOTA`: setquota().
    VfsSetquota = 340,
    /// `PRIV_VFS_STICKYFILE`: Can set sticky bit on file.
    VfsStickyfile = 341,
    /// `PRIV_VFS_SYSFLAGS`: Can modify system flags.
    VfsSysflags = 342,
    /// `PRIV_VFS_UNMOUNT`: Can unmount().
    VfsUnmount = 343,
    /// `PRIV_VFS_STAT`: Override vnode MAC stat perm.
    VfsStat = 344,
    /// `PRIV_VFS_READ_DIR`: Can read(2) a dirfd, needs sysctl.
    VfsReadDir = 345,
    /// `PRIV_VM_MADV_PROTECT`: Can set MADV_PROTECT.
    VmMadvProtect = 360,
    /// `PRIV_VM_MLOCK`: Can mlock(), mlockall().
    VmMlock = 361,
    /// `PRIV_VM_MUNLOCK`: Can munlock(), munlockall().
    VmMunlock = 362,
    /// `PRIV_VM_SWAP_NOQUOTA`: * Can override the global * swap reservation limits.
    VmSwapNoquota = 363,
    /// `PRIV_VM_SWAP_NORLIMIT`: * Can override the per-uid * swap reservation limits.
    VmSwapNorlimit = 364,
    /// `PRIV_DEVFS_RULE`: Can manage devfs rules.
    DevfsRule = 370,
    /// `PRIV_DEVFS_SYMLINK`: Can create symlinks in devfs.
    DevfsSymlink = 371,
    /// `PRIV_RANDOM_RESEED`: Closing /dev/random reseeds.
    RandomReseed = 380,
    /// `PRIV_NET_RAW`: Open raw socket.
    NetRaw = 395,
    /// `PRIV_NET_ROUTE`: Administer routing.
    NetRoute = 396,
    /// `PRIV_NET_SETIFMTU`: Set interface MTU.
    NetSetifmtu = 398,
    /// `PRIV_NET_SETIFFLAGS`: Set interface flags.
    NetSetifflags = 399,
    /// `PRIV_NET_SETIFCAP`: Set interface capabilities.
    NetSetifcap = 400,
    /// `PRIV_NET_SETIFNAME`: Set interface name.
    NetSetifname = 401,
    /// `PRIV_NET_SETIFMETRIC`: Set interface metrics.
    NetSetifmetric = 402,
    /// `PRIV_NET_ADDIFGROUP`: Add new interface group.
    NetAddifgroup = 409,
    /// `PRIV_NET_DELIFGROUP`: Delete interface group.
    NetDelifgroup = 410,
    /// `PRIV_NET_IFCREATE`: Create cloned interface.
    NetIfcreate = 411,
    /// `PRIV_NET_ADDIFADDR`: Add protocol addr to interface.
    NetAddifaddr = 413,
    /// `PRIV_NET_DELIFADDR`: Delete protocol addr on interface.
    NetDelifaddr = 414,
    /// `PRIV_NET_SETIFDESCR`: Set interface description.
    NetSetifdescr = 418,
    /// `PRIV_NETINET_RESERVEDPORT`: Bind low port number.
    NetinetReservedport = 490,
    /// `PRIV_NETINET_RAW`: Open netinet raw socket.
    NetinetRaw = 497,
    /// `PRIV_NETINET_GETCRED`: Query netinet pcb credentials.
    NetinetGetcred = 498,
    /// `PRIV_NETINET_ADDRCTRL6`: Administer IPv6 address scopes.
    NetinetAddrctl6 = 499,
    /// `PRIV_NETINET_ND6`: Administer IPv6 neighbor disc.
    NetinetNd6 = 500,
    /// `PRIV_NETINET_REUSEPORT`: Allow [rapid] port/address reuse.
    NetinetReuseport = 504,
    /// `PRIV_NETINET_BINDANY`: Allow bind to any address.
    NetinetBindany = 506,
    /// `PRIV_PIPEBUF`: Allow to allocate reserved pipebuf space.
    Pipebuf = 560,
    /// `PRIV_KMEM_READ`: Open mem/kmem for reading.
    KmemRead = 680,
    /// `PRIV_KMEM_WRITE`: Open mem/kmem for writing.
    KmemWrite = 681,
    /// `PRIV_PROC_MEM_WRITE`: Writes via proc_rwmem.
    ProcMemWrite = 682,
    /// `PRIV_VMM_PPTDEV`: Can manipulate ppt devices.
    VmmPptdev = 710,
}
