/* SPDX-License-Identifier: BSD-2-Clause */
/* Hangs each engine of the Radeon render node on purpose (a wait that is never satisfied), waits
 * for the kernel to reset the ring (it does after two seconds), checks that the waiting returns,
 * that the context learns of the reset, and that the engine runs a job again afterwards.
 * Usage: amdhang [node [gfx|dma|compute]] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct gem_create { uint64_t size, align, domains, flags; };
struct gem_mmap { uint64_t offset; };
struct ctx { uint32_t op, flags, id, prio; };
struct ctx_state { uint64_t flags; uint32_t hangs, reset_status; };
struct gem_va { uint32_t handle, pad, op, flags; uint64_t va, off, size; };
struct cs { uint32_t ctx, list, nchunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, len; uint64_t data; };
struct ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct wait { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)

static int fd;
static uint32_t ctx_id;
static volatile uint32_t *data;
static uint8_t *ibmem;
static const uint64_t va_ib = 0x100000, va_data = 0x200000;

static double now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

static int submit(uint32_t ip, uint32_t ring, uint64_t va, uint32_t dwords, uint64_t *handle)
{
	struct ib ib = { .va = va, .bytes = dwords * 4, .ip = ip, .ring = ring };
	struct chunk ck = { .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib };
	uint64_t chunks[1] = { (uintptr_t)&ck };
	struct cs cs = { .ctx = ctx_id, .nchunks = 1, .chunks = (uintptr_t)chunks };
	if (ioctl(fd, CMD(0x04, struct cs), &cs) < 0)
		return -1;
	*handle = *(uint64_t *)&cs;
	return 0;
}

static int wait_for(uint32_t ip, uint32_t ring, uint64_t handle, uint64_t ns, uint64_t *status)
{
	struct wait w = { .handle = handle, .timeout = ns, .ip = ip, .ring = ring, .ctx = ctx_id };
	if (ioctl(fd, CMD(0x09, struct wait), &w) < 0)
		return -1;
	*status = w.handle;
	return 0;
}

/* The packets of a job that waits for a word of memory that never gets its value. */
static uint32_t hang_packets(uint32_t ip, uint32_t *p)
{
	uint64_t at = va_data + 256;
	if (ip == 2) {
		uint32_t d[8] = { 8 | (3u << 28) | (1u << 31), (uint32_t)at, (uint32_t)(at >> 32), 0xdeadbeef,
				  0xffffffff, 10 | (0xfff << 16), 0, 0 };
		memcpy(p, d, sizeof d);
		return 8;
	}
	uint32_t d[6] = { 0xc0053c00 | (ip == 1 ? 2 : 0), 3 | (1 << 4), (uint32_t)at, (uint32_t)(at >> 32),
			  0xdeadbeef, 0xffffffff };
	memcpy(p, d, sizeof d);
	p[6] = 0x10;
	return 7;
}

/* The packets of a job that writes a word. */
static uint32_t write_packets(uint32_t ip, uint32_t *p, uint64_t at, uint32_t value)
{
	if (ip == 2) {
		uint32_t d[8] = { 2, (uint32_t)at, (uint32_t)(at >> 32), 0, value, 0, 0, 0 };
		memcpy(p, d, sizeof d);
		return 8;
	}
	uint32_t d[5] = { 0xc0033700 | (ip == 1 ? 2 : 0), (5 << 8) | (1 << 20), (uint32_t)at,
			  (uint32_t)(at >> 32), value };
	memcpy(p, d, sizeof d);
	return 5;
}

static int one(const char *name, uint32_t ip)
{
	uint64_t handle, status;
	int bad = 0;
	/* A job that never finishes. */
	uint32_t *h = (uint32_t *)(ibmem + 0x100);
	uint32_t n = hang_packets(ip, h);
	data[64] = 0;
	if (submit(ip, 0, va_ib + 0x100, n, &handle) < 0) { perror("CS (hang)"); return 1; }
	double t0 = now();
	if (wait_for(ip, 0, handle, 10000000000ull, &status) < 0) { perror("WAIT_CS (hang)"); return 1; }
	double took = now() - t0;
	printf("%s: the hung job's wait returned after %.1f s (status %llu): %s\n", name, took,
	       (unsigned long long)status, took >= 1.5 && took < 9 ? "the ring was reset" : "UNEXPECTED");
	bad |= !(took >= 1.5 && took < 9);
	/* The context learns of it. */
	struct { struct ctx in; } q;
	memset(&q, 0, sizeof q);
	q.in = (struct ctx){ .op = 4, .id = ctx_id };
	if (ioctl(fd, CMD(0x02, struct ctx), &q) < 0) { perror("CTX query"); return 1; }
	struct ctx_state *st = (struct ctx_state *)&q;
	printf("%s: context state flags %#llx (bit 0 = a reset happened, bit 2 = guilty), hangs %u\n", name,
	       (unsigned long long)st->flags, st->hangs);
	bad |= !(st->flags & 1);
	/* The engine works again. */
	uint32_t *w = (uint32_t *)(ibmem + 0x200);
	uint32_t m = write_packets(ip, w, va_data + 512, 0x600d0000 | ip);
	data[128] = 0;
	if (submit(ip, 0, va_ib + 0x200, m, &handle) < 0) { perror("CS (after)"); return 1; }
	if (wait_for(ip, 0, handle, 3000000000ull, &status) < 0) { perror("WAIT_CS (after)"); return 1; }
	printf("%s: after the reset a job wrote %#x (want %#x)\n", name, data[128], 0x600d0000 | ip);
	bad |= data[128] != (0x600d0000 | ip);
	return bad;
}

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD129";
	const char *which = argc > 2 ? argv[2] : "all";
	fd = open(node, O_RDWR);
	if (fd < 0) { perror(node); return 1; }
	struct ctx c = { .op = 1 };
	if (ioctl(fd, CMD(0x02, struct ctx), &c) < 0) { perror("CTX alloc"); return 1; }
	ctx_id = *(uint32_t *)&c; /* the id is the first dword of the answer */
	printf("context %u\n", ctx_id);
	uint32_t h[2];
	void *map[2];
	for (int i = 0; i < 2; i++) {
		struct gem_create g = { .size = 4096, .align = 4096, .domains = 2 };
		if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); return 1; }
		h[i] = (uint32_t)g.size;
		struct gem_mmap m = { .offset = h[i] };
		if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); return 1; }
		map[i] = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
		if (map[i] == MAP_FAILED) { perror("mmap"); return 1; }
		memset(map[i], 0, 4096);
	}
	struct gem_va mv[2] = {
		{ .handle = h[0], .op = 1, .flags = 0xe, .va = va_ib, .size = 4096 },
		{ .handle = h[1], .op = 1, .flags = 0x6, .va = va_data, .size = 4096 },
	};
	for (int i = 0; i < 2; i++)
		if (ioctl(fd, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }
	ibmem = map[0];
	data = map[1];
	int bad = 0;
	if (!strcmp(which, "all") || !strcmp(which, "dma"))
		bad |= one("dma", 2);
	if (!strcmp(which, "all") || !strcmp(which, "compute"))
		bad |= one("compute", 1);
	if (!strcmp(which, "all") || !strcmp(which, "gfx"))
		bad |= one("gfx", 0);
	printf(bad ? "FAILED\n" : "all engines recovered\n");
	return bad;
}
