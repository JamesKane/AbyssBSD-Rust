/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * stack: checks that stacks grow down on demand: the main stack within
 * RLIMIT_STACK (whose hard limit is MAXSSIZ, and whose current value
 * exec reports as AT_USRSTACKLIM; a login class may raise it from the
 * default), past which a program gets SIGSEGV, at once when the limit is
 * lowered; and a MAP_STACK mapping, which grows down to the page above
 * its guard.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#ifdef __aarch64__
#define	DFLSSIZ	((rlim_t)128 << 20)
#define	MAXSSIZ	((rlim_t)1 << 30)
#else
#define	DFLSSIZ	((rlim_t)8 << 20)
#define	MAXSSIZ	((rlim_t)512 << 20)
#endif

static int failed;

static void
fail(const char *what)
{
	printf("stack: %s\n", what);
	failed = 1;
}

/* Touches `n` bytes of stack below this frame, a page at a time. */
static int __noinline
use_stack(size_t n)
{
	volatile char buf[n];

	for (size_t i = 0; i < n; i += 4096)
		buf[i] = 1;
	return (buf[0]);
}

/* Never set: recursion that only a fault ends. */
static volatile int stop;

/* Recurses until it faults. */
static int __noinline
forever(int depth)
{
	volatile char pad[1024];

	pad[0] = (char)depth;
	if (stop)
		return (0);
	return (forever(depth + 1) + pad[0]);
}

/* Runs `f` in a child; returns the signal that ended it, or 0. */
static int
in_child(void (*f)(void))
{
	pid_t pid;
	int status;

	pid = fork();
	if (pid == 0) {
		f();
		_exit(0);
	}
	if (pid < 0 || waitpid(pid, &status, 0) != pid)
		return (-1);
	return (WIFSIGNALED(status) ? WTERMSIG(status) : 0);
}

static void
overflow(void)
{
	struct rlimit rl = { DFLSSIZ, MAXSSIZ };

	if (setrlimit(RLIMIT_STACK, &rl) != 0)
		_exit(2);
	forever(0);
}

static void
under_lowered_limit(void)
{
	struct rlimit rl = { 1 << 20, MAXSSIZ };

	if (setrlimit(RLIMIT_STACK, &rl) != 0)
		_exit(2);
	/* Under the old limit, over the new one. */
	use_stack(4 << 20);
}

static char *map_stack;

static void
touch_guard(void)
{
	map_stack[0] = 1;
}

int
main(void)
{
	struct rlimit rl;
	u_long lim = 0;
	size_t len = 1 << 20;

	if (getrlimit(RLIMIT_STACK, &rl) != 0 || rl.rlim_cur < DFLSSIZ ||
	    rl.rlim_max != MAXSSIZ) {
		printf("stack: RLIMIT_STACK %#jx/%#jx\n",
		    (uintmax_t)rl.rlim_cur, (uintmax_t)rl.rlim_max);
		failed = 1;
	}
	if (elf_aux_info(AT_USRSTACKLIM, &lim, sizeof(lim)) != 0 ||
	    lim != (u_long)rl.rlim_cur)
		fail("AT_USRSTACKLIM is not RLIMIT_STACK");
	/* Before this process's stack has grown, which a child inherits. */
	if (in_child(under_lowered_limit) != SIGSEGV)
		fail("a lowered limit did not stop the stack");
	/* Most of the limit, far below what exec mapped at first. */
	use_stack(DFLSSIZ * 3 / 4);
	if (in_child(overflow) != SIGSEGV)
		fail("recursing forever did not end in SIGSEGV");

	/* MAP_STACK: anonymous memory, readable and writable. */
	if (mmap(NULL, len, PROT_READ, MAP_STACK, -1, 0) != MAP_FAILED ||
	    errno != EINVAL)
		fail("MAP_STACK without PROT_WRITE");
	map_stack = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_STACK, -1, 0);
	if (map_stack == MAP_FAILED) {
		fail("MAP_STACK");
	} else {
		/* Top to the page above the guard, which is never given. */
		map_stack[len - 1] = 1;
		map_stack[4096] = 2;
		if (map_stack[4096] != 2 || map_stack[len - 1] != 1)
			fail("MAP_STACK memory does not keep writes");
		if (in_child(touch_guard) != SIGSEGV)
			fail("a MAP_STACK guard page could be touched");
	}
	if (failed)
		return (1);
	printf("stack: ok\n");
	return (0);
}
