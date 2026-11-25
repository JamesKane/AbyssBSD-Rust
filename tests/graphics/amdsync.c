/* SPDX-License-Identifier: BSD-2-Clause */
/* Sync files on the Radeon render node: a job's fence handed to a synchronisation object, exported
 * as a sync file, waited for with poll(2) and kqueue(2), merged with another, described, and taken
 * back into a synchronisation object.
 * Usage: amdsync [node] */
#include <sys/ioctl.h>
#include <sys/event.h>
#include <sys/eventfd.h>
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
struct sobj_array { uint64_t handles, points; uint32_t count, flags; };
struct sobj_arr { uint64_t handles; uint32_t count, pad; };
struct sobj_twait { uint64_t handles, points; int64_t timeout; uint32_t count, flags, first, pad; uint64_t deadline; };
struct sobj_transfer { uint32_t src, dst; uint64_t src_point, dst_point; uint32_t flags, pad; };
struct sobj_eventfd { uint32_t handle, flags; uint64_t point; int32_t fd, pad; };
struct tl_entry { uint32_t handle, flags; uint64_t point; };
struct bo_list { uint32_t op, list, count, entry_size; uint64_t entries; };
struct bo_entry { uint32_t handle, priority; };
struct prime { uint32_t handle, flags; int32_t fd; };
struct dmabuf_sync { uint32_t flags; int32_t fd; };
struct sync_merge { char name[32]; int32_t fd2, fence; uint32_t flags, pad; };
struct sync_info { char name[32]; int32_t status; uint32_t flags, num_fences, pad; uint64_t fences; };

#define CMD(n, t) _IOWR('d', 0x40 + (n), t)
#define SOBJ_CREATE _IOWR('d', 0xbf, struct sobj_create)
#define SOBJ_DESTROY _IOWR('d', 0xc0, struct sobj_create)
#define SOBJ_TO_FD _IOWR('d', 0xc1, struct sobj_handle)
#define SOBJ_FROM_FD _IOWR('d', 0xc2, struct sobj_handle)
#define SOBJ_WAIT _IOWR('d', 0xc3, struct sobj_wait)
#define SOBJ_TWAIT _IOWR('d', 0xca, struct sobj_twait)
#define SOBJ_QUERY _IOWR('d', 0xcb, struct sobj_array)
#define SOBJ_TRANSFER _IOWR('d', 0xcc, struct sobj_transfer)
#define SOBJ_TSIGNAL _IOWR('d', 0xcd, struct sobj_array)
#define SOBJ_EVENTFD _IOWR('d', 0xcf, struct sobj_eventfd)
#define SOBJ_SIGNAL _IOWR('d', 0xc5, struct sobj_arr)
#define PRIME_H2F _IOWR('d', 0x2d, struct prime)
#define DMABUF_EXPORT_SYNC _IOWR('b', 2, struct dmabuf_sync)
#define DMABUF_IMPORT_SYNC _IOW('b', 3, struct dmabuf_sync)
#define SYNC_MERGE _IOWR('>', 3, struct sync_merge)
#define SYNC_INFO _IOWR('>', 4, struct sync_info)

static int fd, bad;
static uint32_t ctx;
static const uint64_t va_ib = 0x100000, va_data = 0x400000;
static uint32_t *ibmap;

#define CHECK(cond, ...) do { if (!(cond)) { printf("FAIL: " __VA_ARGS__); printf("\n"); bad = 1; } } while (0)

/* A job of `packets` WRITE_DATA packets (a few microseconds each) whose fence goes to syncobj `h`
 * (at timeline `point`, if not 0), with buffer list `list`. */
static int submit_ex(uint32_t h, uint64_t point, uint32_t list, unsigned packets)
{
	uint32_t *p = ibmap;
	for (unsigned i = 0; i < packets; i++, p += 5) {
		uint64_t at = va_data + 4 * (i % 1024);
		p[0] = 0xc0033700; p[1] = (5 << 8) | (1 << 20);
		p[2] = (uint32_t)at; p[3] = (uint32_t)(at >> 32); p[4] = 0x5eed0000 + i;
	}
	struct ib ib = { .va = va_ib, .bytes = packets * 5 * 4 };
	struct tl_entry te = { .handle = h, .point = point };
	struct chunk ck[2] = {
		{ .id = 1, .len = sizeof ib / 4, .data = (uintptr_t)&ib },
		point ? (struct chunk){ .id = 9, .len = sizeof te / 4, .data = (uintptr_t)&te }
		      : (struct chunk){ .id = 5, .len = 1, .data = (uintptr_t)&h },
	};
	uint64_t chunks[2] = { (uintptr_t)&ck[0], (uintptr_t)&ck[1] };
	struct cs cs = { .ctx = ctx, .list = list, .nchunks = 2, .chunks = (uintptr_t)chunks };
	return ioctl(fd, CMD(0x04, struct cs), &cs);
}

static int submit(uint32_t h, unsigned packets) { return submit_ex(h, 0, 0, packets); }

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

	/* 5. What is refused: an empty syncobj as a sync file, a bad descriptor, an unknown flag. */
	struct sobj_handle e = { .handle = s1, .flags = 8 };
	CHECK(ioctl(fd, SOBJ_TO_FD, &e) < 0 && errno == EINVAL, "an unknown flag should be EINVAL");
	uint32_t empty = sobj_new();
	struct sobj_handle e2 = { .handle = empty, .flags = 1 };
	CHECK(ioctl(fd, SOBJ_TO_FD, &e2) < 0 && errno == EINVAL, "an empty syncobj should be EINVAL");
	struct sobj_handle bf = { .flags = 1, .fd = 999 };
	CHECK(ioctl(fd, SOBJ_FROM_FD, &bf) < 0 && errno == EBADF, "a bad descriptor should be EBADF");

	/* 6. A descriptor for the object itself: another handle names the same object. */
	uint32_t o1 = sobj_new();
	struct sobj_handle xo = { .handle = o1 };
	if (ioctl(fd, SOBJ_TO_FD, &xo) < 0) { perror("SYNCOBJ_HANDLE_TO_FD (object)"); bad = 1; }
	else {
		struct sobj_handle io = { .fd = xo.fd };
		if (ioctl(fd, SOBJ_FROM_FD, &io) < 0) { perror("SYNCOBJ_FD_TO_HANDLE (object)"); bad = 1; }
		else {
			struct sobj_arr sg = { .handles = (uintptr_t)&io.handle, .count = 1 };
			ioctl(fd, SOBJ_SIGNAL, &sg);
			struct sobj_wait w2 = { .handles = (uintptr_t)&o1, .count = 1 };
			struct timespec now;
			clock_gettime(CLOCK_MONOTONIC, &now);
			w2.timeout = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + 1000000000LL;
			int r = ioctl(fd, SOBJ_WAIT, &w2);
			printf("object descriptor %d: handle %u -> %u, signal through one seen by the other: %d\n",
			       xo.fd, o1, io.handle, r == 0);
			CHECK(r == 0, "a signal through the imported handle was not seen through the first");
		}
		close(xo.fd);
	}

	/* 7. Timeline: a job signals point 5, point 2 is signalled by hand, then query, wait, transfer. */
	uint32_t tl = sobj_new();
	uint64_t p2 = 2, p5 = 5;
	struct sobj_array ts = { .handles = (uintptr_t)&tl, .points = (uintptr_t)&p2, .count = 1 };
	CHECK(ioctl(fd, SOBJ_TSIGNAL, &ts) == 0, "TIMELINE_SIGNAL failed (%d)", errno);
	if (submit_ex(tl, 5, 0, 3000) < 0) { perror("CS (timeline signal)"); bad = 1; }
	struct sobj_twait tw = { .handles = (uintptr_t)&tl, .points = (uintptr_t)&p5, .count = 1 };
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	tw.timeout = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + 2000000000LL;
	int tr = ioctl(fd, SOBJ_TWAIT, &tw);
	uint64_t last = 0, submitted = 0;
	struct sobj_array qa = { .handles = (uintptr_t)&tl, .points = (uintptr_t)&last, .count = 1 };
	ioctl(fd, SOBJ_QUERY, &qa);
	struct sobj_array qs = { .handles = (uintptr_t)&tl, .points = (uintptr_t)&submitted, .count = 1, .flags = 1 };
	ioctl(fd, SOBJ_QUERY, &qs);
	printf("timeline: wait for point 5 %d, last signalled %llu, last submitted %llu\n", tr,
	       (unsigned long long)last, (unsigned long long)submitted);
	CHECK(tr == 0 && last == 5 && submitted == 5, "timeline wait/query wrong");
	/* A point that does not follow the last is refused before the job runs. */
	CHECK(submit_ex(tl, 4, 0, 10) < 0 && errno == EINVAL, "a point that does not grow should be EINVAL");
	/* Transfer point 5 to a binary object, and wait there. */
	uint32_t bin = sobj_new();
	struct sobj_transfer tx = { .src = tl, .dst = bin, .src_point = 5 };
	CHECK(ioctl(fd, SOBJ_TRANSFER, &tx) == 0, "TRANSFER failed (%d)", errno);
	struct sobj_wait w3 = { .handles = (uintptr_t)&bin, .count = 1 };
	clock_gettime(CLOCK_MONOTONIC, &now);
	w3.timeout = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + 1000000000LL;
	CHECK(ioctl(fd, SOBJ_WAIT, &w3) == 0, "waiting for the transferred fence failed (%d)", errno);

	/* 8. SYNCOBJ_EVENTFD: an eventfd is signalled when a job's fence has finished. */
	int efd = eventfd(0, 0);
	uint32_t ev = sobj_new();
	if (submit(ev, 3000) < 0) { perror("CS"); bad = 1; }
	struct sobj_eventfd se = { .handle = ev, .fd = efd };
	CHECK(ioctl(fd, SOBJ_EVENTFD, &se) == 0, "SYNCOBJ_EVENTFD failed (%d)", errno);
	struct pollfd pe = { .fd = efd, .events = POLLIN };
	int pr = poll(&pe, 1, 2000);
	uint64_t cnt = 0;
	if (pr == 1) (void)read(efd, &cnt, sizeof cnt);
	printf("eventfd: poll %d, count %llu\n", pr, (unsigned long long)cnt);
	CHECK(pr == 1 && cnt == 1, "the eventfd was not signalled once");
	/* WAIT_AVAILABLE on an object with no fence yet: signalled when the job is submitted. */
	uint32_t pending_obj = sobj_new();
	int efd2 = eventfd(0, EFD_NONBLOCK);
	struct sobj_eventfd sa = { .handle = pending_obj, .flags = 4, .fd = efd2 };
	CHECK(ioctl(fd, SOBJ_EVENTFD, &sa) == 0, "SYNCOBJ_EVENTFD (available) failed (%d)", errno);
	uint64_t none = 0;
	CHECK(read(efd2, &none, sizeof none) < 0 && errno == EAGAIN, "signalled before any fence");
	if (submit(pending_obj, 10) < 0) { perror("CS"); bad = 1; }
	pe.fd = efd2;
	pr = poll(&pe, 1, 2000);
	printf("eventfd (available): poll %d\n", pr);
	CHECK(pr == 1, "the eventfd was not signalled when the fence arrived");
	close(efd);
	close(efd2);

	/* 9. dma-buf fences: a job that lists the data buffer leaves its fence on it. */
	struct prime pr1 = { .handle = h[1], .flags = O_RDWR | O_CLOEXEC, .fd = -1 };
	if (ioctl(fd, PRIME_H2F, &pr1) < 0) { perror("PRIME_HANDLE_TO_FD"); bad = 1; }
	else {
		struct bo_entry be = { .handle = h[1], .priority = 0 };
		struct bo_list bl = { .op = 0, .count = 1, .entry_size = sizeof be, .entries = (uintptr_t)&be };
		if (ioctl(fd, CMD(0x03, struct bo_list), &bl) < 0) { perror("BO_LIST"); bad = 1; }
		uint32_t list = *(uint32_t *)&bl;
		uint32_t dj = sobj_new();
		if (submit_ex(dj, 0, list, 3000) < 0) { perror("CS (list)"); bad = 1; }
		struct dmabuf_sync ex = { .flags = 2 /* write: wait for all */ };
		if (ioctl(pr1.fd, DMABUF_EXPORT_SYNC, &ex) < 0) { perror("DMA_BUF_IOCTL_EXPORT_SYNC_FILE"); bad = 1; }
		else {
			int e1 = ready(ex.fd, 0);
			int e2 = ready(ex.fd, 2000);
			printf("dma-buf export: sync file %d readable at once %d, within 2 s %d\n", ex.fd, e1, e2);
			CHECK(e2, "the dma-buf's sync file never became readable");
			/* Import it back as a writer's fence, and export again. */
			struct dmabuf_sync im = { .flags = 2, .fd = ex.fd };
			CHECK(ioctl(pr1.fd, DMABUF_IMPORT_SYNC, &im) == 0, "DMA_BUF_IOCTL_IMPORT_SYNC_FILE failed (%d)", errno);
			struct dmabuf_sync ex2 = { .flags = 1 };
			CHECK(ioctl(pr1.fd, DMABUF_EXPORT_SYNC, &ex2) == 0 && ready(ex2.fd, 2000), "export after import failed");
			struct dmabuf_sync badf = { .flags = 0 };
			CHECK(ioctl(pr1.fd, DMABUF_EXPORT_SYNC, &badf) < 0 && errno == EINVAL, "flags 0 should be EINVAL");
		}
		close(pr1.fd);
	}

	printf(bad ? "amdsync: FAILED\n" : "amdsync: ok\n");
	return bad ? 2 : 0;
}
