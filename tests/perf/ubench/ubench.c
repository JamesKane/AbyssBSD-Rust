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
 *
 * p4 uses Tier A system calls (docs/COMPATIBILITY.md §2.1), plus
 * cpuset_setaffinity to bind itself as kbench binds its workers. p5 adds
 * the file-system calls, and runs /rescue's newfs, tar, and rm, and fsx.
 */

#include <sys/param.h>
#include <sys/cpuset.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

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

int
main(int argc, char **argv)
{
	self = argv[0];
	if (argc >= 2 && strcmp(argv[1], "exit") == 0)
		return (0);
	if (argc == 5 && strcmp(argv[1], "chain") == 0)
		return (exec_chain(argv));
	if (argc != 2 || (strcmp(argv[1], "p4") != 0 &&
	    strcmp(argv[1], "p5") != 0)) {
		fprintf(stderr, "usage: ubench p4 | p5\n");
		return (2);
	}
	printf("kbench begin %s cpus=%ld\n", argv[1],
	    sysconf(_SC_NPROCESSORS_ONLN));
	fflush(stdout);
	bind_cpu(1);
	if (strcmp(argv[1], "p4") == 0)
		suite_p4();
	else
		suite_p5();
	printf("kbench end %s\n", argv[1]);
	return (0);
}
