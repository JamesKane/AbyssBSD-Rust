/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * tty: checks a terminal from user mode. A child starts a session, makes
 * the terminal its controlling terminal, and types into it (TIOCSTI):
 * canonical lines with erase and end of file, raw reads with VMIN and
 * VTIME, ^C reaching the foreground group, and job control: a background
 * read stops with SIGTTIN (or fails with EIO when it is ignored), a
 * background write with TOSTOP stops with SIGTTOU, and tcsetpgrp hands
 * the terminal over. A window size change sends SIGWINCH.
 *
 * Uses a pseudo-terminal where there is one (FreeBSD), else /dev/console
 * (AbyssBSD, until ptys). Restores the terminal's settings.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("tty: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

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

static long long
now_ms(void)
{
	struct timespec t;

	clock_gettime(CLOCK_MONOTONIC, &t);
	return (t.tv_sec * 1000LL + t.tv_nsec / 1000000);
}

static void
type(int fd, const char *s)
{
	for (; *s != '\0'; s++)
		if (ioctl(fd, TIOCSTI, s) != 0)
			printf("tty: TIOCSTI: %s\n", strerror(errno));
}

/* Reads once into a fresh buffer; returns the count. */
static ssize_t
read_some(int fd, char *buf, size_t len)
{
	memset(buf, 0, len);
	return (read(fd, buf, len - 1));
}

static void
canonical(int fd)
{
	char buf[64];
	ssize_t n;

	type(fd, "hello\n");
	n = read_some(fd, buf, sizeof(buf));
	CHECK(n == 6 && strcmp(buf, "hello\n") == 0, "read %zd '%s'", n, buf);
	/* Erase, then kill the line and type another. */
	type(fd, "abx\177c\n");
	n = read_some(fd, buf, sizeof(buf));
	CHECK(n == 4 && strcmp(buf, "abc\n") == 0, "erase: %zd '%s'", n, buf);
	type(fd, "junk\025ok\n");
	n = read_some(fd, buf, sizeof(buf));
	CHECK(n == 3 && strcmp(buf, "ok\n") == 0, "kill: %zd '%s'", n, buf);
	/* ^D at the start of a line: end of file. */
	type(fd, "\004");
	CHECK(read_some(fd, buf, sizeof(buf)) == 0, "end of file");
	/* Lines come one per read. */
	type(fd, "one\ntwo\n");
	CHECK(read_some(fd, buf, sizeof(buf)) == 4 && strcmp(buf, "one\n") == 0,
	    "first line '%s'", buf);
	CHECK(read_some(fd, buf, sizeof(buf)) == 4 && strcmp(buf, "two\n") == 0,
	    "second line '%s'", buf);
}

static void
raw(int fd, struct termios *base)
{
	struct termios t = *base;
	char buf[64];
	long long t0, waited;
	int n;

	t.c_lflag &= ~ICANON;
	t.c_cc[VMIN] = 0;
	t.c_cc[VTIME] = 0;
	CHECK(tcsetattr(fd, TCSANOW, &t) == 0, "tcsetattr");
	/* Nothing there: VMIN 0, VTIME 0 returns at once. */
	CHECK(read_some(fd, buf, sizeof(buf)) == 0, "empty raw read");
	type(fd, "xyz");
	CHECK(ioctl(fd, FIONREAD, &n) == 0 && n == 3, "FIONREAD %d", n);
	CHECK(read_some(fd, buf, sizeof(buf)) == 3 && strcmp(buf, "xyz") == 0,
	    "raw read '%s'", buf);
	/* VTIME 2: a read of nothing waits 0.2 s, then returns 0. */
	t.c_cc[VTIME] = 2;
	CHECK(tcsetattr(fd, TCSANOW, &t) == 0, "tcsetattr");
	t0 = now_ms();
	CHECK(read_some(fd, buf, sizeof(buf)) == 0, "timed read");
	waited = now_ms() - t0;
	CHECK(waited >= 150 && waited < 2000, "waited %lld ms", waited);
	CHECK(tcsetattr(fd, TCSANOW, base) == 0, "restore");
}

static void
signals(int fd)
{
	struct winsize w;

	signal(SIGINT, on_int);
	got_int = 0;
	type(fd, "\003");
	CHECK(got_int == 1, "^C: %d SIGINTs", (int)got_int);
	signal(SIGINT, SIG_DFL);

	signal(SIGWINCH, on_winch);
	got_winch = 0;
	CHECK(ioctl(fd, TIOCGWINSZ, &w) == 0, "TIOCGWINSZ");
	w.ws_row = 33;
	w.ws_col = 99;
	CHECK(ioctl(fd, TIOCSWINSZ, &w) == 0, "TIOCSWINSZ");
	CHECK(got_winch == 1, "SIGWINCH %d", (int)got_winch);
	memset(&w, 0, sizeof(w));
	CHECK(ioctl(fd, TIOCGWINSZ, &w) == 0 && w.ws_row == 33 &&
	    w.ws_col == 99, "winsize %dx%d", w.ws_row, w.ws_col);
	signal(SIGWINCH, SIG_DFL);
}

/* A child in a group of its own, in the background, doing `what`. */
static pid_t
background(int fd, int what)
{
	char c;
	pid_t pid;

	pid = fork();
	if (pid == 0) {
		setpgid(0, 0);
		switch (what) {
		case 'r':
			/*
			 * Stopped by SIGTTIN, then continued: the read fails
			 * with EINTR, and once in the foreground it reads.
			 */
			if (read(fd, &c, 1) != -1 || errno != EINTR)
				_exit(1);
			_exit(read(fd, &c, 1) == 1 ? 0 : 1);
		case 'i':
			signal(SIGTTIN, SIG_IGN);
			_exit(read(fd, &c, 1) == -1 && errno == EIO ? 0 : 1);
		case 'w':
			_exit(write(fd, "", 0) == 0 && write(fd, "x", 1) == 1 ?
			    0 : 1);
		}
		_exit(2);
	}
	setpgid(pid, pid);
	return (pid);
}

static void
job_control(int fd, struct termios *base)
{
	struct termios t = *base;
	int st;
	pid_t pid;
	char buf[8];

	/* A background read stops with SIGTTIN. */
	pid = background(fd, 'r');
	CHECK(waitpid(pid, &st, WUNTRACED) == pid && WIFSTOPPED(st) &&
	    WSTOPSIG(st) == SIGTTIN, "status %#x", st);
	/*
	 * Brought to the foreground and continued, it reads. (Typed first:
	 * typing from the background is a change that sends SIGTTOU.)
	 */
	type(fd, "k\n");
	CHECK(tcsetpgrp(fd, pid) == 0, "tcsetpgrp: %s", strerror(errno));
	CHECK(tcgetpgrp(fd) == pid, "tcgetpgrp");
	kill(pid, SIGCONT);
	CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
	    WEXITSTATUS(st) == 0, "foreground read: status %#x", st);
	/* The terminal back (from the background: SIGTTOU ignored). */
	signal(SIGTTOU, SIG_IGN);
	CHECK(tcsetpgrp(fd, getpgrp()) == 0, "tcsetpgrp back: %s",
	    strerror(errno));
	signal(SIGTTOU, SIG_DFL);
	read_some(fd, buf, sizeof(buf));

	/* Ignoring SIGTTIN, a background read fails with EIO. */
	pid = background(fd, 'i');
	CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
	    WEXITSTATUS(st) == 0, "EIO read: status %#x", st);

	/* With TOSTOP, a background write stops with SIGTTOU. */
	t.c_lflag |= TOSTOP;
	CHECK(tcsetattr(fd, TCSANOW, &t) == 0, "tcsetattr");
	pid = background(fd, 'w');
	CHECK(waitpid(pid, &st, WUNTRACED) == pid && WIFSTOPPED(st) &&
	    WSTOPSIG(st) == SIGTTOU, "status %#x", st);
	kill(pid, SIGKILL);
	waitpid(pid, &st, 0);
	CHECK(tcsetattr(fd, TCSANOW, base) == 0, "restore");
}

static int
session(const char *path)
{
	struct termios saved, t;
	int fd;

	/* Whoever started us may have had job control signals ignored. */
	signal(SIGTTIN, SIG_DFL);
	signal(SIGTTOU, SIG_DFL);
	signal(SIGTSTP, SIG_DFL);
	if (setsid() < 0) {
		printf("tty: setsid: %s\n", strerror(errno));
		return (1);
	}
	fd = open(path, O_RDWR);
	if (fd < 0) {
		printf("tty: %s: %s\n", path, strerror(errno));
		return (1);
	}
	CHECK(tcgetpgrp(fd) == -1 && errno == ENOTTY, "a terminal before "
	    "TIOCSCTTY");
	CHECK(ioctl(fd, TIOCSCTTY, 0) == 0, "TIOCSCTTY: %s", strerror(errno));
	CHECK(tcgetpgrp(fd) == getpgrp(), "tcgetpgrp %d", tcgetpgrp(fd));
	CHECK(tcgetsid(fd) == getpid(), "tcgetsid %d", tcgetsid(fd));
	CHECK(tcgetattr(fd, &saved) == 0, "tcgetattr");
	CHECK((saved.c_lflag & (ICANON | ISIG)) == (ICANON | ISIG) &&
	    saved.c_cc[VINTR] == 3 && saved.c_cc[VEOF] == 4,
	    "settings %#x", saved.c_lflag);
	/* No echo onto the console while typing at it. */
	t = saved;
	t.c_lflag &= ~(ECHO | ECHOE | ECHOK | ECHONL);
	CHECK(tcsetattr(fd, TCSAFLUSH, &t) == 0, "tcsetattr");
	canonical(fd);
	raw(fd, &t);
	signals(fd);
	job_control(fd, &t);
	tcsetattr(fd, TCSAFLUSH, &saved);
	return (failed);
}

int
main(void)
{
	char path[64] = "/dev/console";
	int master, st;
	pid_t pid;

	/* The session's child ends with _exit: nothing may sit in a buffer. */
	setvbuf(stdout, NULL, _IONBF, 0);
	master = posix_openpt(O_RDWR | O_NOCTTY);
	if (master >= 0) {
		if (grantpt(master) != 0 || unlockpt(master) != 0) {
			printf("tty: pty: %s\n", strerror(errno));
			return (1);
		}
		strlcpy(path, ptsname(master), sizeof(path));
	}
	pid = fork();
	if (pid == 0)
		_exit(session(path));
	if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) ||
	    WEXITSTATUS(st) != 0) {
		printf("tty: session ended %#x\n", st);
		return (1);
	}
	printf("tty: ok\n");
	return (0);
}
