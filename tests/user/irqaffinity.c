/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * irqaffinity: checks `cpuset -x`'s calls on an interrupt that
 * hw.intrnames lists: bound to CPU 0 it reads back as CPU 0, a set of
 * two CPUs is refused (EINVAL), every CPU unbinds it, and an interrupt
 * that does not exist is ESRCH. Where no source is named "irqN:" (arm64's
 * INTRNG names them by controller), there is nothing to check.
 *
 * Runs on FreeBSD too (as root). Exits 0 on success; prints what failed
 * otherwise.
 */

#include <sys/param.h>
#include <sys/cpuset.h>
#include <sys/sysctl.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define CHECK(cond, ...)                                       \
	do {                                                   \
		if (!(cond)) {                                 \
			printf("irqaffinity: FAIL: " __VA_ARGS__); \
			printf("\n");                          \
			failed = 1;                            \
		}                                              \
	} while (0)

/* The first interrupt named "irqN: ..." in hw.intrnames, or -1. */
static int
first_irq(void)
{
	size_t len = 0, slot;
	char *names;
	int irq = -1;

	if (sysctlbyname("hw.intrnames", NULL, &len, NULL, 0) != 0 || len == 0 ||
	    (names = malloc(len)) == NULL ||
	    sysctlbyname("hw.intrnames", names, &len, NULL, 0) != 0)
		return (-1);
	slot = strnlen(names, len) + 1;
	for (size_t at = 0; at + slot <= len && irq < 0; at += slot)
		if (sscanf(names + at, "irq%d:", &irq) != 1)
			irq = -1;
	free(names);
	return (irq);
}

int
main(void)
{
	cpuset_t one, two, all, got;
	int irq = first_irq();
	long ncpu = sysconf(_SC_NPROCESSORS_ONLN);

	if (irq < 0) {
		printf("irqaffinity: no irqN in hw.intrnames; skipped\n");
		return (0);
	}
	CPU_ZERO(&one);
	CPU_SET(0, &one);
	CPU_ZERO(&all);
	for (long i = 0; i < ncpu; i++)
		CPU_SET(i, &all);
	CHECK(cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, irq,
	    sizeof(one), &one) == 0, "binding irq%d to CPU 0: %d", irq, errno);
	CPU_ZERO(&got);
	CHECK(cpuset_getaffinity(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, irq,
	    sizeof(got), &got) == 0 && CPU_CMP(&got, &one) == 0,
	    "irq%d does not read back as CPU 0", irq);
	if (ncpu > 1) {
		two = one;
		CPU_SET(1, &two);
		errno = 0;
		CHECK(cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, irq,
		    sizeof(two), &two) == -1 && errno == EINVAL,
		    "two CPUs for irq%d: errno %d", irq, errno);
	}
	CHECK(cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, irq,
	    sizeof(all), &all) == 0, "unbinding irq%d: %d", irq, errno);
	errno = 0;
	CHECK(cpuset_getaffinity(CPU_LEVEL_WHICH, CPU_WHICH_IRQ, 100000,
	    sizeof(got), &got) == -1 && errno == ESRCH,
	    "a missing irq: errno %d", errno);
	if (failed)
		return (1);
	printf("irqaffinity: ok (irq%d)\n", irq);
	return (0);
}
