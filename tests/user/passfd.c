/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * passfd: checks descriptors passed over a UNIX-domain socket
 * (SCM_RIGHTS). Several travel in one message, a few or many, and each
 * arrives as the file it was; two messages sent together arrive as two;
 * a control buffer too small for a message gets the whole messages that
 * fit and MSG_CTRUNC, and the descriptors of the rest are closed, as are
 * all of them when the buffer cannot be written (EFAULT); a message
 * naming a descriptor that is not open fails with EBADF and sends
 * nothing.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("passfd: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	MANY	64

static int sv[2];
static int nullfd;

/* How many descriptors are open. */
static int
nopen(void)
{
	int n = 0;

	for (int fd = 0; fd < 1024; fd++)
		if (fcntl(fd, F_GETFD) != -1)
			n++;
	return (n);
}

/* Whether `fd` is open on the file `nullfd` is. */
static int
same(int fd)
{
	struct stat a, b;

	return (fstat(fd, &a) == 0 && fstat(nullfd, &b) == 0 &&
	    a.st_dev == b.st_dev && a.st_ino == b.st_ino);
}

/* Sends one byte with `msgs` messages of `each` descriptors apiece. */
static ssize_t
send_fds(int msgs, int each, int fd)
{
	char space[2 * CMSG_SPACE(MANY * sizeof(int))];
	char byte = 'x';
	struct iovec iov = { &byte, 1 };
	struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1,
	    .msg_control = space };
	struct cmsghdr *c;

	memset(space, 0, sizeof(space));
	m.msg_controllen = msgs * CMSG_SPACE(each * sizeof(int));
	c = CMSG_FIRSTHDR(&m);
	for (int i = 0; i < msgs; i++) {
		c->cmsg_level = SOL_SOCKET;
		c->cmsg_type = SCM_RIGHTS;
		c->cmsg_len = CMSG_LEN(each * sizeof(int));
		for (int k = 0; k < each; k++)
			memcpy(CMSG_DATA(c) + k * sizeof(int), &fd,
			    sizeof(int));
		c = CMSG_NXTHDR(&m, c);
	}
	return (sendmsg(sv[0], &m, 0));
}

/*
 * Receives into `space` of `len` bytes; checks and closes the
 * descriptors that came. Returns how many, and the messages and flags.
 */
static int
recv_fds(void *space, size_t len, int *msgs, int *flags)
{
	char byte;
	struct iovec iov = { &byte, 1 };
	struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1,
	    .msg_control = space, .msg_controllen = len };
	int n = 0;

	*msgs = 0;
	*flags = 0;
	if (recvmsg(sv[1], &m, 0) != 1)
		return (-1);
	*flags = m.msg_flags;
	for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c != NULL;
	    c = CMSG_NXTHDR(&m, c)) {
		int k = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);

		CHECK(c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS,
		    "level %d type %d", c->cmsg_level, c->cmsg_type);
		(*msgs)++;
		for (int i = 0; i < k; i++) {
			int fd;

			memcpy(&fd, CMSG_DATA(c) + i * sizeof(int),
			    sizeof(int));
			CHECK(same(fd), "descriptor %d is another file", fd);
			close(fd);
			n++;
		}
	}
	return (n);
}

/* `msgs` messages of `each` descriptors arrive as sent. */
static void
whole(int msgs, int each)
{
	char space[2 * CMSG_SPACE(MANY * sizeof(int))];
	int before = nopen(), got, m, flags;

	CHECK(send_fds(msgs, each, nullfd) == 1, "%s", strerror(errno));
	got = recv_fds(space, sizeof(space), &m, &flags);
	CHECK(got == msgs * each, "%d of %d descriptors", got, msgs * each);
	CHECK(m == msgs, "%d messages of %d", m, msgs);
	CHECK((flags & MSG_CTRUNC) == 0, "flags %#x", flags);
	CHECK(nopen() == before, "%d open, %d before", nopen(), before);
}

/*
 * Two messages of one descriptor each, received with room for `room`
 * bytes of control: `fit` whole messages come and the rest are closed.
 */
static void
truncated(size_t room, int fit)
{
	char space[2 * CMSG_SPACE(sizeof(int))];
	int before = nopen(), got, m, flags;

	CHECK(send_fds(2, 1, nullfd) == 1, "%s", strerror(errno));
	got = recv_fds(space, room, &m, &flags);
	CHECK(got == fit && m == fit, "%d descriptors in %d messages, not %d",
	    got, m, fit);
	CHECK((flags & MSG_CTRUNC) != 0, "flags %#x", flags);
	CHECK(nopen() == before, "%d open, %d before", nopen(), before);
}

/* A control buffer that cannot be written: the descriptors are closed. */
static void
unwritable(void)
{
	char byte;
	struct iovec iov = { &byte, 1 };
	struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1,
	    .msg_control = (void *)8, .msg_controllen = 64 };
	int before = nopen();

	CHECK(send_fds(1, 2, nullfd) == 1, "%s", strerror(errno));
	CHECK(recvmsg(sv[1], &m, 0) == -1 && errno == EFAULT, "%s (%d)",
	    strerror(errno), errno);
	CHECK(nopen() == before, "%d open, %d before", nopen(), before);
}

/* A descriptor that is not open: EBADF, and nothing is sent. */
static void
bad(void)
{
	char byte;
	int closed = dup(nullfd);

	close(closed);
	CHECK(send_fds(1, 1, closed) == -1 && errno == EBADF, "%s (%d)",
	    strerror(errno), errno);
	CHECK(send_fds(1, 1, -1) == -1 && errno == EBADF, "%s (%d)",
	    strerror(errno), errno);
	CHECK(recv(sv[1], &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN,
	    "something was sent");
}

int
main(void)
{
	if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0 ||
	    (nullfd = open("/dev/null", O_RDONLY)) < 0) {
		printf("passfd: %s\n", strerror(errno));
		return (1);
	}
	whole(1, 1);
	whole(1, 3);
	whole(1, MANY);
	whole(2, 1);
	whole(2, MANY);
	truncated(CMSG_SPACE(sizeof(int)), 1);
	truncated(CMSG_SPACE(sizeof(int)) + CMSG_LEN(0), 1);
	truncated(CMSG_LEN(0), 0);
	unwritable();
	bad();
	if (failed)
		return (1);
	printf("passfd: ok\n");
	return (0);
}
