/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * kbench: the FreeBSD side of AbyssBSD's in-kernel benchmarks.
 *
 * Test code. It runs only on FreeBSD, as the baseline that AbyssBSD's
 * kernel/src/bench.rs is compared against. Both follow the workload
 * specification in tests/perf/README.md; change them together.
 *
 *	kldload ./kbench.ko && sysctl debug.kbench.run=p2
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/callout.h>
#include <sys/kernel.h>
#include <sys/kthread.h>
#include <sys/lock.h>
#include <sys/malloc.h>
#include <sys/module.h>
#include <sys/mutex.h>
#include <sys/priority.h>
#include <sys/proc.h>
#include <sys/rwlock.h>
#include <sys/sched.h>
#include <sys/smp.h>
#include <sys/sx.h>
#include <sys/sysctl.h>

#include <vm/vm.h>
#include <vm/pmap.h>
#include <vm/vm_extern.h>
#include <vm/vm_page.h>

#include <machine/atomic.h>
#include <machine/clock.h>
#include <machine/cpufunc.h>

MALLOC_DEFINE(M_KBENCH, "kbench", "kbench benchmark buffers");

#define	KB_MAXT		8
#define	KB_ROUNDS	3	/* rounds per throughput benchmark; median */
#define	KB_NCPU		8
#define	KB_LAT_CALLOUT	1000
#define	KB_LAT_WAKEUP	10000
#define	KB_MISS_OBJS	4096
#define	KB_MISS_PAGES	1024

static uint64_t
tsc_ns(uint64_t t)
{
	/* No 128-bit division in the kernel; this cannot overflow. */
	return (t / tsc_freq * 1000000000 +
	    t % tsc_freq * 1000000000 / tsc_freq);
}

/* One benchmark run: workers, start/stop flags, and per-worker results. */
struct kb_run;
typedef void kb_body_t(struct kb_run *, int);

struct kb_slot {
	uint64_t	ops;
	char		pad[CACHE_LINE_SIZE - sizeof(uint64_t)];
} __aligned(CACHE_LINE_SIZE);

struct kb_run {
	kb_body_t	*body;
	int		nthreads;
	int		cpus[KB_MAXT];
	volatile int	ready;
	volatile int	go;
	volatile int	stop;
	volatile int	done;
	struct kb_slot	slot[KB_MAXT];
};

struct kb_arg {
	struct kb_run	*run;
	int		idx;
};

static struct kb_arg kb_args[KB_MAXT];
static struct mtx kb_go_mtx;

static void
kb_bind(int cpu)
{
	thread_lock(curthread);
	sched_prio(curthread, PRI_MIN_KERN);
	sched_bind(curthread, cpu);
	thread_unlock(curthread);
}

static void
kb_worker(void *arg)
{
	struct kb_arg *a = arg;
	struct kb_run *r = a->run;

	kb_bind(r->cpus[a->idx]);
	/*
	 * Sleep until the start: kernel-priority threads are not time-sliced,
	 * so a spinning worker would keep another bound to its CPU from ever
	 * checking in.
	 */
	mtx_lock(&kb_go_mtx);
	r->ready++;
	while (r->go == 0)
		mtx_sleep(__DEVOLATILE(void *, &r->go), &kb_go_mtx, 0, "kbgo", 0);
	mtx_unlock(&kb_go_mtx);
	r->body(r, a->idx);
	atomic_add_rel_int(&r->done, 1);
	kthread_exit();
}

/* Default placement: worker i on CPU (i + 1) mod 8. */
static void
kb_default_cpus(struct kb_run *r)
{
	for (int i = 0; i < r->nthreads; i++)
		r->cpus[i] = (i + 1) % KB_NCPU;
}

/*
 * Starts the workers, lets them run for `ms` milliseconds (0: until they
 * finish by themselves), and returns the elapsed TSC ticks between go and
 * stop.
 */
static uint64_t
kb_launch(struct kb_run *r, int ms)
{
	struct thread *td;
	uint64_t t0, t1;

	r->ready = r->go = r->stop = r->done = 0;
	for (int i = 0; i < r->nthreads; i++) {
		r->slot[i].ops = 0;
		kb_args[i].run = r;
		kb_args[i].idx = i;
		if (kthread_add(kb_worker, &kb_args[i], NULL, &td, 0, 0,
		    "kbench%d", i) != 0)
			panic("kbench: kthread_add");
	}
	for (;;) {
		mtx_lock(&kb_go_mtx);
		if (r->ready == r->nthreads)
			break;
		mtx_unlock(&kb_go_mtx);
		pause("kbrdy", 1);
	}
	t0 = rdtsc();
	r->go = 1;
	wakeup(__DEVOLATILE(void *, &r->go));
	mtx_unlock(&kb_go_mtx);
	if (ms > 0) {
		pause("kbrun", ms * hz / 1000);
		atomic_store_rel_int(&r->stop, 1);
		t1 = rdtsc();
	}
	while (atomic_load_acq_int(&r->done) < r->nthreads)
		pause("kbdone", 1);
	if (ms == 0)
		t1 = rdtsc();
	return (t1 - t0);
}

/* One round's result: thousandths of a nanosecond per operation. */
static uint64_t
kb_milli_per_op(struct kb_run *r, uint64_t ticks)
{
	uint64_t ops = 0;

	for (int i = 0; i < r->nthreads; i++)
		ops += r->slot[i].ops;
	if (ops == 0)
		ops = 1;
	return (tsc_ns(ticks) * 1000 / ops);
}

static int kb_cmp(const void *, const void *);

/* Runs KB_ROUNDS rounds of `r` and reports the median. */
static void
kb_report_median(const char *name, struct kb_run *r)
{
	uint64_t m[KB_ROUNDS], milli;

	for (int i = 0; i < KB_ROUNDS; i++)
		m[i] = kb_milli_per_op(r, kb_launch(r, 1000));
	qsort(m, KB_ROUNDS, sizeof(m[0]), kb_cmp);
	milli = m[KB_ROUNDS / 2];
	printf("kbench %s %ju.%03ju ns/op\n", name, (uintmax_t)(milli / 1000),
	    (uintmax_t)(milli % 1000));
}

static int
kb_cmp(const void *a, const void *b)
{
	uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;

	return (x < y ? -1 : x > y);
}

static void
kb_report_lat(const char *name, uint64_t *ns, int n)
{
	qsort(ns, n, sizeof(*ns), kb_cmp);
	printf("kbench %s.p50 %ju ns\n", name, (uintmax_t)ns[n / 2]);
	printf("kbench %s.p99 %ju ns\n", name, (uintmax_t)ns[n * 99 / 100]);
}

static int
kb_stopped(struct kb_run *r)
{
	return (atomic_load_acq_int(&r->stop));
}

/* Throughput benchmarks over T workers in the default placement. */
static void
kb_throughput(const char *name, kb_body_t *body, int nthreads)
{
	static struct kb_run r;

	r.body = body;
	r.nthreads = nthreads;
	kb_default_cpus(&r);
	kb_report_median(name, &r);
}

/* Locks. */

/*
 * Lock benchmarks run on KB_PLACEMENTS lock sets, each in its own page,
 * and report the mean: a contended lock's cost depends on where its cache
 * line lives (its L3 slice), by up to 2x on the build server.
 */
#define	KB_PLACEMENTS	8
#define	KB_PLACEMENT_MS	250

struct kb_lockset {
	struct mtx	m;
	uint64_t	mc;
	struct rwlock	rw;
	uint64_t	rwc;
	struct sx	sx;
	uint64_t	sxc;
};

static struct kb_lockset *kb_sets[KB_PLACEMENTS];
static struct kb_lockset *kb_ls;	/* the set in use */
static struct mtx kb_mtx;		/* for ctxsw and wakeup */

static void
kb_mtx_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		mtx_lock(&kb_ls->m);
		kb_ls->mc++;
		mtx_unlock(&kb_ls->m);
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_rw_rd_body(struct kb_run *r, int idx)
{
	uint64_t n = 0, sink = 0;

	while (!kb_stopped(r)) {
		rw_rlock(&kb_ls->rw);
		sink += *(volatile uint64_t *)&kb_ls->rwc;
		rw_runlock(&kb_ls->rw);
		n++;
	}
	r->slot[idx].ops = n + (sink & 0);
}

static void
kb_rw_wr_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		rw_wlock(&kb_ls->rw);
		kb_ls->rwc++;
		rw_wunlock(&kb_ls->rw);
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_sx_rd_body(struct kb_run *r, int idx)
{
	uint64_t n = 0, sink = 0;

	while (!kb_stopped(r)) {
		sx_slock(&kb_ls->sx);
		sink += *(volatile uint64_t *)&kb_ls->sxc;
		sx_sunlock(&kb_ls->sx);
		n++;
	}
	r->slot[idx].ops = n + (sink & 0);
}

static void
kb_sx_wr_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		sx_xlock(&kb_ls->sx);
		kb_ls->sxc++;
		sx_xunlock(&kb_ls->sx);
		n++;
	}
	r->slot[idx].ops = n;
}

/* A lock benchmark: the mean over the lock placements. */
static void
kb_lock_throughput(const char *name, kb_body_t *body, int nthreads)
{
	static struct kb_run r;
	uint64_t sum = 0, milli;

	r.body = body;
	r.nthreads = nthreads;
	kb_default_cpus(&r);
	for (int i = 0; i < KB_PLACEMENTS; i++) {
		kb_ls = kb_sets[i];
		sum += kb_milli_per_op(&r, kb_launch(&r, KB_PLACEMENT_MS));
	}
	milli = sum / KB_PLACEMENTS;
	printf("kbench %s %ju.%03ju ns/op\n", name, (uintmax_t)(milli / 1000),
	    (uintmax_t)(milli % 1000));
}

/* Allocators. */

static void
kb_malloc_hit_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		free(malloc(256, M_KBENCH, M_WAITOK), M_KBENCH);
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_malloc_miss_body(struct kb_run *r, int idx)
{
	void **p;
	uint64_t n = 0;

	p = malloc(KB_MISS_OBJS * sizeof(*p), M_KBENCH, M_WAITOK);
	while (!kb_stopped(r)) {
		for (int i = 0; i < KB_MISS_OBJS; i++)
			p[i] = malloc(256, M_KBENCH, M_WAITOK);
		for (int i = 0; i < KB_MISS_OBJS; i++)
			free(p[i], M_KBENCH);
		n += KB_MISS_OBJS;
	}
	free(p, M_KBENCH);
	r->slot[idx].ops = n;
}

static void
kb_page_hit_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		vm_page_free(vm_page_alloc_noobj(VM_ALLOC_WAITOK));
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_page_miss_body(struct kb_run *r, int idx)
{
	vm_page_t *m;
	uint64_t n = 0;

	m = malloc(KB_MISS_PAGES * sizeof(*m), M_KBENCH, M_WAITOK);
	while (!kb_stopped(r)) {
		for (int i = 0; i < KB_MISS_PAGES; i++)
			m[i] = vm_page_alloc_noobj(VM_ALLOC_WAITOK);
		for (int i = 0; i < KB_MISS_PAGES; i++)
			vm_page_free(m[i]);
		n += KB_MISS_PAGES;
	}
	free(m, M_KBENCH);
	r->slot[idx].ops = n;
}

/* Context switches: strict alternation between two workers. */

static volatile int kb_turn;

static void
kb_ctxsw_body(struct kb_run *r, int idx)
{
	int other = 1 - idx;
	uint64_t n = 0;

	for (;;) {
		mtx_lock(&kb_mtx);
		while (kb_turn != idx && !kb_stopped(r))
			mtx_sleep(__DEVOLATILE(void *, &kb_turn), &kb_mtx, 0,
			    "kbcs", 0);
		kb_turn = other;
		if (kb_stopped(r)) {
			wakeup(__DEVOLATILE(void *, &kb_turn));
			mtx_unlock(&kb_mtx);
			break;
		}
		n++;
		wakeup_one(__DEVOLATILE(void *, &kb_turn));
		mtx_unlock(&kb_mtx);
	}
	r->slot[idx].ops = n;
}

static void
kb_ctxsw(const char *name, int cpu0, int cpu1)
{
	static struct kb_run r;

	kb_turn = 0;
	r.body = kb_ctxsw_body;
	r.nthreads = 2;
	r.cpus[0] = cpu0;
	r.cpus[1] = cpu1;
	kb_report_median(name, &r);
}

/* Callouts. */

static struct callout kb_callout;
static struct mtx kb_spin;
static volatile int kb_fired;
static volatile uint64_t kb_fire_tsc;

static void
kb_nop_callout(void *arg __unused)
{
}

static void
kb_armcancel_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		callout_reset_sbt(&kb_callout, SBT_1S, 0, kb_nop_callout, NULL,
		    0);
		callout_stop(&kb_callout);
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_fire(void *arg __unused)
{
	kb_fire_tsc = rdtsc();
	mtx_lock_spin(&kb_spin);
	kb_fired = 1;
	wakeup(__DEVOLATILE(void *, &kb_fired));
	mtx_unlock_spin(&kb_spin);
}

static uint64_t *kb_samples;

static void
kb_callout_lat_body(struct kb_run *r __unused, int idx __unused)
{
	uint64_t one_ms = tsc_freq / 1000;

	for (int i = 0; i < KB_LAT_CALLOUT; i++) {
		uint64_t t0;
		sbintime_t s0;

		mtx_lock_spin(&kb_spin);
		kb_fired = 0;
		t0 = rdtsc();
		s0 = sbinuptime();
		callout_reset_sbt_on(&kb_callout, s0 + SBT_1MS, 0, kb_fire,
		    NULL, curcpu, C_ABSOLUTE | C_DIRECT_EXEC);
		while (!kb_fired)
			msleep_spin(__DEVOLATILE(void *, &kb_fired), &kb_spin,
			    "kbco", 0);
		mtx_unlock_spin(&kb_spin);
		kb_samples[i] = tsc_ns(kb_fire_tsc - t0 > one_ms ?
		    kb_fire_tsc - t0 - one_ms : 0);
	}
}

/* Wakeup latency. */

static volatile int kb_asleep;
static volatile uint64_t kb_seq;
static volatile uint64_t kb_wake_tsc;

static void
kb_wakeup_body(struct kb_run *r __unused, int idx)
{
	if (idx == 0) {
		/* Sleeper. */
		for (int i = 0; i < KB_LAT_WAKEUP; i++) {
			uint64_t t1;

			mtx_lock(&kb_mtx);
			kb_asleep = i + 1;
			while (kb_seq == (uint64_t)i)
				mtx_sleep(__DEVOLATILE(void *, &kb_seq),
				    &kb_mtx, 0, "kbwl", 0);
			t1 = rdtsc();
			mtx_unlock(&kb_mtx);
			kb_samples[i] = tsc_ns(t1 - kb_wake_tsc);
		}
	} else {
		/* Waker. */
		uint64_t spin = tsc_freq / 20000;	/* 50 us */

		for (int i = 0; i < KB_LAT_WAKEUP; i++) {
			uint64_t t;

			/* The sleeper announces each iteration it sleeps for. */
			while (atomic_load_acq_int(&kb_asleep) != i + 1)
				cpu_spinwait();
			t = rdtsc();
			while (rdtsc() - t < spin)
				cpu_spinwait();
			mtx_lock(&kb_mtx);
			kb_wake_tsc = rdtsc();
			kb_seq = i + 1;
			wakeup(__DEVOLATILE(void *, &kb_seq));
			mtx_unlock(&kb_mtx);
		}
	}
}

/* IPIs and TLB shootdowns. */

static void
kb_ipi_nop(void *arg __unused)
{
}

static void
kb_ipi_body(struct kb_run *r, int idx)
{
	cpuset_t map;
	uint64_t n = 0;

	CPU_SETOF(2, &map);
	while (!kb_stopped(r)) {
		smp_rendezvous_cpus(map, smp_no_rendezvous_barrier, kb_ipi_nop,
		    smp_no_rendezvous_barrier, NULL);
		n++;
	}
	r->slot[idx].ops = n;
}

static vm_offset_t kb_va;

static void
kb_tlb_body(struct kb_run *r, int idx)
{
	uint64_t n = 0;

	while (!kb_stopped(r)) {
		pmap_invalidate_page(kernel_pmap, kb_va);
		n++;
	}
	r->slot[idx].ops = n;
}

static void
kb_suite_p2(void)
{
	static const int threads[] = { 1, 2, 4, 8 };
	static struct kb_run r;
	char name[32];
	vm_page_t m;

	kb_ctxsw("ctxsw.same", 1, 1);
	kb_ctxsw("ctxsw.cross", 1, 2);

	for (int i = 0; i < nitems(threads); i++) {
		int t = threads[i];

		snprintf(name, sizeof(name), "mtx.t%d", t);
		kb_lock_throughput(name, kb_mtx_body, t);
		snprintf(name, sizeof(name), "rw.rd.t%d", t);
		kb_lock_throughput(name, kb_rw_rd_body, t);
		snprintf(name, sizeof(name), "rw.wr.t%d", t);
		kb_lock_throughput(name, kb_rw_wr_body, t);
		snprintf(name, sizeof(name), "sx.rd.t%d", t);
		kb_lock_throughput(name, kb_sx_rd_body, t);
		snprintf(name, sizeof(name), "sx.wr.t%d", t);
		kb_lock_throughput(name, kb_sx_wr_body, t);
	}

	kb_throughput("malloc.hit.t1", kb_malloc_hit_body, 1);
	kb_throughput("malloc.hit.t8", kb_malloc_hit_body, 8);
	kb_throughput("malloc.miss.t1", kb_malloc_miss_body, 1);
	kb_throughput("malloc.miss.t8", kb_malloc_miss_body, 8);
	kb_throughput("page.hit.t1", kb_page_hit_body, 1);
	kb_throughput("page.hit.t8", kb_page_hit_body, 8);
	kb_throughput("page.miss.t1", kb_page_miss_body, 1);

	kb_throughput("callout.armcancel", kb_armcancel_body, 1);

	kb_samples = malloc(KB_LAT_WAKEUP * sizeof(*kb_samples), M_KBENCH,
	    M_WAITOK | M_ZERO);
	r.body = kb_callout_lat_body;
	r.nthreads = 1;
	kb_default_cpus(&r);
	kb_launch(&r, 0);
	kb_report_lat("callout.latency", kb_samples, KB_LAT_CALLOUT);

	kb_asleep = 0;
	kb_seq = 0;
	r.body = kb_wakeup_body;
	r.nthreads = 2;
	r.cpus[0] = 1;
	r.cpus[1] = 2;
	kb_launch(&r, 0);
	kb_report_lat("wakeup.latency", kb_samples, KB_LAT_WAKEUP);
	free(kb_samples, M_KBENCH);

	kb_throughput("ipi.rtt", kb_ipi_body, 1);

	m = vm_page_alloc_noobj(VM_ALLOC_WAITOK | VM_ALLOC_WIRED);
	kb_va = kva_alloc(PAGE_SIZE);
	pmap_qenter(kb_va, &m, 1);
	kb_throughput("tlb.shootdown", kb_tlb_body, 1);
	pmap_qremove(kb_va, 1);
	kva_free(kb_va, PAGE_SIZE);
	vm_page_unwire_noq(m);
	vm_page_free(m);
}

/* Diagnostics: IPI round trips to an idle and to a busy CPU 2. */
static volatile int kb_spinning;

static void
kb_spinner(void *arg __unused)
{
	kb_bind(2);
	while (atomic_load_acq_int((volatile u_int *)&kb_spinning))
		cpu_spinwait();
	kthread_exit();
}

static void
kb_suite_diag(void)
{
	struct thread *td;

	kb_throughput("diag.ping_idle", kb_ipi_body, 1);
	kb_spinning = 1;
	if (kthread_add(kb_spinner, NULL, NULL, &td, 0, 0, "kbspin") != 0)
		panic("kbench: kthread_add");
	pause("kbsp", hz / 100);
	kb_throughput("diag.ping_busy", kb_ipi_body, 1);
	atomic_store_rel_int((volatile u_int *)&kb_spinning, 0);
	pause("kbsp", hz / 100);
}

static int
kb_sysctl_run(SYSCTL_HANDLER_ARGS)
{
	char suite[16] = "";
	int error;

	error = sysctl_handle_string(oidp, suite, sizeof(suite), req);
	if (error != 0 || req->newptr == NULL)
		return (error);
	if (strcmp(suite, "p2") != 0 && strcmp(suite, "diag") != 0)
		return (EINVAL);
	if (mp_ncpus < KB_NCPU) {
		printf("kbench: needs %d CPUs, have %d\n", KB_NCPU, mp_ncpus);
		return (ENXIO);
	}
	kb_bind(0);
	/* A fresh line, in case other console output is mid-line. */
	printf("\nkbench begin %s cpus=%d\n", suite, mp_ncpus);
	if (strcmp(suite, "p2") == 0)
		kb_suite_p2();
	else
		kb_suite_diag();
	printf("kbench end %s\n", suite);
	thread_lock(curthread);
	sched_unbind(curthread);
	thread_unlock(curthread);
	return (0);
}

static SYSCTL_NODE(_debug, OID_AUTO, kbench, CTLFLAG_RW | CTLFLAG_MPSAFE, 0,
    "In-kernel benchmarks");
SYSCTL_PROC(_debug_kbench, OID_AUTO, run,
    CTLTYPE_STRING | CTLFLAG_RW | CTLFLAG_MPSAFE, NULL, 0, kb_sysctl_run, "A",
    "Run a benchmark suite (p2)");

static int
kb_modevent(module_t mod __unused, int type, void *data __unused)
{
	switch (type) {
	case MOD_LOAD:
		mtx_init(&kb_mtx, "kbench mtx", NULL, MTX_DEF);
		mtx_init(&kb_go_mtx, "kbench go", NULL, MTX_DEF);
		mtx_init(&kb_spin, "kbench spin", NULL, MTX_SPIN);
		for (int i = 0; i < KB_PLACEMENTS; i++) {
			kb_sets[i] = malloc(PAGE_SIZE, M_KBENCH,
			    M_WAITOK | M_ZERO);
			mtx_init(&kb_sets[i]->m, "kbench mtx", NULL, MTX_DEF);
			rw_init(&kb_sets[i]->rw, "kbench rw");
			sx_init(&kb_sets[i]->sx, "kbench sx");
		}
		callout_init(&kb_callout, 1);
		return (0);
	case MOD_UNLOAD:
		callout_drain(&kb_callout);
		for (int i = 0; i < KB_PLACEMENTS; i++) {
			sx_destroy(&kb_sets[i]->sx);
			rw_destroy(&kb_sets[i]->rw);
			mtx_destroy(&kb_sets[i]->m);
			free(kb_sets[i], M_KBENCH);
		}
		mtx_destroy(&kb_spin);
		mtx_destroy(&kb_go_mtx);
		mtx_destroy(&kb_mtx);
		return (0);
	default:
		return (EOPNOTSUPP);
	}
}

static moduledata_t kb_mod = { "kbench", kb_modevent, NULL };
DECLARE_MODULE(kbench, kb_mod, SI_SUB_DRIVERS, SI_ORDER_ANY);
MODULE_VERSION(kbench, 1);
