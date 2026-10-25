/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ubench: AbyssBSD's userland benchmarks (the P4 suite on).
 *
 * One static FreeBSD binary, run unchanged on both kernels: FreeBSD 15.1 is
 * the baseline, and the same file runs on AbyssBSD. The workloads are
 * specified in tests/perf/README.md; it prints the kbench line format.
 *
 *	./ubench p4
 *	./ubench p5	(as root: makes and mounts its file systems)
 *	./ubench vt	(as root, with a framebuffer: writes to ttyv1 and ttyv2)
 *	./ubench p6	(built with -DUB_P6 -lthr; runs base's programs)
 *
 * p4 uses Tier A system calls (docs/COMPATIBILITY.md §2.1), plus
 * cpuset_setaffinity to bind itself as kbench binds its workers. p5 adds
 * the file-system calls, and runs /rescue's newfs, tar, and rm, and fsx.
 * p6 adds threads, kqueue, poll and select, UNIX sockets, and ptys, and
 * execs base's dynamic programs.
 */

#ifdef UB_P6
#define	UB_HAS_P6	1
#define	FD_SETSIZE	4096	/* event.select1000's 2000 descriptors */
#else
#define	UB_HAS_P6	0
#endif

#include <sys/param.h>
#include <sys/consio.h>
#include <sys/cpuset.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#ifdef UB_P6
#include <sys/event.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/un.h>
#endif

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <fts.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#ifdef UB_P6
#include <poll.h>
#include <pthread.h>
#include <semaphore.h>
#include <termios.h>
#endif

#define	UB_ROUNDS	3		/* rounds per benchmark; median */
#define	UB_ROUND_NS	1000000000ULL	/* each round runs for 1 s */
#define	UB_PAGE		4096
#define	UB_ANON		(64UL << 20)	/* vm.fault.anon region */
#define	UB_COW		(16UL << 20)	/* vm.fault.cow region */
#define	UB_PIPE_BW	65536		/* pipe.bw transfer size */
#define	UB_EXEC_CHAIN	100		/* execs per proc.exec round */
#define	UB_DIR_ENTRIES	10000		/* fs.*.readdir directory size */
#define	UB_STORM	1000		/* files per create/rename/unlink round */
#define	UB_FILE		(64UL << 20)	/* fs.*.seq_* and rand_* file size */
#define	UB_IO		65536		/* fs.*.seq_* transfer size */
#define	UB_DISK		"/dev/nda0"	/* the scratch disk p5 formats */
#define	UB_TREE		"/tmp/tree.tar"	/* fs.*.extract input */
#define	UB_FSX		"/tmp/fsx"

static const char *self;

static void
die(const char *what)
{
	fprintf(stderr, "ubench: %s: %s\n", what, strerror(errno));
	exit(1);
}

/*
 * Binds the calling process (and the children it creates after) to `cpu`.
 * Unbound, a freshly exec'd child lands on an idle CPU, and proc.vfork_exec
 * measured that CPU's wake-up and cold caches: 550 us where it takes 100.
 */
static void
bind_cpu(int cpu)
{
	cpuset_t set;

	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	if (cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1, sizeof(set),
	    &set) != 0)
		die("cpuset_setaffinity");
}

static uint64_t
now(void)
{
	struct timespec ts;

	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ((uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec);
}

static int
cmp_double(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;

	return ((x > y) - (x < y));
}

static void
report(const char *name, double v[UB_ROUNDS])
{
	qsort(v, UB_ROUNDS, sizeof(v[0]), cmp_double);
	printf("kbench %s %.1f ns/op\n", name, v[UB_ROUNDS / 2]);
	fflush(stdout);
}

/*
 * A throughput benchmark: `op` runs in batches until a round's second is
 * up; the result is elapsed time over operations completed. `op` returns
 * how many operations it did (pages touched, for the fault benchmarks).
 */
static void
throughput(const char *name, uint64_t (*op)(void *), void *arg)
{
	double v[UB_ROUNDS];
	uint64_t ops, start, t;

	for (int r = 0; r < UB_ROUNDS; r++) {
		ops = 0;
		start = now();
		do {
			ops += op(arg);
			t = now();
		} while (t - start < UB_ROUND_NS);
		v[r] = (double)(t - start) / (double)ops;
	}
	report(name, v);
}

/* syscall.getpid: the null system call (libc does not cache it). */
static uint64_t
op_getpid(void *arg __unused)
{
	for (int i = 0; i < 1000; i++)
		(void)getpid();
	return (1000);
}

static int null_fd, zero_fd;
static char io_buf[UB_PAGE];

static uint64_t
op_null_write(void *arg __unused)
{
	for (int i = 0; i < 1000; i++)
		if (write(null_fd, io_buf, 1) != 1)
			die("write /dev/null");
	return (1000);
}

static uint64_t
op_zero_read(void *arg __unused)
{
	for (int i = 0; i < 1000; i++)
		if (read(zero_fd, io_buf, UB_PAGE) != UB_PAGE)
			die("read /dev/zero");
	return (1000);
}

static void
reap(pid_t pid)
{
	int status;

	if (wait4(pid, &status, 0, NULL) != pid)
		die("wait4");
	if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
		fprintf(stderr, "ubench: child failed (status %#x)\n", status);
		exit(1);
	}
}

/* proc.fork: fork, the child exits at once, the parent reaps it. */
static uint64_t
op_fork(void *arg __unused)
{
	pid_t pid = fork();

	if (pid < 0)
		die("fork");
	if (pid == 0)
		_exit(0);
	reap(pid);
	return (1);
}

/* proc.vfork_exec: vfork, the child execs this binary, which exits. */
static uint64_t
op_vfork_exec(void *arg __unused)
{
	char *argv[] = { (char *)self, (char *)"exit", NULL };
	char *envp[] = { NULL };
	pid_t pid = vfork();

	if (pid < 0)
		die("vfork");
	if (pid == 0) {
		execve(self, argv, envp);
		_exit(127);
	}
	reap(pid);
	return (1);
}

/*
 * proc.exec: a child execs this binary UB_EXEC_CHAIN times in a row, each
 * image counting down; the last writes the elapsed time to a pipe. The
 * child's own fork and the parent's wait fall outside the timed span.
 */
static uint64_t
op_exec(void *arg)
{
	double *ns = arg;
	char n[16], fd[16], t0[32];
	int p[2];
	uint64_t elapsed;
	pid_t pid;

	if (pipe(p) != 0)
		die("pipe");
	pid = fork();
	if (pid < 0)
		die("fork");
	if (pid == 0) {
		char *argv[] = { (char *)self, (char *)"chain", n, fd, t0, NULL };
		char *envp[] = { NULL };

		close(p[0]);
		snprintf(n, sizeof(n), "%d", UB_EXEC_CHAIN);
		snprintf(fd, sizeof(fd), "%d", p[1]);
		snprintf(t0, sizeof(t0), "%ju", (uintmax_t)now());
		execve(self, argv, envp);
		_exit(127);
	}
	close(p[1]);
	if (read(p[0], &elapsed, sizeof(elapsed)) != sizeof(elapsed))
		die("read exec chain");
	close(p[0]);
	reap(pid);
	*ns += (double)elapsed;
	return (UB_EXEC_CHAIN);
}

static int
exec_chain(char **argv)
{
	int n = atoi(argv[2]), fd = atoi(argv[3]);
	uint64_t t0 = strtoull(argv[4], NULL, 10), elapsed;
	char next[16];
	char *envp[] = { NULL };

	if (n > 1) {
		snprintf(next, sizeof(next), "%d", n - 1);
		argv[2] = next;
		execve(self, argv, envp);
		return (127);
	}
	elapsed = now() - t0;
	if (write(fd, &elapsed, sizeof(elapsed)) != sizeof(elapsed))
		return (1);
	return (0);
}

/* proc.exec reports exec time only, so it times its own rounds. */
static void
bench_exec(void)
{
	double v[UB_ROUNDS];
	uint64_t ops, start;
	double ns;

	for (int r = 0; r < UB_ROUNDS; r++) {
		ops = 0;
		ns = 0;
		start = now();
		do
			ops += op_exec(&ns);
		while (now() - start < UB_ROUND_NS);
		v[r] = ns / (double)ops;
	}
	report("proc.exec", v);
}

/* vm.fault.anon: map anonymous memory, write each page, unmap. */
static uint64_t
op_fault_anon(void *arg __unused)
{
	char *p = mmap(NULL, UB_ANON, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_PRIVATE, -1, 0);

	if (p == MAP_FAILED)
		die("mmap");
	for (size_t off = 0; off < UB_ANON; off += UB_PAGE)
		p[off] = 1;
	if (munmap(p, UB_ANON) != 0)
		die("munmap");
	return (UB_ANON / UB_PAGE);
}

/*
 * vm.fault.cow: after fork, the child writes each page of a region the
 * parent had written, and reports the time its writes took.
 */
static void
bench_fault_cow(void)
{
	double v[UB_ROUNDS];
	uint64_t ops, start, ns, t;
	char *p;
	int pp[2];
	pid_t pid;

	p = mmap(NULL, UB_COW, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE,
	    -1, 0);
	if (p == MAP_FAILED)
		die("mmap");
	for (size_t off = 0; off < UB_COW; off += UB_PAGE)
		p[off] = 1;
	for (int r = 0; r < UB_ROUNDS; r++) {
		ops = ns = 0;
		start = now();
		do {
			if (pipe(pp) != 0)
				die("pipe");
			pid = fork();
			if (pid < 0)
				die("fork");
			if (pid == 0) {
				t = now();
				for (size_t off = 0; off < UB_COW; off += UB_PAGE)
					p[off] = 2;
				t = now() - t;
				_exit(write(pp[1], &t, sizeof(t)) == sizeof(t) ?
				    0 : 1);
			}
			if (read(pp[0], &t, sizeof(t)) != sizeof(t))
				die("read cow time");
			close(pp[0]);
			close(pp[1]);
			reap(pid);
			ns += t;
			ops += UB_COW / UB_PAGE;
		} while (now() - start < UB_ROUND_NS);
		v[r] = (double)ns / (double)ops;
	}
	munmap(p, UB_COW);
	report("vm.fault.cow", v);
}

/* vm.mmap: map one anonymous page, untouched, and unmap it. */
static uint64_t
op_mmap(void *arg __unused)
{
	for (int i = 0; i < 100; i++) {
		void *p = mmap(NULL, UB_PAGE, PROT_READ | PROT_WRITE,
		    MAP_ANON | MAP_PRIVATE, -1, 0);

		if (p == MAP_FAILED)
			die("mmap");
		if (munmap(p, UB_PAGE) != 0)
			die("munmap");
	}
	return (100);
}

static volatile sig_atomic_t caught;

static void
on_usr1(int sig __unused)
{
	caught++;
}

/* sig.deliver: send SIGUSR1 to ourselves; it is delivered on return. */
static uint64_t
op_signal(void *arg __unused)
{
	pid_t me = getpid();

	for (int i = 0; i < 100; i++)
		kill(me, SIGUSR1);
	return (100);
}

/*
 * The pipe benchmarks run a partner process. It exits when its input
 * reaches end of file.
 */
struct pipes {
	int to[2];	/* parent -> partner */
	int from[2];	/* partner -> parent */
	pid_t pid;
};

static void
partner(struct pipes *pp, size_t chunk, int echo)
{
	static char buf[UB_PIPE_BW];
	ssize_t n;

	if (pipe(pp->to) != 0 || pipe(pp->from) != 0)
		die("pipe");
	pp->pid = fork();
	if (pp->pid < 0)
		die("fork");
	if (pp->pid != 0) {
		close(pp->to[0]);
		close(pp->from[1]);
		return;
	}
	close(pp->to[1]);
	close(pp->from[0]);
	bind_cpu(2);
	while ((n = read(pp->to[0], buf, chunk)) > 0)
		if (echo && write(pp->from[1], buf, (size_t)n) != n)
			_exit(1);
	_exit(n == 0 ? 0 : 1);
}

static void
partner_done(struct pipes *pp)
{
	close(pp->to[1]);
	reap(pp->pid);
	close(pp->from[0]);
}

/* pipe.pingpong: one byte there and back. One op = one round trip. */
static uint64_t
op_pingpong(void *arg)
{
	struct pipes *pp = arg;
	char c = 0;

	for (int i = 0; i < 100; i++) {
		if (write(pp->to[1], &c, 1) != 1 ||
		    read(pp->from[0], &c, 1) != 1)
			die("pipe ping-pong");
	}
	return (100);
}

/* pipe.bw: 64 KiB writes to a reader. One op = one write. */
static uint64_t
op_pipe_bw(void *arg)
{
	static char buf[UB_PIPE_BW];
	struct pipes *pp = arg;

	for (int i = 0; i < 16; i++)
		if (write(pp->to[1], buf, sizeof(buf)) != sizeof(buf))
			die("pipe write");
	return (16);
}

static void
suite_p4(void)
{
	struct sigaction sa;
	struct pipes pp;

	throughput("syscall.getpid", op_getpid, NULL);

	null_fd = open("/dev/null", O_WRONLY);
	zero_fd = open("/dev/zero", O_RDONLY);
	if (null_fd < 0 || zero_fd < 0)
		die("open /dev/null, /dev/zero");
	throughput("io.null.write", op_null_write, NULL);
	throughput("io.zero.read", op_zero_read, NULL);
	close(null_fd);
	close(zero_fd);

	throughput("proc.fork", op_fork, NULL);
	throughput("proc.vfork_exec", op_vfork_exec, NULL);
	bench_exec();

	throughput("vm.fault.anon", op_fault_anon, NULL);
	bench_fault_cow();
	throughput("vm.mmap", op_mmap, NULL);

	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_usr1;
	sigemptyset(&sa.sa_mask);
	if (sigaction(SIGUSR1, &sa, NULL) != 0)
		die("sigaction");
	throughput("sig.deliver", op_signal, NULL);

	partner(&pp, 1, 1);
	throughput("pipe.pingpong", op_pingpong, &pp);
	partner_done(&pp);
	partner(&pp, UB_PIPE_BW, 0);
	throughput("pipe.bw", op_pipe_bw, &pp);
	partner_done(&pp);
}


/*
 * Suite p5: the file systems. Each benchmark runs on a UFS2 file system
 * (newfs -U on the scratch disk: soft updates, FreeBSD's default without a
 * journal) and on tmpfs, as fs.ufs.* and fs.tmpfs.*.
 */

#define	UB_UFS		"/bench"
#define	UB_TMPFS	"/benchtmp"

/* Runs `argv` (a full path first) with output discarded, and waits. */
static void
run_quiet(char *const argv[])
{
	pid_t pid = vfork();

	if (pid < 0)
		die("vfork");
	if (pid == 0) {
		int null = open("/dev/null", O_WRONLY);

		if (null >= 0) {
			dup2(null, 1);
			dup2(null, 2);
		}
		execv(argv[0], argv);
		_exit(127);
	}
	reap(pid);
}

static void
iov_set(struct iovec *iov, const char *name, const char *value)
{
	iov[0].iov_base = (void *)(uintptr_t)name;
	iov[0].iov_len = strlen(name) + 1;
	iov[1].iov_base = (void *)(uintptr_t)value;
	iov[1].iov_len = strlen(value) + 1;
}

static void
mount_fs(const char *type, const char *path, const char *from)
{
	struct iovec iov[6];
	int n = 4;

	iov_set(&iov[0], "fstype", type);
	iov_set(&iov[2], "fspath", path);
	if (from != NULL) {
		iov_set(&iov[4], "from", from);
		n = 6;
	}
	if (mkdir(path, 0755) != 0 && errno != EEXIST)
		die(path);
	if (nmount(iov, n, 0) != 0)
		die("nmount");
}

/* `<dir>/<name>`, in a static buffer per slot. */
static const char *
at(int slot, const char *dir, const char *name)
{
	static char buf[4][PATH_MAX];

	snprintf(buf[slot], sizeof(buf[slot]), "%s/%s", dir, name);
	return (buf[slot]);
}

static void
create_file(const char *path, size_t size)
{
	static char chunk[UB_IO];
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);

	if (fd < 0)
		die(path);
	for (size_t off = 0; off < size; off += sizeof(chunk))
		if (write(fd, chunk, sizeof(chunk)) != sizeof(chunk))
			die("write");
	if (fsync(fd) != 0)
		die("fsync");
	close(fd);
}

static const char *fs_hit, *fs_miss;

static uint64_t
op_stat_hit(void *arg __unused)
{
	struct stat st;

	for (int i = 0; i < 1000; i++)
		if (stat(fs_hit, &st) != 0)
			die("stat");
	return (1000);
}

static uint64_t
op_stat_miss(void *arg __unused)
{
	struct stat st;

	for (int i = 0; i < 1000; i++)
		if (stat(fs_miss, &st) == 0 || errno != ENOENT)
			die("stat of a missing file");
	return (1000);
}

static uint64_t
op_open_close(void *arg __unused)
{
	for (int i = 0; i < 1000; i++) {
		int fd = open(fs_hit, O_RDONLY);

		if (fd < 0)
			die("open");
		close(fd);
	}
	return (1000);
}

/* One pass over a large directory; one op = one entry. */
static uint64_t
op_readdir(void *arg)
{
	static char buf[65536];
	uint64_t entries = 0;
	off_t base;
	ssize_t n;
	int fd = open(arg, O_RDONLY | O_DIRECTORY);

	if (fd < 0)
		die("open directory");
	while ((n = getdirentries(fd, buf, sizeof(buf), &base)) > 0) {
		for (char *p = buf; p < buf + n;
		    p += ((struct dirent *)(void *)p)->d_reclen)
			entries++;
	}
	if (n < 0)
		die("getdirentries");
	close(fd);
	return (entries);
}

/* Rounds of each storm phase sampled when profiling (UB_PROF set). */
#define	UB_PROF_ROUNDS	30

/*
 * Tells AbyssBSD's profiler (abyss.prof) to stop or go on sampling, so a
 * profile of one storm phase leaves the others out. FreeBSD prints it too.
 */
static void
prof_sampling(int on)
{
	printf("kprof %s\n", on ? "resume" : "pause");
	fflush(stdout);
}

/* One storm phase over UB_STORM files: 0 creates, 1 renames, 2 unlinks. */
static uint64_t
storm_phase(const char *dir, int phase)
{
	char a[PATH_MAX], b[PATH_MAX];
	uint64_t t0 = now();

	for (int i = 0; i < UB_STORM; i++) {
		if (phase < 2)
			snprintf(a, sizeof(a), "%s/f%d", dir, i);
		if (phase > 0)
			snprintf(b, sizeof(b), "%s/g%d", dir, i);
		if (phase == 0) {
			int fd = open(a, O_WRONLY | O_CREAT | O_EXCL, 0644);

			if (fd < 0)
				die("create");
			close(fd);
		} else if (phase == 1) {
			if (rename(a, b) != 0)
				die("rename");
		} else if (unlink(b) != 0)
			die("unlink");
	}
	return (now() - t0);
}

/*
 * Creates, renames, and unlinks UB_STORM files per round, timed apart. To
 * profile, each phase runs UB_PROF_ROUNDS rounds with only it sampled.
 */
static void
bench_storms(const char *fs, const char *dir)
{
	static const char *const phases[] = { "create", "rename", "unlink" };
	double v[3][UB_ROUNDS];
	char name[64];
	int prof = getenv("UB_PROF") != NULL;

	if (mkdir(dir, 0755) != 0 && errno != EEXIST)
		die("mkdir");
	if (!prof) {
		for (int r = 0; r < UB_ROUNDS; r++)
			for (int p = 0; p < 3; p++)
				v[p][r] = (double)storm_phase(dir, p) / UB_STORM;
		for (int p = 0; p < 3; p++) {
			snprintf(name, sizeof(name), "fs.%s.%s", fs, phases[p]);
			report(name, v[p]);
		}
		return;
	}
	for (int sampled = 0; sampled < 3; sampled++) {
		prof_sampling(0);
		for (int r = 0; r < UB_PROF_ROUNDS; r++)
			for (int p = 0; p < 3; p++) {
				if (p == sampled)
					prof_sampling(1);
				uint64_t ns = storm_phase(dir, p);
				if (p == sampled) {
					prof_sampling(0);
					v[p][r % UB_ROUNDS] = (double)ns / UB_STORM;
				}
			}
		prof_sampling(1);
		snprintf(name, sizeof(name), "fs.%s.%s", fs, phases[sampled]);
		report(name, v[sampled]);
	}
}

/* Rewrites the whole file sequentially; one op = one UB_IO write. */
static void
bench_seq_write(const char *name, const char *path)
{
	static char chunk[UB_IO];
	double v[UB_ROUNDS];

	for (int r = 0; r < UB_ROUNDS; r++) {
		int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
		uint64_t t0;

		if (fd < 0)
			die("open");
		t0 = now();
		for (size_t off = 0; off < UB_FILE; off += sizeof(chunk))
			if (write(fd, chunk, sizeof(chunk)) != sizeof(chunk))
				die("write");
		v[r] = (double)(now() - t0) / (UB_FILE / UB_IO);
		close(fd);
	}
	report(name, v);
}

static int io_fd;
static off_t io_off;
static uint64_t io_rng = 0x9e3779b97f4a7c15ULL;

static uint64_t
next_rand(void)
{
	io_rng ^= io_rng << 13;
	io_rng ^= io_rng >> 7;
	io_rng ^= io_rng << 17;
	return (io_rng);
}

/* Sequential UB_IO reads, wrapping at the end; warm. */
static uint64_t
op_seq_read(void *arg __unused)
{
	static char chunk[UB_IO];

	for (int i = 0; i < 64; i++) {
		if (pread(io_fd, chunk, sizeof(chunk), io_off) != sizeof(chunk))
			die("pread");
		io_off = (io_off + UB_IO) % UB_FILE;
	}
	return (64);
}

static uint64_t
op_rand_read(void *arg __unused)
{
	for (int i = 0; i < 256; i++) {
		off_t off = (off_t)(next_rand() % (UB_FILE / UB_PAGE)) * UB_PAGE;

		if (pread(io_fd, io_buf, UB_PAGE, off) != UB_PAGE)
			die("pread");
	}
	return (256);
}

static uint64_t
op_rand_write(void *arg __unused)
{
	for (int i = 0; i < 256; i++) {
		off_t off = (off_t)(next_rand() % (UB_FILE / UB_PAGE)) * UB_PAGE;

		if (pwrite(io_fd, io_buf, UB_PAGE, off) != UB_PAGE)
			die("pwrite");
	}
	return (256);
}

/* fsync latency: rewrite one block, then fsync. */
static uint64_t
op_fsync(void *arg __unused)
{
	for (int i = 0; i < 16; i++) {
		if (pwrite(io_fd, io_buf, UB_PAGE, 0) != UB_PAGE)
			die("pwrite");
		if (fsync(io_fd) != 0)
			die("fsync");
	}
	return (16);
}

/* Cold sequential read: unmount and mount UFS between rounds. */
static void
bench_cold_read(const char *path)
{
	static char chunk[UB_IO];
	double v[UB_ROUNDS];

	for (int r = 0; r < UB_ROUNDS; r++) {
		uint64_t t0;
		int fd;

		if (unmount(UB_UFS, 0) != 0)
			die("unmount");
		mount_fs("ufs", UB_UFS, UB_DISK);
		if ((fd = open(path, O_RDONLY)) < 0)
			die("open");
		t0 = now();
		for (size_t off = 0; off < UB_FILE; off += sizeof(chunk))
			if (read(fd, chunk, sizeof(chunk)) != sizeof(chunk))
				die("read");
		v[r] = (double)(now() - t0) / (UB_FILE / UB_IO);
		close(fd);
	}
	report("fs.ufs.seq_read.cold", v);
}

/* Extracts the tree; one op = one whole extraction. */
static void
bench_extract(const char *name, const char *dir)
{
	char *rm[] = { "/rescue/rm", "-rf", (char *)(uintptr_t)dir, NULL };
	char *tar[] = { "/rescue/tar", "-xf", UB_TREE, "-C",
	    (char *)(uintptr_t)dir, NULL };
	double v[UB_ROUNDS];

	for (int r = 0; r < UB_ROUNDS; r++) {
		uint64_t t0;

		run_quiet(rm);
		if (mkdir(dir, 0755) != 0)
			die("mkdir");
		t0 = now();
		run_quiet(tar);
		v[r] = (double)(now() - t0);
	}
	report(name, v);
}

/* One walk of the extracted tree, every entry stat'ed (as find(1) does). */
static uint64_t
op_find(void *arg)
{
	char *paths[] = { arg, NULL };
	FTS *fts = fts_open(paths, FTS_PHYSICAL | FTS_NOCHDIR, NULL);
	FTSENT *e;

	if (fts == NULL)
		die("fts_open");
	while ((e = fts_read(fts)) != NULL)
		;
	fts_close(fts);
	return (1);
}

/* FreeBSD's fsx with a fixed seed; one op = one run. */
static void
bench_fsx(const char *name, const char *dir)
{
	double v[UB_ROUNDS];

	for (int r = 0; r < UB_ROUNDS; r++) {
		char seed[16];
		char *fsx[] = { UB_FSX, "-q", "-N", "10000", "-S", seed,
		    (char *)(uintptr_t)at(3, dir, "fsx.file"), NULL };
		uint64_t t0;

		snprintf(seed, sizeof(seed), "%d", r + 1);
		t0 = now();
		run_quiet(fsx);
		v[r] = (double)(now() - t0);
	}
	report(name, v);
}

static void
suite_fs(const char *fs, const char *root)
{
	char name[64], d[PATH_MAX];

#define	NAME(n)	(snprintf(name, sizeof(name), "fs.%s.%s", fs, n), name)
	snprintf(d, sizeof(d), "%s/a/b/c", root);
	mkdir(at(0, root, "a"), 0755);
	mkdir(at(0, root, "a/b"), 0755);
	if (mkdir(d, 0755) != 0)
		die("mkdir");
	create_file(at(1, d, "file"), 0);
	fs_hit = at(1, d, "file");
	fs_miss = at(2, d, "missing");
	throughput(NAME("stat.hit"), op_stat_hit, NULL);
	throughput(NAME("stat.miss"), op_stat_miss, NULL);
	throughput(NAME("open_close"), op_open_close, NULL);

	if (mkdir(at(0, root, "big"), 0755) != 0)
		die("mkdir");
	for (int i = 0; i < UB_DIR_ENTRIES; i++) {
		char n[32];

		snprintf(n, sizeof(n), "big/entry-%05d", i);
		create_file(at(0, root, n), 0);
	}
	throughput(NAME("readdir"), op_readdir, (void *)(uintptr_t)at(0, root, "big"));

	bench_storms(fs, at(0, root, "storm"));

	bench_seq_write(NAME("seq_write"), at(1, root, "data"));
	create_file(at(1, root, "data"), UB_FILE);
	if ((io_fd = open(at(1, root, "data"), O_RDWR)) < 0)
		die("open");
	throughput(NAME("seq_read.warm"), op_seq_read, NULL);
	throughput(NAME("rand_read"), op_rand_read, NULL);
	throughput(NAME("rand_write"), op_rand_write, NULL);
	close(io_fd);
	if (strcmp(fs, "ufs") == 0)
		bench_cold_read(at(1, root, "data"));

	create_file(at(1, root, "small"), 1 << 20);
	if ((io_fd = open(at(1, root, "small"), O_RDWR)) < 0)
		die("open");
	throughput(NAME("fsync"), op_fsync, NULL);
	close(io_fd);

	bench_extract(NAME("extract"), at(1, root, "tree"));
	throughput(NAME("find"), op_find, (void *)(uintptr_t)at(1, root, "tree"));
	bench_fsx(NAME("fsx"), root);
#undef NAME
}

/*
 * The vt suite: text written to a virtual terminal. Each operation is a
 * write(2) of a batch of lines; the unit is one line.
 */
#define	UB_VT_LINES	64		/* lines per write */
static int vt_fd;
static char vt_cat[UB_VT_LINES * 81];	/* 80 columns and a newline */
static char vt_scroll[UB_VT_LINES * 11];	/* "line NNNNN\n" */
static char vt_color[UB_VT_LINES * 181];	/* 20 x (SGR + 4 chars), NL */
static size_t vt_scroll_len, vt_color_len;

static void
write_all(int fd, const char *buf, size_t len)
{
	ssize_t n;

	while (len > 0) {
		if ((n = write(fd, buf, len)) < 0)
			die("write");
		buf += n;
		len -= (size_t)n;
	}
}

static uint64_t
op_vt_cat(void *arg __unused)
{
	write_all(vt_fd, vt_cat, sizeof(vt_cat));
	return (UB_VT_LINES);
}

static uint64_t
op_vt_scroll(void *arg __unused)
{
	write_all(vt_fd, vt_scroll, vt_scroll_len);
	return (UB_VT_LINES);
}

static uint64_t
op_vt_color(void *arg __unused)
{
	write_all(vt_fd, vt_color, vt_color_len);
	return (UB_VT_LINES);
}

static int
open_vt(const char *path)
{
	int fd;

	if ((fd = open(path, O_RDWR | O_NOCTTY)) < 0)
		die(path);
	return (fd);
}

static void
suite_vt(void)
{
	char *p;
	int visible, hidden;

	for (int l = 0; l < UB_VT_LINES; l++) {
		for (int c = 0; c < 80; c++)
			vt_cat[l * 81 + c] = (char)(' ' + (l + c) % 95);
		vt_cat[l * 81 + 80] = '\n';
	}
	p = vt_scroll;
	for (int l = 0; l < UB_VT_LINES; l++)
		p += sprintf(p, "line %05d\n", l);
	vt_scroll_len = (size_t)(p - vt_scroll);
	p = vt_color;
	for (int l = 0; l < UB_VT_LINES; l++) {
		for (int g = 0; g < 20; g++)
			p += sprintf(p, "\033[3%dm%c%c%c%c", (l + g) % 8,
			    'a' + g, 'b' + g, 'c' + g, 'd' + g);
		*p++ = '\n';
	}
	vt_color_len = (size_t)(p - vt_color);

	/* ttyv1 visible; vt switches only to an open window. */
	visible = open_vt("/dev/ttyv1");
	hidden = open_vt("/dev/ttyv2");
	if (ioctl(visible, VT_ACTIVATE, 2) != 0 ||
	    ioctl(visible, VT_WAITACTIVE, 2) != 0)
		die("VT_ACTIVATE");
	vt_fd = visible;
	throughput("vt.cat", op_vt_cat, NULL);
	throughput("vt.scroll", op_vt_scroll, NULL);
	throughput("vt.color", op_vt_color, NULL);
	vt_fd = hidden;
	throughput("vt.hidden", op_vt_cat, NULL);
	close(visible);
	close(hidden);
}

static void
suite_p5(void)
{
	char *newfs[] = { "/rescue/newfs", "-U", UB_DISK, NULL };

	run_quiet(newfs);
	mount_fs("ufs", UB_UFS, UB_DISK);
	mount_fs("tmpfs", UB_TMPFS, NULL);
	suite_fs("ufs", UB_UFS);
	suite_fs("tmpfs", UB_TMPFS);
	if (unmount(UB_UFS, 0) != 0 || unmount(UB_TMPFS, 0) != 0)
		die("unmount");
}

#ifdef UB_P6
/*
 * Suite p6: threads, events, UNIX sockets, ptys, and dynamic exec
 * (docs/MULTIUSER.md §16). Built apart, with -DUB_P6 and libthr, so the
 * earlier suites' binary stays single-threaded and unchanged.
 */

#define	UB_P6_PIPES	1000		/* event.*1000 descriptors */
#define	UB_SOCK		"/tmp/ubench.sock"

static void
bind_thread(int cpu)
{
	cpuset_t set;

	CPU_ZERO(&set);
	CPU_SET(cpu, &set);
	if (cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_TID, -1, sizeof(set),
	    &set) != 0)
		die("cpuset_setaffinity");
}

static void
pcheck(int error, const char *what)
{
	if (error != 0) {
		errno = error;
		die(what);
	}
}

/*
 * A team benchmark (kbench's rules): T workers, worker i bound to CPU
 * (i + 1) mod 8, the coordinator on CPU 0. Each round the workers start
 * together, run until the coordinator's stop after 1 s, and count their
 * operations; the result is elapsed time over the total, median of three.
 */
struct team {
	void	(*op)(int);		/* one operation by worker i */
	_Atomic int stop;
	pthread_barrier_t go;
	struct {
		uint64_t ops;
		char pad[120];
	} count[8];
};

static struct team team;

struct worker {
	int id;
};

static void *
team_worker(void *arg)
{
	int id = ((struct worker *)arg)->id;
	uint64_t ops = 0;

	bind_thread((id + 1) % 8);
	pthread_barrier_wait(&team.go);
	while (!team.stop) {
		team.op(id);
		ops++;
	}
	team.count[id].ops = ops;
	return (NULL);
}

static void
run_team(const char *name, int threads, void (*op)(int))
{
	double v[UB_ROUNDS];
	pthread_t tid[8];
	struct worker w[8];
	struct timespec second = { 1, 0 };
	uint64_t start, ops;

	bind_thread(0);
	team.op = op;
	for (int r = 0; r < UB_ROUNDS; r++) {
		team.stop = 0;
		pcheck(pthread_barrier_init(&team.go, NULL, threads + 1),
		    "pthread_barrier_init");
		for (int i = 0; i < threads; i++) {
			w[i].id = i;
			pcheck(pthread_create(&tid[i], NULL, team_worker, &w[i]),
			    "pthread_create");
		}
		pthread_barrier_wait(&team.go);
		start = now();
		nanosleep(&second, NULL);
		team.stop = 1;
		ops = 0;
		for (int i = 0; i < threads; i++) {
			pcheck(pthread_join(tid[i], NULL), "pthread_join");
			ops += team.count[i].ops;
		}
		v[r] = (double)(now() - start) / (double)ops;
		pthread_barrier_destroy(&team.go);
	}
	report(name, v);
	bind_thread(1);
}

static pthread_mutex_t *mtx;
static pthread_rwlock_t rwl;
static volatile uint64_t counter;

static void
op_mutex(int id __unused)
{
	pthread_mutex_lock(mtx);
	counter++;
	pthread_mutex_unlock(mtx);
}

/* Nine reads to a write. */
static void
op_rwlock(int id)
{
	static _Thread_local unsigned n;

	(void)id;
	if (++n % 10 == 0) {
		pthread_rwlock_wrlock(&rwl);
		counter++;
	} else {
		pthread_rwlock_rdlock(&rwl);
		(void)counter;
	}
	pthread_rwlock_unlock(&rwl);
}

/*
 * The ping-pong benchmarks pair the coordinator (CPU 1) with a partner
 * thread (CPU 2): each op function makes 100 round trips, and the partner
 * leaves when it finds `pp_stop` on waking.
 */
static pthread_t pp_tid;
static _Atomic int pp_stop;

static void
pp_start(void *(*fn)(void *), void *arg)
{
	pp_stop = 0;
	pcheck(pthread_create(&pp_tid, NULL, fn, arg), "pthread_create");
}

static pthread_mutex_t cv_mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv[2] = { PTHREAD_COND_INITIALIZER,
    PTHREAD_COND_INITIALIZER };
static int cv_turn;

static void *
cv_partner(void *arg __unused)
{
	bind_thread(2);
	pthread_mutex_lock(&cv_mtx);
	for (;;) {
		while (cv_turn != 1 && !pp_stop)
			pthread_cond_wait(&cv[1], &cv_mtx);
		if (pp_stop)
			break;
		cv_turn = 0;
		pthread_cond_signal(&cv[0]);
	}
	pthread_mutex_unlock(&cv_mtx);
	return (NULL);
}

static uint64_t
op_cond(void *arg __unused)
{
	pthread_mutex_lock(&cv_mtx);
	for (int i = 0; i < 100; i++) {
		cv_turn = 1;
		pthread_cond_signal(&cv[1]);
		while (cv_turn != 0)
			pthread_cond_wait(&cv[0], &cv_mtx);
	}
	pthread_mutex_unlock(&cv_mtx);
	return (100);
}

static void
cv_stop(void)
{
	pthread_mutex_lock(&cv_mtx);
	pp_stop = 1;
	pthread_cond_signal(&cv[1]);
	pthread_mutex_unlock(&cv_mtx);
	pcheck(pthread_join(pp_tid, NULL), "pthread_join");
}

static sem_t sem[2];

static void *
sem_partner(void *arg __unused)
{
	bind_thread(2);
	for (;;) {
		while (sem_wait(&sem[1]) != 0)
			if (errno != EINTR)
				die("sem_wait");
		if (pp_stop)
			break;
		sem_post(&sem[0]);
	}
	return (NULL);
}

static uint64_t
op_sem(void *arg __unused)
{
	for (int i = 0; i < 100; i++) {
		sem_post(&sem[1]);
		while (sem_wait(&sem[0]) != 0)
			if (errno != EINTR)
				die("sem_wait");
	}
	return (100);
}

static void *
thread_nop(void *arg)
{
	return (arg);
}

/* thr.create: create a thread that returns at once, and join it. */
static uint64_t
op_create(void *arg __unused)
{
	pthread_t t;

	pcheck(pthread_create(&t, NULL, thread_nop, NULL), "pthread_create");
	pcheck(pthread_join(t, NULL), "pthread_join");
	return (1);
}

static void
suite_thr(void)
{
	static const char *kinds[] = { "", ".pi", ".pshared" };
	static const int teams[] = { 1, 2, 4, 8 };
	char name[64];
	pthread_mutexattr_t ma;
	pthread_mutex_t m;

	for (int k = 0; k < 3; k++) {
		pcheck(pthread_mutexattr_init(&ma), "pthread_mutexattr_init");
		if (k == 1)
			pcheck(pthread_mutexattr_setprotocol(&ma,
			    PTHREAD_PRIO_INHERIT), "setprotocol");
		if (k == 2)
			pcheck(pthread_mutexattr_setpshared(&ma,
			    PTHREAD_PROCESS_SHARED), "setpshared");
		pcheck(pthread_mutex_init(&m, &ma), "pthread_mutex_init");
		mtx = &m;
		for (int t = 0; t < 4; t++) {
			snprintf(name, sizeof(name), "thr.mutex%s.t%d",
			    kinds[k], teams[t]);
			run_team(name, teams[t], op_mutex);
		}
		pthread_mutex_destroy(&m);
		pthread_mutexattr_destroy(&ma);
	}

	pcheck(pthread_rwlock_init(&rwl, NULL), "pthread_rwlock_init");
	for (int t = 0; t < 4; t++) {
		snprintf(name, sizeof(name), "thr.rwlock.t%d", teams[t]);
		run_team(name, teams[t], op_rwlock);
	}
	pthread_rwlock_destroy(&rwl);

	cv_turn = 0;
	pp_start(cv_partner, NULL);
	throughput("thr.cond.pingpong", op_cond, NULL);
	cv_stop();

	if (sem_init(&sem[0], 0, 0) != 0 || sem_init(&sem[1], 0, 0) != 0)
		die("sem_init");
	pp_start(sem_partner, NULL);
	throughput("thr.sem.pingpong", op_sem, NULL);
	pp_stop = 1;
	sem_post(&sem[1]);
	pcheck(pthread_join(pp_tid, NULL), "pthread_join");
	sem_destroy(&sem[0]);
	sem_destroy(&sem[1]);

	throughput("thr.create", op_create, NULL);
}

/* event.*: pipes, descriptors in one kqueue, poll set, and select set. */
static int ev_rd[UB_P6_PIPES], ev_wr[UB_P6_PIPES];
static int ev_kq, ev_next;

static void
ev_cycle_write(int i)
{
	char c = 0;

	if (write(ev_wr[i], &c, 1) != 1)
		die("pipe write");
}

static void
ev_cycle_read(int i)
{
	char c;

	if (read(ev_rd[i], &c, 1) != 1)
		die("pipe read");
}

/* One byte into the next pipe, kevent finds it, the byte read back. */
static uint64_t
op_kevent(void *arg)
{
	int n = *(int *)arg;
	struct kevent ev;

	for (int i = 0; i < 100; i++) {
		int p = ev_next;

		ev_next = (ev_next + 1) % n;
		ev_cycle_write(p);
		if (kevent(ev_kq, NULL, 0, &ev, 1, NULL) != 1 ||
		    (int)ev.ident != ev_rd[p])
			die("kevent");
		ev_cycle_read(p);
	}
	return (100);
}

static struct pollfd ev_pfd[UB_P6_PIPES];

static uint64_t
op_poll(void *arg __unused)
{
	for (int i = 0; i < 10; i++) {
		int p = ev_next;

		ev_next = (ev_next + 1) % UB_P6_PIPES;
		ev_cycle_write(p);
		if (poll(ev_pfd, UB_P6_PIPES, -1) != 1 ||
		    ev_pfd[p].revents != POLLIN)
			die("poll");
		ev_cycle_read(p);
	}
	return (10);
}

static fd_set ev_set;
static int ev_maxfd;

static uint64_t
op_select(void *arg __unused)
{
	fd_set rd;

	for (int i = 0; i < 10; i++) {
		int p = ev_next;

		ev_next = (ev_next + 1) % UB_P6_PIPES;
		ev_cycle_write(p);
		rd = ev_set;
		if (select(ev_maxfd + 1, &rd, NULL, NULL, NULL) != 1 ||
		    !FD_ISSET(ev_rd[p], &rd))
			die("select");
		ev_cycle_read(p);
	}
	return (10);
}

/* EVFILT_USER: each side triggers the other's event and waits for its own. */
static int user_kq[2];

static void
user_trigger(int side)
{
	struct kevent ev;

	EV_SET(&ev, 1, EVFILT_USER, 0, NOTE_TRIGGER, 0, NULL);
	if (kevent(user_kq[side], &ev, 1, NULL, 0, NULL) != 0)
		die("kevent NOTE_TRIGGER");
}

static void
user_wait(int side)
{
	struct kevent ev;

	if (kevent(user_kq[side], NULL, 0, &ev, 1, NULL) != 1)
		die("kevent EVFILT_USER");
}

static void *
user_partner(void *arg __unused)
{
	bind_thread(2);
	for (;;) {
		user_wait(1);
		if (pp_stop)
			break;
		user_trigger(0);
	}
	return (NULL);
}

static uint64_t
op_user(void *arg __unused)
{
	for (int i = 0; i < 100; i++) {
		user_trigger(1);
		user_wait(0);
	}
	return (100);
}

/* EVFILT_TIMER: a one-shot timer 1 us ahead, armed and waited for. */
static uint64_t
op_timer(void *arg __unused)
{
	struct kevent ev;

	for (int i = 0; i < 10; i++) {
		EV_SET(&ev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT, NOTE_USECONDS,
		    1, NULL);
		if (kevent(ev_kq, &ev, 1, &ev, 1, NULL) != 1)
			die("kevent EVFILT_TIMER");
	}
	return (10);
}

static void
suite_event(void)
{
	struct kevent ev;
	int one = 1, all = UB_P6_PIPES;

	for (int i = 0; i < UB_P6_PIPES; i++) {
		int p[2];

		if (pipe(p) != 0)
			die("pipe");
		ev_rd[i] = p[0];
		ev_wr[i] = p[1];
		ev_pfd[i].fd = p[0];
		ev_pfd[i].events = POLLIN;
		FD_SET(p[0], &ev_set);
		if (p[0] > ev_maxfd)
			ev_maxfd = p[0];
	}

	if ((ev_kq = kqueue()) < 0)
		die("kqueue");
	EV_SET(&ev, ev_rd[0], EVFILT_READ, EV_ADD, 0, 0, NULL);
	if (kevent(ev_kq, &ev, 1, NULL, 0, NULL) != 0)
		die("kevent EV_ADD");
	ev_next = 0;
	throughput("event.kevent.pipe1", op_kevent, &one);
	for (int i = 1; i < UB_P6_PIPES; i++) {
		EV_SET(&ev, ev_rd[i], EVFILT_READ, EV_ADD, 0, 0, NULL);
		if (kevent(ev_kq, &ev, 1, NULL, 0, NULL) != 0)
			die("kevent EV_ADD");
	}
	ev_next = 0;
	throughput("event.kevent.pipe1000", op_kevent, &all);
	close(ev_kq);

	ev_next = 0;
	throughput("event.poll1000", op_poll, NULL);
	ev_next = 0;
	throughput("event.select1000", op_select, NULL);
	for (int i = 0; i < UB_P6_PIPES; i++) {
		close(ev_rd[i]);
		close(ev_wr[i]);
	}

	for (int s = 0; s < 2; s++) {
		if ((user_kq[s] = kqueue()) < 0)
			die("kqueue");
		EV_SET(&ev, 1, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, NULL);
		if (kevent(user_kq[s], &ev, 1, NULL, 0, NULL) != 0)
			die("kevent EVFILT_USER");
	}
	pp_start(user_partner, NULL);
	throughput("event.user.pingpong", op_user, NULL);
	pp_stop = 1;
	user_trigger(1);
	pcheck(pthread_join(pp_tid, NULL), "pthread_join");
	close(user_kq[0]);
	close(user_kq[1]);

	if ((ev_kq = kqueue()) < 0)
		die("kqueue");
	throughput("event.timer", op_timer, NULL);
	close(ev_kq);
}

/*
 * unix.*: a partner process on CPU 2 at the other end of a socket pair.
 * It echoes (`echo`), or reads and discards, until end of file; for
 * unix.dgram it answers each window of datagrams with one byte (a full
 * datagram socket fails the send with ENOBUFS rather than blocking); for
 * unix.rights it receives each descriptor and closes it.
 */
#define	UB_DGRAM_WINDOW	16

enum partner_mode { P_ECHO, P_SINK, P_DGRAM, P_RIGHTS };

static pid_t
sock_partner(int fd, int other, size_t chunk, enum partner_mode mode)
{
	static char buf[UB_PIPE_BW];
	pid_t pid = fork();
	ssize_t n;

	if (pid < 0)
		die("fork");
	if (pid != 0) {
		close(other);
		return (pid);
	}
	close(fd);
	bind_cpu(2);
	if (mode == P_RIGHTS) {
		for (;;) {
			char c;
			struct iovec iov = { &c, 1 };
			union {
				struct cmsghdr h;
				char space[CMSG_SPACE(sizeof(int))];
			} cm;
			struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1,
			    .msg_control = &cm, .msg_controllen = sizeof(cm) };

			n = recvmsg(other, &msg, 0);
			if (n <= 0)
				_exit(n == 0 ? 0 : 1);
			if (msg.msg_controllen < CMSG_LEN(sizeof(int)) ||
			    cm.h.cmsg_type != SCM_RIGHTS)
				_exit(1);
			close(*(int *)CMSG_DATA(&cm.h));
		}
	}
	if (mode == P_DGRAM) {
		for (;;) {
			for (int i = 0; i < UB_DGRAM_WINDOW; i++)
				if (read(other, buf, chunk) <= 0)
					_exit(1);
			if (write(other, buf, 1) != 1)
				_exit(1);
		}
	}
	while ((n = read(other, buf, chunk)) > 0)
		if (mode == P_ECHO && write(other, buf, (size_t)n) != n)
			_exit(1);
	_exit(n == 0 ? 0 : 1);
}

static int sock_fd;

static uint64_t
op_sock_pingpong(void *arg __unused)
{
	char c = 0;

	for (int i = 0; i < 100; i++)
		if (write(sock_fd, &c, 1) != 1 || read(sock_fd, &c, 1) != 1)
			die("socket ping-pong");
	return (100);
}

static uint64_t
op_sock_write(void *arg)
{
	static char buf[UB_PIPE_BW];
	size_t len = *(size_t *)arg;

	for (int i = 0; i < 16; i++)
		if (write(sock_fd, buf, len) != (ssize_t)len)
			die("socket write");
	return (16);
}

/* unix.dgram: a window of 64-byte datagrams, then the partner's answer. */
static uint64_t
op_dgram(void *arg __unused)
{
	char buf[64] = { 0 };

	for (int i = 0; i < UB_DGRAM_WINDOW; i++)
		if (write(sock_fd, buf, sizeof(buf)) != sizeof(buf))
			die("datagram write");
	if (read(sock_fd, buf, sizeof(buf)) != 1)
		die("datagram answer");
	return (UB_DGRAM_WINDOW);
}

static int rights_fd;

static uint64_t
op_rights(void *arg __unused)
{
	char c = 0;
	struct iovec iov = { &c, 1 };
	union {
		struct cmsghdr h;
		char space[CMSG_SPACE(sizeof(int))];
	} cm;
	struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1,
	    .msg_control = &cm, .msg_controllen = sizeof(cm) };

	memset(&cm, 0, sizeof(cm));
	cm.h.cmsg_len = CMSG_LEN(sizeof(int));
	cm.h.cmsg_level = SOL_SOCKET;
	cm.h.cmsg_type = SCM_RIGHTS;
	memcpy(CMSG_DATA(&cm.h), &rights_fd, sizeof(int));
	for (int i = 0; i < 100; i++)
		if (sendmsg(sock_fd, &msg, 0) != 1)
			die("sendmsg SCM_RIGHTS");
	return (100);
}

static struct sockaddr_un sun_addr;

/*
 * unix.connect: socket, connect, the partner's one-byte greeting once it
 * has accepted, close. (Unpaced, the client overflows the listen queue,
 * and a UNIX connect to a full queue is refused.)
 */
static uint64_t
op_connect(void *arg __unused)
{
	for (int i = 0; i < 10; i++) {
		int s = socket(AF_UNIX, SOCK_STREAM, 0);
		char c;

		if (s < 0)
			die("socket");
		if (connect(s, (struct sockaddr *)&sun_addr,
		    sizeof(sun_addr)) != 0)
			die("connect");
		if (read(s, &c, 1) != 1)
			die("greeting");
		close(s);
	}
	return (10);
}

static void
sock_pair(int type, size_t chunk, enum partner_mode mode, pid_t *pid)
{
	int sv[2];

	if (socketpair(AF_UNIX, type, 0, sv) != 0)
		die("socketpair");
	sock_fd = sv[0];
	*pid = sock_partner(sv[0], sv[1], chunk, mode);
}

static void
sock_done(pid_t pid)
{
	close(sock_fd);
	reap(pid);
}

static void
suite_unix(void)
{
	size_t small = 4096, big = UB_PIPE_BW;
	pid_t pid;
	int l;

	sock_pair(SOCK_STREAM, 1, P_ECHO, &pid);
	throughput("unix.stream.pingpong", op_sock_pingpong, NULL);
	sock_done(pid);
	sock_pair(SOCK_SEQPACKET, 1, P_ECHO, &pid);
	throughput("unix.seqpacket.pingpong", op_sock_pingpong, NULL);
	sock_done(pid);
	sock_pair(SOCK_STREAM, UB_PIPE_BW, P_SINK, &pid);
	throughput("unix.stream.bw4k", op_sock_write, &small);
	throughput("unix.stream.bw64k", op_sock_write, &big);
	sock_done(pid);
	sock_pair(SOCK_DGRAM, UB_PIPE_BW, P_DGRAM, &pid);
	throughput("unix.dgram", op_dgram, NULL);
	/* The partner sees no end of file on a datagram socket. */
	kill(pid, SIGKILL);
	close(sock_fd);
	waitpid(pid, NULL, 0);
	if ((rights_fd = open("/dev/null", O_RDONLY)) < 0)
		die("open /dev/null");
	sock_pair(SOCK_STREAM, 0, P_RIGHTS, &pid);
	throughput("unix.rights", op_rights, NULL);
	sock_done(pid);
	close(rights_fd);

	sun_addr.sun_family = AF_UNIX;
	strlcpy(sun_addr.sun_path, UB_SOCK, sizeof(sun_addr.sun_path));
	unlink(UB_SOCK);
	if ((l = socket(AF_UNIX, SOCK_STREAM, 0)) < 0 ||
	    bind(l, (struct sockaddr *)&sun_addr, sizeof(sun_addr)) != 0 ||
	    listen(l, 128) != 0)
		die("listen");
	if ((pid = fork()) < 0)
		die("fork");
	if (pid == 0) {
		int s;

		bind_cpu(2);
		while ((s = accept(l, NULL, NULL)) >= 0) {
			if (write(s, "", 1) != 1)
				_exit(1);
			close(s);
		}
		_exit(1);
	}
	throughput("unix.connect", op_connect, NULL);
	kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
	close(l);
	unlink(UB_SOCK);
}

/*
 * pty.*: a pair from posix_openpt. A partner process on CPU 2 reads one
 * side and discards; this process writes the other. pty.relay adds a relay
 * process between the pty and a pipe, as script(1) is.
 */
static int pty_master, pty_slave;

static void
pty_open(int raw)
{
	struct termios t;
	const char *name;

	if ((pty_master = posix_openpt(O_RDWR | O_NOCTTY)) < 0 ||
	    grantpt(pty_master) != 0 || unlockpt(pty_master) != 0 ||
	    (name = ptsname(pty_master)) == NULL ||
	    (pty_slave = open(name, O_RDWR | O_NOCTTY)) < 0)
		die("pty");
	if (tcgetattr(pty_slave, &t) != 0)
		die("tcgetattr");
	if (raw)
		cfmakeraw(&t);
	else
		t.c_lflag &= ~(tcflag_t)(ECHO | ECHOE | ECHOK | ECHONL);
	if (tcsetattr(pty_slave, TCSANOW, &t) != 0)
		die("tcsetattr");
}

static pid_t
pty_reader(int fd)
{
	static char buf[UB_PIPE_BW];
	pid_t pid = fork();
	ssize_t n;

	if (pid < 0)
		die("fork");
	if (pid != 0)
		return (pid);
	bind_cpu(2);
	while ((n = read(fd, buf, sizeof(buf))) > 0)
		;
	_exit(0);
}

static int pty_out;

static uint64_t
op_pty_write(void *arg)
{
	static char buf[4096];
	size_t len = *(size_t *)arg;

	if (buf[0] == 0) {
		memset(buf, 'x', sizeof(buf));
		buf[80] = '\n';
	}
	for (int i = 0; i < 16; i++)
		if (write(pty_out, buf, len) != (ssize_t)len)
			die("pty write");
	return (16);
}

static void
pty_close(pid_t pid)
{
	kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
	close(pty_master);
	close(pty_slave);
}

static void
suite_pty(void)
{
	size_t block = 4096, line = 81;
	pid_t pid, relay;
	int p[2];

	/* Output: raw writes to the slave, read from the master. */
	pty_open(1);
	pid = pty_reader(pty_master);
	pty_out = pty_slave;
	throughput("pty.raw", op_pty_write, &block);
	pty_close(pid);

	/* Input: lines to the master, read a line at a time on the slave. */
	pty_open(0);
	pid = pty_reader(pty_slave);
	pty_out = pty_master;
	throughput("pty.canon", op_pty_write, &line);
	pty_close(pid);

	/*
	 * Relay: writes to the slave; the relay polls the master and its
	 * input pipe as script does, and copies the master to a pipe whose
	 * far end the reader drains.
	 */
	pty_open(1);
	if (pipe(p) != 0)
		die("pipe");
	pid = pty_reader(p[0]);
	if ((relay = fork()) < 0)
		die("fork");
	if (relay == 0) {
		static char buf[UB_PIPE_BW];
		struct pollfd pfd[2] = { { pty_master, POLLIN, 0 },
		    { STDIN_FILENO, 0, 0 } };
		ssize_t n;

		bind_cpu(3);
		close(p[0]);
		for (;;) {
			if (poll(pfd, 2, -1) < 0)
				_exit(1);
			if ((n = read(pty_master, buf, sizeof(buf))) <= 0 ||
			    write(p[1], buf, (size_t)n) != n)
				_exit(1);
		}
	}
	close(p[0]);
	close(p[1]);
	pty_out = pty_slave;
	throughput("pty.relay", op_pty_write, &block);
	kill(relay, SIGKILL);
	waitpid(relay, NULL, 0);
	pty_close(pid);
}

/* exec.*: vfork, exec a program of base with output discarded, wait. */
static uint64_t
op_spawn(void *arg)
{
	char *const *argv = arg;
	char *envp[] = { (char *)"PATH=/sbin:/bin:/usr/sbin:/usr/bin", NULL };
	pid_t pid = vfork();

	if (pid < 0)
		die("vfork");
	if (pid == 0) {
		int null = open("/dev/null", O_WRONLY);

		if (null >= 0) {
			dup2(null, 1);
			dup2(null, 2);
		}
		execve(argv[0], argv, envp);
		_exit(127);
	}
	reap(pid);
	return (1);
}

static void
suite_exec(void)
{
	char *truth[] = { (char *)"/usr/bin/true", NULL };
	char *tar[] = { (char *)"/usr/bin/tar", (char *)"--version", NULL };
	char *service[] = { (char *)"/usr/sbin/service", (char *)"-e", NULL };

	throughput("exec.true", op_spawn, truth);
	throughput("exec.lib10", op_spawn, tar);
	throughput("exec.rcsubr", op_spawn, service);
}

static void
suite_p6(void)
{
	signal(SIGPIPE, SIG_IGN);
	suite_thr();
	suite_event();
	suite_unix();
	suite_pty();
	suite_exec();
}
#endif /* UB_P6 */

int
main(int argc, char **argv)
{
	self = argv[0];
	if (argc >= 2 && strcmp(argv[1], "exit") == 0)
		return (0);
	if (argc == 5 && strcmp(argv[1], "chain") == 0)
		return (exec_chain(argv));
	if (argc != 2 || (strcmp(argv[1], "p4") != 0 &&
	    strcmp(argv[1], "p5") != 0 && strcmp(argv[1], "vt") != 0 &&
	    !(UB_HAS_P6 && strcmp(argv[1], "p6") == 0))) {
		fprintf(stderr, "usage: ubench p4 | p5 | vt%s\n",
		    UB_HAS_P6 ? " | p6" : "");
		return (2);
	}
	printf("kbench begin %s cpus=%ld\n", argv[1],
	    sysconf(_SC_NPROCESSORS_ONLN));
	fflush(stdout);
	bind_cpu(1);
	if (strcmp(argv[1], "p4") == 0)
		suite_p4();
	else if (strcmp(argv[1], "p5") == 0)
		suite_p5();
	else if (strcmp(argv[1], "vt") == 0)
		suite_vt();
#ifdef UB_P6
	else
		suite_p6();
#endif
	printf("kbench end %s\n", argv[1]);
	return (0);
}
