/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * capsicum: checks Capsicum from user mode. A descriptor's rights only
 * narrow, travel with dup(2) and SCM_RIGHTS, and stop each operation they
 * lack (ENOTCAPABLE), as its fcntl and ioctl limits do; poll(2) reports a
 * descriptor without CAP_EVENT as POLLNVAL. In capability mode, calls not
 * marked for it fail with ECAPMODE, paths resolve only beneath a directory
 * descriptor (no AT_FDCWD, no absolute paths, no escaping ".."), a file
 * opened through a limited directory inherits its rights, signals go only
 * to the process itself, sysctl reads only what is marked for it (EPERM
 * otherwise, as 15.1 has it), and
 * PROC_TRAPCAP_CTL turns capability errors into SIGTRAP (TRAP_CAP).
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/capsicum.h>
#include <sys/filio.h>
#include <sys/procctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("capsicum: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* `call` fails with `err`. */
#define	FAILS(call, err)						\
	CHECK((call) == -1 && errno == (err), "%s (%d)", strerror(errno), errno)

static char dir[] = "/tmp/capsicum.XXXXXX";

/* A file in the test directory holding "hello", open as `flags`. */
static int
file(const char *name, int flags)
{
	char path[128];
	int fd;

	snprintf(path, sizeof(path), "%s/%s", dir, name);
	fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
	if (fd < 0 || write(fd, "hello", 5) != 5) {
		printf("capsicum: %s: %s\n", path, strerror(errno));
		exit(1);
	}
	close(fd);
	fd = open(path, flags);
	if (fd < 0) {
		printf("capsicum: %s: %s\n", path, strerror(errno));
		exit(1);
	}
	return (fd);
}

/* Runs `f` in a child, which exits with whether it passed. */
static void
child(const char *what, void (*f)(void))
{
	pid_t pid;
	int status;

	fflush(stdout);
	pid = fork();
	if (pid == 0) {
		failed = 0;
		f();
		fflush(stdout);
		_exit(failed);
	}
	CHECK(waitpid(pid, &status, 0) == pid, "%s: waitpid: %s", what,
	    strerror(errno));
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
	    "%s: child status %#x", what, status);
}

static void
rights(void)
{
	cap_rights_t r, got;
	char buf[8];
	int fd, fd2;
	unsigned long cmds[4];
	uint32_t fcntls;

	fd = file("rights", O_RDWR);
	CHECK(cap_rights_get(fd, &got) == 0, "%s", strerror(errno));
	CHECK(cap_rights_is_set(&got, CAP_READ, CAP_WRITE, CAP_IOCTL),
	    "a new descriptor has every right");
	cap_rights_init(&r, CAP_READ, CAP_SEEK, CAP_FSTAT, CAP_FCNTL);
	CHECK(cap_rights_limit(fd, &r) == 0, "%s", strerror(errno));
	CHECK(pread(fd, buf, 5, 0) == 5, "%s", strerror(errno));
	FAILS(write(fd, "x", 1), ENOTCAPABLE);
	FAILS(ftruncate(fd, 0), ENOTCAPABLE);
	/* Rights only narrow. */
	cap_rights_init(&r, CAP_READ, CAP_WRITE);
	FAILS(cap_rights_limit(fd, &r), ENOTCAPABLE);
	CHECK(cap_rights_get(fd, &got) == 0, "%s", strerror(errno));
	CHECK(cap_rights_is_set(&got, CAP_READ, CAP_SEEK) &&
	    !cap_rights_is_set(&got, CAP_WRITE), "the rights stay");
	/* Without CAP_IOCTL, no ioctl stays allowed. */
	FAILS(ioctl(fd, FIOCLEX), ENOTCAPABLE);
	CHECK(cap_ioctls_get(fd, cmds, 4) == 0, "%s", strerror(errno));
	/* dup keeps them. */
	fd2 = dup(fd);
	CHECK(fd2 >= 0, "%s", strerror(errno));
	FAILS(write(fd2, "x", 1), ENOTCAPABLE);
	CHECK(fcntl(fd2, F_GETFL) == O_RDWR, "%s", strerror(errno));
	CHECK(cap_fcntls_limit(fd2, CAP_FCNTL_SETFL) == 0, "%s",
	    strerror(errno));
	FAILS(fcntl(fd2, F_GETFL), ENOTCAPABLE);
	FAILS(cap_fcntls_limit(fd2, CAP_FCNTL_GETFL), ENOTCAPABLE);
	CHECK(cap_fcntls_get(fd2, &fcntls) == 0 && fcntls == CAP_FCNTL_SETFL,
	    "%#x", fcntls);
	/* The original's fcntl limits are its own. */
	CHECK(fcntl(fd, F_GETFL) == O_RDWR, "%s", strerror(errno));
	close(fd2);
	close(fd);

	/* An ioctl list. */
	fd = file("ioctls", O_RDWR);
	cmds[0] = FIONREAD;
	CHECK(cap_ioctls_limit(fd, cmds, 1) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, FIONREAD, &(int){ 0 }) == 0, "%s", strerror(errno));
	FAILS(ioctl(fd, FIOCLEX), ENOTCAPABLE);
	cmds[1] = FIOCLEX;
	FAILS(cap_ioctls_limit(fd, cmds, 2), ENOTCAPABLE);
	CHECK(cap_ioctls_get(fd, cmds, 4) == 1 && cmds[0] == FIONREAD,
	    "%s", strerror(errno));
	close(fd);
	fd = file("ioctls", O_RDWR);
	CHECK(cap_ioctls_get(fd, NULL, 0) == CAP_IOCTLS_ALL, "every ioctl");
	close(fd);
}

static void
events(void)
{
	cap_rights_t r;
	struct pollfd p;
	int fd;

	fd = file("events", O_RDONLY);
	cap_rights_init(&r, CAP_READ);
	CHECK(cap_rights_limit(fd, &r) == 0, "%s", strerror(errno));
	p.fd = fd;
	p.events = POLLIN;
	CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLNVAL, "revents %#x",
	    p.revents);
	cap_rights_init(&r, CAP_READ, CAP_EVENT);
	fd = file("events", O_RDONLY);
	CHECK(cap_rights_limit(fd, &r) == 0, "%s", strerror(errno));
	p.fd = fd;
	CHECK(poll(&p, 1, 0) == 1 && p.revents == POLLIN, "revents %#x",
	    p.revents);
	close(fd);
}

static void
passing(void)
{
	cap_rights_t r, got;
	char space[CMSG_SPACE(sizeof(int))];
	struct msghdr m;
	struct cmsghdr *c;
	struct iovec iov;
	char byte = 'x';
	int fd, sv[2];

	CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "%s",
	    strerror(errno));
	fd = file("passing", O_RDWR);
	cap_rights_init(&r, CAP_READ, CAP_FSTAT);
	CHECK(cap_rights_limit(fd, &r) == 0, "%s", strerror(errno));
	memset(&m, 0, sizeof(m));
	iov.iov_base = &byte;
	iov.iov_len = 1;
	m.msg_iov = &iov;
	m.msg_iovlen = 1;
	m.msg_control = space;
	m.msg_controllen = sizeof(space);
	c = CMSG_FIRSTHDR(&m);
	c->cmsg_level = SOL_SOCKET;
	c->cmsg_type = SCM_RIGHTS;
	c->cmsg_len = CMSG_LEN(sizeof(int));
	memcpy(CMSG_DATA(c), &fd, sizeof(int));
	CHECK(sendmsg(sv[0], &m, 0) == 1, "%s", strerror(errno));
	close(fd);
	memset(space, 0, sizeof(space));
	m.msg_controllen = sizeof(space);
	CHECK(recvmsg(sv[1], &m, 0) == 1, "%s", strerror(errno));
	c = CMSG_FIRSTHDR(&m);
	CHECK(c != NULL && c->cmsg_type == SCM_RIGHTS, "no descriptor came");
	if (c != NULL) {
		memcpy(&fd, CMSG_DATA(c), sizeof(int));
		CHECK(cap_rights_get(fd, &got) == 0, "%s", strerror(errno));
		CHECK(cap_rights_is_set(&got, CAP_READ, CAP_FSTAT) &&
		    !cap_rights_is_set(&got, CAP_WRITE),
		    "the rights went with it");
		FAILS(write(fd, "x", 1), ENOTCAPABLE);
		close(fd);
	}
	close(sv[0]);
	close(sv[1]);
}

/* In capability mode. */
static void
sandboxed(void)
{
	cap_rights_t r, got;
	char ostype[32];
	size_t len;
	u_int mode;
	int d, fd, openfiles;

	d = open(dir, O_RDONLY | O_DIRECTORY);
	CHECK(d >= 0, "%s", strerror(errno));
	(void)close(file("inside", O_RDONLY));
	CHECK(cap_getmode(&mode) == 0 && mode == 0, "not yet");
	CHECK(cap_enter() == 0, "%s", strerror(errno));
	CHECK(cap_getmode(&mode) == 0 && mode == 1, "in capability mode");
	CHECK(cap_enter() == 0, "%s", strerror(errno));
	FAILS(open("/etc/passwd", O_RDONLY), ECAPMODE);
	FAILS(openat(AT_FDCWD, "inside", O_RDONLY), ECAPMODE);
	FAILS(openat(d, "/etc/passwd", O_RDONLY), ENOTCAPABLE);
	FAILS(openat(d, "../etc/passwd", O_RDONLY), ENOTCAPABLE);
	fd = openat(d, "inside", O_RDONLY);
	CHECK(fd >= 0, "%s", strerror(errno));
	close(fd);
	FAILS(chdir("/"), ECAPMODE);
	/* Signals only to itself. */
	CHECK(kill(getpid(), 0) == 0, "%s", strerror(errno));
	FAILS(kill(getppid(), 0), ECAPMODE);
	/* Some sysctl nodes, not others. */
	len = sizeof(ostype);
	CHECK(sysctlbyname("kern.ostype", ostype, &len, NULL, 0) == 0,
	    "%s", strerror(errno));
	len = sizeof(openfiles);
	FAILS(sysctlbyname("kern.openfiles", &openfiles, &len, NULL, 0), EPERM);
	/* Through a limited directory, files inherit its rights. */
	cap_rights_init(&r, CAP_LOOKUP, CAP_READ, CAP_FSTAT);
	CHECK(cap_rights_limit(d, &r) == 0, "%s", strerror(errno));
	FAILS(openat(d, "inside", O_RDWR), ENOTCAPABLE);
	fd = openat(d, "inside", O_RDONLY);
	CHECK(fd >= 0, "%s", strerror(errno));
	CHECK(cap_rights_get(fd, &got) == 0, "%s", strerror(errno));
	CHECK(cap_rights_contains(&r, &got) && cap_rights_contains(&got, &r),
	    "inherited");
	close(fd);
	close(d);
}

static volatile int trapped;
static volatile int trap_code, trap_errno;

static void
on_trap(int sig, siginfo_t *si, void *ctx)
{
	(void)sig;
	(void)ctx;
	trapped++;
	trap_code = si->si_code;
	trap_errno = si->si_errno;
}

static void
trapping(void)
{
	struct sigaction sa;
	int on = PROC_TRAPCAP_CTL_ENABLE, status;

	memset(&sa, 0, sizeof(sa));
	sa.sa_sigaction = on_trap;
	sa.sa_flags = SA_SIGINFO;
	CHECK(sigaction(SIGTRAP, &sa, NULL) == 0, "%s", strerror(errno));
	CHECK(procctl(P_PID, 0, PROC_TRAPCAP_CTL, &on) == 0, "%s",
	    strerror(errno));
	CHECK(procctl(P_PID, 0, PROC_TRAPCAP_STATUS, &status) == 0 &&
	    status == PROC_TRAPCAP_CTL_ENABLE, "status %d", status);
	/* Outside capability mode, nothing traps. */
	FAILS(open("/nonexistent/x", O_RDONLY), ENOENT);
	CHECK(trapped == 0, "no trap yet");
	CHECK(cap_enter() == 0, "%s", strerror(errno));
	FAILS(open("/etc/passwd", O_RDONLY), ECAPMODE);
	CHECK(trapped == 1 && trap_code == TRAP_CAP && trap_errno == ECAPMODE,
	    "trapped %d, code %d, errno %d", trapped, trap_code, trap_errno);
}

int
main(void)
{
	if (mkdtemp(dir) == NULL) {
		printf("capsicum: mkdtemp: %s\n", strerror(errno));
		return (1);
	}
	rights();
	events();
	passing();
	child("sandboxed", sandboxed);
	child("trapping", trapping);
	{
		char path[128];
		const char *names[] = { "rights", "ioctls", "events",
		    "passing", "inside" };
		for (size_t i = 0; i < nitems(names); i++) {
			snprintf(path, sizeof(path), "%s/%s", dir, names[i]);
			(void)unlink(path);
		}
		(void)rmdir(dir);
	}
	if (failed)
		return (1);
	printf("capsicum: ok\n");
	return (0);
}
