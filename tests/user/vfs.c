/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * vfs: checks the calls that change a file system, on a tmpfs it mounts
 * on /mnt with nmount: creating and removing files and directories,
 * link counts, rename's rules, symbolic links, writing and truncating,
 * modes and the umask, times, flags, directory listings, FIFOs, advisory
 * locks (flock and fcntl, between processes too), getcwd, statfs, and
 * unmount's EBUSY.
 *
 * Runs on FreeBSD too (as root, with nothing mounted on /mnt), where the
 * expected errnos come from.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("vfs: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* `r` failed with `want` (strerror's buffer is shared: copy each). */
#define	FAILS(r, want) do {						\
	errno = 0;							\
	long r_ = (long)(r);						\
	int e_ = errno;							\
	char got_[64], want_[64];					\
	strlcpy(got_, strerror(e_), sizeof(got_));			\
	strlcpy(want_, strerror(want), sizeof(want_));			\
	CHECK(r_ == -1 && e_ == (want), "got %ld (%s), want %s", r_,	\
	    got_, want_);						\
} while (0)

#define	OK(r) do {							\
	errno = 0;							\
	long r_ = (long)(r);						\
	CHECK(r_ != -1, "failed: %s", strerror(errno));			\
} while (0)

static void
iov_set(struct iovec *iov, const char *name, const char *value)
{
	iov[0].iov_base = (void *)(uintptr_t)name;
	iov[0].iov_len = strlen(name) + 1;
	iov[1].iov_base = (void *)(uintptr_t)value;
	iov[1].iov_len = strlen(value) + 1;
}

static nlink_t
links(const char *path)
{
	struct stat st;

	if (lstat(path, &st) != 0)
		return (0);
	return (st.st_nlink);
}

static void
put(const char *path, const char *data)
{
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);

	CHECK(fd >= 0, "%s: %s", path, strerror(errno));
	if (fd < 0)
		return;
	CHECK(write(fd, data, strlen(data)) == (ssize_t)strlen(data), "write");
	close(fd);
}

static void
contents(const char *path, const char *want)
{
	char buf[256];
	int fd = open(path, O_RDONLY);
	ssize_t n;

	CHECK(fd >= 0, "%s: %s", path, strerror(errno));
	if (fd < 0)
		return;
	n = read(fd, buf, sizeof(buf) - 1);
	close(fd);
	buf[n < 0 ? 0 : n] = '\0';
	CHECK(strcmp(buf, want) == 0, "%s holds \"%s\", want \"%s\"", path,
	    buf, want);
}

static void
directories(void)
{
	OK(mkdir("/mnt/d", 0755));
	FAILS(mkdir("/mnt/d", 0755), EEXIST);
	FAILS(mkdir("/mnt/nope/x", 0755), ENOENT);
	CHECK(links("/mnt") == 3, "/mnt links %ju", (uintmax_t)links("/mnt"));
	CHECK(links("/mnt/d") == 2, "links %ju", (uintmax_t)links("/mnt/d"));
	OK(mkdir("/mnt/d/e", 0755));
	CHECK(links("/mnt/d") == 3, "links %ju", (uintmax_t)links("/mnt/d"));
	FAILS(rmdir("/mnt/d"), ENOTEMPTY);
	FAILS(rmdir("/mnt/d/e/."), EINVAL);
	OK(mkdir("/mnt/d/e/f", 0755));
	FAILS(rmdir("/mnt/d/e/f/.."), EINVAL);
	OK(rmdir("/mnt/d/e/f"));
	FAILS(unlink("/mnt/d/e"), EPERM);
	FAILS(unlink("/mnt/d/e/."), EPERM);
	FAILS(unlink("/mnt/d/e/.."), EPERM);
	FAILS(rename("/mnt/d/e/..", "/mnt/x"), EINVAL);
	FAILS(mkdir("/mnt/d/.", 0755), EEXIST);
	FAILS(mkdir("/mnt/d/..", 0755), EEXIST);
	FAILS(mkdir("/", 0755), EISDIR);
	FAILS(rmdir("/"), EISDIR);
	FAILS(unlink("/"), EISDIR);
	FAILS(symlink("x", "/mnt/sl/"), ENOENT);
	OK(mkdir("/mnt/d/slash/", 0755));
	OK(rmdir("/mnt/d/slash/"));
	put("/mnt/d/f", "x");
	FAILS(rmdir("/mnt/d/f"), ENOTDIR);
	FAILS(mkdir("/mnt/d/f/g", 0755), ENOTDIR);
	OK(unlink("/mnt/d/f"));
	OK(rmdir("/mnt/d/e"));
	OK(rmdir("/mnt/d"));
	CHECK(links("/mnt") == 2, "/mnt links %ju", (uintmax_t)links("/mnt"));
	FAILS(rmdir("/mnt"), EBUSY);
}

static void
files(void)
{
	struct stat st;
	char buf[64];
	int fd, fd2;

	fd = open("/mnt/f", O_RDWR | O_CREAT | O_EXCL, 0644);
	CHECK(fd >= 0, "create: %s", strerror(errno));
	FAILS(open("/mnt/f", O_RDWR | O_CREAT | O_EXCL, 0644), EEXIST);
	CHECK(write(fd, "hello", 5) == 5, "write");
	CHECK(pwrite(fd, "world", 5, 10) == 5, "pwrite");
	CHECK(fstat(fd, &st) == 0 && st.st_size == 15, "size %jd",
	    (intmax_t)st.st_size);
	CHECK(lseek(fd, 0, SEEK_END) == 15, "SEEK_END");
	CHECK(pread(fd, buf, sizeof(buf), 0) == 15 &&
	    memcmp(buf, "hello\0\0\0\0\0world", 15) == 0, "pread");
	OK(ftruncate(fd, 3));
	CHECK(fstat(fd, &st) == 0 && st.st_size == 3, "size %jd",
	    (intmax_t)st.st_size);
	close(fd);
	contents("/mnt/f", "hel");

	/* O_APPEND writes at the end wherever the offset is. */
	fd = open("/mnt/f", O_WRONLY | O_APPEND);
	CHECK(fd >= 0, "append: %s", strerror(errno));
	lseek(fd, 0, SEEK_SET);
	CHECK(write(fd, "lo", 2) == 2, "append write");
	close(fd);
	contents("/mnt/f", "hello");
	OK(truncate("/mnt/f", 0));
	contents("/mnt/f", "");
	FAILS(truncate("/mnt", 0), EISDIR);
	FAILS(open("/mnt", O_WRONLY), EISDIR);
	FAILS(open("/mnt/new/", O_WRONLY | O_CREAT, 0644), ENOENT);
	FAILS(open("/mnt", O_RDONLY | O_CREAT, 0644), EISDIR);
	FAILS(open(".", O_RDONLY | O_CREAT | O_EXCL, 0644), EEXIST);

	/* An unlinked open file lives on. */
	put("/mnt/f", "still here");
	fd = open("/mnt/f", O_RDONLY);
	OK(unlink("/mnt/f"));
	FAILS(open("/mnt/f", O_RDONLY), ENOENT);
	CHECK(fstat(fd, &st) == 0 && st.st_nlink == 0, "nlink %ju",
	    (uintmax_t)st.st_nlink);
	memset(buf, 0, sizeof(buf));
	CHECK(read(fd, buf, sizeof(buf)) == 10 &&
	    strcmp(buf, "still here") == 0, "read after unlink");
	close(fd);

	/* Hard links. */
	put("/mnt/a", "same");
	OK(link("/mnt/a", "/mnt/b"));
	CHECK(links("/mnt/a") == 2, "links %ju", (uintmax_t)links("/mnt/a"));
	FAILS(link("/mnt/a", "/mnt/b"), EEXIST);
	FAILS(link("/mnt", "/mnt/dirlink"), EPERM);
	contents("/mnt/b", "same");
	OK(unlink("/mnt/a"));
	CHECK(links("/mnt/b") == 1, "links %ju", (uintmax_t)links("/mnt/b"));
	OK(unlink("/mnt/b"));

	/* copy_file_range between two files. */
	put("/mnt/src", "copied bytes");
	fd = open("/mnt/src", O_RDONLY);
	fd2 = open("/mnt/dst", O_WRONLY | O_CREAT | O_TRUNC, 0644);
	CHECK(copy_file_range(fd, NULL, fd2, NULL, 100, 0) == 12,
	    "copy_file_range: %s", strerror(errno));
	close(fd);
	close(fd2);
	contents("/mnt/dst", "copied bytes");
	unlink("/mnt/src");
	unlink("/mnt/dst");
}

static void
renames(void)
{
	struct stat a, b;

	OK(mkdir("/mnt/r", 0755));
	OK(mkdir("/mnt/r/sub", 0755));
	put("/mnt/r/f", "f");
	put("/mnt/r/g", "g");
	OK(mkdir("/mnt/r/empty", 0755));
	FAILS(rename("/mnt/r", "/mnt/r/sub/inside"), EINVAL);
	FAILS(rename("/mnt/r/sub", "/mnt/r/f"), ENOTDIR);
	FAILS(rename("/mnt/r/f", "/mnt/r/sub"), EISDIR);
	FAILS(rename("/mnt/r/missing", "/mnt/r/x"), ENOENT);
	FAILS(rename("/mnt/r/f", "/mnt/r/."), EINVAL);
	put("/mnt/r/sub/x", "x");
	FAILS(rename("/mnt/r/empty", "/mnt/r/sub"), ENOTEMPTY);
	CHECK(stat("/mnt/r/f", &a) == 0, "stat");
	OK(rename("/mnt/r/f", "/mnt/r/g"));
	CHECK(stat("/mnt/r/g", &b) == 0 && b.st_ino == a.st_ino, "replaced");
	contents("/mnt/r/g", "f");
	FAILS(stat("/mnt/r/f", &a), ENOENT);
	/* A directory moves, its `..` and the link counts with it. */
	OK(rename("/mnt/r/empty", "/mnt/r/sub/moved"));
	CHECK(stat("/mnt/r/sub/moved/..", &a) == 0 &&
	    stat("/mnt/r/sub", &b) == 0 && a.st_ino == b.st_ino, "..");
	CHECK(links("/mnt/r/sub") == 3, "links %ju",
	    (uintmax_t)links("/mnt/r/sub"));
	/* Two names for one file: nothing happens. */
	OK(link("/mnt/r/g", "/mnt/r/h"));
	OK(rename("/mnt/r/g", "/mnt/r/h"));
	CHECK(links("/mnt/r/g") == 2, "links %ju", (uintmax_t)links("/mnt/r/g"));
	unlink("/mnt/r/g");
	unlink("/mnt/r/h");
	unlink("/mnt/r/sub/x");
	rmdir("/mnt/r/sub/moved");
	rmdir("/mnt/r/sub");
	OK(rmdir("/mnt/r"));
}

static void
symlinks(void)
{
	char buf[64];
	struct stat st;
	ssize_t n;

	put("/mnt/target", "via link");
	OK(symlink("target", "/mnt/link"));
	FAILS(symlink("target", "/mnt/link"), EEXIST);
	n = readlink("/mnt/link", buf, sizeof(buf));
	CHECK(n == 6 && memcmp(buf, "target", 6) == 0, "readlink");
	CHECK(lstat("/mnt/link", &st) == 0 && S_ISLNK(st.st_mode) &&
	    st.st_size == 6, "lstat");
	contents("/mnt/link", "via link");
	FAILS(open("/mnt/link", O_RDONLY | O_NOFOLLOW), EMLINK);
	FAILS(readlink("/mnt/target", buf, sizeof(buf)), EINVAL);
	OK(unlink("/mnt/link"));
	CHECK(stat("/mnt/target", &st) == 0, "the target stays");
	/* A dangling link with O_CREAT makes its target. */
	OK(symlink("made", "/mnt/dangling"));
	OK(close(open("/mnt/dangling", O_WRONLY | O_CREAT, 0644)));
	CHECK(stat("/mnt/made", &st) == 0 && S_ISREG(st.st_mode), "made");
	unlink("/mnt/dangling");
	unlink("/mnt/made");
	unlink("/mnt/target");
}

static void
attributes(void)
{
	struct timespec ts[2];
	struct stat st;
	mode_t old;
	int fd;

	old = umask(022);
	put("/mnt/m", "");
	CHECK(stat("/mnt/m", &st) == 0 && (st.st_mode & 07777) == 0644,
	    "mode %o", st.st_mode & 07777);
	OK(mkdir("/mnt/md", 0777));
	CHECK(stat("/mnt/md", &st) == 0 && (st.st_mode & 07777) == 0755,
	    "dir mode %o", st.st_mode & 07777);
	umask(old);
	OK(chmod("/mnt/m", 0600));
	CHECK(stat("/mnt/m", &st) == 0 && (st.st_mode & 07777) == 0600,
	    "chmod %o", st.st_mode & 07777);
	fd = open("/mnt/m", O_RDONLY);
	OK(fchmod(fd, 0640));
	CHECK(fstat(fd, &st) == 0 && (st.st_mode & 07777) == 0640, "fchmod");
	OK(fchown(fd, 1001, 1002));
	CHECK(fstat(fd, &st) == 0 && st.st_uid == 1001 && st.st_gid == 1002,
	    "fchown");
	OK(chown("/mnt/m", -1, 0));
	CHECK(stat("/mnt/m", &st) == 0 && st.st_uid == 1001 && st.st_gid == 0,
	    "chown -1");
	ts[0].tv_sec = 1000000000;
	ts[0].tv_nsec = 5;
	ts[1].tv_sec = 1200000000;
	ts[1].tv_nsec = 7;
	OK(utimensat(AT_FDCWD, "/mnt/m", ts, 0));
	CHECK(stat("/mnt/m", &st) == 0 && st.st_atim.tv_sec == 1000000000 &&
	    st.st_atim.tv_nsec == 5 && st.st_mtim.tv_sec == 1200000000 &&
	    st.st_mtim.tv_nsec == 7, "utimensat");
	ts[0].tv_nsec = UTIME_OMIT;
	ts[1].tv_sec = 1300000000;
	ts[1].tv_nsec = 0;
	OK(futimens(fd, ts));
	CHECK(fstat(fd, &st) == 0 && st.st_atim.tv_sec == 1000000000 &&
	    st.st_mtim.tv_sec == 1300000000, "UTIME_OMIT");
	ts[1].tv_nsec = 1000000000;
	FAILS(futimens(fd, ts), EINVAL);
	close(fd);
	/* Flags: an immutable file cannot be removed or changed. */
	OK(chflags("/mnt/m", SF_IMMUTABLE));
	FAILS(unlink("/mnt/m"), EPERM);
	FAILS(chmod("/mnt/m", 0644), EPERM);
	CHECK(stat("/mnt/m", &st) == 0 && st.st_flags == SF_IMMUTABLE,
	    "flags %#x", st.st_flags);
	OK(chflags("/mnt/m", 0));
	OK(unlink("/mnt/m"));
	OK(rmdir("/mnt/md"));
}

static void
special(void)
{
	struct stat st;

	OK(mkfifo("/mnt/fifo", 0644));
	CHECK(stat("/mnt/fifo", &st) == 0 && S_ISFIFO(st.st_mode), "fifo");
	FAILS(mkfifo("/mnt/fifo", 0644), EEXIST);
	OK(unlink("/mnt/fifo"));
	OK(mknod("/mnt/chr", S_IFCHR | 0600, makedev(0, 77)));
	CHECK(stat("/mnt/chr", &st) == 0 && S_ISCHR(st.st_mode) &&
	    st.st_rdev == makedev(0, 77), "mknod");
	OK(unlink("/mnt/chr"));
	FAILS(mknod("/mnt/bad", 0600, 0), EINVAL);
}

static void
listing(void)
{
	static const char *names[] = { "one", "two", "three" };
	int seen[3] = { 0 }, dots = 0, other = 0;
	struct dirent *e;
	DIR *d;

	OK(mkdir("/mnt/l", 0755));
	for (int i = 0; i < 3; i++) {
		char p[64];

		snprintf(p, sizeof(p), "/mnt/l/%s", names[i]);
		put(p, "");
	}
	d = opendir("/mnt/l");
	CHECK(d != NULL, "opendir: %s", strerror(errno));
	if (d == NULL)
		return;
	while ((e = readdir(d)) != NULL) {
		int known = 0;

		if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) {
			dots++;
			CHECK(e->d_type == DT_DIR, "dot type %d", e->d_type);
			continue;
		}
		for (int i = 0; i < 3; i++)
			if (strcmp(e->d_name, names[i]) == 0) {
				seen[i]++;
				known = 1;
				CHECK(e->d_type == DT_REG, "type %d", e->d_type);
			}
		other += !known;
	}
	closedir(d);
	CHECK(dots == 2 && seen[0] == 1 && seen[1] == 1 && seen[2] == 1 &&
	    other == 0, "dots %d one %d two %d three %d other %d", dots,
	    seen[0], seen[1], seen[2], other);
	for (int i = 0; i < 3; i++) {
		char p[64];

		snprintf(p, sizeof(p), "/mnt/l/%s", names[i]);
		unlink(p);
	}
	OK(rmdir("/mnt/l"));
}

static void
locks(void)
{
	struct flock fl;
	int fd1, fd2, p[2], st;
	char c;
	pid_t pid;

	put("/mnt/lk", "0123456789");
	fd1 = open("/mnt/lk", O_RDWR);
	fd2 = open("/mnt/lk", O_RDWR);
	/* flock belongs to the open file. */
	OK(flock(fd1, LOCK_EX));
	FAILS(flock(fd2, LOCK_EX | LOCK_NB), EWOULDBLOCK);
	FAILS(flock(fd2, LOCK_SH | LOCK_NB), EWOULDBLOCK);
	OK(flock(fd1, LOCK_UN));
	OK(flock(fd2, LOCK_SH | LOCK_NB));
	OK(flock(fd1, LOCK_SH | LOCK_NB));
	close(fd2);
	OK(flock(fd1, LOCK_UN));

	/* fcntl locks belong to the process: another sees them. */
	memset(&fl, 0, sizeof(fl));
	fl.l_type = F_WRLCK;
	fl.l_whence = SEEK_SET;
	fl.l_start = 2;
	fl.l_len = 3;
	OK(fcntl(fd1, F_SETLK, &fl));
	OK(pipe(p));
	pid = fork();
	if (pid == 0) {
		struct flock q;
		int fd = open("/mnt/lk", O_RDWR), bad = 0;

		memset(&q, 0, sizeof(q));
		q.l_type = F_RDLCK;
		q.l_whence = SEEK_SET;
		q.l_start = 0;
		q.l_len = 10;
		if (fcntl(fd, F_SETLK, &q) != -1 || errno != EAGAIN)
			bad |= 1;
		if (fcntl(fd, F_GETLK, &q) != 0 || q.l_type != F_WRLCK ||
		    q.l_start != 2 || q.l_len != 3 || q.l_pid != getppid())
			bad |= 2;
		/* Outside the parent's range. */
		q.l_type = F_WRLCK;
		q.l_start = 6;
		q.l_len = 2;
		if (fcntl(fd, F_SETLK, &q) != 0)
			bad |= 4;
		c = (char)bad;
		write(p[1], &c, 1);
		_exit(0);
	}
	CHECK(read(p[0], &c, 1) == 1 && c == 0, "child saw %d", c);
	waitpid(pid, &st, 0);
	/* The process's own lock never conflicts with itself. */
	fl.l_type = F_RDLCK;
	fl.l_start = 0;
	fl.l_len = 0;
	OK(fcntl(fd1, F_SETLK, &fl));
	fl.l_type = F_GETLK;
	FAILS(fcntl(fd1, F_SETLK, &fl), EINVAL);
	/* A read lock needs the file open for reading. */
	fd2 = open("/mnt/lk", O_WRONLY);
	fl.l_type = F_RDLCK;
	FAILS(fcntl(fd2, F_SETLK, &fl), EBADF);
	/* Closing any descriptor for the file drops the process's locks. */
	close(fd2);
	pid = fork();
	if (pid == 0) {
		struct flock q;
		int fd = open("/mnt/lk", O_RDWR);

		memset(&q, 0, sizeof(q));
		q.l_type = F_WRLCK;
		q.l_whence = SEEK_SET;
		_exit(fcntl(fd, F_SETLK, &q) == 0 ? 0 : 1);
	}
	CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
	    WEXITSTATUS(st) == 0, "locks outlived a close");
	close(fd1);
	close(p[0]);
	close(p[1]);
	OK(unlink("/mnt/lk"));
}

static void
mappings(void)
{
	char buf[16];
	char *p, *q;
	int fd;

	put("/mnt/map", "");
	fd = open("/mnt/map", O_RDWR);
	OK(ftruncate(fd, 8192));
	p = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(p != MAP_FAILED, "mmap: %s", strerror(errno));
	if (p == MAP_FAILED) {
		close(fd);
		return;
	}
	/* Through the mapping, seen by read; through write, seen mapped. */
	memcpy(p + 4096, "mapped", 6);
	CHECK(pread(fd, buf, 6, 4096) == 6 && memcmp(buf, "mapped", 6) == 0,
	    "read after a mapped write");
	CHECK(pwrite(fd, "written", 7, 100) == 7, "pwrite");
	CHECK(memcmp(p + 100, "written", 7) == 0, "mapping after write");
	/* A second shared mapping is the same memory. */
	q = mmap(NULL, 8192, PROT_READ, MAP_SHARED, fd, 0);
	CHECK(q != MAP_FAILED && memcmp(q + 4096, "mapped", 6) == 0,
	    "second mapping");
	/* A private one copies on write. */
	char *r = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	CHECK(r != MAP_FAILED, "private mmap: %s", strerror(errno));
	if (r != MAP_FAILED) {
		r[100] = 'X';
		CHECK(p[100] == 'w', "a private write reached the file");
		munmap(r, 4096);
	}
	OK(msync(p, 8192, MS_SYNC));
	munmap(q, 8192);
	munmap(p, 8192);
	close(fd);
	OK(unlink("/mnt/map"));
}

static void
places(void)
{
	struct statfs sf;
	char cwd[PATH_MAX];

	CHECK(statfs("/mnt", &sf) == 0 && strcmp(sf.f_fstypename, "tmpfs") == 0
	    && strcmp(sf.f_mntonname, "/mnt") == 0, "statfs %s on %s",
	    sf.f_fstypename, sf.f_mntonname);
	OK(mkdir("/mnt/w", 0755));
	OK(mkdir("/mnt/w/x", 0755));
	OK(chdir("/mnt/w/x"));
	CHECK(getcwd(cwd, sizeof(cwd)) != NULL && strcmp(cwd, "/mnt/w/x") == 0,
	    "getcwd %s", cwd);
	OK(rename("/mnt/w", "/mnt/v"));
	CHECK(getcwd(cwd, sizeof(cwd)) != NULL && strcmp(cwd, "/mnt/v/x") == 0,
	    "getcwd after rename %s", cwd);
	/* Busy while the working directory is in it. */
	FAILS(unmount("/mnt", 0), EBUSY);
	OK(chdir("/"));
	OK(rmdir("/mnt/v/x"));
	OK(rmdir("/mnt/v"));
}

int
main(void)
{
	struct iovec iov[4];

	iov_set(&iov[0], "fstype", "tmpfs");
	iov_set(&iov[2], "fspath", "/mnt");
	if (nmount(iov, 4, 0) != 0) {
		printf("vfs: cannot mount tmpfs on /mnt: %s\n", strerror(errno));
		return (1);
	}
	directories();
	files();
	renames();
	symlinks();
	attributes();
	special();
	listing();
	locks();
	mappings();
	places();
	if (unmount("/mnt", 0) != 0) {
		printf("vfs: unmount: %s\n", strerror(errno));
		failed = 1;
	}
	if (!failed)
		printf("vfs: ok\n");
	return (failed);
}
