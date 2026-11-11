/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * aio: checks POSIX asynchronous I/O from user mode, as FreeBSD behaves,
 * on what is safe without vfs.aio.enable_unsafe (FreeBSD's own aio tests
 * skip their file cases unless it is set): reads and writes on a regular
 * file, plain and vectored, at an offset; aio_error, aio_return,
 * aio_suspend, aio_waitcomplete; aio_fsync waiting for earlier writes;
 * lio_listio waiting and not; completion by signal (SI_ASYNCIO, the
 * value) and by kevent (EVFILT_AIO, EVFILT_LIO); a socket request that
 * waits, is cancelled, blocks listen, and is cancelled at exit; a pipe,
 * refused without vfs.aio.enable_unsafe and served with it (set for the
 * check and put back); argument errors; the sysctls and sysconf limits;
 * and the messages counted in getrusage. Run as root.
 *
 * Exits 0 when every check holds; prints each one that fails.
 */

#include <sys/types.h>
#include <sys/event.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/sysctl.h>
#include <sys/un.h>
#include <sys/wait.h>

#include <aio.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failures;

#define CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("aio: " __VA_ARGS__);				\
		printf(" (line %d, errno %d)\n", __LINE__, errno);	\
		failures++;						\
	}								\
} while (0)

/* Waits for one request and returns its result. */
static ssize_t
finish(struct aiocb *cb)
{
	const struct aiocb *list[1] = { cb };

	while (aio_error(cb) == EINPROGRESS)
		aio_suspend(list, 1, NULL);
	return (aio_return(cb));
}

static void
file_io(int fd)
{
	struct aiocb cb, *done;
	struct iovec iov[2];
	char out[8192], in[8192], a[100], b[300];
	ssize_t n;
	int i;

	for (i = 0; i < (int)sizeof(out); i++)
		out[i] = (char)(i * 7 + 3);

	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	cb.aio_buf = out;
	cb.aio_nbytes = sizeof(out);
	cb.aio_offset = 4096;
	CHECK(aio_write(&cb) == 0, "aio_write");
	n = finish(&cb);
	CHECK(n == (ssize_t)sizeof(out), "aio_write returned %zd", n);
	CHECK(aio_return(&cb) == -1 && errno == EINVAL,
	    "a second aio_return");
	CHECK(aio_error(&cb) == EINVAL, "aio_error after aio_return");

	/* Positional: the file's offset did not move. */
	CHECK(lseek(fd, 0, SEEK_CUR) == 0, "the file offset moved");

	memset(in, 0, sizeof(in));
	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	cb.aio_buf = in;
	cb.aio_nbytes = sizeof(in);
	cb.aio_offset = 4096;
	CHECK(aio_read(&cb) == 0, "aio_read");
	CHECK(aio_waitcomplete(&done, NULL) == (ssize_t)sizeof(in) &&
	    done == &cb, "aio_waitcomplete");
	CHECK(memcmp(in, out, sizeof(in)) == 0, "read back differs");

	/* Vectored, swapped halves. */
	memset(&cb, 0, sizeof(cb));
	iov[0].iov_base = out + 100;
	iov[0].iov_len = 300;
	iov[1].iov_base = out;
	iov[1].iov_len = 100;
	cb.aio_fildes = fd;
	cb.aio_iov = iov;
	cb.aio_iovcnt = 2;
	cb.aio_offset = 0;
	CHECK(aio_writev(&cb) == 0, "aio_writev");
	CHECK(finish(&cb) == 400, "aio_writev's count");
	memset(&cb, 0, sizeof(cb));
	iov[0].iov_base = b;
	iov[0].iov_len = 300;
	iov[1].iov_base = a;
	iov[1].iov_len = 100;
	cb.aio_fildes = fd;
	cb.aio_iov = iov;
	cb.aio_iovcnt = 2;
	cb.aio_offset = 0;
	CHECK(aio_readv(&cb) == 0, "aio_readv");
	CHECK(finish(&cb) == 400, "aio_readv's count");
	CHECK(memcmp(b, out + 100, 300) == 0 && memcmp(a, out, 100) == 0,
	    "vectored data");

	/* Past the end: nothing. */
	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	cb.aio_buf = in;
	cb.aio_nbytes = sizeof(in);
	cb.aio_offset = 1 << 20;
	CHECK(aio_read(&cb) == 0 && finish(&cb) == 0, "a read past the end");
}

static void
fsync_after_writes(int fd)
{
	struct aiocb w[8], s, *done;
	char buf[8][512];
	int i, seen_sync, writes;
	ssize_t r;

	for (i = 0; i < 8; i++) {
		memset(&w[i], 0, sizeof(w[i]));
		memset(buf[i], 'a' + i, sizeof(buf[i]));
		w[i].aio_fildes = fd;
		w[i].aio_buf = buf[i];
		w[i].aio_nbytes = sizeof(buf[i]);
		w[i].aio_offset = 512 * i;
		CHECK(aio_write(&w[i]) == 0, "aio_write %d", i);
	}
	memset(&s, 0, sizeof(s));
	s.aio_fildes = fd;
	CHECK(aio_fsync(O_SYNC, &s) == 0, "aio_fsync");
	seen_sync = writes = 0;
	for (i = 0; i < 9; i++) {
		r = aio_waitcomplete(&done, NULL);
		if (done == &s) {
			CHECK(r == 0, "aio_fsync returned %zd", r);
			/* Every write queued before it finished first. */
			CHECK(writes == 8, "aio_fsync before %d writes", 8 - writes);
			seen_sync = 1;
		} else {
			CHECK(r == 512, "a write returned %zd", r);
			writes++;
		}
	}
	CHECK(seen_sync, "no aio_fsync");
	memset(&s, 0, sizeof(s));
	s.aio_fildes = fd;
	CHECK(aio_fsync(O_DSYNC, &s) == 0 && finish(&s) == 0, "O_DSYNC");
}

static void
listio(int fd)
{
	struct aiocb a, b, *list[3];
	struct sigevent sev;
	struct kevent ev;
	struct timespec to = { 30, 0 };
	char x[64], y[64];
	int kq;

	memset(x, 'x', sizeof(x));
	memset(y, 'y', sizeof(y));
	memset(&a, 0, sizeof(a));
	a.aio_fildes = fd;
	a.aio_buf = x;
	a.aio_nbytes = sizeof(x);
	a.aio_offset = 0;
	a.aio_lio_opcode = LIO_WRITE;
	memset(&b, 0, sizeof(b));
	b.aio_fildes = fd;
	b.aio_buf = y;
	b.aio_nbytes = sizeof(y);
	b.aio_offset = 64;
	b.aio_lio_opcode = LIO_WRITE;
	list[0] = &a;
	list[1] = NULL;
	list[2] = &b;
	CHECK(lio_listio(LIO_WAIT, list, 3, NULL) == 0, "lio_listio LIO_WAIT");
	CHECK(aio_error(&a) == 0 && aio_error(&b) == 0,
	    "LIO_WAIT returned before its requests finished");
	CHECK(aio_return(&a) == 64 && aio_return(&b) == 64, "lio results");

	/* LIO_NOWAIT, told by kevent; reads back what went. */
	kq = kqueue();
	memset(&sev, 0, sizeof(sev));
	sev.sigev_notify = SIGEV_KEVENT;
	sev.sigev_notify_kqueue = kq;
	sev.sigev_value.sival_ptr = (void *)0x1234;
	a.aio_lio_opcode = LIO_READ;
	a.aio_buf = y;
	b.aio_lio_opcode = LIO_READ;
	b.aio_buf = x;
	CHECK(lio_listio(LIO_NOWAIT, list, 3, &sev) == 0, "lio_listio LIO_NOWAIT");
	CHECK(kevent(kq, NULL, 0, &ev, 1, &to) == 1, "no EVFILT_LIO event");
	CHECK(ev.filter == EVFILT_LIO && ev.ident == (uintptr_t)list &&
	    ev.udata == (void *)0x1234, "EVFILT_LIO's event");
	CHECK(aio_return(&a) == 64 && aio_return(&b) == 64, "lio reads");
	CHECK(x[0] == 'y' && y[0] == 'x', "lio read data");

	/* A bad opcode fails the call with EIO, the others still go. */
	a.aio_lio_opcode = LIO_READ;
	b.aio_lio_opcode = 42;
	CHECK(lio_listio(LIO_WAIT, list, 3, NULL) == -1 && errno == EIO,
	    "a bad opcode");
	CHECK(aio_error(&b) == EINVAL, "the bad one's error %d", aio_error(&b));
	CHECK(aio_return(&a) == 64, "the good one");
	CHECK(lio_listio(42, list, 3, NULL) == -1 && errno == EINVAL,
	    "a bad mode");
	close(kq);
}

static void
notifications(int fd)
{
	struct aiocb cb;
	struct kevent ev;
	struct timespec to = { 30, 0 };
	siginfo_t si;
	sigset_t set;
	char buf[32];
	int kq;

	/* By kevent: EV_EOF, data 0, the aiocb as ident. */
	kq = kqueue();
	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	cb.aio_buf = buf;
	cb.aio_nbytes = sizeof(buf);
	cb.aio_sigevent.sigev_notify = SIGEV_KEVENT;
	cb.aio_sigevent.sigev_notify_kqueue = kq;
	cb.aio_sigevent.sigev_value.sival_ptr = (void *)0xdeadbeef;
	CHECK(aio_read(&cb) == 0, "aio_read with SIGEV_KEVENT");
	CHECK(kevent(kq, NULL, 0, &ev, 1, &to) == 1, "no EVFILT_AIO event");
	CHECK(ev.ident == (uintptr_t)&cb && ev.filter == EVFILT_AIO &&
	    ev.flags == EV_EOF && ev.data == 0 &&
	    ev.udata == (void *)0xdeadbeef, "EVFILT_AIO's event: flags %#x data %jd",
	    ev.flags, (intmax_t)ev.data);
	CHECK(aio_return(&cb) == (ssize_t)sizeof(buf), "kevent read");
	/* No one else may register EVFILT_AIO. */
	EV_SET(&ev, (uintptr_t)&cb, EVFILT_AIO, EV_ADD, 0, 0, NULL);
	CHECK(kevent(kq, &ev, 1, NULL, 0, NULL) == -1 && errno == EPERM,
	    "a user's EVFILT_AIO");
	/* A bad kqueue fails the request. */
	cb.aio_sigevent.sigev_notify_kqueue = fd;
	CHECK(aio_read(&cb) == -1 && errno == EINVAL, "not a kqueue");
	close(kq);

	/* By signal: SI_ASYNCIO with the value. */
	sigemptyset(&set);
	sigaddset(&set, SIGUSR1);
	sigprocmask(SIG_BLOCK, &set, NULL);
	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	cb.aio_buf = buf;
	cb.aio_nbytes = sizeof(buf);
	cb.aio_sigevent.sigev_notify = SIGEV_SIGNAL;
	cb.aio_sigevent.sigev_signo = SIGUSR1;
	cb.aio_sigevent.sigev_value.sival_int = 77;
	CHECK(aio_read(&cb) == 0, "aio_read with SIGEV_SIGNAL");
	CHECK(sigtimedwait(&set, &si, &to) == SIGUSR1, "no SIGUSR1");
	CHECK(si.si_code == SI_ASYNCIO && si.si_value.sival_int == 77,
	    "siginfo: code %#x value %d", si.si_code, si.si_value.sival_int);
	CHECK(aio_return(&cb) == (ssize_t)sizeof(buf), "signalled read");
	cb.aio_sigevent.sigev_signo = 0;
	CHECK(aio_read(&cb) == -1 && errno == EINVAL, "signal 0");
	sigprocmask(SIG_UNBLOCK, &set, NULL);
}

static void
sockets(void)
{
	struct aiocb cb, w;
	struct sockaddr_un sun;
	struct rusage r0, r1;
	char buf[16], msg[16] = "hello";
	int s[2], l;
	pid_t pid;
	int status;

	CHECK(socketpair(PF_UNIX, SOCK_STREAM, 0, s) == 0, "socketpair");
	/* A read waits for data; it can be cancelled while it waits. */
	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = s[0];
	cb.aio_buf = buf;
	cb.aio_nbytes = sizeof(buf);
	CHECK(aio_read(&cb) == 0, "aio_read on a socket");
	CHECK(aio_error(&cb) == EINPROGRESS, "a socket read finished at once");
	CHECK(aio_cancel(s[0], &cb) == AIO_CANCELED, "aio_cancel");
	CHECK(aio_error(&cb) == ECANCELED, "after aio_cancel: %d",
	    aio_error(&cb));
	CHECK(aio_return(&cb) == -1 && errno == ECANCELED, "aio_return");
	CHECK(aio_cancel(s[0], NULL) == AIO_ALLDONE, "nothing left");

	/* A write and a read, one message each. */
	getrusage(RUSAGE_SELF, &r0);
	memset(&w, 0, sizeof(w));
	w.aio_fildes = s[1];
	w.aio_buf = msg;
	w.aio_nbytes = sizeof(msg);
	CHECK(aio_write(&w) == 0, "aio_write on a socket");
	CHECK(finish(&w) == (ssize_t)sizeof(msg), "socket write");
	CHECK(aio_read(&cb) == 0, "aio_read on a socket");
	CHECK(finish(&cb) == (ssize_t)sizeof(msg) && strcmp(buf, "hello") == 0,
	    "socket read");
	getrusage(RUSAGE_SELF, &r1);
	CHECK(r1.ru_msgsnd == r0.ru_msgsnd + 1 && r1.ru_msgrcv == r0.ru_msgrcv + 1,
	    "messages %ld+%ld", r1.ru_msgsnd - r0.ru_msgsnd,
	    r1.ru_msgrcv - r0.ru_msgrcv);

	/* Exit cancels a read still waiting. */
	pid = fork();
	if (pid == 0) {
		if (aio_read(&cb) != 0)
			_exit(1);
		_exit(0);
	}
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0, "exit with a request waiting");
	close(s[0]);
	close(s[1]);

	/* listen refuses a socket with requests, which refuses a listener. */
	l = socket(PF_UNIX, SOCK_STREAM, 0);
	memset(&sun, 0, sizeof(sun));
	sun.sun_family = AF_UNIX;
	snprintf(sun.sun_path, sizeof(sun.sun_path), "/tmp/aio.%d", getpid());
	unlink(sun.sun_path);
	CHECK(bind(l, (struct sockaddr *)&sun, SUN_LEN(&sun)) == 0, "bind");
	cb.aio_fildes = l;
	CHECK(aio_read(&cb) == 0, "aio_read on an unconnected socket");
	CHECK(listen(l, 5) == -1 && errno == EINVAL, "listen with requests");
	CHECK(aio_cancel(l, &cb) == AIO_CANCELED, "aio_cancel");
	aio_return(&cb);
	CHECK(listen(l, 5) == 0, "listen");
	CHECK(aio_read(&cb) == -1 && errno == EINVAL, "aio_read on a listener");
	close(l);
	unlink(sun.sun_path);
}

static void
errors(int fd)
{
	struct aiocb cb, *done;
	struct timespec zero = { 0, 0 };
	char buf[8];
	int p[2], unsafe;
	size_t len;

	memset(&cb, 0, sizeof(cb));
	cb.aio_fildes = fd;
	CHECK(aio_fsync(666, &cb) == -1 && errno == EINVAL, "aio_fsync(666)");
	cb.aio_fildes = 666;
	CHECK(aio_fsync(O_SYNC, &cb) == -1 && errno == EBADF, "a bad fd");
	CHECK(aio_error(&cb) == EBADF, "the bad fd's error %d", aio_error(&cb));
	cb.aio_fildes = fd;
	cb.aio_buf = buf;
	cb.aio_nbytes = sizeof(buf);
	cb.aio_offset = -1;
	CHECK(aio_read(&cb) == -1 && errno == EINVAL, "a negative offset");
	cb.aio_offset = 0;
	cb.aio_sigevent.sigev_notify = 666;
	CHECK(aio_read(&cb) == -1 && errno == EINVAL, "a bad sigev_notify");
	CHECK(aio_waitcomplete(&done, &zero) == -1 && errno == EAGAIN &&
	    done == NULL, "aio_waitcomplete with nothing done");
	CHECK(aio_return(&cb) == -1 && errno == EINVAL, "aio_return of none");
	memset(&cb, 0, sizeof(cb));
	{
		const struct aiocb *list[1] = { &cb };

		CHECK(aio_suspend(list, 1, &zero) == 0,
		    "aio_suspend on no request");
	}

	/* A pipe is not safe for the workers. */
	len = sizeof(unsafe);
	CHECK(sysctlbyname("vfs.aio.enable_unsafe", &unsafe, &len, NULL, 0) == 0,
	    "vfs.aio.enable_unsafe");
	if (unsafe == 0) {
		pipe(p);
		memset(&cb, 0, sizeof(cb));
		cb.aio_fildes = p[0];
		cb.aio_buf = buf;
		cb.aio_nbytes = sizeof(buf);
		CHECK(aio_read(&cb) == -1 && errno == EOPNOTSUPP,
		    "a pipe without vfs.aio.enable_unsafe");
		CHECK(aio_error(&cb) == EOPNOTSUPP, "its error");
		close(p[0]);
		close(p[1]);
	}

	/* With vfs.aio.enable_unsafe, a pipe works through the workers. */
	{
		struct aiocb w;
		char msg[8] = "pipe!";
		int on = 1;

		CHECK(sysctlbyname("vfs.aio.enable_unsafe", NULL, NULL, &on,
		    sizeof(on)) == 0, "setting vfs.aio.enable_unsafe");
		pipe(p);
		memset(&w, 0, sizeof(w));
		w.aio_fildes = p[1];
		w.aio_buf = msg;
		w.aio_nbytes = sizeof(msg);
		CHECK(aio_write(&w) == 0, "aio_write on a pipe");
		CHECK(finish(&w) == (ssize_t)sizeof(msg), "pipe write");
		memset(&cb, 0, sizeof(cb));
		memset(buf, 0, sizeof(buf));
		cb.aio_fildes = p[0];
		cb.aio_buf = buf;
		cb.aio_nbytes = sizeof(buf);
		CHECK(aio_read(&cb) == 0, "aio_read on a pipe");
		CHECK(finish(&cb) == (ssize_t)sizeof(msg) &&
		    strcmp(buf, "pipe!") == 0, "pipe read");
		close(p[0]);
		close(p[1]);
		CHECK(sysctlbyname("vfs.aio.enable_unsafe", NULL, NULL, &unsafe,
		    sizeof(unsafe)) == 0, "restoring vfs.aio.enable_unsafe");
	}
}

static void
limits(void)
{
	int v;
	size_t len;

	len = sizeof(v);
	CHECK(sysctlbyname("vfs.aio.max_aio_queue_per_proc", &v, &len, NULL,
	    0) == 0 && v == 256, "vfs.aio.max_aio_queue_per_proc %d", v);
	len = sizeof(v);
	CHECK(sysctlbyname("vfs.aio.max_aio_per_proc", &v, &len, NULL,
	    0) == 0 && v == 32, "vfs.aio.max_aio_per_proc %d", v);
	len = sizeof(v);
	CHECK(sysctlbyname("vfs.aio.max_aio_queue", &v, &len, NULL,
	    0) == 0 && v == 1024, "vfs.aio.max_aio_queue %d", v);
	len = sizeof(v);
	CHECK(sysctlbyname("vfs.aio.num_aio_procs", &v, &len, NULL,
	    0) == 0 && v >= 1, "vfs.aio.num_aio_procs %d", v);
	CHECK(sysconf(_SC_AIO_LISTIO_MAX) == 256, "_SC_AIO_LISTIO_MAX %ld",
	    sysconf(_SC_AIO_LISTIO_MAX));
	CHECK(sysconf(_SC_ASYNCHRONOUS_IO) == 200112L,
	    "_SC_ASYNCHRONOUS_IO %ld", sysconf(_SC_ASYNCHRONOUS_IO));
	CHECK(sysconf(_SC_AIO_MAX) == 1024, "_SC_AIO_MAX %ld",
	    sysconf(_SC_AIO_MAX));
}

int
main(void)
{
	char path[] = "/tmp/aio.XXXXXX";
	int fd;

	fd = mkstemp(path);
	CHECK(fd >= 0, "mkstemp");
	unlink(path);
	file_io(fd);
	fsync_after_writes(fd);
	listio(fd);
	notifications(fd);
	sockets();
	errors(fd);
	limits();
	close(fd);
	return (failures != 0);
}
