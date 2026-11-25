/* SPDX-License-Identifier: BSD-2-Clause */
/* A single-thread loop timed with the monotonic clock: a way to see whether the processor runs at
 * its usual speed (the chip reset test used it to tell the graphics slowdown from a slow CPU). */
#include <stdint.h>
#include <stdio.h>
#include <time.h>

int main(void)
{
	struct timespec a, b;
	volatile uint64_t x = 1;
	clock_gettime(CLOCK_MONOTONIC, &a);
	for (uint64_t i = 0; i < 400000000ull; i++)
		x = x * 3 + i;
	clock_gettime(CLOCK_MONOTONIC, &b);
	printf("400 M iterations in %.3f s\n", (b.tv_sec - a.tv_sec) + (b.tv_nsec - a.tv_nsec) / 1e9);
	return 0;
}
