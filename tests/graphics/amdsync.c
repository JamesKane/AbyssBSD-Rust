/* SPDX-License-Identifier: BSD-2-Clause */
/* Sync files on the Radeon render node: a job's fence handed to a synchronisation object, exported
 * as a sync file, waited for with poll(2) and kqueue(2), merged with another, described, and taken
 * back into a synchronisation object.
 * Usage: amdsync [node] */
#include <sys/ioctl.h>
#include <sys/event.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <fcntl.h>
#include <poll.h>
#include <errno.h>
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
struct sobj_create { uint32_t handle, flags; };
struct sobj_handle { uint32_t handle, flags; int32_t fd; uint32_t pad; uint64_t point; };
struct sobj_wait { uint64_t handles; int64_t timeout; uint32_t count, flags, first, pad; uint64_t deadline; };
struct sync_merge { char name[32]; int32_t fd2, fence; uint32_t flags, pad; };
struct sync_info { char name[32]; int32_t status; uint32_t flags, num_fences, pad; uint64_t fences; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)
#define SOBJ_CREATE _IOWR('d', 0xbf, struct sobj_create)
#define SOBJ_DESTROY _IOWR('d', 0xc0, struct sobj_create)
#define SOBJ_TO_FD _IOWR('d', 0xc1, struct sobj_handle)
#define SOBJ_FROM_FD _IOWR('d', 0xc2, struct sobj_handle)
#define SOBJ_WAIT _IOWR('d', 0xc3, struct sobj_wait)
#define SYNC_MERGE _IOWR('>', 3, struct sync_merge)
#define SYNC_INFO _IOWR('>', 4, struct sync_info)

static int fd, bad;
static uint32_t ctx;
static const uint64_t va_ib = 0x100000, va_data = 0x400000;
static uint32_t *ibmap;

#define CHECK(cond, ...) do { if (!(cond)) { printf("FAIL: " __VA_ARGS__); printf("\n"); bad = 1; } } while (0)

/* A job of `packets` WRITE_DATA packets (a few microseconds each) whose fence goes to syncobj `h`. */
static int submit(uint32_t h, unsigned packets)
{
	uint32_t *p = ibmap;
	for (unsigned i = 0; i < packets; i++, p += 5) {
		uint64_t at = va_data + 4 * (i % 1024);
		p[0] = 0xc0033700; p[1] = (5 << 8) | (1 << 20);
		p[2] = (uint32_t)at; p[3] = (uint32_t)(at >> 32); p[4] = 0x5eed0000 + i;
	}
	struct ib ib = { .va = va_ib, .bytes = packets * 5 * 4 };
	struct chunk ck[2] = {
		{ .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib },
		{ .id = 5, .len = 1, .data = (uintptr_t)&h },
	};
	uint64_t chunks[2] = { (uintptr_t)&ck[0], (uintptr_t)&ck[1] };
	struct cs cs = { .ctx = ctx, .nchunks = 2, .chunks = (uintptr_t)chunks };
	return ioctl(fd, CMD(0x04, struct cs), &cs);
}

static uint32_t sobj_new(void)
{
	struct sobj_create c = { 0 };
	if (ioctl(fd, SOBJ_CREATE, &c) < 0) { perror("SYNCOBJ_CREATE"); bad = 1; }
	return c.handle;
}

static int export_sync_file(uint32_t h)
{
	struct sobj_handle e = { .handle = h, .flags = 1 };
	if (ioctl(fd, SOBJ_TO_FD, &e) < 0) { perror("SYNCOBJ_HANDLE_TO_FD"); bad = 1; return -1; }
	return e.fd;
}

static int ready(int sf, int timeout_ms)
{
	struct pollfd p = { .fd = sf, .events = POLLIN };
	return poll(&p, 1, timeout_ms) == 1 && (p.revents & POLLIN);
}

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
	fd = open(node, O_RDWR);
	if (fd < 0) { perror(node); return 1; }

	struct ctx c = { .op = 1 };
	if (ioctl(fd, CMD(0x02, struct ctx), &c) < 0) { perror("CTX alloc"); return 1; }
	ctx = *(uint32_t *)&c;

	/* A 64 KiB command buffer and a data page, mapped in the GPU's address space. */
	uint32_t h[2];
	void *map[2];
	uint64_t sizes[2] = { 65536, 4096 };
	for (int i = 0; i < 2; i++) {
		struct gem_create g = { .size = sizes[i], .align = 4096, .domains = 2 };
		if (ioctl(fd, CMD(0x00, struct gem_create), &g) < 0) { perror("GEM_CREATE"); return 1; }
		h[i] = (uint32_t)g.size;
		struct gem_mmap m = { .offset = h[i] };
		if (ioctl(fd, CMD(0x01, struct gem_mmap), &m) < 0) { perror("GEM_MMAP"); return 1; }
		map[i] = mmap(0, sizes[i], PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.offset);
		if (map[i] == MAP_FAILED) { perror("mmap"); return 1; }
		memset(map[i], 0, sizes[i]);
	}
	ibmap = map[0];
	struct gem_va mv[2] = {
		{ .handle = h[0], .op = 1, .flags = 0xe, .va = va_ib, .size = 65536 },
		{ .handle = h[1], .op = 1, .flags = 0x6, .va = va_data, .size = 4096 },
	};
	for (int i = 0; i < 2; i++)
		if (ioctl(fd, CMD(0x08, struct gem_va), &mv[i]) < 0) { perror("GEM_VA"); return 1; }

	/* 1. A job's fence as a sync file: pending right after the submission, readable soon after. */
	uint32_t s1 = sobj_new();
	if (submit(s1, 3000) < 0) { perror("CS"); return 1; }
	int sf1 = export_sync_file(s1);
	int early = ready(sf1, 0);
	int late = ready(sf1, 2000);
	printf("sync file %d: readable at once %d, within 2 s %d\n", sf1, early, late);
	CHECK(late, "the sync file never became readable");
	CHECK(ready(sf1, 0), "it did not stay readable");

	/* 2. kqueue: EVFILT_READ on a sync file of a new job. */
	uint32_t s2 = sobj_new();
	if (submit(s2, 3000) < 0) { perror("CS"); return 1; }
	int sf2 = export_sync_file(s2);
	int kq = kqueue();
	struct kevent ch;
	EV_SET(&ch, sf2, EVFILT_READ, EV_ADD | EV_CLEAR, 0, 0, NULL);
	struct timespec two = { 2, 0 };
	struct kevent out;
	int n = kevent(kq, &ch, 1, &out, 1, &two);
	printf("kqueue: %d event(s), data %lld\n", n, n > 0 ? (long long)out.data : -1LL);
	CHECK(n == 1 && out.ident == (uintptr_t)sf2, "no EVFILT_READ event");

	/* 3. Merge two and describe the result. */
	uint32_t s3 = sobj_new();
	if (submit(s3, 3000) < 0) { perror("CS"); return 1; }
	int sf3 = export_sync_file(s3);
	struct sync_merge m = { .fd2 = sf3 };
	strcpy(m.name, "merged");
	if (ioctl(sf1, SYNC_MERGE, &m) < 0) { perror("SYNC_IOC_MERGE"); bad = 1; }
	else {
		struct sync_info info = { 0 };
		if (ioctl(m.fence, SYNC_INFO, &info) < 0) { perror("SYNC_IOC_FILE_INFO"); bad = 1; }
		printf("merged %d: %u fences, status %d\n", m.fence, info.num_fences, info.status);
		CHECK(info.num_fences == 2, "expected 2 fences, got %u", info.num_fences);
		CHECK(ready(m.fence, 2000), "the merged sync file never became readable");
		struct sync_info again = { 0 };
		ioctl(m.fence, SYNC_INFO, &again);
		CHECK(again.status == 1, "status %d, want 1", again.status);
		close(m.fence);
	}

	/* 4. Back into a synchronisation object, and waited for there. */
	struct sobj_handle imp = { .flags = 1, .fd = sf3 };
	if (ioctl(fd, SOBJ_FROM_FD, &imp) < 0) { perror("SYNCOBJ_FD_TO_HANDLE"); bad = 1; }
	else {
		struct sobj_wait w = { .handles = (uintptr_t)&imp.handle, .timeout = 2000000000LL, .count = 1 };
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		w.timeout = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + 2000000000LL;
		int r = ioctl(fd, SOBJ_WAIT, &w);
		printf("imported handle %u: wait %d\n", imp.handle, r);
		CHECK(r == 0, "waiting for the imported handle failed (%d)", errno);
	}

	/* 5. What is refused: a syncobj descriptor (not a sync file), an empty syncobj, a bad descriptor. */
	struct sobj_handle e = { .handle = s1, .flags = 0 };
	CHECK(ioctl(fd, SOBJ_TO_FD, &e) < 0 && errno == EOPNOTSUPP, "a plain syncobj descriptor should be EOPNOTSUPP");
	uint32_t empty = sobj_new();
	struct sobj_handle e2 = { .handle = empty, .flags = 1 };
	CHECK(ioctl(fd, SOBJ_TO_FD, &e2) < 0 && errno == EINVAL, "an empty syncobj should be EINVAL");
	struct sobj_handle bf = { .flags = 1, .fd = 999 };
	CHECK(ioctl(fd, SOBJ_FROM_FD, &bf) < 0 && errno == EBADF, "a bad descriptor should be EBADF");

	printf(bad ? "amdsync: FAILED\n" : "amdsync: ok\n");
	return bad ? 2 : 0;
}
