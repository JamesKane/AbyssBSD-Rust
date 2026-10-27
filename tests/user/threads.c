/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * threads: checks threads from user mode, through libthr: creation and
 * join, thread IDs and names, TLS, signals sent to a thread and to the
 * process, per-thread masks, alternate stacks, and sigwait,
 * thr_suspend/thr_wake, amd64's GS base (each thread's own, a new thread
 * starting with its creator's), and what a threaded process does as a whole: one
 * thread's exit ends all of them, exec leaves one thread, fork copies
 * only the caller, a stop stops every thread, and the process lives on
 * after its first thread leaves with pthread_exit.
 *
 * "threads stress N" runs N rounds of single-threading under load
 * instead: a child whose threads sleep, fault in fresh pages, make system
 * calls, and block in read, while another of its threads execs or exits.
 *
 * Nothing here contends a pthread mutex: libthr's contended mutexes need
 * the umtx operations of ROADMAP P6.3.
 *
 * Links with: -lthr
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/mman.h>
#include <sys/sysctl.h>
#include <sys/thr.h>
#include <sys/user.h>
#include <sys/wait.h>

#ifdef __amd64__
#include <machine/sysarch.h>
#endif

#include <errno.h>
#include <pthread.h>
#include <pthread_np.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int failures;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("threads: " __VA_ARGS__);			\
		printf("\n");						\
		failures++;						\
	}								\
} while (0)

static __thread int tls_value;

/* Creation, join, distinct tids above every pid, and TLS. */

#define	NWORKERS 8

struct worker {
	pthread_t thr;
	int index;
	long tid;
	int tls_seen;
};

static void *
worker_main(void *arg)
{
	struct worker *w = arg;

	tls_value = w->index * 10;
	thr_self(&w->tid);
	/* Give the others a chance to overwrite ours, if TLS were shared. */
	for (int i = 0; i < 1000; i++)
		sched_yield();
	w->tls_seen = tls_value;
	return ((void *)(long)(w->index + 100));
}

static void
check_create_join(void)
{
	struct worker w[NWORKERS];
	long main_tid;

	thr_self(&main_tid);
	tls_value = -1;
	for (int i = 0; i < NWORKERS; i++) {
		w[i].index = i;
		CHECK(pthread_create(&w[i].thr, NULL, worker_main, &w[i]) == 0,
		    "pthread_create %d failed", i);
	}
	for (int i = 0; i < NWORKERS; i++) {
		void *rv;

		CHECK(pthread_join(w[i].thr, &rv) == 0, "join %d failed", i);
		CHECK(rv == (void *)(long)(i + 100), "join %d returned %p", i,
		    rv);
		CHECK(w[i].tls_seen == i * 10, "thread %d saw TLS %d", i,
		    w[i].tls_seen);
		CHECK(w[i].tid > 99999, "thread %d has tid %ld", i, w[i].tid);
		CHECK(w[i].tid != main_tid, "thread %d has the main tid", i);
		for (int j = 0; j < i; j++)
			CHECK(w[i].tid != w[j].tid, "threads %d, %d share %ld",
			    i, j, w[i].tid);
	}
	CHECK(tls_value == -1, "main's TLS became %d", tls_value);
	CHECK(main_tid == pthread_getthreadid_np(),
	    "thr_self %ld, pthread_getthreadid_np %d", main_tid,
	    pthread_getthreadid_np());
}

/* Signals to a thread, and to the process. */

static _Atomic(long) handled_by;
static _Atomic(int) handled;

static void
record(int sig)
{
	long tid;

	(void)sig;
	thr_self(&tid);
	atomic_store(&handled_by, tid);
	atomic_fetch_add(&handled, 1);
}

struct waiter {
	long tid;
	sigset_t block;
	_Atomic(int) ready;
	_Atomic(int) stop;
};

/* Waits with the mask it is given until told to stop. */
static void *
waiter_main(void *arg)
{
	struct waiter *w = arg;

	pthread_sigmask(SIG_SETMASK, &w->block, NULL);
	thr_self(&w->tid);
	atomic_store(&w->ready, 1);
	while (!atomic_load(&w->stop))
		usleep(1000);
	return (NULL);
}

static void
start_waiter(struct waiter *w, pthread_t *t, const sigset_t *block)
{
	w->block = *block;
	atomic_store(&w->ready, 0);
	atomic_store(&w->stop, 0);
	pthread_create(t, NULL, waiter_main, w);
	while (!atomic_load(&w->ready))
		sched_yield();
}

static void
wait_handled(int n)
{
	for (int i = 0; i < 5000 && atomic_load(&handled) < n; i++)
		usleep(1000);
}

static void
check_signals(void)
{
	struct sigaction sa;
	struct waiter w;
	pthread_t t;
	sigset_t none, usr, old;
	long me;

	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = record;
	sigaction(SIGUSR1, &sa, NULL);
	sigaction(SIGUSR2, &sa, NULL);
	sigemptyset(&none);
	sigemptyset(&usr);
	sigaddset(&usr, SIGUSR1);
	sigaddset(&usr, SIGUSR2);
	thr_self(&me);

	/* pthread_kill reaches that thread. */
	start_waiter(&w, &t, &none);
	atomic_store(&handled, 0);
	CHECK(pthread_kill(t, SIGUSR1) == 0, "pthread_kill failed");
	wait_handled(1);
	CHECK(atomic_load(&handled) == 1 && atomic_load(&handled_by) == w.tid,
	    "pthread_kill: handled %d times by %ld, not by %ld",
	    atomic_load(&handled), atomic_load(&handled_by), w.tid);

	/*
	 * A signal to the process goes to a thread that does not block it:
	 * here the waiter, with the main thread blocking it.
	 */
	pthread_sigmask(SIG_BLOCK, &usr, &old);
	atomic_store(&handled, 0);
	kill(getpid(), SIGUSR2);
	wait_handled(1);
	CHECK(atomic_load(&handled) == 1 && atomic_load(&handled_by) == w.tid,
	    "kill: handled %d times by %ld, not by %ld",
	    atomic_load(&handled), atomic_load(&handled_by), w.tid);
	atomic_store(&w.stop, 1);
	pthread_join(t, NULL);

	/*
	 * Blocked everywhere, it stays pending for the process until a
	 * thread unblocks it.
	 */
	start_waiter(&w, &t, &usr);
	atomic_store(&handled, 0);
	kill(getpid(), SIGUSR2);
	usleep(20000);
	CHECK(atomic_load(&handled) == 0, "a blocked signal was handled");
	sigset_t pend;
	sigpending(&pend);
	CHECK(sigismember(&pend, SIGUSR2), "SIGUSR2 is not pending");
	pthread_sigmask(SIG_SETMASK, &old, NULL);
	wait_handled(1);
	CHECK(atomic_load(&handled) == 1 && atomic_load(&handled_by) == me,
	    "unblocked: handled %d times by %ld, not by %ld (main)",
	    atomic_load(&handled), atomic_load(&handled_by), me);

	/* A thread's own mask is its own. */
	sigset_t mine;
	pthread_sigmask(SIG_SETMASK, NULL, &mine);
	CHECK(!sigismember(&mine, SIGUSR1),
	    "the waiter's mask became the main thread's");
	atomic_store(&w.stop, 1);
	pthread_join(t, NULL);

	/* pthread_kill of a thread that blocks it leaves it pending there. */
	start_waiter(&w, &t, &usr);
	atomic_store(&handled, 0);
	pthread_kill(t, SIGUSR1);
	usleep(20000);
	CHECK(atomic_load(&handled) == 0, "a thread's blocked signal ran");
	sigpending(&pend);
	CHECK(!sigismember(&pend, SIGUSR1),
	    "another thread's pending signal shows in the main thread's");
	atomic_store(&w.stop, 1);
	pthread_join(t, NULL);
	/* It went with the thread. */
	usleep(10000);
	CHECK(atomic_load(&handled) == 0, "a dead thread's signal ran");

	signal(SIGUSR1, SIG_DFL);
	signal(SIGUSR2, SIG_DFL);
}

/* sigwait in one thread takes a signal sent to the process. */

static sigset_t waitset;
static _Atomic(int) waited_sig;

static void *
sigwaiter_main(void *arg)
{
	int sig;

	(void)arg;
	if (sigwait(&waitset, &sig) == 0)
		atomic_store(&waited_sig, sig);
	return (NULL);
}

static void
check_sigwait(void)
{
	pthread_t t;
	sigset_t old;

	sigemptyset(&waitset);
	sigaddset(&waitset, SIGUSR1);
	pthread_sigmask(SIG_BLOCK, &waitset, &old);
	atomic_store(&waited_sig, 0);
	pthread_create(&t, NULL, sigwaiter_main, NULL);
	usleep(20000);
	kill(getpid(), SIGUSR1);
	pthread_join(t, NULL);
	CHECK(atomic_load(&waited_sig) == SIGUSR1, "sigwait got %d",
	    atomic_load(&waited_sig));
	pthread_sigmask(SIG_SETMASK, &old, NULL);
}

/* An alternate stack belongs to the thread that set it. */

static _Atomic(int) altstack_set;

static void *
altstack_main(void *arg)
{
	stack_t ss;

	ss.ss_sp = arg;
	ss.ss_size = SIGSTKSZ;
	ss.ss_flags = 0;
	atomic_store(&altstack_set, sigaltstack(&ss, NULL) == 0);
	return (NULL);
}

static void
check_altstack(void)
{
	static char stack[SIGSTKSZ];
	pthread_t t;
	stack_t ss;

	pthread_create(&t, NULL, altstack_main, stack);
	pthread_join(t, NULL);
	CHECK(atomic_load(&altstack_set), "a thread's sigaltstack failed");
	sigaltstack(NULL, &ss);
	CHECK(ss.ss_flags & SS_DISABLE,
	    "another thread's alternate stack is the main thread's");
}

/* thr_suspend and thr_wake. */

static _Atomic(long) sleeper_tid;
static _Atomic(int) sleeper_rv = -2;

static void *
sleeper_main(void *arg)
{
	long tid;

	(void)arg;
	thr_self(&tid);
	atomic_store(&sleeper_tid, tid);
	atomic_store(&sleeper_rv, thr_suspend(NULL) == 0 ? 0 : errno);
	return (NULL);
}

static void
check_suspend(void)
{
	struct timespec ts = { 0, 20000000 };
	pthread_t t;
	long me;
	int r;

	thr_self(&me);
	r = thr_suspend(&ts);
	CHECK(r == -1 && errno == ETIMEDOUT, "thr_suspend timed: %d, %s", r,
	    strerror(errno));
	ts.tv_nsec = 0;
	r = thr_suspend(&ts);
	CHECK(r == -1 && errno == ETIMEDOUT, "thr_suspend for 0: %d, %s", r,
	    strerror(errno));
	ts.tv_nsec = 1000000000;
	r = thr_suspend(&ts);
	CHECK(r == -1 && errno == EINVAL, "thr_suspend bad: %d, %s", r,
	    strerror(errno));

	/* A wake before the suspend is kept for it. */
	CHECK(thr_wake(me) == 0, "thr_wake of self failed");
	CHECK(thr_suspend(NULL) == 0, "thr_suspend after a wake: %s",
	    strerror(errno));

	pthread_create(&t, NULL, sleeper_main, NULL);
	while (atomic_load(&sleeper_tid) == 0)
		sched_yield();
	usleep(20000);
	CHECK(atomic_load(&sleeper_rv) == -2, "thr_suspend returned early");
	CHECK(thr_wake(atomic_load(&sleeper_tid)) == 0, "thr_wake failed");
	pthread_join(t, NULL);
	CHECK(atomic_load(&sleeper_rv) == 0, "woken thr_suspend: %s",
	    strerror(atomic_load(&sleeper_rv)));
	CHECK(thr_wake(99999) == -1 && errno == ESRCH,
	    "thr_wake of no thread: %s", strerror(errno));
}

#ifdef __amd64__
/* amd64's GS base: inherited by a new thread, then each thread's own. */
static void *
gs_main(void *arg)
{
	void **seen = arg;

	amd64_get_gsbase(&seen[0]);
	amd64_set_gsbase((void *)0x2000);
	for (int i = 0; i < 1000; i++)
		sched_yield();
	amd64_get_gsbase(&seen[1]);
	return (NULL);
}

static void
check_gsbase(void)
{
	void *seen[2] = { NULL, NULL }, *mine = NULL;
	pthread_t t;

	CHECK(amd64_set_gsbase((void *)0x1000) == 0, "amd64_set_gsbase: %s",
	    strerror(errno));
	pthread_create(&t, NULL, gs_main, seen);
	for (int i = 0; i < 1000; i++)
		sched_yield();
	pthread_join(t, NULL);
	amd64_get_gsbase(&mine);
	CHECK(seen[0] == (void *)0x1000, "a new thread's GS base is %p",
	    seen[0]);
	CHECK(seen[1] == (void *)0x2000, "a thread's GS base became %p",
	    seen[1]);
	CHECK(mine == (void *)0x1000, "the creator's GS base became %p", mine);
	amd64_set_gsbase(NULL);
}
#endif

/* Names, as kern.proc shows them per thread. */

static _Atomic(int) name_stop;

static void *
idle_main(void *arg)
{
	(void)arg;
	while (!atomic_load(&name_stop))
		usleep(1000);
	return (NULL);
}

static void
check_names(void)
{
	struct kinfo_proc *kp;
	pthread_t t;
	size_t len;
	int mib[4] = { CTL_KERN, KERN_PROC,
	    KERN_PROC_PID | KERN_PROC_INC_THREAD, getpid() };
	int n, found = 0;

	atomic_store(&name_stop, 0);
	pthread_create(&t, NULL, idle_main, NULL);
	pthread_set_name_np(t, "abyss-worker");
	len = 0;
	CHECK(sysctl(mib, 4, NULL, &len, NULL, 0) == 0, "kern.proc size: %s",
	    strerror(errno));
	len += 4 * sizeof(*kp);
	kp = malloc(len);
	CHECK(sysctl(mib, 4, kp, &len, NULL, 0) == 0, "kern.proc: %s",
	    strerror(errno));
	n = len / sizeof(*kp);
	CHECK(n == 2, "kern.proc lists %d threads", n);
	for (int i = 0; i < n; i++) {
		if (strcmp(kp[i].ki_tdname, "abyss-worker") == 0)
			found++;
		CHECK(kp[i].ki_pid == getpid(), "thread %d has pid %d", i,
		    kp[i].ki_pid);
	}
	CHECK(found == 1, "the name was set on %d threads", found);
	CHECK(n < 2 || kp[0].ki_tid != kp[1].ki_tid, "threads share a tid");
	free(kp);
	atomic_store(&name_stop, 1);
	pthread_join(t, NULL);
}

/* A threaded process as a whole: in children, as each ends it. */

static void *
spin_main(void *arg)
{
	_Atomic(long) *counter = arg;

	for (;;)
		atomic_fetch_add(counter, 1);
	return (NULL);
}

static void *
block_main(void *arg)
{
	int *fds = arg;
	char c;

	/* Blocks in the kernel until the process goes. */
	read(fds[0], &c, 1);
	return (NULL);
}

static int
child_status(pid_t pid)
{
	int st;

	if (waitpid(pid, &st, 0) != pid)
		return (-1);
	return (st);
}

/* One thread's exit ends the others, wherever they are. */
static void
check_exit_from_thread(void)
{
	static _Atomic(long) counter;
	pid_t pid = fork();

	if (pid == 0) {
		int fds[2];
		pthread_t t;

		pipe(fds);
		pthread_create(&t, NULL, spin_main, &counter);
		pthread_create(&t, NULL, block_main, fds);
		pthread_create(&t, NULL, block_main, fds);
		usleep(20000);
		exit(5);
	}
	int st = child_status(pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 5,
	    "exit from a threaded child: status %#x", st);
}

static void *
exiting_thread_main(void *arg)
{
	(void)arg;
	usleep(20000);
	_exit(6);
}

/* An exit from a thread other than the first. */
static void
check_exit_from_other_thread(void)
{
	pid_t pid = fork();

	if (pid == 0) {
		pthread_t t;
		int fds[2];

		pipe(fds);
		pthread_create(&t, NULL, exiting_thread_main, NULL);
		block_main(fds);
		_exit(1);
	}
	int st = child_status(pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 6,
	    "exit from a second thread: status %#x", st);
}

/* A signal that kills does so for every thread. */
static void
check_kill(void)
{
	static _Atomic(long) counter;
	pid_t pid = fork();

	if (pid == 0) {
		int fds[2];
		pthread_t t;

		pipe(fds);
		pthread_create(&t, NULL, spin_main, &counter);
		pthread_create(&t, NULL, block_main, fds);
		block_main(fds);
		_exit(1);
	}
	usleep(50000);
	kill(pid, SIGTERM);
	int st = child_status(pid);
	CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM,
	    "SIGTERM to a threaded child: status %#x", st);
}

/* The process outlives its first thread's pthread_exit. */
static void *
late_main(void *arg)
{
	(void)arg;
	usleep(30000);
	return (NULL);
}

static void
check_main_pthread_exit(void)
{
	pid_t pid = fork();

	if (pid == 0) {
		pthread_t t;

		pthread_create(&t, NULL, late_main, NULL);
		pthread_exit(NULL);
	}
	int st = child_status(pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0,
	    "after the first thread's pthread_exit: status %#x", st);
}

/* Exec from a thread: the new program has one thread, the old ones go. */
static char self_path[1024];

static void *
exec_main(void *arg)
{
	(void)arg;
	execl(self_path, "threads", "exec-child", (char *)NULL);
	_exit(2);
}

static void
check_exec_from_thread(void)
{
	static _Atomic(long) counter;
	pid_t pid = fork();

	if (pid == 0) {
		int fds[2];
		pthread_t t;

		pipe(fds);
		pthread_create(&t, NULL, spin_main, &counter);
		pthread_create(&t, NULL, block_main, fds);
		pthread_create(&t, NULL, exec_main, NULL);
		block_main(fds);
		_exit(1);
	}
	int st = child_status(pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7,
	    "exec from a thread: status %#x", st);
}

/* What the exec'd program checks: it is alone. */
static int
exec_child(void)
{
	struct kinfo_proc kp[4];
	size_t len = sizeof(kp);
	int mib[4] = { CTL_KERN, KERN_PROC,
	    KERN_PROC_PID | KERN_PROC_INC_THREAD, getpid() };

	if (sysctl(mib, 4, kp, &len, NULL, 0) != 0)
		return (3);
	return (len == sizeof(kp[0]) ? 7 : 4);
}

/* Fork from a thread copies only that thread. */
static void *
forker_main(void *arg)
{
	struct kinfo_proc kp[4];
	int *result = arg;
	pid_t pid = fork();

	if (pid == 0) {
		size_t len = sizeof(kp);
		int mib[4] = { CTL_KERN, KERN_PROC,
		    KERN_PROC_PID | KERN_PROC_INC_THREAD, getpid() };

		if (sysctl(mib, 4, kp, &len, NULL, 0) != 0)
			_exit(3);
		_exit(len == sizeof(kp[0]) ? 8 : 4);
	}
	*result = child_status(pid);
	return (NULL);
}

static void
check_fork_from_thread(void)
{
	static _Atomic(long) counter;
	pthread_t spinner, forker;
	int result = -1;
	pid_t pid = fork();

	if (pid == 0) {
		pthread_create(&spinner, NULL, spin_main, &counter);
		pthread_create(&forker, NULL, forker_main, &result);
		pthread_join(forker, NULL);
		_exit(WIFEXITED(result) ? WEXITSTATUS(result) : 9);
	}
	int st = child_status(pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 8,
	    "fork from a thread: status %#x", st);
}

/* A stop stops every thread, and SIGCONT starts them all again. */
static void
check_stop(void)
{
	_Atomic(long) *counter = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
	    MAP_SHARED | MAP_ANON, -1, 0);
	pid_t pid = fork();

	if (pid == 0) {
		pthread_t t;
		int fds[2];

		pipe(fds);
		pthread_create(&t, NULL, spin_main, (void *)counter);
		block_main(fds);
		_exit(1);
	}
	while (atomic_load(counter) == 0)
		usleep(1000);
	kill(pid, SIGSTOP);
	int st;
	CHECK(waitpid(pid, &st, WUNTRACED) == pid && WIFSTOPPED(st) &&
	    WSTOPSIG(st) == SIGSTOP, "stop: status %#x", st);
	long a = atomic_load(counter);
	usleep(50000);
	long b = atomic_load(counter);
	CHECK(a == b, "a stopped process's thread ran (%ld to %ld)", a, b);
	kill(pid, SIGCONT);
	usleep(50000);
	CHECK(atomic_load(counter) != b, "SIGCONT did not restart the thread");
	kill(pid, SIGKILL);
	st = child_status(pid);
	CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL,
	    "SIGKILL after a stop: status %#x", st);
	munmap((void *)counter, 4096);
}

/* Single-threading under load. */

static void *
napper_main(void *arg)
{
	struct timespec ts = { 0, 1000000 };

	(void)arg;
	for (;;)
		nanosleep(&ts, NULL);
	return (NULL);
}

static void *
faulter_main(void *arg)
{
	(void)arg;
	for (;;) {
		char *p = mmap(NULL, 64 * 4096, PROT_READ | PROT_WRITE,
		    MAP_ANON | MAP_PRIVATE, -1, 0);

		if (p == MAP_FAILED)
			continue;
		for (int i = 0; i < 64; i++)
			p[i * 4096] = 1;
		munmap(p, 64 * 4096);
	}
	return (NULL);
}

static void *
caller_main(void *arg)
{
	(void)arg;
	for (;;)
		getppid();
	return (NULL);
}

static void *
leaver_main(void *arg)
{
	int how = (int)(long)arg;

	usleep(2000);
	if (how == 0)
		_exit(9);
	execl(self_path, "threads", "exec-child", (char *)NULL);
	_exit(2);
}

/* Round `r`: even rounds exit, odd ones exec; every fourth from main. */
static int
stress_round(int r)
{
	static _Atomic(long) counter;
	int how = r % 2, want = how == 0 ? 9 : 7;
	pid_t pid = fork();

	if (pid == 0) {
		int fds[2];
		pthread_t t;

		pipe(fds);
		pthread_create(&t, NULL, napper_main, NULL);
		pthread_create(&t, NULL, faulter_main, NULL);
		pthread_create(&t, NULL, caller_main, NULL);
		pthread_create(&t, NULL, spin_main, &counter);
		pthread_create(&t, NULL, block_main, fds);
		if (r % 4 < 2)
			leaver_main((void *)(long)how);
		pthread_create(&t, NULL, leaver_main, (void *)(long)how);
		block_main(fds);
		_exit(1);
	}
	int st = child_status(pid);
	if (!WIFEXITED(st) || WEXITSTATUS(st) != want) {
		printf("threads: stress round %d: status %#x, want exit %d\n",
		    r, st, want);
		return (1);
	}
	return (0);
}

int
main(int argc, char **argv)
{
	int mib[4] = { CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, -1 };
	size_t len = sizeof(self_path);

	if (argc > 1 && strcmp(argv[1], "exec-child") == 0)
		return (exec_child());
	if (sysctl(mib, 4, self_path, &len, NULL, 0) != 0) {
		printf("threads: kern.proc.pathname: %s\n", strerror(errno));
		return (1);
	}
	if (argc > 2 && strcmp(argv[1], "stress") == 0) {
		int rounds = atoi(argv[2]);

		for (int r = 0; r < rounds; r++)
			if (stress_round(r) != 0)
				return (1);
		printf("threads: stress: %d rounds ok\n", rounds);
		return (0);
	}
	check_create_join();
	check_signals();
	check_sigwait();
	check_altstack();
	check_suspend();
#ifdef __amd64__
	check_gsbase();
#endif
	check_names();
	check_exit_from_thread();
	check_exit_from_other_thread();
	check_kill();
	check_main_pthread_exit();
	check_exec_from_thread();
	check_fork_from_thread();
	check_stop();
	if (failures == 0)
		printf("threads: ok\n");
	return (failures != 0);
}
