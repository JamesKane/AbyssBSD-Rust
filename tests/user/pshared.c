/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * pshared: checks the umtx protocols libthr builds on from user mode:
 * process-shared mutexes, condition variables, reader-writer locks, and
 * semaphores between a parent and its child (in shared memory, through
 * libthr's UMTX_OP_SHM pages); robust mutexes whose owner dies, a thread
 * or a whole process (EOWNERDEAD, then ENOTRECOVERABLE unless made
 * consistent); contended priority-inheritance and priority-protection
 * mutexes; timed locks; and POSIX shared memory: named objects across
 * fork, shm_unlink, shm_rename, and memfd_create with seals.
 *
 * Links with: -lthr
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int failures;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("pshared: " __VA_ARGS__);			\
		printf("\n");						\
		failures++;						\
	}								\
} while (0)

struct shared {
	pthread_mutex_t m;
	pthread_cond_t c;
	pthread_rwlock_t rw;
	pthread_mutex_t robust;
	sem_t sem;
	int turn;
	int count;
	int readers, writers;
};

static struct shared *
shared_mem(void)
{
	void *p = mmap(NULL, sizeof(struct shared), PROT_READ | PROT_WRITE,
	    MAP_SHARED | MAP_ANON, -1, 0);

	if (p == MAP_FAILED) {
		printf("pshared: mmap: %s\n", strerror(errno));
		exit(1);
	}
	return (p);
}

static int
wait_child(pid_t pid)
{
	int st;

	if (waitpid(pid, &st, 0) != pid)
		return (-1);
	return (WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
}

/* Parent and child take turns through a shared mutex and condvar. */
static void
ping_pong(void)
{
	struct shared *sh = shared_mem();
	pthread_mutexattr_t ma;
	pthread_condattr_t ca;
	const int rounds = 2000;

	pthread_mutexattr_init(&ma);
	pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
	pthread_condattr_init(&ca);
	pthread_condattr_setpshared(&ca, PTHREAD_PROCESS_SHARED);
	CHECK(pthread_mutex_init(&sh->m, &ma) == 0, "pshared mutex init");
	CHECK(pthread_cond_init(&sh->c, &ca) == 0, "pshared cond init");
	sh->turn = 0;
	sh->count = 0;

	pid_t pid = fork();
	int me = pid == 0 ? 1 : 0;
	for (int i = 0; i < rounds; i++) {
		pthread_mutex_lock(&sh->m);
		while (sh->turn != me)
			pthread_cond_wait(&sh->c, &sh->m);
		sh->count++;
		sh->turn = !me;
		pthread_cond_signal(&sh->c);
		pthread_mutex_unlock(&sh->m);
	}
	if (pid == 0)
		_exit(0);
	CHECK(wait_child(pid) == 0, "ping-pong child failed");
	CHECK(sh->count == 2 * rounds, "ping-pong count %d, want %d",
	    sh->count, 2 * rounds);
	pthread_cond_destroy(&sh->c);
	pthread_mutex_destroy(&sh->m);
	munmap(sh, sizeof(*sh));
}

/* Two processes and their threads contend one shared mutex. */
static void *
contend(void *arg)
{
	struct shared *sh = arg;

	for (int i = 0; i < 5000; i++) {
		pthread_mutex_lock(&sh->m);
		int c = sh->count;
		if (i % 64 == 0)
			sched_yield();
		sh->count = c + 1;
		pthread_mutex_unlock(&sh->m);
	}
	return (NULL);
}

static void
contention(int protocol, const char *name)
{
	struct shared *sh = shared_mem();
	pthread_mutexattr_t ma;
	pthread_t t[2];

	pthread_mutexattr_init(&ma);
	pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
	pthread_mutexattr_setprotocol(&ma, protocol);
	if (protocol == PTHREAD_PRIO_PROTECT)
		pthread_mutexattr_setprioceiling(&ma, 10);
	CHECK(pthread_mutex_init(&sh->m, &ma) == 0, "%s mutex init", name);
	sh->count = 0;
	pid_t pid = fork();
	for (int i = 0; i < 2; i++)
		pthread_create(&t[i], NULL, contend, sh);
	for (int i = 0; i < 2; i++)
		pthread_join(t[i], NULL);
	if (pid == 0)
		_exit(0);
	CHECK(wait_child(pid) == 0, "%s child failed", name);
	CHECK(sh->count == 4 * 5000, "%s: count %d, want %d", name, sh->count,
	    4 * 5000);
	pthread_mutex_destroy(&sh->m);
	munmap(sh, sizeof(*sh));
}

/* A robust mutex whose owner process dies. */
static void
robust_process(void)
{
	struct shared *sh = shared_mem();
	pthread_mutexattr_t ma;
	int r;

	pthread_mutexattr_init(&ma);
	pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED);
	pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST);
	CHECK(pthread_mutex_init(&sh->robust, &ma) == 0, "robust init");

	/* The parent is already waiting when the owner dies. */
	pid_t pid = fork();
	if (pid == 0) {
		pthread_mutex_lock(&sh->robust);
		usleep(100000);
		_exit(0);
	}
	usleep(30000);
	r = pthread_mutex_lock(&sh->robust);
	CHECK(r == EOWNERDEAD, "lock after owner died: %s", strerror(r));
	CHECK(wait_child(pid) == 0, "robust child failed");
	/* Unlocked without being made consistent: not recoverable. */
	pthread_mutex_unlock(&sh->robust);
	r = pthread_mutex_lock(&sh->robust);
	CHECK(r == ENOTRECOVERABLE, "lock after inconsistent unlock: %s",
	    strerror(r));
	pthread_mutex_destroy(&sh->robust);

	/* Made consistent, it works again. */
	CHECK(pthread_mutex_init(&sh->robust, &ma) == 0, "robust reinit");
	pid = fork();
	if (pid == 0) {
		pthread_mutex_lock(&sh->robust);
		_exit(0);
	}
	CHECK(wait_child(pid) == 0, "robust child 2 failed");
	r = pthread_mutex_lock(&sh->robust);
	CHECK(r == EOWNERDEAD, "lock after owner exited: %s", strerror(r));
	CHECK(pthread_mutex_consistent(&sh->robust) == 0, "consistent");
	CHECK(pthread_mutex_unlock(&sh->robust) == 0, "unlock consistent");
	r = pthread_mutex_lock(&sh->robust);
	CHECK(r == 0, "lock consistent: %s", strerror(r));
	pthread_mutex_unlock(&sh->robust);
	pthread_mutex_destroy(&sh->robust);
	munmap(sh, sizeof(*sh));
}

static pthread_mutex_t robust_local;

static void *
die_holding(void *arg)
{
	(void)arg;
	pthread_mutex_lock(&robust_local);
	return (NULL);
}

/* A robust mutex whose owner thread exits. */
static void
robust_thread(void)
{
	pthread_mutexattr_t ma;
	pthread_t t;

	pthread_mutexattr_init(&ma);
	pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST);
	pthread_mutex_init(&robust_local, &ma);
	pthread_create(&t, NULL, die_holding, NULL);
	pthread_join(t, NULL);
	int r = pthread_mutex_lock(&robust_local);
	CHECK(r == EOWNERDEAD, "lock after owner thread exited: %s",
	    strerror(r));
	pthread_mutex_consistent(&robust_local);
	pthread_mutex_unlock(&robust_local);
	pthread_mutex_destroy(&robust_local);
}

/* A shared reader-writer lock: writers alone, readers together. */
static void
rwlock(void)
{
	struct shared *sh = shared_mem();
	pthread_rwlockattr_t ra;

	pthread_rwlockattr_init(&ra);
	pthread_rwlockattr_setpshared(&ra, PTHREAD_PROCESS_SHARED);
	CHECK(pthread_rwlock_init(&sh->rw, &ra) == 0, "rwlock init");
	sh->readers = sh->writers = 0;
	sh->count = 0;
	pid_t pid = fork();
	for (int i = 0; i < 3000; i++) {
		if (i % 3 == 0) {
			pthread_rwlock_wrlock(&sh->rw);
			if (__atomic_fetch_add(&sh->writers, 1, __ATOMIC_SEQ_CST) != 0 ||
			    __atomic_load_n(&sh->readers, __ATOMIC_SEQ_CST) != 0)
				__atomic_fetch_add(&sh->count, 1, __ATOMIC_SEQ_CST);
			__atomic_fetch_sub(&sh->writers, 1, __ATOMIC_SEQ_CST);
		} else {
			pthread_rwlock_rdlock(&sh->rw);
			__atomic_fetch_add(&sh->readers, 1, __ATOMIC_SEQ_CST);
			if (__atomic_load_n(&sh->writers, __ATOMIC_SEQ_CST) != 0)
				__atomic_fetch_add(&sh->count, 1, __ATOMIC_SEQ_CST);
			__atomic_fetch_sub(&sh->readers, 1, __ATOMIC_SEQ_CST);
		}
		pthread_rwlock_unlock(&sh->rw);
	}
	if (pid == 0)
		_exit(0);
	CHECK(wait_child(pid) == 0, "rwlock child failed");
	CHECK(sh->count == 0, "rwlock: %d overlaps", sh->count);
	pthread_rwlock_destroy(&sh->rw);
	munmap(sh, sizeof(*sh));
}

/* A process-shared semaphore posted by the child. */
static void
semaphore(void)
{
	struct shared *sh = shared_mem();

	CHECK(sem_init(&sh->sem, 1, 0) == 0, "sem_init: %s", strerror(errno));
	pid_t pid = fork();
	if (pid == 0) {
		for (int i = 0; i < 500; i++)
			sem_post(&sh->sem);
		_exit(0);
	}
	for (int i = 0; i < 500; i++)
		CHECK(sem_wait(&sh->sem) == 0, "sem_wait: %s", strerror(errno));
	CHECK(wait_child(pid) == 0, "sem child failed");
	int v = -1;
	sem_getvalue(&sh->sem, &v);
	CHECK(v == 0, "sem value %d", v);
	struct timespec ts;
	clock_gettime(CLOCK_REALTIME, &ts);
	ts.tv_nsec += 20000000;
	if (ts.tv_nsec >= 1000000000) {
		ts.tv_sec++;
		ts.tv_nsec -= 1000000000;
	}
	CHECK(sem_timedwait(&sh->sem, &ts) == -1 && errno == ETIMEDOUT,
	    "sem_timedwait: %s", strerror(errno));
	sem_destroy(&sh->sem);
	munmap(sh, sizeof(*sh));
}

/* A timed lock of a held mutex times out. */
static pthread_mutex_t held = PTHREAD_MUTEX_INITIALIZER;

static void *
timed(void *arg)
{
	struct timespec ts;
	int r;

	(void)arg;
	clock_gettime(CLOCK_REALTIME, &ts);
	ts.tv_nsec += 20000000;
	if (ts.tv_nsec >= 1000000000) {
		ts.tv_sec++;
		ts.tv_nsec -= 1000000000;
	}
	r = pthread_mutex_timedlock(&held, &ts);
	CHECK(r == ETIMEDOUT, "timedlock: %s", strerror(r));
	return (NULL);
}

static void
timed_locks(void)
{
	pthread_t t;

	pthread_mutex_lock(&held);
	pthread_create(&t, NULL, timed, NULL);
	pthread_join(t, NULL);
	pthread_mutex_unlock(&held);
}

/* A priority-protection mutex's ceiling. */
static void
ceiling(void)
{
	pthread_mutexattr_t ma;
	pthread_mutex_t m;
	int old = -1, now = -1;

	pthread_mutexattr_init(&ma);
	pthread_mutexattr_setprotocol(&ma, PTHREAD_PRIO_PROTECT);
	pthread_mutexattr_setprioceiling(&ma, 5);
	CHECK(pthread_mutex_init(&m, &ma) == 0, "pp init");
	CHECK(pthread_mutex_setprioceiling(&m, 9, &old) == 0 && old == 5,
	    "setprioceiling: old %d", old);
	CHECK(pthread_mutex_getprioceiling(&m, &now) == 0 && now == 9,
	    "getprioceiling: %d", now);
	CHECK(pthread_mutex_lock(&m) == 0, "pp lock");
	CHECK(pthread_mutex_unlock(&m) == 0, "pp unlock");
	pthread_mutex_destroy(&m);
}

/* A named object, seen by a child, renamed, and unlinked. */
static void
named(void)
{
	char name[64], other[64];
	int fd;

	/* Per process: several may run at once. */
	snprintf(name, sizeof(name), "/pshared-test.%d", (int)getpid());
	snprintf(other, sizeof(other), "/pshared-other.%d", (int)getpid());

	shm_unlink(name);
	shm_unlink(other);
	fd = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
	CHECK(fd >= 0, "shm_open: %s", strerror(errno));
	CHECK(shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600) == -1 &&
	    errno == EEXIST, "shm_open twice: %s", strerror(errno));
	CHECK(ftruncate(fd, 8192) == 0, "ftruncate: %s", strerror(errno));
	struct stat sb;
	CHECK(fstat(fd, &sb) == 0 && sb.st_size == 8192 &&
	    S_ISREG(sb.st_mode) && (sb.st_mode & 0777) == 0600,
	    "fstat: size %jd mode %o", (intmax_t)sb.st_size, sb.st_mode);
	char *p = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(p != MAP_FAILED, "mmap: %s", strerror(errno));
	pid_t pid = fork();
	if (pid == 0) {
		int cfd = shm_open(name, O_RDWR, 0);
		char *q = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED,
		    cfd, 0);
		if (q == MAP_FAILED)
			_exit(1);
		strcpy(q + 4096, "from the child");
		_exit(0);
	}
	CHECK(wait_child(pid) == 0, "shm child failed");
	CHECK(strcmp(p + 4096, "from the child") == 0, "child's write unseen");
	char buf[16] = { 0 };
	CHECK(pread(fd, buf, 14, 4096) == 14 &&
	    strcmp(buf, "from the child") == 0, "pread: %s", buf);
	CHECK(pwrite(fd, "xyz", 3, 0) == 3 && memcmp(p, "xyz", 3) == 0,
	    "pwrite unseen in the mapping");
	CHECK(lseek(fd, 0, SEEK_END) == 8192, "lseek end");
	CHECK(lseek(fd, 1, SEEK_END) == -1 && errno == EINVAL,
	    "lseek past the end: %s", strerror(errno));
	CHECK(shm_rename(name, other, 0) == 0, "shm_rename: %s",
	    strerror(errno));
	CHECK(shm_open(name, O_RDWR, 0) == -1 && errno == ENOENT,
	    "old name after rename: %s", strerror(errno));
	int fd2 = shm_open(other, O_RDWR, 0);
	CHECK(fd2 >= 0, "new name after rename: %s", strerror(errno));
	CHECK(fstat(fd2, &sb) == 0 && sb.st_size == 8192, "renamed size");
	close(fd2);
	CHECK(shm_unlink(other) == 0, "shm_unlink: %s", strerror(errno));
	CHECK(shm_unlink(other) == -1 && errno == ENOENT, "unlink twice");
	/* Unlinked, the object lives while open and mapped. */
	CHECK(strcmp(p + 4096, "from the child") == 0, "data after unlink");
	munmap(p, 8192);
	close(fd);
	CHECK(shm_open("relative", O_RDWR | O_CREAT, 0600) == -1 &&
	    errno == EINVAL, "a relative name: %s", strerror(errno));
}

/* memfd_create and its seals. */
static void
seals(void)
{
	int fd = memfd_create("pshared", MFD_ALLOW_SEALING);

	CHECK(fd >= 0, "memfd_create: %s", strerror(errno));
	CHECK(ftruncate(fd, 4096) == 0, "memfd ftruncate");
	CHECK(fcntl(fd, F_GET_SEALS) == 0, "initial seals %d",
	    fcntl(fd, F_GET_SEALS));
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK) == 0,
	    "add grow and shrink seals: %s", strerror(errno));
	CHECK(ftruncate(fd, 8192) == -1 && errno == EPERM, "grow sealed: %s",
	    strerror(errno));
	CHECK(ftruncate(fd, 0) == -1 && errno == EPERM, "shrink sealed: %s",
	    strerror(errno));
	char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(p != MAP_FAILED, "memfd mmap: %s", strerror(errno));
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE) == -1 && errno == EBUSY,
	    "write seal with a writable mapping: %s", strerror(errno));
	munmap(p, 4096);
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE) == 0, "write seal: %s",
	    strerror(errno));
	CHECK(write(fd, "x", 1) == -1 && errno == EPERM, "write sealed: %s",
	    strerror(errno));
	CHECK(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) ==
	    MAP_FAILED && errno == EACCES, "writable mapping sealed: %s",
	    strerror(errno));
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL) == 0, "seal seals");
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_GROW) == -1 && errno == EPERM,
	    "seal after F_SEAL_SEAL: %s", strerror(errno));
	CHECK(fcntl(fd, F_GET_SEALS) ==
	    (F_SEAL_SEAL | F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_WRITE),
	    "seals %d", fcntl(fd, F_GET_SEALS));
	close(fd);
	/* Without MFD_ALLOW_SEALING, no seals at all. */
	fd = memfd_create("pshared", 0);
	CHECK(fcntl(fd, F_ADD_SEALS, F_SEAL_GROW) == -1 && errno == EPERM,
	    "seal unsealable: %s", strerror(errno));
	CHECK(fcntl(0, F_GET_SEALS) == -1 && errno == EINVAL,
	    "seals of a non-memory file: %s", strerror(errno));
	close(fd);
}

/* What runs now, for the watchdog. */
static const char *volatile phase = "start";

/* Ends the test, saying where, if it hangs. */
static void *
watchdog(void *arg)
{
	(void)arg;
	sleep(240);
	printf("pshared: hung in %s\n", phase);
	fflush(stdout);
	_exit(1);
}

#define	RUN(f) do { phase = #f; f; } while (0)

int
main(void)
{
	pthread_t w;

	pthread_create(&w, NULL, watchdog, NULL);
	phase = "ping_pong";
	ping_pong();
	RUN(contention(PTHREAD_PRIO_NONE, "normal"));
	RUN(contention(PTHREAD_PRIO_INHERIT, "inherit"));
	RUN(contention(PTHREAD_PRIO_PROTECT, "protect"));
	RUN(robust_process());
	RUN(robust_thread());
	RUN(rwlock());
	RUN(semaphore());
	RUN(timed_locks());
	RUN(ceiling());
	RUN(named());
	RUN(seals());
	if (failures == 0)
		printf("pshared: ok\n");
	return (failures != 0);
}
