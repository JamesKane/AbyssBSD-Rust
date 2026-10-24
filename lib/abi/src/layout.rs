// SPDX-License-Identifier: BSD-2-Clause
//! Machine-readable layouts of every boundary structure, for
//! `cargo xtask abi-check`.
//!
//! Offsets and sizes come from the Rust definitions themselves
//! (`offset_of!`, `size_of`), so the check compares what the kernel will
//! actually use against what FreeBSD's C compiler computes.

use crate::{bus, consio, evdev, kbio, md, mouse, signal, socket, stat, time, ucontext, vdso};

/// Architectures a layout applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arches {
    /// Identical on every supported 64-bit architecture.
    All,
    /// amd64 only.
    Amd64,
    /// arm64 only.
    Arm64,
}

/// The layout of one field.
#[derive(Clone, Copy, Debug)]
pub struct FieldLayout {
    /// Member designator in C (may be a macro such as `sa_handler`).
    pub c_name: &'static str,
    /// Byte offset.
    pub offset: usize,
    /// Size in bytes.
    pub size: usize,
}

/// The layout of one structure.
#[derive(Clone, Copy, Debug)]
pub struct StructLayout {
    /// The C type, e.g. `struct stat`.
    pub c_type: &'static str,
    /// Headers that declare it, in include order.
    pub headers: &'static [&'static str],
    /// Which architectures share this layout.
    pub arches: Arches,
    /// `sizeof`.
    pub size: usize,
    /// `_Alignof`.
    pub align: usize,
    /// Every field the kernel reads or writes.
    pub fields: &'static [FieldLayout],
}

/// An ABI constant: a C macro or enumerator and the value we use for it.
#[derive(Clone, Copy, Debug)]
pub struct ConstValue {
    /// The C name.
    pub c_name: &'static str,
    /// The header that defines it.
    pub header: &'static str,
    /// Our value.
    pub value: i64,
    /// Where it exists.
    pub arches: Arches,
}

/// Every ABI constant checked against FreeBSD's headers.
pub const CONSTANTS: &[&[ConstValue]] = &[
    crate::sysctl::CONSTANTS,
    BUS_CONSTANTS,
    crate::errno::CONSTANTS,
    crate::evdev::CONSTANTS,
    crate::kbio::CONSTANTS,
    crate::mouse::CONSTANTS,
    crate::consio::CONSTANTS,
    crate::sysarch::CONSTANTS,
    crate::exec::CONSTANTS,
    crate::mman::CONSTANTS,
    crate::thr::CONSTANTS,
    crate::signal::CONSTANTS,
    crate::time::CONSTANTS,
    crate::resource::CONSTANTS,
    crate::vdso::CONSTANTS,
    crate::ucontext::CONSTANTS,
    crate::fcntl::CONSTANTS,
    crate::dirent::CONSTANTS,
    crate::mount::CONSTANTS,
    crate::ioctl::CONSTANTS,
    crate::md::CONSTANTS,
    crate::socket::CONSTANTS,
    crate::procdesc::CONSTANTS,
    crate::stat::CONSTANTS,
    crate::termios::CONSTANTS,
    crate::kinfo::CONSTANTS,
];

const fn bus_const(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/bus.h",
        value,
        arches: Arches::All,
    }
}

const BUS_CONSTANTS: &[ConstValue] = &[
    bus_const("BUS_USER_VERSION", bus::BUS_USER_VERSION as i64),
    bus_const("BUS_USER_BUFFER", bus::BUS_USER_BUFFER as i64),
    bus_const("DS_NOTPRESENT", bus::ds::NOTPRESENT as i64),
    bus_const("DS_ALIVE", bus::ds::ALIVE as i64),
    bus_const("DS_ATTACHING", bus::ds::ATTACHING as i64),
    bus_const("DS_ATTACHED", bus::ds::ATTACHED as i64),
    bus_const("DF_ENABLED", bus::df::ENABLED as i64),
    bus_const("DF_FIXEDCLASS", bus::df::FIXEDCLASS as i64),
    bus_const("DF_WILDCARD", bus::df::WILDCARD as i64),
    bus_const("DF_DESCMALLOCED", bus::df::DESCMALLOCED as i64),
    bus_const("DF_QUIET", bus::df::QUIET as i64),
    bus_const("DF_DONENOMATCH", bus::df::DONENOMATCH as i64),
    bus_const("DF_EXTERNALSOFTC", bus::df::EXTERNALSOFTC as i64),
    bus_const("DF_SUSPENDED", bus::df::SUSPENDED as i64),
    bus_const("DF_QUIET_CHILDREN", bus::df::QUIET_CHILDREN as i64),
    bus_const("DF_ATTACHED_ONCE", bus::df::ATTACHED_ONCE as i64),
    bus_const("DF_NEEDNOMATCH", bus::df::NEEDNOMATCH as i64),
];

/// Size of the field selected by `_f`. The function pointer only carries
/// the field type; it is never called.
#[doc(hidden)]
pub const fn field_size<T, F>(_f: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

macro_rules! layout {
    ($ty:ty, $c_type:literal, [$($h:literal),*], $arches:ident, { $($field:ident => $c:literal),* $(,)? }) => {
        StructLayout {
            c_type: $c_type,
            headers: &[$($h),*],
            arches: Arches::$arches,
            size: size_of::<$ty>(),
            align: align_of::<$ty>(),
            fields: &[$(FieldLayout {
                c_name: $c,
                offset: core::mem::offset_of!($ty, $field),
                size: field_size(|s: &$ty| &s.$field),
            }),*],
        }
    };
}

/// Every structure checked by `abi-check`.
pub static LAYOUTS: &[StructLayout] = &[
    layout!(bus::UBusinfo, "struct u_businfo", ["sys/types.h", "sys/bus.h"], All, {
        ub_version => "ub_version",
        ub_generation => "ub_generation",
    }),
    layout!(bus::UDevice, "struct u_device", ["sys/types.h", "sys/bus.h"], All, {
        dv_handle => "dv_handle",
        dv_parent => "dv_parent",
        dv_devflags => "dv_devflags",
        dv_flags => "dv_flags",
        dv_state => "dv_state",
        dv_fields => "dv_fields",
    }),
    layout!(evdev::InputEvent, "struct input_event", ["sys/types.h", "dev/evdev/input.h"], All, {
        time => "time",
        kind => "type",
        code => "code",
        value => "value",
    }),
    layout!(kbio::KeyboardInfo, "struct keyboard_info", ["sys/types.h", "sys/kbio.h"], All, {
        kb_index => "kb_index",
        kb_name => "kb_name",
        kb_unit => "kb_unit",
        kb_type => "kb_type",
        kb_config => "kb_config",
        kb_flags => "kb_flags",
    }),
    layout!(kbio::KeyEnt, "struct keyent_t", ["sys/types.h", "sys/kbio.h"], All, {
        map => "map",
        spcl => "spcl",
        flgs => "flgs",
    }),
    layout!(kbio::Keymap, "keymap_t", ["sys/types.h", "sys/kbio.h"], All, {
        n_keys => "n_keys",
        key => "key",
    }),
    layout!(kbio::AccT, "struct acc_t", ["sys/types.h", "sys/kbio.h"], All, {
        accchar => "accchar",
        map => "map",
    }),
    layout!(kbio::AccentMap, "accentmap_t", ["sys/types.h", "sys/kbio.h"], All, {
        n_accs => "n_accs",
        acc => "acc",
    }),
    layout!(kbio::KeyArg, "keyarg_t", ["sys/types.h", "sys/kbio.h"], All, {
        keynum => "keynum",
        key => "key",
    }),
    layout!(kbio::FkeyArg, "fkeyarg_t", ["sys/types.h", "sys/kbio.h"], All, {
        keynum => "keynum",
        keydef => "keydef",
        flen => "flen",
    }),
    layout!(kbio::KeyboardRepeat, "keyboard_repeat_t", ["sys/types.h", "sys/kbio.h"], All, {
        kb_repeat => "kb_repeat",
    }),
    layout!(mouse::MouseStatus, "mousestatus_t", ["sys/types.h", "sys/mouse.h"], All, {
        flags => "flags",
        button => "button",
        obutton => "obutton",
        dx => "dx",
        dy => "dy",
        dz => "dz",
    }),
    layout!(mouse::MouseHw, "mousehw_t", ["sys/types.h", "sys/mouse.h"], All, {
        buttons => "buttons",
        iftype => "iftype",
        r#type => "type",
        model => "model",
        hwid => "hwid",
    }),
    layout!(mouse::MouseMode, "mousemode_t", ["sys/types.h", "sys/mouse.h"], All, {
        protocol => "protocol",
        rate => "rate",
        resolution => "resolution",
        accelfactor => "accelfactor",
        level => "level",
        packetsize => "packetsize",
        syncmask => "syncmask",
    }),
    layout!(mouse::MouseData, "mousedata_t", ["sys/types.h", "sys/mouse.h"], All, {
        len => "len",
        buf => "buf",
    }),
    layout!(consio::MouseInfo, "mouse_info_t", ["sys/types.h", "sys/consio.h"], All, {
        operation => "operation",
        u => "u",
    }),
    layout!(consio::VtMode, "struct vt_mode", ["sys/types.h", "sys/consio.h"], All, {
        mode => "mode",
        waitv => "waitv",
        relsig => "relsig",
        acqsig => "acqsig",
        frsig => "frsig",
    }),
    layout!(evdev::InputId, "struct input_id", ["sys/types.h", "dev/evdev/input.h"], All, {
        bustype => "bustype",
        vendor => "vendor",
        product => "product",
        version => "version",
    }),
    layout!(evdev::InputAbsinfo, "struct input_absinfo", ["sys/types.h", "dev/evdev/input.h"], All, {
        value => "value",
        minimum => "minimum",
        maximum => "maximum",
        fuzz => "fuzz",
        flat => "flat",
        resolution => "resolution",
    }),
    layout!(md::MdIoctl, "struct md_ioctl", ["sys/types.h", "sys/mdioctl.h"], All, {
        md_version => "md_version",
        md_unit => "md_unit",
        md_type => "md_type",
        md_file => "md_file",
        md_mediasize => "md_mediasize",
        md_sectorsize => "md_sectorsize",
        md_options => "md_options",
        md_base => "md_base",
        md_fwheads => "md_fwheads",
        md_fwsectors => "md_fwsectors",
        md_label => "md_label",
        md_pad => "md_pad",
    }),
    layout!(socket::SockaddrUn, "struct sockaddr_un", ["sys/types.h", "sys/socket.h", "sys/un.h"], All, {
        sun_len => "sun_len",
        sun_family => "sun_family",
        sun_path => "sun_path",
    }),
    layout!(time::Timespec, "struct timespec", ["sys/types.h", "time.h"], All, {
        tv_sec => "tv_sec",
        tv_nsec => "tv_nsec",
    }),
    layout!(stat::Stat, "struct stat", ["sys/types.h", "sys/stat.h"], All, {
        st_dev => "st_dev",
        st_ino => "st_ino",
        st_nlink => "st_nlink",
        st_mode => "st_mode",
        st_bsdflags => "st_bsdflags",
        st_uid => "st_uid",
        st_gid => "st_gid",
        st_padding1 => "st_padding1",
        st_rdev => "st_rdev",
        st_atim => "st_atim",
        st_mtim => "st_mtim",
        st_ctim => "st_ctim",
        st_birthtim => "st_birthtim",
        st_size => "st_size",
        st_blocks => "st_blocks",
        st_blksize => "st_blksize",
        st_flags => "st_flags",
        st_gen => "st_gen",
        st_filerev => "st_filerev",
        st_spare => "st_spare",
    }),
    layout!(crate::fcntl::Flock, "struct flock", ["fcntl.h"], All, {
        l_start => "l_start",
        l_len => "l_len",
        l_pid => "l_pid",
        l_type => "l_type",
        l_whence => "l_whence",
        l_sysid => "l_sysid",
    }),
    layout!(crate::dirent::Dirent, "struct dirent", ["sys/types.h", "dirent.h"], All, {
        d_fileno => "d_fileno",
        d_off => "d_off",
        d_reclen => "d_reclen",
        d_type => "d_type",
        d_pad0 => "d_pad0",
        d_namlen => "d_namlen",
        d_pad1 => "d_pad1",
        d_name => "d_name",
    }),
    layout!(crate::mount::Xvfsconf, "struct xvfsconf", ["sys/param.h", "sys/mount.h"], All, {
        vfc_vfsops => "vfc_vfsops",
        vfc_name => "vfc_name",
        vfc_typenum => "vfc_typenum",
        vfc_refcount => "vfc_refcount",
        vfc_flags => "vfc_flags",
        vfc_next => "vfc_next",
    }),
    layout!(crate::mount::Statfs, "struct statfs", ["sys/param.h", "sys/mount.h"], All, {
        f_version => "f_version",
        f_type => "f_type",
        f_flags => "f_flags",
        f_bsize => "f_bsize",
        f_iosize => "f_iosize",
        f_blocks => "f_blocks",
        f_bfree => "f_bfree",
        f_bavail => "f_bavail",
        f_files => "f_files",
        f_ffree => "f_ffree",
        f_syncwrites => "f_syncwrites",
        f_asyncwrites => "f_asyncwrites",
        f_syncreads => "f_syncreads",
        f_asyncreads => "f_asyncreads",
        f_nvnodelistsize => "f_nvnodelistsize",
        f_spare0 => "f_spare0",
        f_spare => "f_spare",
        f_namemax => "f_namemax",
        f_owner => "f_owner",
        f_fsid => "f_fsid",
        f_charspare => "f_charspare",
        f_fstypename => "f_fstypename",
        f_mntfromname => "f_mntfromname",
        f_mntonname => "f_mntonname",
    }),
    layout!(crate::termios::Termios, "struct termios", ["termios.h"], All, {
        c_iflag => "c_iflag",
        c_oflag => "c_oflag",
        c_cflag => "c_cflag",
        c_lflag => "c_lflag",
        c_cc => "c_cc",
        c_ispeed => "c_ispeed",
        c_ospeed => "c_ospeed",
    }),
    layout!(crate::termios::Winsize, "struct winsize", ["sys/ioctl.h"], All, {
        ws_row => "ws_row",
        ws_col => "ws_col",
        ws_xpixel => "ws_xpixel",
        ws_ypixel => "ws_ypixel",
    }),
    layout!(crate::kinfo::KinfoProc, "struct kinfo_proc", ["sys/param.h", "sys/proc.h", "sys/user.h"], All, {
        ki_structsize => "ki_structsize",
        ki_layout => "ki_layout",
        ki_args => "ki_args",
        ki_paddr => "ki_paddr",
        ki_wchan => "ki_wchan",
        ki_pid => "ki_pid",
        ki_ppid => "ki_ppid",
        ki_pgid => "ki_pgid",
        ki_tpgid => "ki_tpgid",
        ki_sid => "ki_sid",
        ki_tsid => "ki_tsid",
        ki_jobc => "ki_jobc",
        ki_tdev_freebsd11 => "ki_tdev_freebsd11",
        ki_siglist => "ki_siglist",
        ki_sigmask => "ki_sigmask",
        ki_sigignore => "ki_sigignore",
        ki_sigcatch => "ki_sigcatch",
        ki_uid => "ki_uid",
        ki_ruid => "ki_ruid",
        ki_svuid => "ki_svuid",
        ki_rgid => "ki_rgid",
        ki_svgid => "ki_svgid",
        ki_ngroups => "ki_ngroups",
        ki_groups => "ki_groups",
        ki_size => "ki_size",
        ki_rssize => "ki_rssize",
        ki_swrss => "ki_swrss",
        ki_tsize => "ki_tsize",
        ki_dsize => "ki_dsize",
        ki_ssize => "ki_ssize",
        ki_xstat => "ki_xstat",
        ki_acflag => "ki_acflag",
        ki_pctcpu => "ki_pctcpu",
        ki_estcpu => "ki_estcpu",
        ki_slptime => "ki_slptime",
        ki_swtime => "ki_swtime",
        ki_cow => "ki_cow",
        ki_runtime => "ki_runtime",
        ki_start => "ki_start",
        ki_childtime => "ki_childtime",
        ki_flag => "ki_flag",
        ki_kiflag => "ki_kiflag",
        ki_traceflag => "ki_traceflag",
        ki_stat => "ki_stat",
        ki_nice => "ki_nice",
        ki_lock => "ki_lock",
        ki_rqindex => "ki_rqindex",
        ki_oncpu_old => "ki_oncpu_old",
        ki_lastcpu_old => "ki_lastcpu_old",
        ki_tdname => "ki_tdname",
        ki_wmesg => "ki_wmesg",
        ki_login => "ki_login",
        ki_lockname => "ki_lockname",
        ki_comm => "ki_comm",
        ki_emul => "ki_emul",
        ki_loginclass => "ki_loginclass",
        ki_moretdname => "ki_moretdname",
        ki_sparestrings => "ki_sparestrings",
        ki_spareints => "ki_spareints",
        ki_reaper => "ki_reaper",
        ki_reapsubtree => "ki_reapsubtree",
        ki_tdev => "ki_tdev",
        ki_oncpu => "ki_oncpu",
        ki_lastcpu => "ki_lastcpu",
        ki_tracer => "ki_tracer",
        ki_flag2 => "ki_flag2",
        ki_fibnum => "ki_fibnum",
        ki_cr_flags => "ki_cr_flags",
        ki_jid => "ki_jid",
        ki_numthreads => "ki_numthreads",
        ki_tid => "ki_tid",
        ki_pri => "ki_pri",
        ki_rusage => "ki_rusage",
        ki_rusage_ch => "ki_rusage_ch",
        ki_pcb => "ki_pcb",
        ki_kstack => "ki_kstack",
        ki_udata => "ki_udata",
        ki_tdaddr => "ki_tdaddr",
        ki_pd => "ki_pd",
        ki_uerrmsg => "ki_uerrmsg",
        ki_spareptrs => "ki_spareptrs",
        ki_sparelongs => "ki_sparelongs",
        ki_sflag => "ki_sflag",
        ki_tdflags => "ki_tdflags",
    }),
    layout!(crate::kinfo::Priority, "struct priority", ["sys/param.h", "sys/priority.h"], All, {
        pri_class => "pri_class",
        pri_level => "pri_level",
        pri_native => "pri_native",
        pri_user => "pri_user",
    }),
    layout!(crate::exec::PsStrings, "struct ps_strings", ["sys/types.h", "sys/exec.h"], All, {
        ps_argvstr => "ps_argvstr",
        ps_nargvstr => "ps_nargvstr",
        ps_envstr => "ps_envstr",
        ps_nenvstr => "ps_nenvstr",
    }),
    layout!(crate::thr::Rtprio, "struct rtprio", ["sys/types.h", "sys/rtprio.h"], All, {
        type_ => "type",
        prio => "prio",
    }),
    layout!(crate::thr::UmtxTime, "struct _umtx_time", ["sys/types.h", "sys/umtx.h"], All, {
        timeout => "_timeout",
        flags => "_flags",
        clockid => "_clockid",
    }),
    layout!(crate::uio::Iovec, "struct iovec", ["sys/types.h", "sys/uio.h"], All, {
        iov_base => "iov_base",
        iov_len => "iov_len",
    }),
    layout!(crate::time::Timeval, "struct timeval", ["sys/types.h", "sys/time.h"], All, {
        tv_sec => "tv_sec",
        tv_usec => "tv_usec",
    }),
    layout!(crate::resource::Rlimit, "struct rlimit", ["sys/types.h", "sys/resource.h"], All, {
        rlim_cur => "rlim_cur",
        rlim_max => "rlim_max",
    }),
    layout!(crate::resource::Rusage, "struct rusage", ["sys/types.h", "sys/resource.h"], All, {
        ru_utime => "ru_utime",
        ru_stime => "ru_stime",
        ru_maxrss => "ru_maxrss",
        ru_ixrss => "ru_ixrss",
        ru_idrss => "ru_idrss",
        ru_isrss => "ru_isrss",
        ru_minflt => "ru_minflt",
        ru_majflt => "ru_majflt",
        ru_nswap => "ru_nswap",
        ru_inblock => "ru_inblock",
        ru_oublock => "ru_oublock",
        ru_msgsnd => "ru_msgsnd",
        ru_msgrcv => "ru_msgrcv",
        ru_nsignals => "ru_nsignals",
        ru_nvcsw => "ru_nvcsw",
        ru_nivcsw => "ru_nivcsw",
    }),
    layout!(signal::Sigset, "sigset_t", ["sys/types.h", "signal.h"], All, {
        bits => "__bits",
    }),
    layout!(signal::Sigaction, "struct sigaction", ["sys/types.h", "signal.h"], All, {
        sa_handler => "sa_handler",
        sa_flags => "sa_flags",
        sa_mask => "sa_mask",
    }),
    layout!(vdso::Bintime, "struct bintime", ["sys/types.h", "sys/time.h"], All, {
        sec => "sec",
        frac => "frac",
    }),
    layout!(vdso::Timekeep, "struct vdso_timekeep", ["sys/types.h", "sys/time.h", "sys/vdso.h"], All, {
        tk_ver => "tk_ver",
        tk_enabled => "tk_enabled",
        tk_current => "tk_current",
    }),
    layout!(vdso::TimehandsX86, "struct vdso_timehands", ["sys/types.h", "sys/time.h", "sys/vdso.h"], Amd64, {
        th_algo => "th_algo",
        th_gen => "th_gen",
        th_scale => "th_scale",
        th_offset_count => "th_offset_count",
        th_counter_mask => "th_counter_mask",
        th_offset => "th_offset",
        th_boottime => "th_boottime",
        th_x86_shift => "th_x86_shift",
        th_x86_hpet_idx => "th_x86_hpet_idx",
        th_x86_pvc_last_systime => "th_x86_pvc_last_systime",
        th_x86_pvc_stable_mask => "th_x86_pvc_stable_mask",
        th_res => "th_res",
    }),
    layout!(vdso::TimehandsArm, "struct vdso_timehands", ["sys/types.h", "sys/time.h", "sys/vdso.h"], Arm64, {
        th_algo => "th_algo",
        th_gen => "th_gen",
        th_scale => "th_scale",
        th_offset_count => "th_offset_count",
        th_counter_mask => "th_counter_mask",
        th_offset => "th_offset",
        th_boottime => "th_boottime",
        th_physical => "th_physical",
        th_res => "th_res",
    }),
    layout!(signal::Siginfo, "siginfo_t", ["sys/types.h", "signal.h"], All, {
        si_signo => "si_signo",
        si_errno => "si_errno",
        si_code => "si_code",
        si_pid => "si_pid",
        si_uid => "si_uid",
        si_status => "si_status",
        si_addr => "si_addr",
        si_value => "si_value",
        si_trapno => "si_trapno",
    }),
    layout!(signal::StackT, "stack_t", ["sys/types.h", "signal.h"], All, {
        ss_sp => "ss_sp",
        ss_size => "ss_size",
        ss_flags => "ss_flags",
    }),
    layout!(ucontext::McontextX86, "mcontext_t", ["sys/types.h", "ucontext.h"], Amd64, {
        mc_onstack => "mc_onstack",
        mc_rdi => "mc_rdi",
        mc_rsi => "mc_rsi",
        mc_rdx => "mc_rdx",
        mc_rcx => "mc_rcx",
        mc_r8 => "mc_r8",
        mc_r9 => "mc_r9",
        mc_rax => "mc_rax",
        mc_rbx => "mc_rbx",
        mc_rbp => "mc_rbp",
        mc_r10 => "mc_r10",
        mc_r11 => "mc_r11",
        mc_r12 => "mc_r12",
        mc_r13 => "mc_r13",
        mc_r14 => "mc_r14",
        mc_r15 => "mc_r15",
        mc_trapno => "mc_trapno",
        mc_fs => "mc_fs",
        mc_gs => "mc_gs",
        mc_addr => "mc_addr",
        mc_flags => "mc_flags",
        mc_es => "mc_es",
        mc_ds => "mc_ds",
        mc_err => "mc_err",
        mc_rip => "mc_rip",
        mc_cs => "mc_cs",
        mc_rflags => "mc_rflags",
        mc_rsp => "mc_rsp",
        mc_ss => "mc_ss",
        mc_len => "mc_len",
        mc_fpformat => "mc_fpformat",
        mc_ownedfp => "mc_ownedfp",
        mc_fpstate => "mc_fpstate",
        mc_fsbase => "mc_fsbase",
        mc_gsbase => "mc_gsbase",
        mc_xfpustate => "mc_xfpustate",
        mc_xfpustate_len => "mc_xfpustate_len",
        mc_tlsbase => "mc_tlsbase",
        mc_spare => "mc_spare",
    }),
    layout!(ucontext::McontextArm, "mcontext_t", ["sys/types.h", "ucontext.h"], Arm64, {
        mc_gpregs => "mc_gpregs",
        mc_fpregs => "mc_fpregs",
        mc_flags => "mc_flags",
        mc_pad => "mc_pad",
        mc_ptr => "mc_ptr",
        mc_spare => "mc_spare",
    }),
    layout!(ucontext::Gpregs, "struct gpregs", ["sys/types.h", "ucontext.h"], Arm64, {
        gp_x => "gp_x",
        gp_lr => "gp_lr",
        gp_sp => "gp_sp",
        gp_elr => "gp_elr",
        gp_spsr => "gp_spsr",
    }),
    layout!(ucontext::Fpregs, "struct fpregs", ["sys/types.h", "ucontext.h"], Arm64, {
        fp_q => "fp_q",
        fp_sr => "fp_sr",
        fp_cr => "fp_cr",
        fp_flags => "fp_flags",
        fp_pad => "fp_pad",
    }),
    layout!(ucontext::Ucontext<ucontext::McontextX86>, "ucontext_t", ["sys/types.h", "ucontext.h"], Amd64, {
        uc_sigmask => "uc_sigmask",
        uc_mcontext => "uc_mcontext",
        uc_link => "uc_link",
        uc_stack => "uc_stack",
        uc_flags => "uc_flags",
        uc_spare => "__spare__",
    }),
    layout!(ucontext::Ucontext<ucontext::McontextArm>, "ucontext_t", ["sys/types.h", "ucontext.h"], Arm64, {
        uc_sigmask => "uc_sigmask",
        uc_mcontext => "uc_mcontext",
        uc_link => "uc_link",
        uc_stack => "uc_stack",
        uc_flags => "uc_flags",
        uc_spare => "__spare__",
    }),
    layout!(ucontext::SigframeX86, "struct sigframe", ["sys/types.h", "ucontext.h", "machine/sigframe.h"], Amd64, {
        sf_ahu => "sf_ahu",
        sf_uc => "sf_uc",
        sf_si => "sf_si",
    }),
    layout!(ucontext::SigframeArm, "struct sigframe", ["sys/types.h", "ucontext.h", "machine/frame.h"], Arm64, {
        sf_si => "sf_si",
        sf_uc => "sf_uc",
    }),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sizes() {
        // Values computed by FreeBSD 15.1's clang for amd64; abi-check
        // re-verifies them against the real headers.
        assert_eq!(size_of::<time::Timespec>(), 16);
        assert_eq!(size_of::<stat::Stat>(), 224);
        assert_eq!(size_of::<signal::Sigaction>(), 32);
        assert_eq!(size_of::<vdso::Timekeep>(), 16);
        assert_eq!(size_of::<signal::Siginfo>(), 80);
        assert_eq!(size_of::<ucontext::McontextX86>(), 800);
        assert_eq!(size_of::<ucontext::McontextArm>(), 880);
        assert_eq!(size_of::<ucontext::Ucontext<ucontext::McontextX86>>(), 880);
        assert_eq!(size_of::<ucontext::Ucontext<ucontext::McontextArm>>(), 960);
        assert_eq!(size_of::<ucontext::SigframeX86>(), 976);
        assert_eq!(size_of::<ucontext::SigframeArm>(), 1040);
        assert_eq!(size_of::<vdso::TimehandsX86>(), 88);
        assert_eq!(size_of::<vdso::TimehandsArm>(), 88);
    }

    #[test]
    fn fields_are_in_bounds() {
        for l in LAYOUTS {
            for f in l.fields {
                assert!(f.offset + f.size <= l.size, "{}.{}", l.c_type, f.c_name);
            }
        }
    }
}
