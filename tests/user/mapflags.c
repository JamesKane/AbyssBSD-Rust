/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * mapflags: the mmap and mprotect behavior rtld relies on, as FreeBSD
 * has it: PROT_MAX on both calls, MAP_FIXED | MAP_EXCL, MAP_ALIGNED and
 * MAP_ALIGNED_SUPER, MAP_32BIT, and rtld's way of loading an object (a PROT_NONE
 * reservation, a file mapped MAP_FIXED into it, a relocated page made
 * read-only for RELRO, and bss mapped anonymous over the rest).
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/mman.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

static void
fail(const char *what)
{
	printf("mapflags: %s (errno %d)\n", what, errno);
	failed = 1;
}

#define	MB	(1024 * 1024)

static void
prot_max(void)
{
	size_t pg = getpagesize();
	char *p;

	errno = 0;
	if (mmap(NULL, pg, PROT_READ | PROT_WRITE | PROT_MAX(PROT_READ),
	    MAP_ANON | MAP_PRIVATE, -1, 0) != MAP_FAILED || errno != ENOTSUP)
		fail("mmap's PROT_MAX without the rights is not ENOTSUP");
	p = mmap(NULL, 2 * pg, PROT_READ | PROT_MAX(PROT_READ | PROT_WRITE),
	    MAP_ANON | MAP_PRIVATE, -1, 0);
	if (p == MAP_FAILED) {
		fail("mmap with PROT_MAX");
		return;
	}
	if (mprotect(p, pg, PROT_READ | PROT_WRITE) != 0)
		fail("mprotect within PROT_MAX");
	p[0] = 1;
	errno = 0;
	if (mprotect(p, pg, PROT_EXEC) == 0 || errno != EACCES)
		fail("mprotect beyond PROT_MAX is not EACCES");

	/* mprotect's own PROT_MAX lowers the maximum, never raises it. */
	if (mprotect(p, pg, PROT_READ | PROT_MAX(PROT_READ)) != 0)
		fail("mprotect lowering PROT_MAX");
	errno = 0;
	if (mprotect(p, pg, PROT_READ | PROT_WRITE) == 0 || errno != EACCES)
		fail("mprotect past a lowered PROT_MAX is not EACCES");
	errno = 0;
	if (mprotect(p, pg, PROT_READ | PROT_MAX(PROT_READ | PROT_WRITE)) == 0 ||
	    errno != EACCES)
		fail("mprotect raising PROT_MAX is not EACCES");
	errno = 0;
	if (mprotect(p, pg, PROT_READ | PROT_WRITE | PROT_MAX(PROT_READ)) == 0 ||
	    errno != ENOTSUP)
		fail("mprotect's PROT_MAX without the rights is not ENOTSUP");
	/* The second page kept its maximum. */
	if (mprotect(p + pg, pg, PROT_READ | PROT_WRITE) != 0)
		fail("mprotect next to a lowered PROT_MAX");
	if (p[0] != 1)
		fail("the page lost its contents");
	munmap(p, 2 * pg);
}

static void
excl(void)
{
	size_t pg = getpagesize();
	char *p, *q;

	p = mmap(NULL, 4 * pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE,
	    -1, 0);
	if (p == MAP_FAILED) {
		fail("mmap");
		return;
	}
	errno = 0;
	if (mmap(p, pg, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_EXCL, -1, 0) !=
	    MAP_FAILED || errno != EINVAL)
		fail("MAP_EXCL without MAP_FIXED is not EINVAL");
	errno = 0;
	if (mmap(p + pg, pg, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_FIXED |
	    MAP_EXCL, -1, 0) != MAP_FAILED || errno != ENOMEM)
		fail("MAP_EXCL over a mapping is not ENOMEM");
	munmap(p + 2 * pg, pg);
	q = mmap(p + 2 * pg, pg, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_FIXED |
	    MAP_EXCL, -1, 0);
	if (q != p + 2 * pg)
		fail("MAP_EXCL into a hole");
	/* Without MAP_EXCL, MAP_FIXED replaces. */
	p[pg] = 1;
	if (mmap(p + pg, pg, PROT_READ | PROT_WRITE, MAP_ANON | MAP_PRIVATE |
	    MAP_FIXED, -1, 0) != p + pg || p[pg] != 0)
		fail("MAP_FIXED over a mapping");
	munmap(p, 4 * pg);
}

static void
aligned(void)
{
	char *p;

	errno = 0;
	if (mmap(NULL, MB, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_ALIGNED(11),
	    -1, 0) != MAP_FAILED || errno != EINVAL)
		fail("MAP_ALIGNED below a page is not EINVAL");
	p = mmap(NULL, MB, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_ALIGNED(20),
	    -1, 0);
	if (p == MAP_FAILED || ((uintptr_t)p & (MB - 1)) != 0)
		fail("MAP_ALIGNED(20)");
	else
		munmap(p, MB);
	/* A superpage on both architectures: 2 MiB. */
	p = mmap(NULL, 8 * MB, PROT_READ, MAP_ANON | MAP_PRIVATE |
	    MAP_ALIGNED_SUPER, -1, 0);
	if (p == MAP_FAILED || ((uintptr_t)p & (2 * MB - 1)) != 0)
		fail("MAP_ALIGNED_SUPER");
	else
		munmap(p, 8 * MB);
	/* A fixed address need only be a page's. */
	p = mmap(NULL, 2 * MB, PROT_NONE, MAP_ANON | MAP_PRIVATE |
	    MAP_ALIGNED(20), -1, 0);
	if (p == MAP_FAILED) {
		fail("a reservation");
		return;
	}
	if (mmap(p + getpagesize(), MB, PROT_READ, MAP_ANON | MAP_PRIVATE |
	    MAP_FIXED | MAP_ALIGNED(20), -1, 0) != p + getpagesize())
		fail("MAP_FIXED off its MAP_ALIGNED");
	munmap(p, 2 * MB);
}

/* MAP_32BIT: in the low 2 GiB; the two reserved flags are ignored. */
static void
low(void)
{
	size_t pg = getpagesize();
	char *p;

	p = mmap(NULL, pg, PROT_READ, MAP_ANON | MAP_PRIVATE | MAP_32BIT, -1, 0);
	if (p == MAP_FAILED || (uintptr_t)p + pg > (1UL << 31))
		fail("MAP_32BIT");
	else
		munmap(p, pg);
	p = mmap((void *)(1UL << 40), pg, PROT_READ, MAP_ANON | MAP_PRIVATE |
	    MAP_32BIT, -1, 0);
	if (p == MAP_FAILED || (uintptr_t)p + pg > (1UL << 31))
		fail("MAP_32BIT with a high hint");
	else
		munmap(p, pg);
	errno = 0;
	if (mmap((void *)((1UL << 31) - pg), 2 * pg, PROT_READ, MAP_ANON |
	    MAP_PRIVATE | MAP_FIXED | MAP_32BIT, -1, 0) != MAP_FAILED ||
	    errno != EINVAL)
		fail("MAP_32BIT | MAP_FIXED past 2 GiB is not EINVAL");
	p = mmap(NULL, pg, PROT_READ, MAP_ANON | MAP_PRIVATE |
	    MAP_RESERVED0020 | MAP_RESERVED0040, -1, 0);
	if (p == MAP_FAILED)
		fail("the reserved flags");
	else
		munmap(p, pg);
}

/* What rtld's map_object and relocation do with an object. */
static void
load_like_rtld(void)
{
	size_t pg = getpagesize();
	char path[] = "/tmp/mapflags.XXXXXX";
	char buf[4096 * 4], *base, *seg;
	int fd;

	fd = mkstemp(path);
	if (fd < 0) {
		fail("mkstemp");
		return;
	}
	unlink(path);
	memset(buf, 'f', sizeof(buf));
	if (pg > sizeof(buf) / 2 || ftruncate(fd, 2 * pg) != 0 ||
	    pwrite(fd, buf, 2 * pg, 0) != (ssize_t)(2 * pg)) {
		fail("the object file");
		close(fd);
		return;
	}
	base = mmap(NULL, 2 * MB, PROT_NONE, MAP_ANON | MAP_PRIVATE |
	    MAP_NOCORE | MAP_ALIGNED_SUPER, -1, 0);
	if (base == MAP_FAILED) {
		fail("the reservation");
		close(fd);
		return;
	}
	/* Text, then data a page further, each from the file. */
	seg = mmap(base, pg, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED |
	    MAP_PREFAULT_READ, fd, 0);
	if (seg != base)
		fail("the text segment");
	seg = mmap(base + pg, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE |
	    MAP_FIXED | MAP_PREFAULT_READ, fd, pg);
	if (seg != base + pg)
		fail("the data segment");
	/* bss: anonymous over the next pages. */
	seg = mmap(base + 2 * pg, 3 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE |
	    MAP_ANON | MAP_FIXED, -1, 0);
	if (seg != base + 2 * pg)
		fail("bss");
	if (failed)
		goto out;
	/* Relocate the data page, then make it read-only: RELRO. */
	base[pg] = 'r';
	base[2 * pg] = 'b';
	if (mprotect(base + pg, pg, PROT_READ) != 0)
		fail("mprotect for RELRO");
	if (base[0] != 'f' || base[pg] != 'r' || base[pg + 1] != 'f' ||
	    base[2 * pg] != 'b' || base[3 * pg] != 0)
		fail("the object's contents");
	if (pread(fd, buf, 1, pg) != 1 || buf[0] != 'f')
		fail("a private mapping wrote the file");
	/* The rest of the reservation is still reserved. */
	errno = 0;
	if (mmap(base + 8 * pg, pg, PROT_READ, MAP_ANON | MAP_PRIVATE |
	    MAP_FIXED | MAP_EXCL, -1, 0) != MAP_FAILED || errno != ENOMEM)
		fail("the reservation's tail is not mapped");
out:
	munmap(base, 2 * MB);
	close(fd);
}

int
main(void)
{
	prot_max();
	excl();
	aligned();
	low();
	load_like_rtld();
	if (failed)
		return (1);
	printf("mapflags: ok\n");
	return (0);
}
