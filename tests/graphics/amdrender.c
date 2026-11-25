/* SPDX-License-Identifier: BSD-2-Clause */
/* Exercises the Radeon render node by hand, as libdrm_amdgpu would: the version, the
 * information queries, two buffers (a command buffer and a data page) mapped in the
 * GPU's address space, a command submission whose indirect buffer writes a word to the
 * data page, and the wait for it.
 * Usage: amdrender [node] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

struct version { int major, minor, patchlevel; size_t nlen; char *name; size_t dlen; char *date; size_t dsc; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct gem_mmap { uint64_t offset; };
struct info { uint64_t ret; uint32_t size, query; uint32_t a0, a1, a2, a3; };
struct ctx { uint32_t op, flags, id, prio; };
struct gem_va { uint32_t handle, pad, op, flags; uint64_t va, off, size; };
struct cs { uint32_t ctx, list, nchunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, len; uint64_t data; };
struct ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct wait { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };

#define VERSION _IOWR('d', 0x00, struct version)
#define CMD(n, t) _IOWR('d', 0x40 + (n), t)

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
	int fd = open(node, O_RDWR);
	if (fd < 0) { perror(node); return 1; }

	char name[32] = "", date[32] = "", desc[64] = "";
	struct version v = { .nlen = 31, .name = name, .dlen = 31, .date = date, .dsc = 63, .desc = desc };
	if (ioctl(fd, VERSION, &v) < 0) { perror("VERSION"); return 1; }
	printf("driver %s %d.%d.%d\n", name, v.major, v.minor, v.patchlevel);

	uint32_t working = 0;
	struct info in = { .ret = (uintptr_t)&working, .size = 4, .query = 0 };
	if (ioctl(fd, CMD(0x05, struct info), &in) < 0) { perror("INFO accel_working"); return 1; }
	printf("accel working %u\n", working);

	uint8_t dev[512];
	memset(dev, 0, sizeof dev);
	in = (struct info){ .ret = (uintptr_t)dev, .size = sizeof dev, .query = 0x16 };
	if (ioctl(fd, CMD(0x05, struct info), &in) < 0) perror("INFO dev_info");
	else printf("device id %#x\n", *(uint32_t *)dev);

	struct ctx c = { .op = 1 };
	if (ioctl(fd, CMD(0x02, struct ctx), &c) < 0) { perror("CTX alloc"); return 1; }
	uint32_t ctx = c.id;

	uint32_t h[2];
	void *map[2];
	for (int i = 0; i < 2; i++) {
		struct gem_create g = { .size = 4096, .align = 4096, .domains = 2 };
		if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); return 1; }
		h[i] = (uint32_t)g.size; /* the handle is in the first dword */
		struct gem_mmap m = { .offset = h[i] };
		if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); return 1; }
		map[i] = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
		if (map[i] == MAP_FAILED) { perror("mmap"); return 1; }
		memset(map[i], 0, 4096);
	}
	const uint64_t va_ib = 0x100000, va_data = 0x200000;
	struct gem_va mv[2] = {
		{ .handle = h[0], .op = 1, .flags = 0xe, .va = va_ib, .size = 4096 },
		{ .handle = h[1], .op = 1, .flags = 0x6, .va = va_data, .size = 4096 },
	};
	for (int i = 0; i < 2; i++)
		if (ioctl(fd, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }

	/* WRITE_DATA, memory, confirmed: one dword to the data page. */
	uint32_t *p = map[0];
	p[0] = 0xc0033700; p[1] = (5 << 8) | (1 << 20);
	p[2] = (uint32_t)va_data; p[3] = (uint32_t)(va_data >> 32); p[4] = 0x600dcafe;
	/* The packet is five dwords: a header with count 3 covers four body dwords. */
	struct ib ib = { .va = va_ib, .bytes = 5 * 4 };
	struct chunk ck = { .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib };
	uint64_t chunks[1] = { (uintptr_t)&ck };
	struct cs cs = { .ctx = ctx, .nchunks = 1, .chunks = (uintptr_t)chunks };
	if (ioctl(fd, CMD(0x04, struct cs), &cs) < 0) { perror("CS"); return 1; }
	uint64_t seq = *(uint64_t *)&cs;
	struct wait w = { .handle = seq, .timeout = 2000000000ull, .ctx = ctx };
	if (ioctl(fd, CMD(0x09, struct wait), &w) < 0) { perror("WAIT_CS"); return 1; }
	printf("job %llu: wait status %llu, data word %#x (want 0x600dcafe)\n",
	       (unsigned long long)seq, (unsigned long long)w.handle, ((volatile uint32_t *)map[1])[0]);
	int bad = ((volatile uint32_t *)map[1])[0] != 0x600dcafe;

	/* The system DMA engine (ip type 2): a write, then a copy of what it wrote. */
	uint32_t *d = (uint32_t *)((char *)map[0] + 0x100);
	uint64_t src = va_data + 8, dst = va_data + 16;
	uint32_t pk[16] = { 2, (uint32_t)src, (uint32_t)(src >> 32), 0, 0xd00dfeed,
		1, 3, 0, (uint32_t)src, (uint32_t)(src >> 32), (uint32_t)dst, (uint32_t)(dst >> 32),
		0, 0, 0, 0 };
	for (int i = 0; i < 16; i++) d[i] = pk[i];
	struct ib dib = { .va = va_ib + 0x100, .bytes = 16 * 4, .ip = 2 };
	struct chunk dck = { .id = 1, .len = sizeof dib / 4, .data = (uintptr_t)&dib };
	uint64_t dchunks[1] = { (uintptr_t)&dck };
	struct cs dcs = { .ctx = ctx, .nchunks = 1, .chunks = (uintptr_t)dchunks };
	if (ioctl(fd, CMD(0x04, struct cs), &dcs) < 0) { perror("CS (DMA)"); return 1; }
	struct wait dw = { .handle = *(uint64_t *)&dcs, .timeout = 2000000000ull, .ip = 2, .ctx = ctx };
	if (ioctl(fd, CMD(0x09, struct wait), &dw) < 0) { perror("WAIT_CS (DMA)"); return 1; }
	volatile uint32_t *dp = (volatile uint32_t *)map[1];
	printf("dma job %llu: wait status %llu, written %#x, copied %#x (want 0xd00dfeed twice)\n",
	       (unsigned long long)*(uint64_t *)&dcs, (unsigned long long)dw.handle, dp[2], dp[4]);
	bad |= dp[2] != 0xd00dfeed || dp[4] != 0xd00dfeed;
	return bad ? 2 : 0;
}
