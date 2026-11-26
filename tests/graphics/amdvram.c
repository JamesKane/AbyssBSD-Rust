/* SPDX-License-Identifier: BSD-2-Clause */
/* A buffer in the Radeon's own memory: asked for as the Mesa driver asks for a buffer that is not shared
 * (domain video memory, "always valid" flag), mapped for the processor (write-combined), cleared when made,
 * written by the processor and read back, mapped in the GPU's address space and written by a command
 * buffer's WRITE_DATA, then read by the processor. A buffer asked for without the flag can be shared, one in
 * video memory cannot be, and one too big for the pool falls back to system memory.
 * Usage: amdvram [node] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

struct gem_create { uint64_t size, align, domains, flags; };
struct gem_mmap { uint64_t offset; };
struct ctx { uint32_t op, flags, id, prio; };
struct gem_va { uint32_t handle, pad, op, flags; uint64_t va, off, size; };
struct cs { uint32_t ctx, list, nchunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, len; uint64_t data; };
struct ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct wait { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };
struct prime { uint32_t handle, flags; int32_t fd; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)
#define PRIME_HANDLE_TO_FD _IOWR('d', 0x2d, struct prime)
#define VRAM 4
#define GTT 2
#define ALWAYS_VALID (1 << 6)

static int fd, bad;

static uint32_t make(uint64_t size, uint64_t domains, uint64_t flags)
{
	struct gem_create g = { .size = size, .align = 4096, .domains = domains, .flags = flags };
	if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); return 0; }
	return (uint32_t)g.size;
}

static volatile uint32_t *map(uint32_t h, uint64_t size)
{
	struct gem_mmap m = { .offset = h };
	if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); return 0; }
	void *p = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
	return p == MAP_FAILED ? 0 : p;
}

static void check(const char *what, int ok)
{
	printf("%s: %s\n", what, ok ? "ok" : "FAILED");
	bad |= !ok;
}

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD129";
	fd = open(node, O_RDWR);
	if (fd < 0) { perror(node); return 1; }
	struct ctx c = { .op = 1 };
	if (ioctl(fd, CMD(0x02, struct ctx), &c) < 0) { perror("CTX"); return 1; }
	uint32_t ctx = *(uint32_t *)&c;

	uint32_t hib = make(4096, GTT, 0);
	volatile uint32_t *ibmem = map(hib, 4096);
	uint32_t hv = make(1 << 20, VRAM, ALWAYS_VALID);
	volatile uint32_t *v = map(hv, 1 << 20);
	if (!hib || !ibmem || !hv || !v) return 1;

	/* Cleared when made: nothing an earlier buffer left. */
	int clear = 1;
	for (int i = 0; i < (1 << 18); i += 7) clear &= v[i] == 0;
	check("a new buffer in video memory reads zero", clear);

	/* The processor's writes read back. */
	for (int i = 0; i < (1 << 18); i++) v[i] = 0x5a000000u + i;
	int back = 1;
	for (int i = 0; i < (1 << 18); i += 3) back &= v[i] == 0x5a000000u + i;
	check("the processor reads back what it wrote", back);

	/* The GPU writes into it, a page in the middle, and the processor sees it. */
	const uint64_t va_ib = 0x100000, va_v = 0x400000;
	struct gem_va mv[2] = {
		{ .handle = hib, .op = 1, .flags = 0xe, .va = va_ib, .size = 4096 },
		{ .handle = hv, .op = 1, .flags = 0x6, .va = va_v, .size = 1 << 20 },
	};
	for (int i = 0; i < 2; i++)
		if (ioctl(fd, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }
	uint64_t spot = va_v + 0x80000;
	ibmem[0] = 0xc0033700; ibmem[1] = (5 << 8) | (1 << 20);
	ibmem[2] = (uint32_t)spot; ibmem[3] = (uint32_t)(spot >> 32); ibmem[4] = 0xfeedc0de;
	struct ib ib = { .va = va_ib, .bytes = 20 };
	struct chunk ck = { .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib };
	uint64_t chunks[1] = { (uintptr_t)&ck };
	struct cs cs = { .ctx = ctx, .nchunks = 1, .chunks = (uintptr_t)chunks };
	if (ioctl(fd, CMD(0x04, struct cs), &cs) < 0) { perror("CS"); return 1; }
	struct wait w = { .handle = *(uint64_t *)&cs, .timeout = 2000000000ull, .ctx = ctx };
	if (ioctl(fd, CMD(0x09, struct wait), &w) < 0) { perror("WAIT_CS"); return 1; }
	check("the GPU's write into video memory is read by the processor", v[0x80000 / 4] == 0xfeedc0de);
	check("and the rest is as the processor left it", v[0x80000 / 4 + 1] == 0x5a000000u + 0x80000 / 4 + 1);

	/* A buffer in video memory is not shared; one asked for without the flag can be. */
	uint32_t hs = make(4096, VRAM, 0);
	struct prime p = { .handle = hs, .flags = 2 };
	check("a buffer asked for video memory without the local flag can be shared",
	      hs && ioctl(fd, PRIME_HANDLE_TO_FD, &p) == 0);
	struct prime q = { .handle = hv, .flags = 2 };
	check("a buffer in video memory is not exported", ioctl(fd, PRIME_HANDLE_TO_FD, &q) < 0);

	/* More than the pool has: system memory, made all the same. */
	uint32_t hbig = make(512u << 20, VRAM, ALWAYS_VALID);
	check("a buffer too big for the pool is made (in system memory)", hbig != 0);
	printf(bad ? "FAILED\n" : "video memory buffers work\n");
	return bad;
}
