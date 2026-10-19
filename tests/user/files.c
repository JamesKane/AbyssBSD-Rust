/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * files: checks descriptors and the file system from user mode: stat and
 * lstat, open's errors, reading and seeking, dup and fcntl, directories,
 * the working directory, the *at calls, access, devfs's devices, statfs,
 * umask, close_range, and mapping files.
 *
 * Runs on FreeBSD too: what depends on the root being read-only (as the
 * memory disk is) follows statfs's MNT_RDONLY.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/filio.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("files: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* `r` failed with `want`. */
#define	FAILS(r, want) do {						\
	errno = 0;							\
	long r_ = (long)(r);						\
	int e_ = errno;							\
	CHECK(r_ == -1 && e_ == (want), "got %ld (%s), want %s", r_,	\
	    strerror(e_), strerror(want));				\
} while (0)

static int read_only_root;

static void
stats(void)
{
	struct stat sh, echo, l, root, dev, null, rnd, ur;

	CHECK(stat("/rescue/sh", &sh) == 0, "%s", strerror(errno));
	CHECK(S_ISREG(sh.st_mode) && (sh.st_mode & 0111) != 0, "mode %o",
	    sh.st_mode);
	CHECK(sh.st_size > 100000, "size %jd", (intmax_t)sh.st_size);
	CHECK(sh.st_nlink > 1, "nlink %ju", (uintmax_t)sh.st_nlink);
	CHECK(sh.st_blksize >= 512, "blksize %d", sh.st_blksize);
	/* /rescue is one binary under many names. */
	CHECK(stat("/rescue/echo", &echo) == 0 && echo.st_ino == sh.st_ino &&
	    echo.st_dev == sh.st_dev, "echo is not sh");
	CHECK(lstat("/rescue/sh", &l) == 0 && l.st_ino == sh.st_ino, "lstat");

	CHECK(stat("/", &root) == 0 && S_ISDIR(root.st_mode), "root");
	CHECK(stat("/dev", &dev) == 0 && S_ISDIR(dev.st_mode), "/dev");
	CHECK(dev.st_dev != root.st_dev, "devfs is not mounted on /dev");
	CHECK(stat("/dev/null", &null) == 0 && S_ISCHR(null.st_mode) &&
	    (null.st_mode & 0777) == 0666, "null %o", null.st_mode);
	CHECK(null.st_rdev == null.st_ino, "devfs rdev %#jx ino %ju",
	    (uintmax_t)null.st_rdev, (uintmax_t)null.st_ino);

	/* urandom is a link to random. */
	CHECK(lstat("/dev/urandom", &l) == 0 && S_ISLNK(l.st_mode), "lstat");
	CHECK(stat("/dev/urandom", &ur) == 0 && stat("/dev/random", &rnd) == 0 &&
	    ur.st_ino == rnd.st_ino && S_ISCHR(ur.st_mode), "urandom");
}

static void
open_errors(void)
{
	char name[300];

	FAILS(open("/nonexistent", O_RDONLY), ENOENT);
	FAILS(open("/rescue/sh/x", O_RDONLY), ENOTDIR);
	FAILS(open("/rescue/sh/", O_RDONLY), ENOTDIR);
	FAILS(open("/rescue", O_WRONLY), EISDIR);
	FAILS(open("/rescue/sh", O_RDONLY | O_DIRECTORY), ENOTDIR);
	FAILS(open("/dev/urandom", O_RDONLY | O_NOFOLLOW), EMLINK);
	FAILS(open("/rescue/sh", O_RDONLY | O_CREAT | O_EXCL, 0644), EEXIST);
	FAILS(open("/rescue/sh", O_ACCMODE), EINVAL);
	memset(name, 'x', sizeof(name));
	name[0] = '/';
	name[sizeof(name) - 1] = '\0';
	FAILS(open(name, O_RDONLY), ENAMETOOLONG);
	if (read_only_root) {
		FAILS(open("/rescue/new", O_WRONLY | O_CREAT, 0644), EROFS);
		FAILS(open("/rescue/sh", O_WRONLY), EROFS);
		FAILS(open("/rescue/sh", O_RDONLY | O_TRUNC), EROFS);
	}
}

static void
reading(void)
{
	char buf[8], a[2], b[3];
	struct iovec iov[2] = { { a, sizeof(a) }, { b, sizeof(b) } };
	struct stat st;
	int fd, n;
	pid_t pid;

	fd = open("/rescue/sh", O_RDONLY);
	CHECK(fd >= 3, "open: %s", strerror(errno));
	CHECK(fstat(fd, &st) == 0, "fstat");
	CHECK(read(fd, buf, 4) == 4 && memcmp(buf, "\177ELF", 4) == 0, "magic");
	CHECK(lseek(fd, 0, SEEK_CUR) == 4, "offset");
	CHECK(ioctl(fd, FIONREAD, &n) == 0 && n == st.st_size - 4,
	    "FIONREAD %d", n);
	/* pread leaves the offset. */
	CHECK(pread(fd, buf, 4, 1) == 4 && memcmp(buf, "ELF", 3) == 0, "pread");
	CHECK(lseek(fd, 0, SEEK_CUR) == 4, "pread moved the offset");
	CHECK(lseek(fd, 0, SEEK_SET) == 0 && readv(fd, iov, 2) == 5 &&
	    memcmp(a, "\177E", 2) == 0 && memcmp(b, "LF", 2) == 0, "readv");
	CHECK(lseek(fd, 0, SEEK_END) == st.st_size, "SEEK_END");
	CHECK(read(fd, buf, sizeof(buf)) == 0, "read at the end");
	FAILS(lseek(fd, -1, SEEK_SET), EINVAL);
	FAILS(lseek(fd, 0, 99), EINVAL);
	FAILS(write(fd, "x", 1), EBADF);

	/* A forked child shares the offset. */
	CHECK(lseek(fd, 0, SEEK_SET) == 0, "rewind");
	pid = fork();
	if (pid == 0)
		_exit(read(fd, buf, 4) == 4 ? 0 : 1);
	CHECK(waitpid(pid, &n, 0) == pid && WIFEXITED(n) &&
	    WEXITSTATUS(n) == 0, "child read");
	CHECK(lseek(fd, 0, SEEK_CUR) == 4, "child's read did not move it");
	close(fd);
}

static void
descriptors(void)
{
	int fd, fd2, n;

	fd = open("/rescue/sh", O_RDONLY);
	fd2 = dup(fd);
	CHECK(fd2 == fd + 1, "dup %d after %d", fd2, fd);
	CHECK(lseek(fd, 10, SEEK_SET) == 10 && lseek(fd2, 0, SEEK_CUR) == 10,
	    "dup shares the offset");
	CHECK(dup2(fd, 100) == 100 && lseek(100, 0, SEEK_CUR) == 10, "dup2");
	CHECK(dup2(100, 100) == 100, "dup2 onto itself");
	CHECK(fcntl(fd, F_GETFD) == 0, "fresh descriptor flags");
	CHECK(fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 &&
	    fcntl(fd, F_GETFD) == FD_CLOEXEC, "F_SETFD");
	CHECK(fcntl(fd2, F_GETFD) == 0, "dup copied the descriptor flags");
	CHECK(fcntl(fd, F_DUPFD, 50) == 50, "F_DUPFD");
	CHECK(fcntl(fd, F_DUPFD, 50) == 51, "F_DUPFD lowest free");
	n = fcntl(fd, F_DUPFD_CLOEXEC, 60);
	CHECK(n == 60 && fcntl(60, F_GETFD) == FD_CLOEXEC, "F_DUPFD_CLOEXEC");
	CHECK(ioctl(fd2, FIOCLEX) == 0 && fcntl(fd2, F_GETFD) == FD_CLOEXEC,
	    "FIOCLEX");
	CHECK(ioctl(fd2, FIONCLEX) == 0 && fcntl(fd2, F_GETFD) == 0,
	    "FIONCLEX");
	CHECK(fcntl(fd, F_GETFL) == O_RDONLY, "F_GETFL %#x",
	    fcntl(fd, F_GETFL));
	CHECK(fcntl(fd, F_SETFL, O_NONBLOCK) == 0 &&
	    fcntl(fd2, F_GETFL) == (O_RDONLY | O_NONBLOCK),
	    "F_SETFL is shared");
	CHECK(fcntl(fd, F_SETFL, 0) == 0, "F_SETFL");
	FAILS(dup2(fd, -1), EBADF);
	FAILS(fcntl(fd, F_DUPFD, -1), EINVAL);
	FAILS(close(100) == 0 ? close(100) : 0, EBADF);
	FAILS(dup(1000), EBADF);
	FAILS(fcntl(fd, 9999), EINVAL);

	/* close_range: mark, then close everything from 50 up. */
	CHECK(close_range(50, 51, CLOSE_RANGE_CLOEXEC) == 0 &&
	    fcntl(51, F_GETFD) == FD_CLOEXEC, "close_range(CLOEXEC)");
	CHECK(close_range(50, ~0U, 0) == 0, "close_range");
	FAILS(fcntl(50, F_GETFD), EBADF);
	FAILS(fcntl(60, F_GETFD), EBADF);
	FAILS(close_range(5, 4, 0), EINVAL);
	close(fd);
	close(fd2);
}

static int
dir_has(const char *path, const char *name, int type)
{
	DIR *d;
	struct dirent *e;
	int found = 0;

	d = opendir(path);
	if (d == NULL)
		return (0);
	while ((e = readdir(d)) != NULL)
		if (strcmp(e->d_name, name) == 0 && e->d_type == type)
			found = 1;
	closedir(d);
	return (found);
}

static void
directories(void)
{
	DIR *d;
	struct dirent *e;
	int n = 0;

	CHECK(dir_has("/rescue", ".", DT_DIR), ".");
	CHECK(dir_has("/rescue", "..", DT_DIR), "..");
	CHECK(dir_has("/rescue", "sh", DT_REG), "sh");
	CHECK(dir_has("/", "rescue", DT_DIR), "rescue");
	CHECK(dir_has("/dev", "null", DT_CHR), "null");
	CHECK(dir_has("/dev", "console", DT_CHR), "console");
	CHECK(dir_has("/dev", "urandom", DT_LNK), "urandom");
	d = opendir("/rescue");
	CHECK(d != NULL, "opendir");
	while ((e = readdir(d)) != NULL)
		n++;
	closedir(d);
	CHECK(n > 100, "only %d entries in /rescue", n);
	FAILS(opendir("/rescue/sh") == NULL ? -1 : 0, ENOTDIR);
}

static void
cwd(void)
{
	char buf[MAXPATHLEN];
	struct stat a, b;
	int fd;

	CHECK(chdir("/rescue") == 0, "chdir");
	CHECK(getcwd(buf, sizeof(buf)) != NULL && strcmp(buf, "/rescue") == 0,
	    "getcwd %s", buf);
	CHECK(stat("sh", &a) == 0 && stat("/rescue/sh", &b) == 0 &&
	    a.st_ino == b.st_ino, "relative path");
	CHECK(chdir("..") == 0 && getcwd(buf, sizeof(buf)) != NULL &&
	    strcmp(buf, "/") == 0, "getcwd %s", buf);
	/* Across a mount point and back. */
	CHECK(chdir("/dev") == 0 && getcwd(buf, sizeof(buf)) != NULL &&
	    strcmp(buf, "/dev") == 0, "getcwd %s", buf);
	CHECK(chdir("..") == 0 && getcwd(buf, sizeof(buf)) != NULL &&
	    strcmp(buf, "/") == 0, "getcwd %s", buf);
	fd = open("/usr/tests", O_RDONLY | O_DIRECTORY);
	CHECK(fd >= 0 && fchdir(fd) == 0, "fchdir");
	CHECK(getcwd(buf, sizeof(buf)) != NULL &&
	    strcmp(buf, "/usr/tests") == 0, "getcwd %s", buf);
	FAILS(getcwd(buf, 5) == NULL ? -1 : 0, ERANGE);
	FAILS(chdir("/rescue/sh"), ENOTDIR);
	FAILS(chdir("/nonexistent"), ENOENT);
	close(fd);
	chdir("/");
}

static void
at_calls(void)
{
	struct stat a, b;
	int dir, fd;

	dir = open("/rescue", O_RDONLY | O_DIRECTORY);
	CHECK(dir >= 0, "open dir");
	fd = openat(dir, "sh", O_RDONLY);
	CHECK(fd >= 0 && fstat(fd, &a) == 0, "openat");
	CHECK(fstatat(dir, "sh", &b, AT_SYMLINK_NOFOLLOW) == 0 &&
	    a.st_ino == b.st_ino, "fstatat");
	CHECK(fstatat(fd, "", &b, AT_EMPTY_PATH) == 0 && a.st_ino == b.st_ino,
	    "AT_EMPTY_PATH");
	FAILS(openat(dir, "../rescue/sh", O_RDONLY | O_RESOLVE_BENEATH),
	    ENOTCAPABLE);
	FAILS(openat(dir, "/rescue/sh", O_RDONLY | O_RESOLVE_BENEATH),
	    ENOTCAPABLE);
	FAILS(openat(fd, "x", O_RDONLY), ENOTDIR);
	FAILS(fstatat(dir, "sh", &b, 0x10000), EINVAL);
	close(fd);
	close(dir);

	CHECK(access("/rescue/sh", R_OK | X_OK) == 0, "access");
	CHECK(access("/dev/null", R_OK | W_OK) == 0, "access /dev/null");
	FAILS(access("/nonexistent", F_OK), ENOENT);
	if (read_only_root)
		FAILS(access("/rescue/sh", W_OK), EROFS);
}

static void
devices(void)
{
	char buf[64], zero[64];
	int fd, i, nonzero = 0;

	fd = open("/dev/null", O_RDWR);
	CHECK(fd >= 0 && read(fd, buf, sizeof(buf)) == 0, "null read");
	CHECK(write(fd, "hello", 5) == 5, "null write");
	close(fd);
	memset(zero, 0, sizeof(zero));
	memset(buf, 1, sizeof(buf));
	fd = open("/dev/zero", O_RDONLY);
	CHECK(fd >= 0 && read(fd, buf, sizeof(buf)) == sizeof(buf) &&
	    memcmp(buf, zero, sizeof(buf)) == 0, "zero");
	close(fd);
	fd = open("/dev/urandom", O_RDONLY);
	CHECK(fd >= 0 && read(fd, buf, sizeof(buf)) == sizeof(buf), "random");
	for (i = 0; i < (int)sizeof(buf); i++)
		nonzero += buf[i] != 0;
	CHECK(nonzero > 32, "random bytes look like zeros");
	close(fd);
}

static void
filesystems(void)
{
	struct statfs root, dev;
	int n;

	CHECK(statfs("/dev/null", &dev) == 0, "statfs");
	CHECK(strcmp(dev.f_fstypename, "devfs") == 0 &&
	    strcmp(dev.f_mntonname, "/dev") == 0, "%s on %s",
	    dev.f_fstypename, dev.f_mntonname);
	CHECK(statfs("/rescue", &root) == 0 && root.f_type != dev.f_type,
	    "root type");
	n = getfsstat(NULL, 0, MNT_NOWAIT);
	CHECK(n >= 2, "getfsstat %d", n);
	FAILS(getfsstat(NULL, 0, 99), EINVAL);
}

static void
mapping(void)
{
	char page[4096], *m;
	struct stat st;
	int fd, p[2], dir;

	fd = open("/rescue/sh", O_RDONLY);
	CHECK(fd >= 0 && fstat(fd, &st) == 0, "open");
	m = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE, fd, 0);
	CHECK(m != MAP_FAILED && memcmp(m, "\177ELF", 4) == 0, "map: %s",
	    strerror(errno));
	munmap(m, 8192);
	/* An offset into a page keeps it in the address. */
	m = mmap(NULL, 100, PROT_READ, MAP_SHARED, fd, 4096 + 10);
	CHECK(m != MAP_FAILED && ((uintptr_t)m & 4095) == 10, "offset %p", m);
	CHECK(pread(fd, page, 100, 4096 + 10) == 100 && memcmp(m, page, 100) == 0,
	    "contents at an offset");
	munmap(m - 10, 4096);
	/* A private mapping may be written; the file does not change. */
	m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	CHECK(m != MAP_FAILED, "private writable: %s", strerror(errno));
	m[0] = 'X';
	CHECK(pread(fd, page, 4, 0) == 4 && memcmp(page, "\177ELF", 4) == 0 &&
	    m[0] == 'X', "copy on write");
	munmap(m, 4096);
	/* Past the end of the file, the last page reads as zeros. */
	m = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd,
	    st.st_size & ~(off_t)4095);
	CHECK(m != MAP_FAILED && ((st.st_size & 4095) == 0 ||
	    m[st.st_size & 4095] == 0), "tail of the last page");
	munmap(m, 4096);
	FAILS(mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) ==
	    MAP_FAILED ? -1 : 0, EACCES);
	FAILS(mmap(NULL, 4096, PROT_READ, 0, fd, 0) == MAP_FAILED ? -1 : 0,
	    EINVAL);
	FAILS(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, -4096) ==
	    MAP_FAILED ? -1 : 0, EINVAL);
	close(fd);

	fd = open("/dev/zero", O_RDWR);
	m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	CHECK(m != MAP_FAILED && m[100] == 0, "/dev/zero: %s", strerror(errno));
	m[100] = 1;
	munmap(m, 4096);
	close(fd);

	CHECK(pipe(p) == 0, "pipe");
	FAILS(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, p[0], 0) == MAP_FAILED ?
	    -1 : 0, ENODEV);
	close(p[0]);
	close(p[1]);
	dir = open("/rescue", O_RDONLY | O_DIRECTORY);
	FAILS(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, dir, 0) == MAP_FAILED ?
	    -1 : 0, EINVAL);
	close(dir);
	FAILS(mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 1000, 0) == MAP_FAILED ?
	    -1 : 0, EBADF);
}

int
main(void)
{
	struct statfs sf;
	mode_t old;

	if (statfs("/", &sf) != 0) {
		printf("files: statfs /: %s\n", strerror(errno));
		return (1);
	}
	read_only_root = (sf.f_flags & MNT_RDONLY) != 0;
	stats();
	open_errors();
	reading();
	descriptors();
	directories();
	cwd();
	at_calls();
	devices();
	filesystems();
	mapping();
	old = umask(077);
	CHECK(umask(old) == 077, "umask");
	if (!failed)
		printf("files: ok\n");
	return (failed);
}
