/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * intrcnt: checks the interrupt counters `vmstat -i` reads: hw.intrnames
 * is one slot per source, each name padded to the same width and ended
 * by a NUL (slots no source took are empty), hw.intrcnt one u_long per
 * slot, some counting already, and the counts going up while the system
 * runs.
 *
 * Runs on FreeBSD too. Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/sysctl.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define CHECK(cond, ...)                                   \
	do {                                               \
		if (!(cond)) {                             \
			printf("intrcnt: FAIL: " __VA_ARGS__); \
			printf("\n");                      \
			failed = 1;                        \
		}                                          \
	} while (0)

static void *
fetch(const char *name, size_t *len)
{
	void *p;

	*len = 0;
	if (sysctlbyname(name, NULL, len, NULL, 0) != 0 || *len == 0) {
		printf("intrcnt: FAIL: %s has no size\n", name);
		return (NULL);
	}
	if ((p = malloc(*len)) == NULL ||
	    sysctlbyname(name, p, len, NULL, 0) != 0) {
		printf("intrcnt: FAIL: reading %s\n", name);
		free(p);
		return (NULL);
	}
	return (p);
}

static unsigned long
total(void)
{
	size_t len;
	unsigned long *c = fetch("hw.intrcnt", &len), sum = 0;

	for (size_t i = 0; c != NULL && i < len / sizeof(*c); i++)
		sum += c[i];
	free(c);
	return (sum);
}

int
main(void)
{
	size_t nlen, clen, slot, n;
	char *names = fetch("hw.intrnames", &nlen);
	unsigned long *counts = fetch("hw.intrcnt", &clen), before;
	int counting = 0;

	if (names == NULL || counts == NULL)
		return (1);
	CHECK(clen % sizeof(unsigned long) == 0, "hw.intrcnt is %zu bytes", clen);
	n = clen / sizeof(unsigned long);
	slot = strnlen(names, nlen) + 1;
	CHECK(slot <= nlen && nlen % slot == 0,
	    "hw.intrnames: %zu bytes, first slot %zu", nlen, slot);
	CHECK(nlen / slot == n, "%zu names for %zu counts", nlen / slot, n);
	for (size_t i = 0; i < nlen / slot; i++) {
		char *s = names + i * slot;
		size_t len = strnlen(s, slot);

		CHECK(len == 0 || len == slot - 1, "slot %zu is %zu long", i, len);
		if (i < n && counts[i] != 0)
			counting = 1;
	}
	CHECK(counting, "no source has counted an interrupt");
	before = total();
	usleep(200000);
	CHECK(total() > before, "no interrupt in 200 ms (%lu)", before);
	free(names);
	free(counts);
	if (failed)
		return (1);
	printf("intrcnt: ok (%zu sources, slots of %zu)\n", n, slot);
	return (0);
}
