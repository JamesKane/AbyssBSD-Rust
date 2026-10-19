/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * umtx: checks _umtx_op's waits from user mode: a word that does not match
 * returns at once, a relative or absolute timeout expires with ETIMEDOUT
 * no earlier than asked, and a malformed timeout is EINVAL.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/umtx.h>

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

static long long
now(void)
{
	struct timespec t;

	clock_gettime(CLOCK_MONOTONIC, &t);
	return (t.tv_sec * 1000000000LL + t.tv_nsec);
}

static int
expect(const char *what, int r, int want)
{
	int got = r == 0 ? 0 : errno;

	if (got != want) {
		printf("umtx: %s: got %s, want %s\n", what, strerror(got),
		    strerror(want));
		return (0);
	}
	return (1);
}

int
main(void)
{
	u_int word = 1;
	struct timespec rel = { 0, 50000000 };
	struct timespec bad = { 0, 1000000000 };
	struct _umtx_time abs;
	long long t0, waited;

	/* The word is not 0: no wait at all, even without a timeout. */
	if (!expect("mismatch", _umtx_op(&word, UMTX_OP_WAIT_UINT_PRIVATE, 0,
	    NULL, NULL), 0))
		return (1);

	t0 = now();
	if (!expect("relative", _umtx_op(&word, UMTX_OP_WAIT_UINT_PRIVATE, 1,
	    (void *)sizeof(rel), &rel), ETIMEDOUT))
		return (1);
	waited = now() - t0;
	if (waited < 50000000) {
		printf("umtx: relative: woke after %lld ns\n", waited);
		return (1);
	}

	memset(&abs, 0, sizeof(abs));
	abs._flags = UMTX_ABSTIME;
	abs._clockid = CLOCK_MONOTONIC;
	t0 = now();
	abs._timeout.tv_sec = (t0 + 30000000) / 1000000000;
	abs._timeout.tv_nsec = (t0 + 30000000) % 1000000000;
	if (!expect("absolute", _umtx_op(&word, UMTX_OP_WAIT_UINT, 1,
	    (void *)sizeof(abs), &abs), ETIMEDOUT))
		return (1);
	if (now() < t0 + 30000000) {
		printf("umtx: absolute: woke early\n");
		return (1);
	}

	/* A deadline already past still times out (after checking). */
	if (!expect("past", _umtx_op(&word, UMTX_OP_WAIT_UINT, 1,
	    (void *)sizeof(abs), &abs), ETIMEDOUT))
		return (1);

	if (!expect("bad nsec", _umtx_op(&word, UMTX_OP_WAIT_UINT_PRIVATE, 1,
	    (void *)sizeof(bad), &bad), EINVAL))
		return (1);

	printf("umtx: ok\n");
	return (0);
}
