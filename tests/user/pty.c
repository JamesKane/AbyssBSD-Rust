/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * pty: checks pseudo-terminals from user mode, expect-style: a program
 * on the slave, this one on the master. The slave is named, owned, and
 * numbered as FreeBSD's are; typed lines echo and are read whole; output
 * is processed; ^C signals the slave's foreground group; a window size
 * set on the master reaches the slave with SIGWINCH; packet mode reports
 * flushes, stops, and starts; a megabyte goes each way through the full
 * queues; and a terminal's end is seen from both sides: the slave's last
 * close is the master's end of file, the master's close is the slave's,
 * and a session leader's exit, or revoke(2), leaves the slave's other
 * descriptors dead.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>

#include <errno.h>
#include <stdint.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("pty: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* A new master, its slave's name in `name`. */
static int
newpty(char *name, size_t len)
{
	int m;

	m = posix_openpt(O_RDWR | O_NOCTTY);
	if (m < 0 || grantpt(m) != 0 || unlockpt(m) != 0) {
		printf("pty: posix_openpt: %s\n", strerror(errno));
		exit(1);
	}
	strlcpy(name, ptsname(m), len);
	return (m);
}

/* Reads from `fd` until `want` has come, or 5 seconds; 0 if it did. */
static int
expect(int fd, const char *want)
{
	char buf[512];
	size_t have = 0, n = strlen(want);
	struct pollfd p = { .fd = fd, .events = POLLIN };

	while (have < n) {
		ssize_t r;

		if (poll(&p, 1, 5000) != 1)
			break;
		r = read(fd, buf + have, n - have);
		if (r <= 0)
			break;
		have += (size_t)r;
	}
	buf[have] = '\0';
	if (have != n || memcmp(buf, want, n) != 0) {
		printf("pty: expected '%s', got %zu bytes '%s'\n", want, have,
		    buf);
		return (-1);
	}
	return (0);
}

static void
naming(void)
{
	char name[64], dev[64];
	struct stat sm, ss;
	int m, s, m2;

	m = newpty(name, sizeof(name));
	CHECK(strncmp(name, "/dev/pts/", 9) == 0, "name '%s'", name);
	CHECK(ioctl(m, TIOCPTMASTER) == 0, "TIOCPTMASTER: %s",
	    strerror(errno));
	s = open(name, O_RDWR | O_NOCTTY);
	CHECK(s >= 0, "open %s: %s", name, strerror(errno));
	CHECK(ioctl(s, TIOCPTMASTER) == -1 && errno == ENOTTY,
	    "TIOCPTMASTER on the slave");
	CHECK(grantpt(s) == -1 && errno == EINVAL, "grantpt on the slave");
	CHECK(stat(name, &ss) == 0, "stat: %s", strerror(errno));
	CHECK(S_ISCHR(ss.st_mode) && (ss.st_mode & 07777) == 0620 &&
	    ss.st_uid == getuid() && ss.st_gid == 4, "mode %o owner %d:%d",
	    ss.st_mode, ss.st_uid, ss.st_gid);
	CHECK(fstat(m, &sm) == 0 && S_ISCHR(sm.st_mode) &&
	    sm.st_rdev == ss.st_rdev, "master's st_rdev %#jx, slave's %#jx",
	    (uintmax_t)sm.st_rdev, (uintmax_t)ss.st_rdev);
	CHECK(isatty(m) && isatty(s), "isatty");
	CHECK(ttyname_r(s, dev, sizeof(dev)) == 0 && strcmp(dev, name) == 0,
	    "ttyname '%s'", dev);
	/* Another gets another number; there is no /dev/ptmx (FreeBSD's
	 * `pty` module, not loaded). */
	m2 = newpty(dev, sizeof(dev));
	CHECK(strcmp(dev, name) != 0, "both '%s'", dev);
	CHECK(access("/dev/ptmx", F_OK) == -1, "/dev/ptmx");
	close(m2);
	close(s);
	close(m);
}

static void
lines(void)
{
	char name[64], buf[64];
	int m, s, n, flags;

	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	/* Typed at the master: echoed, then read whole on the slave. */
	CHECK(write(m, "hel", 3) == 3, "write");
	CHECK(expect(m, "hel") == 0, "echo");
	CHECK(ioctl(s, FIONREAD, &n) == 0 && n == 0, "FIONREAD %d", n);
	CHECK(write(m, "lo\r", 3) == 3, "write");
	CHECK(expect(m, "lo\r\n") == 0, "echo of the line's end");
	CHECK(ioctl(s, FIONREAD, &n) == 0 && n == 6, "FIONREAD %d", n);
	memset(buf, 0, sizeof(buf));
	CHECK(read(s, buf, sizeof(buf)) == 6 && strcmp(buf, "hello\n") == 0,
	    "line '%s'", buf);
	/* Written to the slave: NL becomes CR NL. */
	CHECK(write(s, "a\nb", 3) == 3, "write");
	CHECK(ioctl(m, FIONREAD, &n) == 0 && n == 4, "master FIONREAD %d", n);
	CHECK(expect(m, "a\r\nb") == 0, "output");
	/* Nothing more: a non-blocking read fails. */
	flags = fcntl(m, F_GETFL);
	fcntl(m, F_SETFL, flags | O_NONBLOCK);
	CHECK(read(m, buf, sizeof(buf)) == -1 && errno == EAGAIN,
	    "empty master");
	fcntl(m, F_SETFL, flags);
	close(s);
	close(m);
}

static volatile sig_atomic_t got_int, got_winch;

static void
on_int(int sig)
{
	(void)sig;
	got_int++;
}

static void
on_winch(int sig)
{
	(void)sig;
	got_winch++;
}

/* The slave's side of `signals`: a session on it, reporting. */
static int
signals_child(const char *name)
{
	struct sigaction sa;
	struct winsize w;
	char buf[64];
	int s;

	setsid();
	s = open(name, O_RDWR);
	if (s < 0 || ioctl(s, TIOCSCTTY, 0) != 0)
		return (2);
	/* Not restarting: the read must see the signal. */
	memset(&sa, 0, sizeof(sa));
	sigemptyset(&sa.sa_mask);
	sa.sa_handler = on_int;
	sigaction(SIGINT, &sa, NULL);
	sa.sa_handler = on_winch;
	sigaction(SIGWINCH, &sa, NULL);
	write(s, "ready\n", 6);
	/* ^C ends the read; the handler saw it. */
	if (read(s, buf, sizeof(buf)) != -1 || errno != EINTR || got_int != 1)
		return (3);
	write(s, "int\n", 4);
	while (got_winch == 0)
		usleep(10000);
	if (ioctl(s, TIOCGWINSZ, &w) != 0 || w.ws_row != 33 || w.ws_col != 77)
		return (4);
	write(s, "winch\n", 6);
	return (0);
}

static void
signals(void)
{
	char name[64];
	struct winsize w = { .ws_row = 33, .ws_col = 77 };
	int m, st;
	pid_t pid;

	m = newpty(name, sizeof(name));
	pid = fork();
	if (pid == 0)
		_exit(signals_child(name));
	CHECK(expect(m, "ready\r\n") == 0, "child ready");
	CHECK(write(m, "\003", 1) == 1, "^C");
	CHECK(expect(m, "^C") == 0, "^C echoed");
	CHECK(expect(m, "int\r\n") == 0, "SIGINT reached the child");
	CHECK(ioctl(m, TIOCSWINSZ, &w) == 0, "TIOCSWINSZ");
	CHECK(expect(m, "winch\r\n") == 0, "SIGWINCH reached the child");
	CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
	    WEXITSTATUS(st) == 0, "child %#x", st);
	close(m);
}

static void
packet(void)
{
	char name[64], buf[64];
	int m, s, on = 1;

	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	CHECK(ioctl(m, TIOCPKT, &on) == 0, "TIOCPKT");
	/* Data comes after a TIOCPKT_DATA byte. */
	write(s, "x", 1);
	CHECK(read(m, buf, sizeof(buf)) == 2 && buf[0] == TIOCPKT_DATA &&
	    buf[1] == 'x', "data packet %#x", buf[0]);
	/* Flushing the slave's output is reported alone. */
	write(s, "y", 1);
	tcflush(s, TCOFLUSH);
	CHECK(read(m, buf, sizeof(buf)) == 1 &&
	    (buf[0] & TIOCPKT_FLUSHWRITE) != 0, "flush packet %#x", buf[0]);
	/* So are stopping and starting output. */
	tcflow(s, TCOOFF);
	CHECK(read(m, buf, sizeof(buf)) == 1 && (buf[0] & TIOCPKT_STOP) != 0,
	    "stop packet %#x", buf[0]);
	tcflow(s, TCOON);
	CHECK(read(m, buf, sizeof(buf)) == 1 && (buf[0] & TIOCPKT_START) != 0,
	    "start packet %#x", buf[0]);
	close(s);
	close(m);
}

#define	BULK	(1024 * 1024)

/* Moves BULK bytes from `from` to `to`, a child writing; 0 if all came
 * back as they went. */
static int
bulk(int from, int to)
{
	static char out[BULK], in[BULK];
	size_t got = 0;
	pid_t pid;
	int st;

	for (size_t i = 0; i < BULK; i++)
		out[i] = 'a' + (char)(i % 23);
	pid = fork();
	if (pid == 0) {
		size_t done = 0;

		while (done < BULK) {
			ssize_t n = write(from, out + done, BULK - done);

			if (n <= 0)
				_exit(1);
			done += (size_t)n;
		}
		_exit(0);
	}
	while (got < BULK) {
		ssize_t n = read(to, in + got, BULK - got);

		if (n <= 0)
			break;
		got += (size_t)n;
	}
	if (waitpid(pid, &st, 0) != pid || st != 0)
		return (-1);
	return (got == BULK && memcmp(in, out, BULK) == 0 ? 0 : -1);
}

static void
flow(void)
{
	char name[64];
	struct termios t;
	int m, s;

	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	tcgetattr(s, &t);
	cfmakeraw(&t);
	CHECK(tcsetattr(s, TCSANOW, &t) == 0, "raw");
	CHECK(bulk(s, m) == 0, "slave to master");
	CHECK(bulk(m, s) == 0, "master to slave");
	close(s);
	close(m);
}

static void
ends(void)
{
	char name[64], buf[16];
	struct pollfd p;
	struct kevent kev;
	struct timespec zero = { 0, 0 };
	int m, s, kq;

	/* The slave's last close: the master reads end of file, and what
	 * was queued is gone. */
	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	kq = kqueue();
	EV_SET(&kev, m, EVFILT_READ, EV_ADD, 0, 0, NULL);
	CHECK(kevent(kq, &kev, 1, NULL, 0, NULL) == 0, "kevent");
	write(s, "left", 4);
	CHECK(kevent(kq, NULL, 0, &kev, 1, &zero) == 1 && kev.data == 4,
	    "EVFILT_READ data %jd", (intmax_t)kev.data);
	close(s);
	CHECK(read(m, buf, sizeof(buf)) == 0, "master's end of file");
	p.fd = m;
	p.events = POLLIN;
	CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP) != 0,
	    "POLLHUP %#x", p.revents);
	CHECK(kevent(kq, NULL, 0, &kev, 1, &zero) == 1 &&
	    (kev.flags & EV_EOF) != 0, "EV_EOF");
	close(kq);
	close(m);

	/* The master's close: the slave reads end of file, writes fail. */
	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	close(m);
	CHECK(read(s, buf, sizeof(buf)) == 0, "slave's end of file");
	CHECK(write(s, "x", 1) == -1 && errno == ENXIO, "slave write: %s",
	    strerror(errno));
	p.fd = s;
	p.events = POLLIN;
	CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLHUP) != 0,
	    "slave POLLHUP %#x", p.revents);
	close(s);
	/* Its node goes (FreeBSD's `destroy_dev_sched`: soon). */
	for (int i = 0; i < 100 && access(name, F_OK) == 0; i++)
		usleep(10000);
	CHECK(access(name, F_OK) == -1 && errno == ENOENT, "%s still there",
	    name);
}

/* A session leader on `name` whose child outlives it: once told (on
 * `go`) that the leader is gone, the child reports on `report` whether
 * its descriptor is dead. */
static int
leader(const char *name, int go, int report)
{
	char buf[16];
	int s, ready[2];

	setsid();
	s = open(name, O_RDWR);
	if (s < 0 || ioctl(s, TIOCSCTTY, 0) != 0) {
		printf("pty: leader: %s: %s\n", s < 0 ? "open" : "TIOCSCTTY",
		    strerror(errno));
		return (2);
	}
	pipe(ready);
	if (fork() == 0) {
		/* Out of the foreground group, which gets SIGHUP. */
		setpgid(0, 0);
		signal(SIGHUP, SIG_IGN);
		signal(SIGTTOU, SIG_IGN);
		write(ready[1], "r", 1);
		read(go, buf, 1);
		buf[0] = read(s, buf, sizeof(buf)) == 0 ? 'r' : 'R';
		buf[1] = write(s, "x", 1) == -1 && errno == ENXIO ? 'w' : 'W';
		write(report, buf, 2);
		_exit(0);
	}
	read(ready[0], buf, 1);
	return (0);
}

static void
revocation(void)
{
	char name[64], buf[16];
	int m, s, s2, rep[2], go[2], st;
	pid_t pid;

	/* A session's end. */
	m = newpty(name, sizeof(name));
	pipe(rep);
	pipe(go);
	pid = fork();
	if (pid == 0)
		_exit(leader(name, go[0], rep[1]));
	close(rep[1]);
	close(go[0]);
	CHECK(waitpid(pid, &st, 0) == pid && st == 0, "leader %#x", st);
	write(go[1], "g", 1);
	close(go[1]);
	memset(buf, 0, sizeof(buf));
	CHECK(read(rep[0], buf, 2) == 2 && strcmp(buf, "rw") == 0,
	    "after the leader: '%s'", buf);
	close(rep[0]);
	/* Nothing has the slave open now: the master reads end of file. */
	CHECK(read(m, buf, sizeof(buf)) == 0, "master after the session");
	close(m);

	/* revoke(2), by the slave's owner. */
	m = newpty(name, sizeof(name));
	s = open(name, O_RDWR | O_NOCTTY);
	CHECK(revoke(name) == 0, "revoke: %s", strerror(errno));
	CHECK(read(s, buf, sizeof(buf)) == 0, "revoked read");
	CHECK(write(s, "x", 1) == -1 && errno == ENXIO, "revoked write: %s",
	    strerror(errno));
	CHECK(ioctl(s, TIOCGWINSZ, buf) == -1 && errno == ENOTTY,
	    "revoked ioctl: %s", strerror(errno));
	/* A new open works. */
	s2 = open(name, O_RDWR | O_NOCTTY);
	CHECK(s2 >= 0 && write(s2, "y", 1) == 1, "reopen: %s",
	    strerror(errno));
	CHECK(expect(m, "y") == 0, "after the reopen");
	CHECK(revoke("/") == -1 && errno == EINVAL, "revoke /: %s",
	    strerror(errno));
	close(s2);
	close(s);
	close(m);
}

int
main(void)
{
	/* Children end with _exit: nothing may sit in a buffer. */
	setvbuf(stdout, NULL, _IONBF, 0);
	alarm(120);
	naming();
	lines();
	signals();
	packet();
	flow();
	ends();
	revocation();
	if (failed)
		return (1);
	printf("pty: ok\n");
	return (0);
}
