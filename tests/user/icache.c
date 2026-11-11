/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * icache: a new executable mapping of a file runs what the file holds
 * now, however the file changed since code was last run from it, and
 * however often that code ran (so the instruction cache holds it):
 *
 *  - rewritten with write(2) after its executable mapping went;
 *  - rewritten with write(2) while one executable mapping stays;
 *  - written through a shared writable mapping, run through a new
 *    executable one.
 *
 * Each test maps a page of code returning a number, runs it many times,
 * changes the number, and maps it again. On arm64 the kernel syncs the
 * instruction cache for a new executable mapping (FreeBSD: pmap_enter's
 * cpu_icache_sync_range), and AbyssBSD skips that for a file page synced
 * since it last changed: this checks it never skips one that changed.
 * The file is in /var/tmp, on the root file system, so its pages are a
 * file's (tmpfs pages are anonymous memory); /tmp where there is none.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/mman.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;
static size_t pg;

/* A function returning `n` (0 to 255), as machine code. */
static size_t
code(unsigned char *buf, int n)
{
#if defined(__aarch64__)
	uint32_t insn[2] = {
		0x52800000u | ((uint32_t)n << 5),	/* mov w0, #n */
		0xd65f03c0u,				/* ret */
	};

	memcpy(buf, insn, sizeof(insn));
	return (sizeof(insn));
#elif defined(__x86_64__)
	unsigned char insn[6] = { 0xb8, (unsigned char)n, 0, 0, 0, 0xc3 };

	memcpy(buf, insn, sizeof(insn));
	return (sizeof(insn));
#else
#error "no code for this architecture"
#endif
}

/* Writes a page of the file, holding code that returns `n`. */
static void
put(int fd, int n)
{
	unsigned char *page = calloc(1, pg);

	code(page, n);
	if (pwrite(fd, page, pg, 0) != (ssize_t)pg) {
		printf("icache: pwrite: errno %d\n", errno);
		exit(1);
	}
	free(page);
}

static int (*
map_code(int fd))(void)
{
	void *p = mmap(NULL, pg, PROT_READ | PROT_EXEC, MAP_SHARED, fd, 0);

	if (p == MAP_FAILED) {
		printf("icache: mmap exec: errno %d\n", errno);
		exit(1);
	}
	return ((int (*)(void))(uintptr_t)p);
}

/* Runs `f` often enough to have its line in the instruction cache. */
static int
run(int (*f)(void))
{
	int r = 0;

	for (int i = 0; i < 100000; i++)
		r = f();
	return (r);
}

static void
check(const char *what, int got, int want)
{
	if (got != want) {
		printf("icache: %s: ran code returning %d, not %d\n", what,
		    got, want);
		failed = 1;
	}
}

static int
open_file(void)
{
	char path[] = "/var/tmp/icache.XXXXXX";
	char other[] = "/tmp/icache.XXXXXX";
	int fd = mkstemp(path);
	char *at = path;

	/* A root without /var/tmp (a memory disk's). */
	if (fd < 0 && errno == ENOENT) {
		fd = mkstemp(other);
		at = other;
	}
	if (fd < 0) {
		printf("icache: mkstemp: errno %d\n", errno);
		exit(1);
	}
	unlink(at);
	return (fd);
}

static void
rewritten_unmapped(void)
{
	int fd = open_file();
	int (*f)(void);

	for (int n = 1; n <= 5; n++) {
		put(fd, n);
		f = map_code(fd);
		check("rewritten after unmapping", run(f), n);
		munmap((void *)(uintptr_t)f, pg);
	}
	close(fd);
}

static void
rewritten_mapped(void)
{
	int fd = open_file();
	int (*keep)(void), (*f)(void);

	put(fd, 1);
	keep = map_code(fd);
	check("first mapping", run(keep), 1);
	for (int n = 2; n <= 5; n++) {
		put(fd, n);
		f = map_code(fd);
		check("rewritten while mapped, new mapping", run(f), n);
		munmap((void *)(uintptr_t)f, pg);
	}
	munmap((void *)(uintptr_t)keep, pg);
	close(fd);
}

static void
written_through_shared(void)
{
	int fd = open_file();
	unsigned char *w;
	int (*f)(void);

	put(fd, 0);
	w = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (w == MAP_FAILED) {
		printf("icache: mmap shared: errno %d\n", errno);
		exit(1);
	}
	for (int n = 1; n <= 5; n++) {
		code(w, n);
		f = map_code(fd);
		check("written through a shared mapping", run(f), n);
		munmap((void *)(uintptr_t)f, pg);
	}
	munmap(w, pg);
	close(fd);
}

int
main(void)
{
	pg = (size_t)getpagesize();
	rewritten_unmapped();
	rewritten_mapped();
	written_through_shared();
	if (!failed)
		printf("icache: ok\n");
	return (failed);
}
