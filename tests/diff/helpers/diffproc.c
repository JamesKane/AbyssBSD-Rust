/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * diffproc: puts a process in a known state for the differential
 * scenarios (tests/diff), then waits there until it is killed. It creates
 * READY (its second argument) once the state is set up, and the scenario
 * waits for that file before looking.
 *
 *   diffproc threads READY   three named threads, asleep in a condition
 *                            variable, nanosleep, and sigwait; the main
 *                            thread in sigsuspend
 *   diffproc files READY     a descriptor of each kind: a regular file
 *                            (locked with fcntl and flock), a directory,
 *                            a pipe, a FIFO, a socket pair, a listening
 *                            UNIX-domain socket, a kqueue, a terminal
 *                            pair, POSIX shared memory, an eventfd, a
 *                            timerfd, a process descriptor, /dev/null
 *   diffproc vm READY        anonymous, shared, and file mappings
 *   diffproc kqueue READY    a kqueue with knotes of several filters,
 *                            waited on
 *   diffproc zombie READY    a child that exited and is not reaped
 *   diffproc piperd READY    reading an empty pipe
 *   diffproc select READY    in select(2) on an empty pipe
 *   diffproc poll READY      in poll(2) on an empty pipe
 *   diffproc sbwait READY    reading an empty socket
 *   diffproc accept READY    accepting on a UNIX-domain socket
 *   diffproc ttyin READY     reading a pseudo-terminal
 *   diffproc wait READY      waiting for a child (asleep in nanosleep)
 *   diffproc cpu READY       spinning in user mode
 *   diffproc status READY    prints the terminal status line (^T and
 *                            TIOCSTAT) of a sleeping child on a terminal
 *   diffproc syscalls -      a fixed run of system calls between two
 *                            markers (access of diff-begin and diff-end),
 *                            for ktrace and truss: file I/O, fstat, a
 *                            sysctl, utrace, a caught signal, a sleep,
 *                            and a forked child's exit; then exits. Its
 *                            pid and the child's go to .pids
 *
 * The same binary (built static) runs on FreeBSD 15.1 and AbyssBSD.
 *
 * Links with: -lthr
 * Built static only
 */

#include <sys/types.h>
#include <sys/event.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/procdesc.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/ktrace.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <sys/wait.h>

#include <sys/ioctl.h>
#include <sys/ttycom.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <pthread_np.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static const char *ready;

/* Says the state is set up. */
static void
announce(void)
{
	int fd;

	fd = open(ready, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (fd < 0)
		err(1, "%s", ready);
	close(fd);
}

/* Sleeps for good, in sigsuspend ("sigsusp"). */
static void
forever(void)
{
	sigset_t none;

	sigemptyset(&none);
	for (;;)
		sigsuspend(&none);
}

static pthread_mutex_t mtx = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int started;

static void
up(void)
{
	pthread_mutex_lock(&mtx);
	started++;
	pthread_mutex_unlock(&mtx);
}

static void *
condwaiter(void *arg __unused)
{
	pthread_mutex_lock(&mtx);
	started++;
	for (;;)
		pthread_cond_wait(&cv, &mtx);
	return (NULL);
}

static void *
napper(void *arg __unused)
{
	struct timespec ts = { 100000, 0 };

	up();
	for (;;)
		nanosleep(&ts, NULL);
	return (NULL);
}

static void *
sigwaiter(void *arg __unused)
{
	sigset_t set;
	int sig;

	sigemptyset(&set);
	sigaddset(&set, SIGUSR2);
	up();
	for (;;)
		sigwait(&set, &sig);
	return (NULL);
}

static void
threads(void)
{
	static void *(*const fn[])(void *) = { condwaiter, napper, sigwaiter };
	static const char *const name[] = { "worker-cond", "worker-nap",
	    "worker-sigwait" };
	pthread_t t;
	sigset_t set;
	int i, n;

	sigemptyset(&set);
	sigaddset(&set, SIGUSR2);
	pthread_sigmask(SIG_BLOCK, &set, NULL);
	for (i = 0; i < 3; i++) {
		if (pthread_create(&t, NULL, fn[i], NULL) != 0)
			errx(1, "pthread_create");
		pthread_set_name_np(t, name[i]);
	}
	do {
		usleep(10000);
		pthread_mutex_lock(&mtx);
		n = started;
		pthread_mutex_unlock(&mtx);
	} while (n < 3);
	/* Each is asleep by now, or about to be; give them a moment. */
	usleep(200000);
	announce();
	forever();
}

static void
files(void)
{
	struct sockaddr_un sun;
	struct flock fl;
	struct itimerspec its;
	int fds[2], s[2], fd, kq, l, pd;
	pid_t pid;

	if ((fd = open("diff.file", O_RDWR | O_CREAT | O_TRUNC, 0644)) < 0)
		err(1, "diff.file");
	if (write(fd, "hello, world\n", 13) != 13)
		err(1, "write");
	memset(&fl, 0, sizeof(fl));
	fl.l_type = F_WRLCK;
	fl.l_whence = SEEK_SET;
	fl.l_start = 0;
	fl.l_len = 5;
	if (fcntl(fd, F_SETLK, &fl) < 0)
		err(1, "F_SETLK");
	if ((fd = open("diff.flock", O_RDONLY | O_CREAT, 0600)) < 0)
		err(1, "diff.flock");
	if (flock(fd, LOCK_SH) < 0)
		err(1, "flock");
	if (open(".", O_RDONLY | O_DIRECTORY) < 0)
		err(1, ".");
	if (open("/dev/null", O_WRONLY | O_APPEND) < 0)
		err(1, "/dev/null");
	if (pipe(fds) < 0)
		err(1, "pipe");
	if (mkfifo("diff.fifo", 0600) < 0)
		err(1, "mkfifo");
	if (open("diff.fifo", O_RDWR | O_NONBLOCK) < 0)
		err(1, "diff.fifo");
	if (socketpair(AF_UNIX, SOCK_STREAM, 0, s) < 0)
		err(1, "socketpair");
	if (write(s[0], "abc", 3) != 3)
		err(1, "write");
	if ((l = socket(AF_UNIX, SOCK_STREAM, 0)) < 0)
		err(1, "socket");
	memset(&sun, 0, sizeof(sun));
	sun.sun_family = AF_UNIX;
	strlcpy(sun.sun_path, "diff.sock", sizeof(sun.sun_path));
	if (bind(l, (struct sockaddr *)&sun, SUN_LEN(&sun)) < 0 ||
	    listen(l, 5) < 0)
		err(1, "diff.sock");
	if (socket(AF_UNIX, SOCK_DGRAM, 0) < 0)
		err(1, "socket");
	if ((kq = kqueue()) < 0)
		err(1, "kqueue");
	if ((fd = posix_openpt(O_RDWR | O_NOCTTY)) < 0 || grantpt(fd) < 0 ||
	    unlockpt(fd) < 0)
		err(1, "posix_openpt");
	if (open(ptsname(fd), O_RDWR | O_NOCTTY) < 0)
		err(1, "%s", ptsname(fd));
	if ((fd = shm_open(SHM_ANON, O_RDWR, 0600)) < 0 ||
	    ftruncate(fd, 8192) < 0)
		err(1, "shm_open");
	if ((fd = shm_open("/diff.shm", O_RDWR | O_CREAT, 0640)) < 0 ||
	    ftruncate(fd, 4096) < 0)
		err(1, "/diff.shm");
	if (eventfd(3, EFD_NONBLOCK) < 0)
		err(1, "eventfd");
	if ((fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC)) < 0)
		err(1, "timerfd_create");
	memset(&its, 0, sizeof(its));
	its.it_value.tv_sec = 100000;
	if (timerfd_settime(fd, 0, &its, NULL) < 0)
		err(1, "timerfd_settime");
	pid = pdfork(&pd, 0);
	if (pid < 0)
		err(1, "pdfork");
	if (pid == 0)
		forever();
	announce();
	forever();
}

static void
vm(void)
{
	char *p;
	int fd;

	if ((p = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_PRIVATE, -1, 0)) == MAP_FAILED)
		err(1, "mmap anon");
	p[0] = 1;
	p[8192] = 1;
	if ((p = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE,
	    MAP_ANON | MAP_SHARED, -1, 0)) == MAP_FAILED)
		err(1, "mmap shared");
	p[0] = 1;
	if ((p = mmap(NULL, 4096, PROT_READ, MAP_ANON | MAP_PRIVATE, -1,
	    0)) == MAP_FAILED)
		err(1, "mmap ro");
	if ((fd = open("diff.map", O_RDWR | O_CREAT | O_TRUNC, 0644)) < 0 ||
	    ftruncate(fd, 2 * 4096) < 0)
		err(1, "diff.map");
	if ((p = mmap(NULL, 2 * 4096, PROT_READ, MAP_SHARED, fd, 0)) ==
	    MAP_FAILED)
		err(1, "mmap file");
	(void)*(volatile char *)p;
	if ((p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd,
	    4096)) == MAP_FAILED)
		err(1, "mmap file private");
	p[0] = 1;
	if (mmap(NULL, 4096, PROT_NONE, MAP_GUARD, -1, 0) == MAP_FAILED)
		err(1, "mmap guard");
	announce();
	forever();
}

static void
kq(void)
{
	struct kevent ev[6], out;
	int fds[2], fd, k;

	if ((k = kqueue()) < 0 || pipe(fds) < 0)
		err(1, "kqueue");
	if ((fd = open("diff.watched", O_RDONLY | O_CREAT, 0644)) < 0)
		err(1, "diff.watched");
	EV_SET(&ev[0], fds[0], EVFILT_READ, EV_ADD, 0, 0, NULL);
	EV_SET(&ev[1], fds[1], EVFILT_WRITE, EV_ADD | EV_DISABLE, 0, 0, NULL);
	EV_SET(&ev[2], 7, EVFILT_TIMER, EV_ADD, 0, 100000000, NULL);
	EV_SET(&ev[3], 9, EVFILT_USER, EV_ADD | EV_CLEAR, 0, 0, NULL);
	EV_SET(&ev[4], fd, EVFILT_VNODE, EV_ADD | EV_CLEAR,
	    NOTE_WRITE | NOTE_DELETE, 0, NULL);
	EV_SET(&ev[5], SIGUSR1, EVFILT_SIGNAL, EV_ADD, 0, 0, NULL);
	if (kevent(k, ev, 6, NULL, 0, NULL) < 0)
		err(1, "kevent");
	signal(SIGUSR1, SIG_IGN);
	announce();
	for (;;)
		kevent(k, NULL, 0, &out, 1, NULL);
}

static void
zombie(void)
{
	pid_t pid;

	if ((pid = fork()) < 0)
		err(1, "fork");
	if (pid == 0)
		_exit(3);
	/* Not reaped: it stays a zombie. */
	usleep(300000);
	announce();
	forever();
}

static int
emptypipe(void)
{
	int fds[2];

	if (pipe(fds) < 0)
		err(1, "pipe");
	return (fds[0]);
}

static void
piperd(void)
{
	char c;
	int fd;

	fd = emptypipe();
	announce();
	for (;;)
		read(fd, &c, 1);
}

static void
doselect(void)
{
	fd_set r;
	int fd;

	fd = emptypipe();
	announce();
	for (;;) {
		FD_ZERO(&r);
		FD_SET(fd, &r);
		select(fd + 1, &r, NULL, NULL, NULL);
	}
}

static void
dopoll(void)
{
	struct pollfd p;

	p.fd = emptypipe();
	p.events = POLLIN;
	announce();
	for (;;)
		poll(&p, 1, -1);
}

static void
sbwait(void)
{
	char c;
	int s[2];

	if (socketpair(AF_UNIX, SOCK_STREAM, 0, s) < 0)
		err(1, "socketpair");
	announce();
	for (;;)
		recv(s[0], &c, 1, 0);
}

static void
doaccept(void)
{
	struct sockaddr_un sun;
	int l;

	if ((l = socket(AF_UNIX, SOCK_STREAM, 0)) < 0)
		err(1, "socket");
	memset(&sun, 0, sizeof(sun));
	sun.sun_family = AF_UNIX;
	strlcpy(sun.sun_path, "diff.accept", sizeof(sun.sun_path));
	unlink(sun.sun_path);
	if (bind(l, (struct sockaddr *)&sun, SUN_LEN(&sun)) < 0 ||
	    listen(l, 5) < 0)
		err(1, "bind");
	announce();
	for (;;)
		accept(l, NULL, NULL);
}

static void
ttyin(void)
{
	char c;
	int m, s;

	if ((m = posix_openpt(O_RDWR | O_NOCTTY)) < 0 || grantpt(m) < 0 ||
	    unlockpt(m) < 0)
		err(1, "posix_openpt");
	if ((s = open(ptsname(m), O_RDWR | O_NOCTTY)) < 0)
		err(1, "%s", ptsname(m));
	announce();
	for (;;)
		read(s, &c, 1);
}

static void
waiter(void)
{
	struct timespec ts = { 100000, 0 };
	pid_t pid;
	int st;

	if ((pid = fork()) < 0)
		err(1, "fork");
	if (pid == 0) {
		for (;;)
			nanosleep(&ts, NULL);
	}
	usleep(200000);
	announce();
	for (;;)
		waitpid(pid, &st, 0);
}

/*
 * The terminal status line: a child (sleep) in the foreground of a
 * pseudo-terminal of its own, asleep; ^T typed at it (VSTATUS), and
 * TIOCSTAT asked; what the terminal printed each time is shown.
 */
static void
drain(int m, const char *what)
{
	struct pollfd p;
	char buf[512];
	ssize_t n;

	printf("%s:\n", what);
	p.fd = m;
	p.events = POLLIN;
	while (poll(&p, 1, 1000) > 0) {
		n = read(m, buf, sizeof(buf) - 1);
		if (n <= 0)
			break;
		buf[n] = '\0';
		fputs(buf, stdout);
	}
	printf("\n");
	fflush(stdout);
}

static void
status(void)
{
	char c = 0x14;
	int m, s;
	pid_t pid;

	if ((m = posix_openpt(O_RDWR | O_NOCTTY)) < 0 || grantpt(m) < 0 ||
	    unlockpt(m) < 0)
		err(1, "posix_openpt");
	if ((pid = fork()) < 0)
		err(1, "fork");
	if (pid == 0) {
		setsid();
		if ((s = open(ptsname(m), O_RDWR)) < 0)
			err(1, "%s", ptsname(m));
		if (ioctl(s, TIOCSCTTY, NULL) < 0)
			err(1, "TIOCSCTTY");
		dup2(s, 0);
		dup2(s, 1);
		dup2(s, 2);
		execl("/bin/sleep", "sleep", "77779", (char *)NULL);
		_exit(127);
	}
	/* Asleep by then. */
	sleep(2);
	if (write(m, &c, 1) != 1)
		err(1, "write");
	drain(m, "^T");
	if (ioctl(m, TIOCSTAT, NULL) < 0)
		printf("TIOCSTAT: %s\n", strerror(errno));
	drain(m, "TIOCSTAT");
	kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
	announce();
}

static void
cpu(void)
{
	volatile unsigned long n = 0;

	announce();
	for (;;)
		n++;
}

/* Appends "PID NAME" to .pids, for the normalizer. */
static void
note_pid(pid_t pid, const char *name)
{
	FILE *f;

	if ((f = fopen(".pids", "a")) == NULL)
		err(1, ".pids");
	fprintf(f, "%d %s\n", (int)pid, name);
	fclose(f);
}

static void
on_usr1(int sig)
{
	(void)sig;
}

static void
syscalls(void)
{
	struct sigaction sa;
	struct stat st;
	struct timespec ts = { 0, 10 * 1000 * 1000 };
	char buf[32];
	int fd, mib[2], status;
	size_t len;
	pid_t pid;

	note_pid(getpid(), "traced");
	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_usr1;
	sigemptyset(&sa.sa_mask);

	(void)access("diff-begin", F_OK);
	if ((fd = open("data", O_RDWR | O_CREAT | O_TRUNC, 0644)) < 0)
		err(1, "data");
	(void)write(fd, "hello, world\n", 13);
	(void)lseek(fd, 0, SEEK_SET);
	(void)read(fd, buf, sizeof(buf));
	(void)fstat(fd, &st);
	(void)close(fd);
	(void)unlink("data");
	mib[0] = CTL_KERN;
	mib[1] = KERN_OSTYPE;
	len = sizeof(buf);
	(void)sysctl(mib, 2, buf, &len, NULL, 0);
	(void)utrace("diff", 4);
	(void)sigaction(SIGUSR1, &sa, NULL);
	(void)kill(getpid(), SIGUSR1);
	(void)nanosleep(&ts, NULL);
	if ((pid = fork()) == 0)
		_exit(3);
	(void)waitpid(pid, &status, 0);
	(void)access("diff-end", F_OK);

	note_pid(pid, "child");
}

int
main(int argc, char **argv)
{
	static const struct {
		const char *name;
		void (*fn)(void);
	} modes[] = {
		{ "threads", threads }, { "files", files }, { "vm", vm },
		{ "kqueue", kq }, { "zombie", zombie }, { "piperd", piperd },
		{ "select", doselect }, { "poll", dopoll }, { "sbwait", sbwait },
		{ "accept", doaccept }, { "ttyin", ttyin }, { "wait", waiter },
		{ "cpu", cpu }, { "status", status }, { "syscalls", syscalls },
	};
	size_t i;

	if (argc != 3)
		errx(2, "usage: diffproc mode ready-file");
	ready = argv[2];
	for (i = 0; i < sizeof(modes) / sizeof(modes[0]); i++) {
		if (strcmp(argv[1], modes[i].name) == 0) {
			modes[i].fn();
			return (0);
		}
	}
	errx(2, "unknown mode %s", argv[1]);
}
