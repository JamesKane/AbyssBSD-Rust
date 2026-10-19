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
 *
 * It uses Tier A system calls (docs/COMPATIBILITY.md §2.1), plus
 * cpuset_setaffinity to bind itself as kbench binds its workers.
 */

#include <sys/param.h>
#include <sys/cpuset.h>
#include <sys/mman.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
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

int
main(int argc, char **argv)
{
	self = argv[0];
	if (argc >= 2 && strcmp(argv[1], "exit") == 0)
		return (0);
	if (argc == 5 && strcmp(argv[1], "chain") == 0)
		return (exec_chain(argv));
	if (argc != 2 || strcmp(argv[1], "p4") != 0) {
		fprintf(stderr, "usage: ubench p4\n");
		return (2);
	}
	printf("kbench begin p4 cpus=%ld\n", sysconf(_SC_NPROCESSORS_ONLN));
	fflush(stdout);
	bind_cpu(1);
	suite_p4();
	printf("kbench end p4\n");
	return (0);
}
