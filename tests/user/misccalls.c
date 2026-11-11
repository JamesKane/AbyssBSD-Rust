/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * misccalls: system calls libc and FreeBSD's tools reach for here and
 * there: getdtablesize, posix_fadvise, mincore, cpuset_getdomain and
 * cpuset_setdomain, sched_rr_get_interval, yield, uuidgen, membarrier,
 * kcmp, getrlimitusage, renameat2, abort2, sbrk (amd64; arm64 has no
 * break), and the old mount(2)'s refusals.
 *
 * With arguments "mount DEVICE DIR", mounts the UFS file system on
 * DEVICE read-only at DIR through the old mount(2) and ufs_args, checks
 * that /rescue/rescue is there and that a write is refused, and unmounts.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/cpuset.h>
#include <sys/domainset.h>
#include <sys/membarrier.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/uuid.h>
#include <sys/wait.h>

#include <ufs/ufs/extattr.h>
#include <ufs/ufs/quota.h>
#include <ufs/ufs/ufsmount.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("misccalls: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* `r` failed with `want`. */
#define	FAILS(r, want) do {						\
	errno = 0;							\
	long r_ = (long)(r);						\
	int e_ = errno;							\
	CHECK(r_ == -1 && e_ == (want), "got %ld (%s), want %s", r_,	\
	    strerror(e_), strerror(want));				\
} while (0)

static char dir[64];

static void
dtablesize(void)
{
	struct rlimit rl, low;
	int maxfiles;
	size_t len = sizeof(maxfiles);

	CHECK(getrlimit(RLIMIT_NOFILE, &rl) == 0, "%s", strerror(errno));
	CHECK(sysctlbyname("kern.maxfilesperproc", &maxfiles, &len, NULL,
	    0) == 0, "%s", strerror(errno));
	CHECK(getdtablesize() == (int)MIN(rl.rlim_cur, (rlim_t)maxfiles),
	    "%d, limit %jd, maxfilesperproc %d", getdtablesize(),
	    (intmax_t)rl.rlim_cur, maxfiles);
	low = rl;
	low.rlim_cur = 64;
	CHECK(setrlimit(RLIMIT_NOFILE, &low) == 0, "%s", strerror(errno));
	CHECK(getdtablesize() == 64, "%d after lowering it", getdtablesize());
	CHECK(setrlimit(RLIMIT_NOFILE, &rl) == 0, "%s", strerror(errno));
}

static void
fadvise(void)
{
	char path[128];
	int fd, p[2], d, n;

	snprintf(path, sizeof(path), "%s/advised", dir);
	fd = open(path, O_RDWR | O_CREAT, 0644);
	CHECK(fd >= 0, "%s", strerror(errno));
	CHECK(write(fd, "data", 4) == 4, "%s", strerror(errno));
	/* The error is the value, not errno. */
	CHECK(posix_fadvise(fd, 0, 0, POSIX_FADV_SEQUENTIAL) == 0, "seq");
	CHECK(posix_fadvise(fd, 0, 4096, POSIX_FADV_RANDOM) == 0, "random");
	CHECK(posix_fadvise(fd, 0, 0, POSIX_FADV_WILLNEED) == 0, "willneed");
	CHECK(posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED) == 0, "dontneed");
	CHECK(posix_fadvise(fd, 0, 0, POSIX_FADV_NOREUSE) == 0, "noreuse");
	CHECK(posix_fadvise(fd, 0, 0, POSIX_FADV_NORMAL) == 0, "normal");
	n = posix_fadvise(fd, 0, 0, 99);
	CHECK(n == EINVAL, "bad advice: %d", n);
	n = posix_fadvise(fd, -1, 0, POSIX_FADV_NORMAL);
	CHECK(n == EINVAL, "negative offset: %d", n);
	n = posix_fadvise(fd, 0, -1, POSIX_FADV_NORMAL);
	CHECK(n == EINVAL, "negative length: %d", n);
	n = posix_fadvise(fd, INT64_MAX, 1, POSIX_FADV_NORMAL);
	CHECK(n == EINVAL, "past OFF_MAX: %d", n);
	close(fd);
	n = posix_fadvise(fd, 0, 0, POSIX_FADV_NORMAL);
	CHECK(n == EBADF, "closed: %d", n);
	CHECK(pipe(p) == 0, "%s", strerror(errno));
	n = posix_fadvise(p[0], 0, 0, POSIX_FADV_NORMAL);
	CHECK(n == ESPIPE, "pipe: %d", n);
	close(p[0]);
	close(p[1]);
	d = open(dir, O_RDONLY | O_DIRECTORY);
	n = posix_fadvise(d, 0, 0, POSIX_FADV_NORMAL);
	CHECK(n == ENODEV, "directory: %d", n);
	close(d);
	d = open("/dev/null", O_RDONLY);
	n = posix_fadvise(d, 0, 0, POSIX_FADV_NORMAL);
	CHECK(n == ENODEV, "device: %d", n);
	close(d);
	unlink(path);
}

static void
residency(void)
{
	size_t pg = getpagesize();
	char vec[4];
	volatile char *m;
	char c;

	m = mmap(NULL, 4 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE,
	    -1, 0);
	CHECK(m != MAP_FAILED, "%s", strerror(errno));
	m[0] = 1;
	c = m[2 * pg];
	(void)c;
	CHECK(mincore((void *)m, 4 * pg, vec) == 0, "%s", strerror(errno));
	CHECK((vec[0] & (MINCORE_INCORE | MINCORE_MODIFIED)) ==
	    (MINCORE_INCORE | MINCORE_MODIFIED), "written page: %#x", vec[0]);
	CHECK(vec[1] == 0, "untouched page: %#x", vec[1]);
	CHECK((vec[2] & MINCORE_INCORE) != 0, "read page: %#x", vec[2]);
	CHECK(vec[3] == 0, "untouched page: %#x", vec[3]);
	/* A partial page counts whole. */
	CHECK(mincore((void *)(m + 1), 1, vec) == 0, "%s", strerror(errno));
	CHECK((vec[0] & MINCORE_INCORE) != 0, "%#x", vec[0]);
	munmap((void *)(m + 2 * pg), pg);
	FAILS(mincore((void *)m, 4 * pg, vec), ENOMEM);
	FAILS(mincore((void *)(m + 2 * pg), pg, vec), ENOMEM);
	CHECK(mincore((void *)m, 2 * pg, vec) == 0, "%s", strerror(errno));
	munmap((void *)m, 4 * pg);
}

static void
domains(void)
{
	domainset_t mask;
	int policy, status;
	pid_t pid;

	DOMAINSET_ZERO(&mask);
	CHECK(cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, &policy) == 0, "%s", strerror(errno));
	CHECK(policy == DOMAINSET_POLICY_FIRSTTOUCH, "policy %d", policy);
	CHECK(DOMAINSET_ISSET(0, &mask) && DOMAINSET_COUNT(&mask) == 1,
	    "%d domains", DOMAINSET_COUNT(&mask));
	CHECK(cpuset_getdomain(CPU_LEVEL_CPUSET, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, &policy) == 0, "%s", strerror(errno));
	CHECK(policy == DOMAINSET_POLICY_FIRSTTOUCH, "set's policy %d",
	    policy);
	FAILS(cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1, 4, &mask,
	    &policy), ERANGE);
	FAILS(cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, 0,
	    sizeof(mask), &mask, &policy), EINVAL);

	DOMAINSET_ZERO(&mask);
	DOMAINSET_SET(0, &mask);
	CHECK(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_ROUNDROBIN) == 0, "%s",
	    strerror(errno));
	CHECK(cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_TID, -1,
	    sizeof(mask), &mask, &policy) == 0, "%s", strerror(errno));
	CHECK(policy == DOMAINSET_POLICY_ROUNDROBIN, "policy %d", policy);
	/* The thread's own; the set keeps its policy. */
	CHECK(cpuset_getdomain(CPU_LEVEL_CPUSET, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, &policy) == 0, "%s", strerror(errno));
	CHECK(policy == DOMAINSET_POLICY_FIRSTTOUCH, "set's policy %d",
	    policy);
	/* A child inherits it. */
	pid = fork();
	if (pid == 0) {
		if (cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
		    sizeof(mask), &mask, &policy) != 0)
			_exit(1);
		_exit(policy == DOMAINSET_POLICY_ROUNDROBIN ? 0 : 2);
	}
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0, "child: %#x", status);

	/* A preferred domain is reported alone. */
	DOMAINSET_ZERO(&mask);
	DOMAINSET_SET(0, &mask);
	CHECK(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_PREFER) == 0, "%s",
	    strerror(errno));
	CHECK(cpuset_getdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, &policy) == 0, "%s", strerror(errno));
	CHECK(policy == DOMAINSET_POLICY_PREFER && DOMAINSET_ISSET(0, &mask) &&
	    DOMAINSET_COUNT(&mask) == 1, "policy %d", policy);

	FAILS(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_INVALID), EINVAL);
	FAILS(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_MAX + 1), EINVAL);
	DOMAINSET_ZERO(&mask);
	FAILS(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_ROUNDROBIN), EDEADLK);
	/* Only domain 0 exists (one memory domain, as in a VM). */
	DOMAINSET_SET(1, &mask);
	FAILS(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_ROUNDROBIN), EINVAL);
	DOMAINSET_ZERO(&mask);
	DOMAINSET_SET(0, &mask);
	FAILS(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1, 4, &mask,
	    DOMAINSET_POLICY_ROUNDROBIN), ERANGE);
	CHECK(cpuset_setdomain(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask, DOMAINSET_POLICY_FIRSTTOUCH) == 0, "%s",
	    strerror(errno));
}

static void
round_robin(void)
{
	struct clockinfo ci;
	struct timespec ts;
	size_t len = sizeof(ci);
	long slice, ticks;

	CHECK(sysctlbyname("kern.clockrate", &ci, &len, NULL, 0) == 0, "%s",
	    strerror(errno));
	CHECK(sched_rr_get_interval(0, &ts) == 0, "%s", strerror(errno));
	/* ULE's slice, a tenth of stathz, in whole hz ticks. */
	slice = ci.stathz / 10;
	ticks = MAX(1, (slice * ci.hz + ci.stathz / 2) / ci.stathz);
	CHECK(ts.tv_sec == 0 && ts.tv_nsec == 1000000000L / ci.hz * ticks,
	    "%jd.%09ld for hz %d, stathz %d", (intmax_t)ts.tv_sec, ts.tv_nsec,
	    ci.hz, ci.stathz);
	CHECK(sched_rr_get_interval(getpid(), &ts) == 0, "%s",
	    strerror(errno));
	FAILS(sched_rr_get_interval(99999, &ts), ESRCH);
	CHECK(syscall(SYS_yield) == 0, "%s", strerror(errno));
}

static uint64_t
uuid_time(const struct uuid *u)
{
	return ((uint64_t)u->time_low | (uint64_t)u->time_mid << 32 |
	    (uint64_t)(u->time_hi_and_version & 0xfff) << 48);
}

static void
uuids(void)
{
	struct uuid u[3], v;
	int i;

	CHECK(uuidgen(u, 3) == 0, "%s", strerror(errno));
	for (i = 0; i < 3; i++) {
		CHECK(u[i].time_hi_and_version >> 12 == 1, "version %d",
		    u[i].time_hi_and_version >> 12);
		CHECK((u[i].clock_seq_hi_and_reserved & 0xc0) == 0x80,
		    "variant %#x", u[i].clock_seq_hi_and_reserved);
		CHECK(memcmp(u[i].node, u[0].node, sizeof(u[0].node)) == 0 &&
		    u[i].clock_seq_low == u[0].clock_seq_low, "node or sequence");
		CHECK(uuid_time(&u[i]) == uuid_time(&u[0]) + i, "time");
	}
	CHECK(uuidgen(&v, 1) == 0, "%s", strerror(errno));
	CHECK(memcmp(&v, &u[2], sizeof(v)) != 0, "the same UUID twice");
	FAILS(uuidgen(&v, 0), EINVAL);
	FAILS(uuidgen(&v, 2049), EINVAL);
	FAILS(uuidgen(NULL, 1), EFAULT);
}

static void
barriers(void)
{
	int all = MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_GLOBAL_EXPEDITED |
	    MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED |
	    MEMBARRIER_CMD_PRIVATE_EXPEDITED |
	    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
	    MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE |
	    MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE |
	    MEMBARRIER_CMD_GET_REGISTRATIONS;

	CHECK(membarrier(MEMBARRIER_CMD_QUERY, 0, 0) == all, "query: %#x",
	    membarrier(MEMBARRIER_CMD_QUERY, 0, 0));
	CHECK(membarrier(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, 0) == 0, "none");
	FAILS(membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0), EPERM);
	FAILS(membarrier(MEMBARRIER_CMD_GLOBAL_EXPEDITED, 0, 0), EPERM);
	CHECK(membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, 0) == 0,
	    "%s", strerror(errno));
	CHECK(membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) == 0, "%s",
	    strerror(errno));
	FAILS(membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, 0),
	    EPERM);
	CHECK(membarrier(MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED, 0, 0) == 0,
	    "%s", strerror(errno));
	CHECK(membarrier(MEMBARRIER_CMD_GLOBAL_EXPEDITED, 0, 0) == 0, "%s",
	    strerror(errno));
	CHECK(membarrier(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, 0) ==
	    (MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
	    MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED), "registrations");
	CHECK(membarrier(MEMBARRIER_CMD_GLOBAL, 0, 0) == 0, "%s",
	    strerror(errno));
	FAILS(membarrier(MEMBARRIER_CMD_GLOBAL, 1, 0), EINVAL);
	FAILS(membarrier(MEMBARRIER_CMD_GLOBAL | MEMBARRIER_CMD_PRIVATE_EXPEDITED,
	    0, 0), EINVAL);
	FAILS(membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 0, 0), EINVAL);
}

static void
compare(void)
{
	char path[128];
	pid_t me = getpid(), pid;
	int fd, other, copy, p[2], status;
	char c;

	snprintf(path, sizeof(path), "%s/kcmp", dir);
	fd = open(path, O_RDWR | O_CREAT, 0644);
	other = open(path, O_RDONLY);
	copy = dup(fd);
	CHECK(fd >= 0 && other >= 0 && copy >= 0, "%s", strerror(errno));
	CHECK(kcmp(me, me, KCMP_FILE, fd, copy) == 0, "a dup");
	CHECK(kcmp(me, me, KCMP_FILE, fd, other) != 0, "a second open");
	CHECK(kcmp(me, me, KCMP_FILEOBJ, fd, other) == 0, "the same vnode");
	CHECK(pipe(p) == 0, "%s", strerror(errno));
	CHECK(kcmp(me, me, KCMP_FILEOBJ, fd, p[0]) == 3, "a pipe");
	CHECK(kcmp(me, me, KCMP_FILES, 0, 0) == 0, "the table");
	CHECK(kcmp(me, me, KCMP_VM, 0, 0) == 0, "the space");
	FAILS(kcmp(me, me, KCMP_FILE, fd, 1000), EBADF);
	FAILS(kcmp(me, me, 99, 0, 0), EINVAL);
	FAILS(kcmp(me, 99999, KCMP_VM, 0, 0), ESRCH);
	pid = fork();
	if (pid == 0) {
		close(p[1]);
		(void)read(p[0], &c, 1);
		_exit(0);
	}
	/* The child's descriptors are the same files; its space is not. */
	CHECK(kcmp(me, pid, KCMP_FILE, fd, fd) == 0, "inherited");
	CHECK(kcmp(me, pid, KCMP_VM, 0, 0) != 0, "a forked space");
	CHECK(kcmp(me, pid, KCMP_FILES, 0, 0) != 0, "a forked table");
	close(p[1]);
	CHECK(waitpid(pid, &status, 0) == pid, "%s", strerror(errno));
	close(p[0]);
	close(fd);
	close(other);
	close(copy);
	unlink(path);
}

static void
usage(void)
{
	rlim_t v;

	CHECK(getrlimitusage(RLIMIT_NOFILE, 0, &v) == 0, "%s",
	    strerror(errno));
	CHECK(v >= 3, "%jd descriptors", (intmax_t)v);
	CHECK(getrlimitusage(RLIMIT_NPROC, GETRLIMITUSAGE_EUID, &v) == 0,
	    "%s", strerror(errno));
	CHECK(v >= 1, "%jd processes", (intmax_t)v);
	CHECK(getrlimitusage(RLIMIT_VMEM, 0, &v) == 0 && v > 0, "%s",
	    strerror(errno));
	FAILS(getrlimitusage(RLIMIT_FSIZE, 0, &v), ENXIO);
	FAILS(getrlimitusage(RLIMIT_CORE, 0, &v), ENXIO);
	FAILS(getrlimitusage(RLIM_NLIMITS, 0, &v), EINVAL);
	FAILS(getrlimitusage(RLIMIT_NOFILE, 2, &v), EINVAL);
}

static void
renames(void)
{
	char a[128], b[128], c[128];
	struct stat st;
	int fd;

	snprintf(a, sizeof(a), "%s/a", dir);
	snprintf(b, sizeof(b), "%s/b", dir);
	snprintf(c, sizeof(c), "%s/c", dir);
	fd = open(a, O_WRONLY | O_CREAT, 0644);
	close(fd);
	fd = open(b, O_WRONLY | O_CREAT, 0644);
	close(fd);
	FAILS(renameat2(AT_FDCWD, a, AT_FDCWD, b, AT_RENAME_NOREPLACE), EEXIST);
	CHECK(stat(a, &st) == 0 && stat(b, &st) == 0, "both still there");
	/* Before the kinds are compared. */
	FAILS(renameat2(AT_FDCWD, a, AT_FDCWD, dir, AT_RENAME_NOREPLACE),
	    EEXIST);
	FAILS(renameat2(AT_FDCWD, a, AT_FDCWD, c, 0x2), EINVAL);
	CHECK(renameat2(AT_FDCWD, a, AT_FDCWD, c, AT_RENAME_NOREPLACE) == 0,
	    "%s", strerror(errno));
	CHECK(stat(a, &st) == -1 && stat(c, &st) == 0, "moved");
	CHECK(renameat2(AT_FDCWD, c, AT_FDCWD, b, 0) == 0, "%s",
	    strerror(errno));
	unlink(b);
}

/* Runs abort2 in a child; returns the signal that ended it. */
static int
aborted(const char *why, int nargs, void **args)
{
	struct rlimit none = { 0, 0 };
	int status;
	pid_t pid;

	pid = fork();
	if (pid == 0) {
		setrlimit(RLIMIT_CORE, &none);
		abort2(why, nargs, args);
		_exit(1);
	}
	if (waitpid(pid, &status, 0) != pid || !WIFSIGNALED(status))
		return (-1);
	return (WTERMSIG(status));
}

static void
aborts(void)
{
	void *args[2] = { (void *)0x1234, NULL };
	int sig;

	sig = aborted("misccalls test", 2, args);
	CHECK(sig == SIGABRT, "signal %d", sig);
	sig = aborted(NULL, 0, NULL);
	CHECK(sig == SIGABRT, "no reason: signal %d", sig);
	sig = aborted("too many", 17, args);
	CHECK(sig == SIGKILL, "17 arguments: signal %d", sig);
	sig = aborted("unreadable", 2, (void **)8);
	CHECK(sig == SIGKILL, "unreadable arguments: signal %d", sig);
}

static void
heap(void)
{
#if defined(__amd64__)
	char *start, *p;
	size_t pg = getpagesize();

	start = sbrk(0);
	CHECK(start != (char *)-1, "%s", strerror(errno));
	p = sbrk(2 * pg);
	CHECK(p == start, "%p, not %p", p, start);
	memset(p, 0x5a, 2 * pg);
	CHECK(sbrk(0) == start + 2 * pg, "the end moved");
	CHECK(sbrk(-(intptr_t)pg) == start + 2 * pg, "%s", strerror(errno));
	CHECK(p[0] == 0x5a, "kept");
	CHECK(brk(start) == 0, "%s", strerror(errno));
	CHECK(sbrk(0) == start, "back");
	/* Past RLIMIT_DATA. */
	FAILS(sbrk((intptr_t)1 << 46), ENOMEM);
#else
	FAILS(syscall(SYS_break, NULL), ENOSYS);
#endif
}

static void
old_mount(void)
{
	FAILS(mount("nosuchfs", dir, 0, NULL), EINVAL);
	/* devfs has no old interface. */
	FAILS(mount("devfs", dir, 0, NULL), EOPNOTSUPP);
	FAILS(mount("ufs", dir, MNT_RDONLY, NULL), EINVAL);
	FAILS(mount("ufs", dir, MNT_RDONLY, (void *)8), EFAULT);
}

/* The old mount(2) of UFS on `dev` at `at`, read-only. */
static int
mount_ufs(const char *dev, const char *at)
{
	struct ufs_args args;
	struct statfs sf;
	struct stat st;
	char path[MAXPATHLEN];
	int fd;

	memset(&args, 0, sizeof(args));
	args.fspec = (char *)(uintptr_t)dev;
	if (mount("ufs", at, MNT_RDONLY, &args) != 0) {
		printf("misccalls: mount %s: %s\n", dev, strerror(errno));
		return (1);
	}
	CHECK(statfs(at, &sf) == 0, "%s", strerror(errno));
	CHECK(strcmp(sf.f_fstypename, "ufs") == 0 &&
	    (sf.f_flags & MNT_RDONLY) != 0 &&
	    strcmp(sf.f_mntfromname, dev) == 0, "%s %#jx %s", sf.f_fstypename,
	    (uintmax_t)sf.f_flags, sf.f_mntfromname);
	snprintf(path, sizeof(path), "%s/rescue/rescue", at);
	CHECK(stat(path, &st) == 0 && S_ISREG(st.st_mode), "%s",
	    strerror(errno));
	snprintf(path, sizeof(path), "%s/x", at);
	fd = open(path, O_WRONLY | O_CREAT, 0644);
	CHECK(fd == -1 && errno == EROFS, "a write: %s", strerror(errno));
	CHECK(unmount(at, 0) == 0, "%s", strerror(errno));
	return (failed);
}

int
main(int argc, char **argv)
{
	if (argc == 4 && strcmp(argv[1], "mount") == 0)
		return (mount_ufs(argv[2], argv[3]));
	strlcpy(dir, "/tmp/misccalls.XXXXXX", sizeof(dir));
	if (mkdtemp(dir) == NULL) {
		printf("misccalls: mkdtemp: %s\n", strerror(errno));
		return (1);
	}
	dtablesize();
	fadvise();
	residency();
	domains();
	round_robin();
	uuids();
	barriers();
	compare();
	usage();
	renames();
	aborts();
	heap();
	old_mount();
	rmdir(dir);
	return (failed);
}
