/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * pdfork: checks process descriptors from user mode: pdfork returns the
 * child's pid and a descriptor, pdgetpid reads it back, pdkill signals,
 * the child's exit sends no SIGCHLD and is still collected by waitpid on
 * its pid, closing the last descriptor kills a child (but not with
 * PD_DAEMON), and fstat describes the descriptor.
 *
 * Runs on FreeBSD too. Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/procdesc.h>
#include <sys/stat.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("pdfork: line %d: %s: ", __LINE__, #cond);	\
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

static volatile sig_atomic_t chld;

static void
on_chld(int sig)
{
	(void)sig;
	chld++;
}

static void
sleep_ms(int ms)
{
	struct timespec t = { 0, ms * 1000000L };

	nanosleep(&t, NULL);
}

int
main(void)
{
	struct stat st;
	int fd, fd2, status, p[2];
	pid_t pid, got;
	char c;

	signal(SIGCHLD, on_chld);

	/* An exit: no SIGCHLD, but waitpid on the pid collects it. */
	pid = pdfork(&fd, 0);
	if (pid == 0)
		_exit(5);
	CHECK(pid > 0 && fd >= 0, "pdfork: %s", strerror(errno));
	CHECK(pdgetpid(fd, &got) == 0 && got == pid, "pdgetpid %d", got);
	CHECK(fstat(fd, &st) == 0 && S_ISREG(st.st_mode), "fstat mode %o",
	    st.st_mode);
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 5, "status %#x", status);
	CHECK(chld == 0, "SIGCHLD from a pdfork child");
	CHECK(close(fd) == 0, "close");

	/* waitpid(-1) does not see a process-descriptor child. */
	pid = pdfork(&fd, 0);
	if (pid == 0)
		_exit(0);
	sleep_ms(50);
	CHECK(waitpid(-1, &status, WNOHANG) <= 0, "waitpid(-1) reaped it");
	CHECK(waitpid(pid, &status, 0) == pid, "waitpid(pid)");
	close(fd);

	/* pdkill signals; the status says so. */
	pid = pdfork(&fd, 0);
	if (pid == 0) {
		for (;;)
			pause();
	}
	CHECK(pdkill(fd, SIGTERM) == 0, "pdkill: %s", strerror(errno));
	CHECK(waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) &&
	    WTERMSIG(status) == SIGTERM, "status %#x", status);
	FAILS(pdkill(fd, 1000), EINVAL);
	close(fd);

	/*
	 * Closing the last descriptor kills the child: its end of a pipe
	 * closes. A dup keeps it alive until both are gone.
	 */
	CHECK(pipe(p) == 0, "pipe");
	pid = pdfork(&fd, PD_CLOEXEC);
	if (pid == 0) {
		close(p[0]);
		for (;;)
			pause();
	}
	close(p[1]);
	CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC, "PD_CLOEXEC");
	fd2 = dup(fd);
	close(fd);
	sleep_ms(50);
	CHECK(kill(pid, 0) == 0, "child died with a descriptor open");
	close(fd2);
	CHECK(read(p[0], &c, 1) == 0, "child still running");
	close(p[0]);

	/* PD_DAEMON: closing leaves it running. */
	pid = pdfork(&fd, PD_DAEMON);
	if (pid == 0) {
		sleep_ms(100);
		_exit(0);
	}
	close(fd);
	CHECK(kill(pid, 0) == 0, "PD_DAEMON child killed");

	pid = pdfork(&fd, 0x100);
	CHECK(pid == -1 && errno == EINVAL, "bad flags: %d", pid);
	FAILS(pdgetpid(0, &got), EBADF);

	if (!failed)
		printf("pdfork: ok\n");
	return (failed);
}
