/* SPDX-License-Identifier: BSD-2-Clause */
/* Microbenchmarks of the Radeon render node's kernel interface, one binary for AbyssBSD and for
 * FreeBSD with drm-kmod (the components of docs/GRAPHICS.md section 12). It speaks the driver's
 * ioctls directly, as libdrm_amdgpu does, so both kernels run the same requests.
 *
 *   drm.bo.create4k     create, map, touch, unmap and close a 4 KiB buffer in system memory
 *   drm.bo.create1m     the same for 1 MiB
 *   drm.submit.<ring>   a small job submitted and waited for, one at a time (round trip)
 *   drm.submit.<ring>.batch  1000 small jobs submitted, then the last waited for, per job
 *   drm.submit.<ring>.cpu  40 jobs submitted back to back, the time of the submit calls only
 *                       (the jobs run behind; this is the processor's cost of one submission)
 *   drm.fence.signalled the wait for a job that has finished long ago
 *   drm.ioctl.info      the cheapest query (the accelerator is working): the cost of an ioctl itself
 *
 * Every value is nanoseconds per operation (the median of the rounds); larger is slower.
 * Usage: drmbench [node] [rounds] */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct gem_create { uint64_t size, align, domains, flags; };
struct gem_mmap { uint64_t offset; };
struct gem_close { uint32_t handle, pad; };
struct ctx { uint32_t op, flags, id, prio; };
struct gem_va { uint32_t handle, pad, op, flags; uint64_t va, off, size; };
struct cs { uint32_t ctx, list, nchunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, len; uint64_t data; };
struct ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct info { uint64_t ret; uint32_t size, query; uint32_t a0, a1, a2, a3; };
struct wait { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)
#define GEM_CLOSE _IOW('d', 0x09, struct gem_close)

static int fd;
static uint32_t ctx;

static uint64_t now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static int cmp(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return x < y ? -1 : x > y;
}

static void report(const char *name, double *v, int n)
{
	qsort(v, (size_t)n, sizeof *v, cmp);
	printf("drmbench %s %.0f ns/op\n", name, v[n / 2]);
	fflush(stdout);
}

static uint32_t bo_create(uint64_t size)
{
	struct gem_create g = { .size = size, .align = 4096, .domains = 2 };
	if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); exit(1); }
	return (uint32_t)g.size; /* the handle is the first dword of the answer */
}

static void *bo_map(uint32_t h, uint64_t size)
{
	struct gem_mmap m = { .offset = h };
	if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); exit(1); }
	void *p = mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
	if (p == MAP_FAILED) { perror("mmap"); exit(1); }
	return p;
}

static void bo_close(uint32_t h)
{
	struct gem_close c = { .handle = h };
	ioctl(fd, GEM_CLOSE, &c);
}

static uint64_t submit(uint64_t va, unsigned bytes, unsigned ip, unsigned ring)
{
	struct ib ib = { .va = va, .bytes = bytes, .ip = ip, .ring = ring };
	struct chunk ck = { .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib };
	uint64_t chunks[1] = { (uintptr_t)&ck };
	struct cs cs = { .ctx = ctx, .nchunks = 1, .chunks = (uintptr_t)chunks };
	if (ioctl(fd, CMD(0x04, struct cs), &cs) < 0) { perror("CS"); exit(1); }
	return *(uint64_t *)&cs;
}

static void wait_for(uint64_t seq, unsigned ip, unsigned ring)
{
	struct wait w = { .handle = seq, .timeout = 2000000000ull, .ip = ip, .ring = ring, .ctx = ctx };
	if (ioctl(fd, CMD(0x09, struct wait), &w) < 0) { perror("WAIT_CS"); exit(1); }
}

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
	int rounds = argc > 2 ? atoi(argv[2]) : 9;
	fd = open(node, O_RDWR);
	if (fd < 0) { perror(node); return 1; }
	struct ctx c = { .op = 1 };
	if (ioctl(fd, CMD(0x02, struct ctx), &c) < 0) { perror("CTX"); return 1; }
	ctx = *(uint32_t *)&c;
	double *v = calloc((size_t)rounds, sizeof *v);

	/* Buffers: create, map, touch every page, unmap, close. */
	static const struct { const char *name; uint64_t size; int n; } bos[] = {
		{ "drm.bo.create4k", 4096, 2000 }, { "drm.bo.create1m", 1 << 20, 200 } };
	for (unsigned k = 0; k < 2; k++) {
		for (int r = 0; r < rounds; r++) {
			uint64_t t0 = now();
			for (int i = 0; i < bos[k].n; i++) {
				uint32_t h = bo_create(bos[k].size);
				volatile char *p = bo_map(h, bos[k].size);
				for (uint64_t o = 0; o < bos[k].size; o += 4096) p[o] = 1;
				munmap((void *)p, bos[k].size);
				bo_close(h);
			}
			v[r] = (double)(now() - t0) / bos[k].n;
		}
		report(bos[k].name, v, rounds);
	}

	/* The command buffer and a data page, mapped in the GPU's address space. */
	const uint64_t va_ib = 0x100000, va_data = 0x200000;
	uint32_t hib = bo_create(4096), hdata = bo_create(4096);
	uint32_t *p = bo_map(hib, 4096);
	bo_map(hdata, 4096);
	struct gem_va mv[2] = {
		{ .handle = hib, .op = 1, .flags = 0xe, .va = va_ib, .size = 4096 },
		{ .handle = hdata, .op = 1, .flags = 0x6, .va = va_data, .size = 4096 } };
	for (int i = 0; i < 2; i++)
		if (ioctl(fd, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }
	/* WRITE_DATA, one dword to the data page; the compute ring takes the same packet with the
	 * shader-type bit set in the header. */
	for (unsigned ip = 0; ip < 2; ip++) {
		uint32_t *q = p + ip * 16;
		q[0] = 0xc0033700 | (ip << 1); q[1] = (5 << 8) | (1 << 20);
		q[2] = (uint32_t)va_data; q[3] = (uint32_t)(va_data >> 32); q[4] = 0x600dcafe;
	}
	static const struct { const char *name, *batch; unsigned ip, ring; uint64_t at; } rings[] = {
		{ "drm.submit.gfx", "drm.submit.gfx.batch", 0, 0, 0 },
		{ "drm.submit.compute", "drm.submit.compute.batch", 1, 0, 64 } };
	for (unsigned k = 0; k < 2; k++) {
		uint64_t va = va_ib + rings[k].at;
		wait_for(submit(va, 20, rings[k].ip, rings[k].ring), rings[k].ip, rings[k].ring);
		for (int r = 0; r < rounds; r++) {
			uint64_t t0 = now();
			for (int i = 0; i < 1000; i++)
				wait_for(submit(va, 20, rings[k].ip, rings[k].ring), rings[k].ip, rings[k].ring);
			v[r] = (double)(now() - t0) / 1000;
		}
		report(rings[k].name, v, rounds);
		for (int r = 0; r < rounds; r++) {
			uint64_t t0 = now(), last = 0;
			for (int i = 0; i < 1000; i++) last = submit(va, 20, rings[k].ip, rings[k].ring);
			wait_for(last, rings[k].ip, rings[k].ring);
			v[r] = (double)(now() - t0) / 1000;
		}
		report(rings[k].batch, v, rounds);
		static char cpuname[2][40];
		snprintf(cpuname[k], sizeof cpuname[k], "%s.cpu", rings[k].name);
		for (int r = 0; r < rounds; r++) {
			uint64_t last = 0, t0 = now();
			for (int i = 0; i < 40; i++) last = submit(va, 20, rings[k].ip, rings[k].ring);
			v[r] = (double)(now() - t0) / 40;
			wait_for(last, rings[k].ip, rings[k].ring);
		}
		report(cpuname[k], v, rounds);
	}

	/* A wait for a job that has finished long ago. */
	uint64_t old = submit(va_ib, 20, 0, 0);
	wait_for(old, 0, 0);
	for (int r = 0; r < rounds; r++) {
		uint64_t t0 = now();
		for (int i = 0; i < 20000; i++) wait_for(old, 0, 0);
		v[r] = (double)(now() - t0) / 20000;
	}
	report("drm.fence.signalled", v, rounds);

	uint32_t working = 0;
	struct info in = { .ret = (uintptr_t)&working, .size = 4, .query = 0 };
	for (int r = 0; r < rounds; r++) {
		uint64_t t0 = now();
		for (int i = 0; i < 20000; i++) ioctl(fd, CMD(0x05, struct info), &in);
		v[r] = (double)(now() - t0) / 20000;
	}
	report("drm.ioctl.info", v, rounds);
	return 0;
}
