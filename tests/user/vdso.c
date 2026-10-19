/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * vdso: checks the shared page's time keeping from user mode.
 *
 * libc answers clock_gettime and gettimeofday from the timehands the kernel
 * publishes (AT_TIMEKEEP) without a system call. This calls that path
 * directly, so a missing or disabled timekeep fails instead of quietly
 * falling back, and checks it against the system call: each reading lies
 * between two system-call readings, and readings never go backwards over
 * a few of the kernel's updates.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/time.h>

#include <stdio.h>
#include <time.h>

int __vdso_clock_gettime(clockid_t, struct timespec *);
int __vdso_gettimeofday(struct timeval *, struct timezone *);
int __sys_clock_gettime(clockid_t, struct timespec *);

static long long
ns(struct timespec t)
{
	return (t.tv_sec * 1000000000LL + t.tv_nsec);
}

static int
bracketed(clockid_t id, const char *name)
{
	struct timespec a, b, c;
	int error;

	if (__sys_clock_gettime(id, &a) != 0) {
		printf("vdso: %s: system call failed\n", name);
		return (0);
	}
	error = __vdso_clock_gettime(id, &b);
	if (error != 0) {
		printf("vdso: %s: shared page refused (%d)\n", name, error);
		return (0);
	}
	if (__sys_clock_gettime(id, &c) != 0)
		return (0);
	if (ns(a) > ns(b) || ns(b) > ns(c)) {
		printf("vdso: %s: %lld not within [%lld, %lld]\n", name, ns(b),
		    ns(a), ns(c));
		return (0);
	}
	return (1);
}

int
main(void)
{
	struct timespec t, last, start;
	struct timeval tv;
	long reads;

	if (!bracketed(CLOCK_MONOTONIC, "CLOCK_MONOTONIC") ||
	    !bracketed(CLOCK_UPTIME, "CLOCK_UPTIME") ||
	    !bracketed(CLOCK_REALTIME, "CLOCK_REALTIME"))
		return (1);
	if (__vdso_gettimeofday(&tv, NULL) != 0) {
		printf("vdso: gettimeofday: shared page refused\n");
		return (1);
	}

	/* Two and a half seconds spans at least two updates. */
	__vdso_clock_gettime(CLOCK_MONOTONIC, &start);
	last = start;
	for (reads = 0; ns(last) - ns(start) < 2500000000LL; reads++) {
		__vdso_clock_gettime(CLOCK_MONOTONIC, &t);
		if (ns(t) < ns(last)) {
			printf("vdso: went back from %lld to %lld\n", ns(last),
			    ns(t));
			return (1);
		}
		last = t;
	}
	printf("vdso: ok (%ld monotonic reads)\n", reads);
	return (0);
}
