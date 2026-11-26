/* SPDX-License-Identifier: BSD-2-Clause */
/*
 * pt_job: panthor's render node by its own requests, no Mesa. A VM, two buffers mapped in it, a
 * group of one queue, and a three-instruction user stream that adds 5 to a 64-bit word of the
 * target buffer (MOV48, MOV48, SYNC_ADD64: the encodings of the clean-room spec 07 §7.4.5),
 * submitted with a syncobj signal; the syncobj waited on; the word read back. Then a stream at
 * an address nothing maps: the group must report a fatal fault. Then the group destroyed. Looped.
 *
 * cc -O2 -o pt_job pt_job.c -I/usr/local/include/libdrm -I.   (panthor_drm.h beside it)
 * usage: pt_job [rounds [fault]]   ("fault" adds a stream at an unmapped address)
 */
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <drm.h>
#include "panthor_drm.h"

#define STREAM_VA 0x1000000ULL
#define TARGET_VA 0x2000000ULL

static int fd;

static int
x(unsigned long req, void *arg, const char *what)
{
	if (ioctl(fd, req, arg) != 0) {
		printf("pt_job: %s: %s\n", what, strerror(errno));
		return (-1);
	}
	return (0);
}

static uint64_t
mov48(int r, uint64_t imm)
{
	return (1ULL << 56 | (uint64_t)r << 48 | (imm & 0xffffffffffffULL));
}

static int
bo(uint32_t vm, uint64_t va, uint32_t *h, volatile uint64_t **cpu)
{
	struct drm_panthor_bo_create c = { .size = 4096 };
	struct drm_panthor_bo_mmap_offset m;
	struct drm_panthor_vm_bind_op op;
	struct drm_panthor_vm_bind b;

	if (x(DRM_IOCTL_PANTHOR_BO_CREATE, &c, "BO_CREATE"))
		return (-1);
	memset(&m, 0, sizeof(m));
	m.handle = c.handle;
	if (x(DRM_IOCTL_PANTHOR_BO_MMAP_OFFSET, &m, "BO_MMAP_OFFSET"))
		return (-1);
	void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset);
	if (p == MAP_FAILED) {
		printf("pt_job: mmap: %s\n", strerror(errno));
		return (-1);
	}
	memset(&op, 0, sizeof(op));
	op.flags = DRM_PANTHOR_VM_BIND_OP_TYPE_MAP | DRM_PANTHOR_VM_BIND_OP_MAP_UNCACHED;
	op.bo_handle = c.handle;
	op.va = va;
	op.size = 4096;
	memset(&b, 0, sizeof(b));
	b.vm_id = vm;
	b.ops = (struct drm_panthor_obj_array)DRM_PANTHOR_OBJ_ARRAY(1, &op);
	if (x(DRM_IOCTL_PANTHOR_VM_BIND, &b, "VM_BIND"))
		return (-1);
	*h = c.handle;
	*cpu = p;
	return (0);
}

static int
submit(uint32_t group, uint64_t va, uint32_t size, uint32_t syncobj)
{
	struct drm_panthor_sync_op sop = { .flags = DRM_PANTHOR_SYNC_OP_SIGNAL |
	    DRM_PANTHOR_SYNC_OP_HANDLE_TYPE_SYNCOBJ, .handle = syncobj };
	struct drm_panthor_queue_submit qs;
	struct drm_panthor_group_submit gs;

	memset(&qs, 0, sizeof(qs));
	qs.stream_addr = va;
	qs.stream_size = size;
	qs.syncs = (struct drm_panthor_obj_array)DRM_PANTHOR_OBJ_ARRAY(1, &sop);
	memset(&gs, 0, sizeof(gs));
	gs.group_handle = group;
	gs.queue_submits = (struct drm_panthor_obj_array)DRM_PANTHOR_OBJ_ARRAY(1, &qs);
	return (x(DRM_IOCTL_PANTHOR_GROUP_SUBMIT, &gs, "GROUP_SUBMIT"));
}

static int
wait1(uint32_t syncobj, int64_t ns)
{
	struct drm_syncobj_wait w;
	struct timespec now;

	/* The deadline is absolute, on the monotonic clock. */
	clock_gettime(CLOCK_MONOTONIC, &now);
	memset(&w, 0, sizeof(w));
	w.handles = (uintptr_t)&syncobj;
	w.count_handles = 1;
	w.timeout_nsec = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + ns;
	return (ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &w) == 0 ? 0 : -errno);
}

int
main(int argc, char **argv)
{
	int rounds = argc > 1 ? atoi(argv[1]) : 3;
	struct drm_panthor_gpu_info gi;
	struct drm_panthor_dev_query q;

	fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
	if (fd < 0)
		return (printf("pt_job: open: %s\n", strerror(errno)), 1);
	memset(&gi, 0, sizeof(gi));
	q = (struct drm_panthor_dev_query){ .type = DRM_PANTHOR_DEV_QUERY_GPU_INFO,
	    .size = sizeof(gi), .pointer = (uintptr_t)&gi };
	if (x(DRM_IOCTL_PANTHOR_DEV_QUERY, &q, "GPU_INFO"))
		return (1);
	printf("pt_job: gpu_id %#x shader_present %#llx\n", gi.gpu_id,
	    (unsigned long long)gi.shader_present);

	struct drm_panthor_vm_create vc = { 0 };
	if (x(DRM_IOCTL_PANTHOR_VM_CREATE, &vc, "VM_CREATE"))
		return (1);
	printf("pt_job: VM %u, user range %#llx\n", vc.id,
	    (unsigned long long)vc.user_va_range);
	uint32_t sh, th;
	volatile uint64_t *stream, *target;
	if (bo(vc.id, STREAM_VA, &sh, &stream) || bo(vc.id, TARGET_VA, &th, &target))
		return (1);

	int failures = 0;
	for (int r = 0; r < rounds; r++) {
		struct drm_syncobj_create sc = { 0 };
		if (x(DRM_IOCTL_SYNCOBJ_CREATE, &sc, "SYNCOBJ_CREATE"))
			return (1);
		struct drm_panthor_queue_create qc = { .priority = 1, .ringbuf_size = 4096 };
		struct drm_panthor_group_create gc;
		memset(&gc, 0, sizeof(gc));
		gc.queues = (struct drm_panthor_obj_array)DRM_PANTHOR_OBJ_ARRAY(1, &qc);
		gc.max_compute_cores = gc.max_fragment_cores =
		    __builtin_popcountll(gi.shader_present);
		gc.max_tiler_cores = 1;
		gc.priority = PANTHOR_GROUP_PRIORITY_MEDIUM;
		gc.compute_core_mask = gc.fragment_core_mask = gi.shader_present;
		gc.tiler_core_mask = gi.tiler_present;
		gc.vm_id = vc.id;
		if (x(DRM_IOCTL_PANTHOR_GROUP_CREATE, &gc, "GROUP_CREATE"))
			return (1);

		/* The job: [TARGET] += 5. */
		target[0] = 0;
		stream[0] = mov48(0, TARGET_VA);
		stream[1] = mov48(2, 5);
		stream[2] = 0x33ULL << 56 | 0ULL << 40 | 2ULL << 32 | 1;
		__sync_synchronize();
		if (submit(gc.group_handle, STREAM_VA, 24, sc.handle))
			return (1);
		int w = wait1(sc.handle, 2000000000LL);
		printf("pt_job: round %d: job %s, target reads %llu\n", r,
		    w == 0 ? "signalled" : strerror(-w),
		    (unsigned long long)target[0]);
		if (w != 0 || target[0] != 5)
			failures++;

		/* A stream nothing maps: the group faults. */
		struct drm_syncobj_create sc2 = { 0 };
		x(DRM_IOCTL_SYNCOBJ_CREATE, &sc2, "SYNCOBJ_CREATE");
		if (argc > 2 && strcmp(argv[2], "fault") == 0 &&
		    submit(gc.group_handle, 0x7000000ULL, 8, sc2.handle) == 0) {
			int w2 = wait1(sc2.handle, 1000000000LL);
			struct drm_panthor_group_get_state st = { .group_handle = gc.group_handle };
			x(DRM_IOCTL_PANTHOR_GROUP_GET_STATE, &st, "GROUP_GET_STATE");
			printf("pt_job: round %d: bad stream %s, group state %#x fatal %#x\n",
			    r, w2 == 0 ? "signalled" : strerror(-w2), st.state,
			    st.fatal_queues);
			if (st.state == 0)
				failures++;
		}
		struct drm_panthor_group_destroy gd = { .group_handle = gc.group_handle };
		if (x(DRM_IOCTL_PANTHOR_GROUP_DESTROY, &gd, "GROUP_DESTROY"))
			failures++;
	}
	close(fd);
	printf("pt_job: %s (%d failures in %d rounds)\n", failures ? "FAIL" : "PASS",
	    failures, rounds);
	return (failures != 0);
}
