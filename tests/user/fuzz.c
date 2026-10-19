/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * fuzz: random system calls with random arguments, to find kernel panics.
 *
 * A parent forks children; each makes CALLS calls chosen from the calls
 * the kernel implements, with arguments drawn from small and huge
 * integers, -1, pointers into a page of random bytes, pointers that run
 * into an unmapped page, NULL, and kernel addresses. A child may die of
 * a signal (it may unmap its own stack); one still running after a while
 * (blocked in a read, say) is killed. The kernel must survive all of it.
 *
 * Nothing outside the child is touched: no fork, exec, or exit; signals
 * only to itself; no sysctl writes; files opened read-only.
 *
 * Usage: fuzz [seed [children [calls]]]. Exits 0 when every child was
 * accounted for.
 */

#include <sys/types.h>
#include <sys/mman.h>
#include <sys/syscall.h>
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

/* What an argument may be. */
enum { ARG_INT, ARG_PTR };

struct call {
	int	number;
	const char *name;
};

#define	C(n)	{ SYS_##n, #n }
static const struct call calls[] = {
	C(read), C(write), C(open), C(close), C(wait4), C(getpid),
	C(getppid), C(getuid), C(geteuid), C(getgid), C(getegid),
	C(ioctl), C(readlink), C(umask), C(dup), C(dup2), C(fcntl),
	C(pipe2), C(fstat), C(fstatat), C(lseek), C(getdirentries),
	C(chdir), C(fchdir), C(__getcwd), C(access), C(eaccess),
	C(faccessat), C(readlinkat), C(statfs), C(fstatfs), C(getfsstat),
	C(openat), C(pread), C(pwrite), C(readv), C(writev), C(preadv),
	C(pwritev), C(close_range), C(copy_file_range), C(mmap), C(munmap),
	C(mprotect), C(madvise), C(minherit), C(sigaction),
	C(sigprocmask), C(sigpending), C(sigaltstack),
	C(sigreturn), C(getcontext), C(setcontext), C(swapcontext),
	C(sigtimedwait), C(sigfastblock),
	C(clock_gettime), C(clock_getres), C(gettimeofday), C(nanosleep),
	C(getrlimit), C(setrlimit), C(getrandom), C(issetugid),
	C(getgroups), C(getresuid), C(getresgid), C(getpgrp), C(getpgid),
	C(getsid), C(setsid), C(setpgid), C(cpuset_getaffinity),
	C(sysarch), C(thr_self), C(_umtx_op), C(rtprio_thread),
	C(pdgetpid), C(exterrctl), C(__sysctl), C(__sysctlbyname),
};
#define	NCALLS	(sizeof(calls) / sizeof(calls[0]))

static uint64_t rng;

static uint64_t
next(void)
{
	/* xorshift64* */
	rng ^= rng >> 12;
	rng ^= rng << 25;
	rng ^= rng >> 27;
	return (rng * 0x2545f4914f6cdd1dULL);
}

static char *scratch;		/* a page of random bytes */
static char *edge;		/* the last bytes before an unmapped page */
static char *playground;	/* pages memory calls may change at will */
#define	PLAYGROUND	(64 * 4096)

static uint64_t
argument(void)
{
	uint64_t r = next();

	switch (r % 12) {
	case 0: return (0);
	case 1: return ((uint64_t)-1);
	case 2: return (r >> 60);			/* small */
	case 3: return ((r >> 32) & 0xffff);		/* medium */
	case 4: return (r);				/* anything */
	case 5: return ((uint64_t)1 << ((r >> 8) % 64));
	case 6: return ((uintptr_t)scratch + ((r >> 8) % 4096));
	case 7: return ((uintptr_t)edge + ((r >> 8) % 64));
	case 8: return (0xffff800000000000ULL + (r >> 20));	/* kernel */
	case 9: return ((uintptr_t)scratch);
	case 10: return (r >> 40);
	default: return ((r >> 8) & 0xff);		/* a small fd, signal */
	}
}

/* A millisecond, for calls that would otherwise wait for good. */
static struct timespec brief = { 0, 1000000 };

/*
 * Keeps a call from reaching outside the child, and from waiting for
 * something that never comes (a child that blocks anyway is killed, but
 * then makes no more calls).
 */
static int
sanitize(int number, uint64_t *a)
{
	switch (number) {
	case SYS_wait4:
		a[2] |= WNOHANG;
		break;
	case SYS_pipe2:
		a[1] |= O_NONBLOCK;
		break;
	case SYS_sigtimedwait:
		a[2] = (uintptr_t)&brief;
		break;
	case SYS__umtx_op:
		/* Waits time out at once: a relative timeout, by size. */
		a[3] = sizeof(brief);
		a[4] = (uintptr_t)&brief;
		break;
	case SYS_read:
	case SYS_readv:
	case SYS_pread:
	case SYS_preadv:
		/* Not from the console, which waits for typing. */
		if (a[0] <= 2)
			a[0] = 1000;
		break;
	case SYS_open:
		a[1] &= ~(uint64_t)(O_WRONLY | O_RDWR | O_CREAT | O_TRUNC |
		    O_APPEND | O_EXLOCK | O_SHLOCK);
		break;
	case SYS_openat:
		a[2] &= ~(uint64_t)(O_WRONLY | O_RDWR | O_CREAT | O_TRUNC |
		    O_APPEND | O_EXLOCK | O_SHLOCK);
		break;
	case SYS___sysctl:
	case SYS___sysctlbyname:
		a[4] = 0;	/* no new value */
		a[5] = 0;
		break;
	case SYS_setpgid:
		a[0] = 0;	/* itself */
		break;
	case SYS_pwrite:
	case SYS_pwritev:
	case SYS_write:
	case SYS_writev:
		/* Not onto the console. */
		if (a[0] <= 2)
			a[0] = 1000;
		break;
	case SYS_munmap:
	case SYS_mprotect:
	case SYS_madvise:
	case SYS_minherit:
		/*
		 * Within the playground, mostly: the child's own text and
		 * stack would take it down at once.
		 */
		if (next() % 8 != 0)
			a[0] = (uintptr_t)playground + a[0] % PLAYGROUND;
		break;
	case SYS_mmap:
		if (next() % 8 != 0 && (a[3] & MAP_FIXED) != 0)
			a[0] = (uintptr_t)playground + a[0] % PLAYGROUND;
		break;
	case SYS_sigreturn:
	case SYS_setcontext:
	case SYS_swapcontext:
		/* Jumps somewhere random: now and then. */
		return (next() % 16 == 0);
	case SYS_nanosleep:
		return (0);	/* blocking is covered by the others */
	}
	return (1);
}

static void
child(int calls_per_child)
{
	uint64_t a[6];
	int i, j, null;

	/* Quiet: descriptors 0 to 2 on /dev/null. */
	null = open("/dev/null", O_RDWR);
	if (null >= 0) {
		dup2(null, 0);
		dup2(null, 1);
		dup2(null, 2);
		if (null > 2)
			close(null);
	}
	for (i = 0; i < calls_per_child; i++) {
		const struct call *c = &calls[next() % NCALLS];

		for (j = 0; j < 6; j++)
			a[j] = argument();
		if (!sanitize(c->number, a))
			continue;
		/* Refresh what pointers point at, now and then. */
		if (next() % 16 == 0)
			for (j = 0; j < 4096; j += 8)
				*(uint64_t *)(scratch + j) = next();
		__syscall(c->number, a[0], a[1], a[2], a[3], a[4], a[5]);
	}
	_exit(0);
}

int
main(int argc, char **argv)
{
	uint64_t seed = argc > 1 ? strtoull(argv[1], NULL, 0) : 1;
	int children = argc > 2 ? atoi(argv[2]) : 40;
	int per_child = argc > 3 ? atoi(argv[3]) : 3000;
	int i, st, signalled = 0, killed = 0;
	char *pages;

	pages = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_PRIVATE, -1, 0);
	if (pages == MAP_FAILED) {
		printf("fuzz: mmap: %s\n", strerror(errno));
		return (1);
	}
	scratch = pages;
	munmap(pages + 2 * 4096, 4096);
	playground = mmap(NULL, PLAYGROUND, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_PRIVATE, -1, 0);
	if (playground == MAP_FAILED) {
		printf("fuzz: mmap: %s\n", strerror(errno));
		return (1);
	}
	edge = pages + 2 * 4096 - 32;
	for (i = 0; i < children; i++) {
		struct timespec tick = { 0, 5000000 };
		pid_t pid;
		int waited;

		rng = seed * 0x9e3779b97f4a7c15ULL + (uint64_t)i + 1;
		for (int j = 0; j < 4096; j += 8)
			*(uint64_t *)(scratch + j) = next();
		pid = fork();
		if (pid == 0)
			child(per_child);
		if (pid < 0) {
			printf("fuzz: fork: %s\n", strerror(errno));
			return (1);
		}
		/* Two seconds, then it is killed. */
		for (waited = 0; waited < 400; waited++) {
			if (waitpid(pid, &st, WNOHANG) == pid)
				break;
			nanosleep(&tick, NULL);
		}
		if (waited == 400) {
			kill(pid, SIGKILL);
			if (waitpid(pid, &st, 0) != pid) {
				printf("fuzz: child %d lost\n", pid);
				return (1);
			}
			killed++;
		} else if (WIFSIGNALED(st)) {
			signalled++;
		}
	}
	printf("fuzz: ok (seed %llu: %d children x %d calls; %d died of "
	    "signals, %d killed)\n", (unsigned long long)seed, children,
	    per_child, signalled, killed);
	return (0);
}
