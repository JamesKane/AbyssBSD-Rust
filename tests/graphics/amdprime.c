/* SPDX-License-Identifier: BSD-2-Clause */
/* Shares a buffer made on the Radeon render node: exports it as a descriptor, imports that in a
 * second open file (the other side sees the same pages), has the GPU write to it through the
 * second file's own address space, and imports it on the card's own node and makes a
 * framebuffer of it (the compositor's scanout path). Nothing is shown on the display.
 * Usage: amdprime [render node [card node]] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
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
struct fb2 { uint32_t id, w, h, fmt, flags, handles[4], pitches[4], offsets[4]; uint64_t mod[4]; };
struct gem_close { uint32_t handle, pad; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)
#define PRIME_H2F _IOWR('d', 0x2d, struct prime)
#define PRIME_F2H _IOWR('d', 0x2e, struct prime)
#define ADDFB2 _IOWR('d', 0xB8, struct fb2)
#define RMFB _IOWR('d', 0xAF, unsigned int)
#define CLOSE _IOW('d', 0x09, struct gem_close)

static int bad;
#define CHECK(c, ...) do { if (!(c)) { printf("FAIL: " __VA_ARGS__); printf("\n"); bad = 1; } } while (0)

static uint32_t create(int fd, uint64_t size)
{
	struct gem_create g = { .size = size, .align = 4096, .domains = 2 };
	if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); return 0; }
	return (uint32_t)g.size;
}

static void *map(int fd, uint32_t handle, uint64_t size)
{
	struct gem_mmap m = { .offset = handle };
	if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); return NULL; }
	void *p = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
	return p == MAP_FAILED ? NULL : p;
}

int main(int argc, char **argv)
{
	const char *rn = argc > 1 ? argv[1] : "/dev/dri/renderD129";
	const char *cn = argc > 2 ? argv[2] : "/dev/dri/card1";
	int a = open(rn, O_RDWR), b = open(rn, O_RDWR), c = open(cn, O_RDWR);
	if (a < 0 || b < 0 || c < 0) { perror("open"); return 1; }

	/* A makes a buffer and writes a pattern. */
	uint64_t size = 1920 * 1080 * 4;
	uint32_t ha = create(a, size);
	uint32_t *pa = map(a, ha, size);
	if (!ha || !pa) return 1;
	for (int i = 0; i < 1024; i++) pa[i] = 0xa0000000 + i;

	/* A exports it; B imports it. */
	struct prime ex = { .handle = ha, .flags = O_RDWR | O_CLOEXEC, .fd = -1 };
	if (ioctl(a, PRIME_H2F, &ex) < 0) { perror("PRIME_HANDLE_TO_FD"); return 1; }
	printf("exported as descriptor %d\n", ex.fd);
	struct prime im = { .fd = ex.fd };
	if (ioctl(b, PRIME_F2H, &im) < 0) { perror("PRIME_FD_TO_HANDLE (render)"); return 1; }
	uint32_t hb = im.handle;
	uint32_t *pb = map(b, hb, size);
	if (!pb) return 1;
	int same = 1;
	for (int i = 0; i < 1024; i++) if (pb[i] != 0xa0000000 + (uint32_t)i) same = 0;
	CHECK(same, "the other file does not see the pattern");
	/* Importing again gives the same handle. */
	struct prime im2 = { .fd = ex.fd };
	CHECK(ioctl(b, PRIME_F2H, &im2) == 0 && im2.handle == hb, "a second import gave handle %u, not %u", im2.handle, hb);
	/* Writes cross both ways. */
	pb[2000] = 0xb0b0b0b0;
	CHECK(pa[2000] == 0xb0b0b0b0, "a write by the importer is not seen by the exporter");

	/* The GPU writes to it through B's own address space. */
	struct ctx cx = { .op = 1 };
	ioctl(b, CMD(0x02, struct ctx), &cx);
	uint32_t ctx = *(uint32_t *)&cx;
	uint32_t hib = create(b, 4096);
	uint32_t *ibm = map(b, hib, 4096);
	uint64_t va_ib = 0x100000, va_buf = 0x400000;
	struct gem_va mv[2] = {
		{ .handle = hib, .op = 1, .flags = 0xe, .va = va_ib, .size = 4096 },
		{ .handle = hb, .op = 1, .flags = 0x6, .va = va_buf, .size = size },
	};
	for (int i = 0; i < 2; i++)
		if (ioctl(b, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }
	uint64_t at = va_buf + 4 * 3000;
	ibm[0] = 0xc0033700; ibm[1] = (5 << 8) | (1 << 20); ibm[2] = (uint32_t)at; ibm[3] = (uint32_t)(at >> 32);
	ibm[4] = 0x6e75abcd;
	struct ib ib = { .va = va_ib, .bytes = 5 * 4 };
	struct chunk ck = { .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib };
	uint64_t chunks[1] = { (uintptr_t)&ck };
	struct cs cs = { .ctx = ctx, .nchunks = 1, .chunks = (uintptr_t)chunks };
	if (ioctl(b, CMD(0x04, struct cs), &cs) < 0) { perror("CS"); return 1; }
	struct wait w = { .handle = *(uint64_t *)&cs, .timeout = 2000000000ull, .ctx = ctx };
	if (ioctl(b, CMD(0x09, struct wait), &w) < 0) { perror("WAIT_CS"); return 1; }
	CHECK(pa[3000] == 0x6e75abcd, "the GPU's write through B's mapping is not seen by A (read %#x)", pa[3000]);
	printf("the GPU wrote %#x through the importer's address space; the exporter reads %#x\n", 0x6e75abcd, pa[3000]);

	/* The card's own node imports it and makes a framebuffer of it (not shown). */
	struct prime ic = { .fd = ex.fd };
	if (ioctl(c, PRIME_F2H, &ic) < 0) { perror("PRIME_FD_TO_HANDLE (card)"); return 1; }
	struct fb2 f = { .w = 1920, .h = 1080, .fmt = 0x34325258 /* XR24 */, .handles = { ic.handle }, .pitches = { 1920 * 4 } };
	if (ioctl(c, ADDFB2, &f) < 0) perror("ADDFB2 (card)");
	CHECK(f.id != 0, "no framebuffer from the imported buffer");
	printf("card node: imported as handle %u, framebuffer %u\n", ic.handle, f.id);
	if (f.id) { unsigned int id = f.id; ioctl(c, RMFB, &id); }

	/* Closing the exporter's handle leaves the importers' buffer alive. */
	struct gem_close cl = { .handle = ha };
	munmap(pa, size);
	ioctl(a, CLOSE, &cl);
	CHECK(pb[3000] == 0x6e75abcd, "the importer lost the buffer when the exporter closed its handle");
	close(ex.fd);
	printf(bad ? "FAILED\n" : "buffer sharing works\n");
	return bad;
}
