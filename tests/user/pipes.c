/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * pipes: checks pipes from user mode: both ends read and write, the end
 * of file once the writer closes, EPIPE and SIGPIPE once the reader
 * closes, non-blocking ends, a buffer's capacity, writes of PIPE_BUF
 * bytes staying whole between two writers, a large transfer between
 * processes, EINTR against SA_RESTART, and close-on-exec across execve.
 *
 * Runs on FreeBSD too. Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/filio.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("pipes: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	FAILS(r, want) do {						\
	errno = 0;							\
	long r_ = (long)(r);						\
	int e_ = errno;							\
	CHECK(r_ == -1 && e_ == (want), "got %ld (%s), want %s", r_,	\
	    strerror(e_), strerror(want));				\
} while (0)

static volatile sig_atomic_t caught;

static void
handler(int sig)
{
	caught = sig;
}

static int
exited_ok(pid_t pid)
{
	int status;

	return (waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0);
}

static void
basics(void)
{
	int p[2], n;
	char buf[16];
	struct stat st;

	CHECK(pipe(p) == 0, "pipe: %s", strerror(errno));
	CHECK(write(p[1], "hello", 5) == 5, "write");
	CHECK(fstat(p[0], &st) == 0 && S_ISFIFO(st.st_mode) &&
	    st.st_size == 5, "fstat mode %o size %jd", st.st_mode,
	    (intmax_t)st.st_size);
	CHECK(ioctl(p[0], FIONREAD, &n) == 0 && n == 5, "FIONREAD %d", n);
	CHECK(read(p[0], buf, sizeof(buf)) == 5 && memcmp(buf, "hello", 5) == 0,
	    "read");
	/* Both ends go both ways. */
	CHECK(write(p[0], "back", 4) == 4 && read(p[1], buf, 4) == 4 &&
	    memcmp(buf, "back", 4) == 0, "reverse direction");
	FAILS(lseek(p[0], 0, SEEK_SET), ESPIPE);
	FAILS(pread(p[0], buf, 1, 0), ESPIPE);
	/* The writer closes: what is left, then the end. */
	CHECK(write(p[1], "xy", 2) == 2, "write");
	close(p[1]);
	CHECK(read(p[0], buf, sizeof(buf)) == 2, "leftover");
	CHECK(read(p[0], buf, sizeof(buf)) == 0, "end of file");
	close(p[0]);
}

static void
broken(void)
{
	int p[2];

	CHECK(pipe(p) == 0, "pipe");
	close(p[0]);
	signal(SIGPIPE, SIG_IGN);
	FAILS(write(p[1], "x", 1), EPIPE);
	signal(SIGPIPE, handler);
	caught = 0;
	FAILS(write(p[1], "x", 1), EPIPE);
	CHECK(caught == SIGPIPE, "no SIGPIPE");
	signal(SIGPIPE, SIG_DFL);
	close(p[1]);
}

static void
nonblocking(void)
{
	int p[2], total = 0, r;
	char buf[1024];

	CHECK(pipe2(p, O_NONBLOCK | O_CLOEXEC) == 0, "pipe2");
	CHECK(fcntl(p[0], F_GETFL) == (O_RDWR | O_NONBLOCK), "F_GETFL %#x",
	    fcntl(p[0], F_GETFL));
	CHECK(fcntl(p[0], F_GETFD) == FD_CLOEXEC, "O_CLOEXEC");
	FAILS(read(p[0], buf, sizeof(buf)), EAGAIN);
	memset(buf, 'z', sizeof(buf));
	while ((r = write(p[1], buf, sizeof(buf))) > 0)
		total += r;
	CHECK(r == -1 && errno == EAGAIN, "full: %s", strerror(errno));
	/* The buffer grew from PIPE_SIZE to BIG_PIPE_SIZE. */
	CHECK(total == 65536, "capacity %d", total);
	/* A small write waits for room for all of it: none now. */
	FAILS(write(p[1], buf, 1), EAGAIN);
	CHECK(read(p[0], buf, 100) == 100, "drain");
	/* 100 free: a PIPE_BUF write does not fit whole, a short one does. */
	FAILS(write(p[1], buf, PIPE_BUF), EAGAIN);
	CHECK(write(p[1], buf, 100) == 100, "short write");
	FAILS(pipe2(p, 0x80000000), EINVAL);
	close(p[0]);
	close(p[1]);
}

/* A child that writes `n` bytes of a pattern, then exits. */
static void
pattern_child(int fd, int n)
{
	char buf[4096];
	int i, done = 0;

	while (done < n) {
		int chunk = n - done < (int)sizeof(buf) ? n - done :
		    (int)sizeof(buf);
		for (i = 0; i < chunk; i++)
			buf[i] = (char)((done + i) % 251);
		if (write(fd, buf, chunk) != chunk)
			_exit(1);
		done += chunk;
	}
	_exit(0);
}

static void
transfer(void)
{
	int p[2], got = 0, r, i, bad = 0;
	char buf[3000];
	pid_t pid;
	const int n = 300000;

	CHECK(pipe(p) == 0, "pipe");
	pid = fork();
	if (pid == 0) {
		close(p[0]);
		pattern_child(p[1], n);
	}
	close(p[1]);
	while ((r = read(p[0], buf, sizeof(buf))) > 0) {
		for (i = 0; i < r; i++)
			bad += buf[i] != (char)((got + i) % 251);
		got += r;
	}
	CHECK(r == 0 && got == n && bad == 0, "got %d of %d, %d wrong", got,
	    n, bad);
	CHECK(exited_ok(pid), "writer");
	close(p[0]);
}

static void
atomic_writes(void)
{
	int p[2], i, w, r, have = 0, blocks = 0, mixed = 0;
	char buf[PIPE_BUF * 8], block[PIPE_BUF];
	pid_t pids[2];

	CHECK(pipe(p) == 0, "pipe");
	for (w = 0; w < 2; w++) {
		pids[w] = fork();
		if (pids[w] == 0) {
			close(p[0]);
			memset(block, 'a' + w, sizeof(block));
			for (i = 0; i < 200; i++)
				if (write(p[1], block, sizeof(block)) !=
				    (ssize_t)sizeof(block))
					_exit(1);
			_exit(0);
		}
	}
	close(p[1]);
	while ((r = read(p[0], buf + have, sizeof(buf) - have)) > 0) {
		have += r;
		while (have >= PIPE_BUF) {
			for (i = 1; i < PIPE_BUF; i++)
				mixed += buf[i] != buf[0];
			memmove(buf, buf + PIPE_BUF, have - PIPE_BUF);
			have -= PIPE_BUF;
			blocks++;
		}
	}
	CHECK(blocks == 400 && have == 0 && mixed == 0,
	    "%d blocks, %d left, %d mixed bytes", blocks, have, mixed);
	CHECK(exited_ok(pids[0]) && exited_ok(pids[1]), "writers");
	close(p[0]);
}

static void
interrupted(void)
{
	struct sigaction sa;
	struct timespec ms50 = { 0, 50000000 };
	int p[2];
	char c;
	pid_t pid, me = getpid();

	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = handler;
	sigaction(SIGUSR1, &sa, NULL);
	CHECK(pipe(p) == 0, "pipe");
	/* No SA_RESTART: the read fails. */
	pid = fork();
	if (pid == 0) {
		nanosleep(&ms50, NULL);
		kill(me, SIGUSR1);
		_exit(0);
	}
	caught = 0;
	FAILS(read(p[0], &c, 1), EINTR);
	CHECK(caught == SIGUSR1, "handler did not run");
	CHECK(exited_ok(pid), "signaller");
	/* SA_RESTART: the read goes on, and gets what comes next. */
	sa.sa_flags = SA_RESTART;
	sigaction(SIGUSR1, &sa, NULL);
	pid = fork();
	if (pid == 0) {
		nanosleep(&ms50, NULL);
		kill(me, SIGUSR1);
		nanosleep(&ms50, NULL);
		write(p[1], "r", 1);
		_exit(0);
	}
	caught = 0;
	CHECK(read(p[0], &c, 1) == 1 && c == 'r', "restarted read");
	CHECK(caught == SIGUSR1, "handler did not run");
	CHECK(exited_ok(pid), "signaller");
	signal(SIGUSR1, SIG_DFL);
	close(p[0]);
	close(p[1]);
}

/* Run again as `pipes exec <kept> <closed>`: check what survived. */
static int
after_exec(int kept, int closed)
{
	if (fcntl(kept, F_GETFD) != 0 || write(kept, "k", 1) != 1)
		return (2);
	if (fcntl(closed, F_GETFD) != -1 || errno != EBADF)
		return (3);
	return (0);
}

static void
close_on_exec(const char *self)
{
	int keep[2], gone[2];
	char a[16], b[16], c;
	pid_t pid;

	CHECK(pipe(keep) == 0 && pipe2(gone, O_CLOEXEC) == 0, "pipes");
	pid = fork();
	if (pid == 0) {
		snprintf(a, sizeof(a), "%d", keep[1]);
		snprintf(b, sizeof(b), "%d", gone[1]);
		execl(self, "pipes", "exec", a, b, (char *)NULL);
		_exit(9);
	}
	close(keep[1]);
	close(gone[1]);
	CHECK(read(keep[0], &c, 1) == 1 && c == 'k', "kept descriptor");
	/* The child's copy closed at exec: only ours held it, and it went. */
	CHECK(read(gone[0], &c, 1) == 0, "close-on-exec descriptor");
	CHECK(exited_ok(pid), "exec'd child");
	close(keep[0]);
	close(gone[0]);
}

int
main(int argc, char **argv)
{
	if (argc == 4 && strcmp(argv[1], "exec") == 0)
		return (after_exec(atoi(argv[2]), atoi(argv[3])));
	basics();
	broken();
	nonblocking();
	transfer();
	atomic_writes();
	interrupted();
	close_on_exec(argc > 0 && argv[0][0] == '/' ? argv[0] :
	    "/usr/tests/abyss/pipes");
	if (!failed)
		printf("pipes: ok\n");
	return (failed);
}
