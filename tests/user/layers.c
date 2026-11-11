/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * layers: nullfs and fdescfs, mounted with nmount on a tmpfs on /mnt.
 *
 * nullfs: the lower tree shows through, with the lower files' numbers and
 * the layer's device; one page cache (a mapping through the layer sees
 * writes below); one set of advisory locks; changes made through it land
 * below; renames and links between it and another mount are EXDEV; "ro"
 * refuses writes; getcwd names the layer; a file mounts over a file; the
 * lower mount and a busy layer do not unmount; a layer over its own
 * lower vnode is EDEADLK.
 *
 * In a jail with allow.mount and allow.mount.nullfs (enforce_statfs 1),
 * nullfs and fdescfs mount within it, named from its root; with
 * allow.mount.nonullfs, nullfs is EPERM.
 *
 * fdescfs (FreeBSD 15.1's): the caller's descriptors as character
 * devices numbered fd + 3 (listed as fd + 5), opens that share the
 * descriptor, EBADF for a closed one, EROFS to remove; "linrdlnk" makes
 * them links readlink(2) reads; "nodup" makes a vnode's descriptor that
 * vnode.
 *
 * Runs on FreeBSD too (as root, with nothing mounted on /mnt), where the
 * expected answers come from.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/file.h>
#include <sys/jail.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("layers: line %d: %s: ", __LINE__, #cond);	\
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

/*
 * Mounts `fstype` on `path` with the name and value pairs that follow
 * (NULL ends them; a NULL value is an empty one); returns nmount's
 * result, errno set.
 */
static int
mount_with(const char *fstype, const char *path, int flags, ...)
{
	struct iovec iov[16];
	const char *name, *value;
	va_list ap;
	int n = 0;

	iov[n].iov_base = (void *)(uintptr_t)"fstype";
	iov[n++].iov_len = sizeof("fstype");
	iov[n].iov_base = (void *)(uintptr_t)fstype;
	iov[n++].iov_len = strlen(fstype) + 1;
	iov[n].iov_base = (void *)(uintptr_t)"fspath";
	iov[n++].iov_len = sizeof("fspath");
	iov[n].iov_base = (void *)(uintptr_t)path;
	iov[n++].iov_len = strlen(path) + 1;
	va_start(ap, flags);
	while ((name = va_arg(ap, const char *)) != NULL && n < 14) {
		value = va_arg(ap, const char *);
		iov[n].iov_base = (void *)(uintptr_t)name;
		iov[n++].iov_len = strlen(name) + 1;
		iov[n].iov_base = (void *)(uintptr_t)value;
		iov[n++].iov_len = value == NULL ? 0 : strlen(value) + 1;
	}
	va_end(ap);
	return (nmount(iov, n, flags));
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
nullfs(void)
{
	struct stat ls, us;
	struct statfs lf, uf;
	char cwd[PATH_MAX], *map;
	int fd, lfd, ufd;

	OK(mkdir("/mnt/lower", 0755));
	OK(mkdir("/mnt/upper", 0755));
	OK(mkdir("/mnt/lower/d", 0755));
	put("/mnt/lower/f", "hello");
	/* mount_nullfs passes "target". */
	if (mount_with("nullfs", "/mnt/upper", 0, "target", "/mnt/lower",
	    NULL) != 0) {
		CHECK(0, "nullfs: %s", strerror(errno));
		return;
	}
	contents("/mnt/upper/f", "hello");
	OK(stat("/mnt/lower/f", &ls));
	OK(stat("/mnt/upper/f", &us));
	CHECK(ls.st_ino == us.st_ino && ls.st_dev != us.st_dev,
	    "ino %ju/%ju dev %ju/%ju", (uintmax_t)ls.st_ino,
	    (uintmax_t)us.st_ino, (uintmax_t)ls.st_dev, (uintmax_t)us.st_dev);
	OK(statfs("/mnt/lower", &lf));
	OK(statfs("/mnt/upper", &uf));
	CHECK(strcmp(uf.f_fstypename, "nullfs") == 0 &&
	    strcmp(uf.f_mntfromname, "/mnt/lower") == 0 &&
	    strcmp(uf.f_mntonname, "/mnt/upper") == 0 &&
	    uf.f_type == lf.f_type, "statfs %s from %s on %s type %u/%u",
	    uf.f_fstypename, uf.f_mntfromname, uf.f_mntonname, uf.f_type,
	    lf.f_type);
	CHECK(uf.f_fsid.val[0] != lf.f_fsid.val[0], "same fsid");

	/* One page cache: a mapping through the layer sees writes below. */
	ufd = open("/mnt/upper/f", O_RDWR);
	lfd = open("/mnt/lower/f", O_RDWR);
	CHECK(ufd >= 0 && lfd >= 0, "open: %s", strerror(errno));
	map = mmap(NULL, 4096, PROT_READ, MAP_SHARED, ufd, 0);
	CHECK(map != MAP_FAILED, "mmap: %s", strerror(errno));
	CHECK(pwrite(lfd, "HE", 2, 0) == 2, "pwrite");
	if (map != MAP_FAILED) {
		CHECK(memcmp(map, "HEllo", 5) == 0, "mapping holds %.5s", map);
		munmap(map, 4096);
	}
	/* One set of advisory locks. */
	OK(flock(ufd, LOCK_EX));
	FAILS(flock(lfd, LOCK_EX | LOCK_NB), EWOULDBLOCK);
	OK(flock(ufd, LOCK_UN));
	OK(flock(lfd, LOCK_EX | LOCK_NB));
	close(lfd);
	close(ufd);

	/* Changes through the layer are made below. */
	put("/mnt/upper/new", "made above");
	contents("/mnt/lower/new", "made above");
	OK(mkdir("/mnt/upper/d/e", 0755));
	OK(rename("/mnt/upper/new", "/mnt/upper/d/e/moved"));
	contents("/mnt/lower/d/e/moved", "made above");
	OK(link("/mnt/upper/d/e/moved", "/mnt/upper/linked"));
	OK(lstat("/mnt/lower/linked", &ls));
	CHECK(ls.st_nlink == 2, "nlink %ju", (uintmax_t)ls.st_nlink);
	OK(unlink("/mnt/upper/linked"));
	FAILS(lstat("/mnt/lower/linked", &ls), ENOENT);
	/* Between mounts, the lower one too: EXDEV. */
	FAILS(rename("/mnt/upper/d/e/moved", "/mnt/lower/x"), EXDEV);
	FAILS(rename("/mnt/lower/f", "/mnt/upper/x"), EXDEV);
	FAILS(link("/mnt/lower/f", "/mnt/upper/x"), EXDEV);
	/* getcwd names the layer. */
	OK(chdir("/mnt/upper/d/e"));
	CHECK(getcwd(cwd, sizeof(cwd)) != NULL &&
	    strcmp(cwd, "/mnt/upper/d/e") == 0, "getcwd %s", cwd);
	/* Busy while the working directory is in it. */
	FAILS(unmount("/mnt/upper", 0), EBUSY);
	OK(chdir("/"));
	/* The lower mount stays while the layer is on it. */
	FAILS(unmount("/mnt", 0), EBUSY);
	/* Not over itself through another layer. */
	FAILS(mount_with("nullfs", "/mnt/upper/d", 0, "target", "/mnt/lower/d",
	    NULL), EDEADLK);
	/* A file over a directory: EINVAL. */
	FAILS(mount_with("nullfs", "/mnt/upper/d", 0, "target", "/mnt/lower/f",
	    NULL), EINVAL);
	/* Busy while a file in it is open; then it goes. */
	fd = open("/mnt/upper/f", O_RDONLY);
	CHECK(fd >= 0, "open: %s", strerror(errno));
	FAILS(unmount("/mnt/upper", 0), EBUSY);
	close(fd);
	OK(unmount("/mnt/upper", 0));
	FAILS(stat("/mnt/upper/f", &us), ENOENT);

	/* Read-only, with nocache. */
	if (mount_with("nullfs", "/mnt/upper", MNT_RDONLY, "target",
	    "/mnt/lower", "nocache", NULL, NULL) != 0) {
		CHECK(0, "nullfs ro: %s", strerror(errno));
		return;
	}
	contents("/mnt/upper/f", "HEllo");
	FAILS(open("/mnt/upper/f", O_WRONLY), EROFS);
	FAILS(open("/mnt/upper/g", O_WRONLY | O_CREAT, 0644), EROFS);
	FAILS(mkdir("/mnt/upper/g", 0755), EROFS);
	FAILS(unlink("/mnt/upper/f"), EROFS);
	FAILS(chmod("/mnt/upper/f", 0600), EROFS);
	/* Below, the file still changes. */
	put("/mnt/lower/f", "changed below");
	contents("/mnt/upper/f", "changed below");
	OK(statfs("/mnt/upper", &uf));
	CHECK(uf.f_flags & MNT_RDONLY, "flags %#jx", (uintmax_t)uf.f_flags);
	OK(unmount("/mnt/upper", 0));

	/* A file over a file. */
	put("/mnt/over", "");
	if (mount_with("nullfs", "/mnt/over", 0, "target", "/mnt/lower/f",
	    NULL) == 0) {
		contents("/mnt/over", "changed below");
		OK(unmount("/mnt/over", 0));
	} else
		CHECK(0, "file over file: %s", strerror(errno));
	OK(unlink("/mnt/over"));
}

/* A persistent jail at `path`, nullfs mounts allowed or not; its jid. */
static int
make_jail(const char *path, int nullfs_allowed)
{
	struct iovec iov[14];
	char errmsg[256];
	int one = 1, statfs1 = 1, n = 0, jid;

#define	PARAM(name, value, len) do {					\
	iov[n].iov_base = (void *)(uintptr_t)(name);			\
	iov[n++].iov_len = strlen(name) + 1;				\
	iov[n].iov_base = (void *)(uintptr_t)(value);			\
	iov[n++].iov_len = (len);					\
} while (0)
	PARAM("path", path, strlen(path) + 1);
	PARAM("persist", NULL, 0);
	PARAM("allow.mount", &one, sizeof(one));
	/* A flag is set by its name, whatever its value (vfs_flagopt). */
	if (nullfs_allowed)
		PARAM("allow.mount.nullfs", NULL, 0);
	else
		PARAM("allow.mount.nonullfs", NULL, 0);
	PARAM("allow.mount.fdescfs", &one, sizeof(one));
	PARAM("enforce_statfs", &statfs1, sizeof(statfs1));
	PARAM("errmsg", errmsg, sizeof(errmsg));
#undef PARAM
	errmsg[0] = '\0';
	jid = jail_set(iov, n, JAIL_CREATE);
	CHECK(jid > 0, "jail_set: %s %s", strerror(errno), errmsg);
	return (jid);
}

/* What happens inside jail `jid`, as a child; its exit status. */
static int
in_jail(int jid, int nullfs_allowed)
{
	struct statfs sf;
	pid_t pid;
	int status;

	pid = fork();
	if (pid == 0) {
		if (jail_attach(jid) != 0)
			_exit(10);
		if (!nullfs_allowed)
			_exit(mount_with("nullfs", "/dst", 0, "target", "/src",
			    NULL) == -1 && errno == EPERM ? 0 : 11);
		if (mount_with("nullfs", "/dst", 0, "target", "/src",
		    NULL) != 0)
			_exit(12);
		if (statfs("/dst", &sf) != 0 ||
		    strcmp(sf.f_fstypename, "nullfs") != 0 ||
		    strcmp(sf.f_mntonname, "/dst") != 0)
			_exit(13);
		if (access("/dst/file", F_OK) != 0)
			_exit(14);
		if (mount_with("fdescfs", "/fd", 0, "from", "fdescfs",
		    NULL) != 0 || access("/fd/0", F_OK) != 0 ||
		    unmount("/fd", 0) != 0)
			_exit(15);
		_exit(unmount("/dst", 0) == 0 ? 0 : 16);
	}
	if (pid < 0 || waitpid(pid, &status, 0) != pid)
		return (-1);
	return (WIFEXITED(status) ? WEXITSTATUS(status) : -2);
}

static void
jailed(void)
{
	int jid, r;

	OK(mkdir("/mnt/jroot", 0755));
	OK(mkdir("/mnt/jroot/src", 0755));
	OK(mkdir("/mnt/jroot/dst", 0755));
	OK(mkdir("/mnt/jroot/fd", 0755));
	put("/mnt/jroot/src/file", "in the jail");
	jid = make_jail("/mnt/jroot", 1);
	if (jid > 0) {
		r = in_jail(jid, 1);
		CHECK(r == 0, "in the jail: %d", r);
		OK(jail_remove(jid));
	}
	jid = make_jail("/mnt/jroot", 0);
	if (jid > 0) {
		r = in_jail(jid, 0);
		CHECK(r == 0, "in the jail with allow.mount.nonullfs: %d", r);
		OK(jail_remove(jid));
	}
	OK(unlink("/mnt/jroot/src/file"));
	OK(rmdir("/mnt/jroot/fd"));
	OK(rmdir("/mnt/jroot/dst"));
	OK(rmdir("/mnt/jroot/src"));
	OK(rmdir("/mnt/jroot"));
}

/* Whether directory `dir` lists `name`, of type `type`, numbered `ino`. */
static int
listed(const char *dir, const char *name, int type, ino_t ino)
{
	struct dirent *e;
	DIR *d = opendir(dir);
	int found = 0;

	if (d == NULL)
		return (0);
	while ((e = readdir(d)) != NULL)
		if (strcmp(e->d_name, name) == 0 && e->d_type == type &&
		    e->d_fileno == ino)
			found = 1;
	closedir(d);
	return (found);
}

static void
fdescfs(void)
{
	struct stat st, fst;
	struct statfs sf;
	char path[64], buf[PATH_MAX];
	int fd, copy, pipes[2], dfd;
	ssize_t n;

	OK(mkdir("/mnt/fd", 0755));
	put("/mnt/x", "0123456789");
	if (mount_with("fdescfs", "/mnt/fd", 0, "from", "fdescfs", NULL) != 0) {
		CHECK(0, "fdescfs: %s", strerror(errno));
		return;
	}
	OK(statfs("/mnt/fd", &sf));
	CHECK(strcmp(sf.f_fstypename, "fdescfs") == 0 &&
	    strcmp(sf.f_mntfromname, "fdescfs") == 0 &&
	    (sf.f_flags & MNT_LOCAL) == 0, "statfs %s from %s flags %#jx",
	    sf.f_fstypename, sf.f_mntfromname, (uintmax_t)sf.f_flags);
	fd = open("/mnt/x", O_RDONLY);
	CHECK(fd >= 0, "open: %s", strerror(errno));
	snprintf(path, sizeof(path), "/mnt/fd/%d", fd);
	OK(stat(path, &st));
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0555 &&
	    st.st_ino == (ino_t)fd + 3 && st.st_nlink == 1 && st.st_size == 0 &&
	    st.st_uid == 0, "mode %o ino %ju nlink %ju", st.st_mode,
	    (uintmax_t)st.st_ino, (uintmax_t)st.st_nlink);
	OK(stat("/mnt/fd", &st));
	CHECK(S_ISDIR(st.st_mode) && (st.st_mode & 07777) == 0555 &&
	    st.st_ino == 1 && st.st_nlink == 2, "root mode %o ino %ju",
	    st.st_mode, (uintmax_t)st.st_ino);
	snprintf(path, sizeof(path), "%d", fd);
	CHECK(listed("/mnt/fd", path, DT_CHR, (ino_t)fd + 5), "not listed");
	/* An open shares the descriptor: its offset, its access. */
	OK(lseek(fd, 3, SEEK_SET));
	snprintf(path, sizeof(path), "/mnt/fd/%d", fd);
	copy = open(path, O_RDONLY);
	CHECK(copy >= 0 && lseek(copy, 0, SEEK_CUR) == 3, "open copy: %s",
	    strerror(errno));
	if (copy >= 0)
		close(copy);
	FAILS(open(path, O_RDWR), EACCES);
	FAILS(readlink(path, buf, sizeof(buf)), EINVAL);
	FAILS(unlink(path), EROFS);
	FAILS(stat("/mnt/fd/99", &st), EBADF);
	FAILS(stat("/mnt/fd/007", &st), ENOENT);
	FAILS(stat("/mnt/fd/x", &st), ENOENT);
	FAILS(mkdir("/mnt/fd/x", 0755), ENOENT);
	OK(unmount("/mnt/fd", 0));

	/* linrdlnk: links readlink reads, which opens still share. */
	if (mount_with("fdescfs", "/mnt/fd", 0, "from", "fdescfs", "linrdlnk",
	    NULL, NULL) != 0) {
		CHECK(0, "fdescfs linrdlnk: %s", strerror(errno));
		close(fd);
		return;
	}
	OK(lstat(path, &st));
	CHECK(S_ISLNK(st.st_mode), "lstat mode %o", st.st_mode);
	OK(stat(path, &st));
	CHECK(S_ISLNK(st.st_mode), "stat mode %o", st.st_mode);
	n = readlink(path, buf, sizeof(buf));
	CHECK(n == 6 && memcmp(buf, "/mnt/x", 6) == 0, "readlink %zd %.*s", n,
	    (int)(n < 0 ? 0 : n), buf);
	copy = open(path, O_RDONLY);
	CHECK(copy >= 0 && lseek(copy, 0, SEEK_CUR) == 3, "open copy: %s",
	    strerror(errno));
	if (copy >= 0)
		close(copy);
	snprintf(buf, sizeof(buf), "%d", fd);
	CHECK(listed("/mnt/fd", buf, DT_LNK, (ino_t)fd + 5), "not listed");
	OK(pipe(pipes));
	snprintf(path, sizeof(path), "/mnt/fd/%d", pipes[0]);
	n = readlink(path, buf, sizeof(buf));
	CHECK(n == 20 && memcmp(buf, "anon_inode:[unknown]", 20) == 0,
	    "readlink pipe %zd", n);
	close(pipes[0]);
	close(pipes[1]);
	OK(unmount("/mnt/fd", 0));

	/* nodup: a vnode's descriptor is the vnode, opened anew. */
	if (mount_with("fdescfs", "/mnt/fd", 0, "from", "fdescfs", "nodup",
	    NULL, NULL) != 0) {
		CHECK(0, "fdescfs nodup: %s", strerror(errno));
		close(fd);
		return;
	}
	snprintf(path, sizeof(path), "/mnt/fd/%d", fd);
	OK(fstat(fd, &fst));
	OK(stat(path, &st));
	CHECK(st.st_ino == fst.st_ino && st.st_dev == fst.st_dev &&
	    S_ISREG(st.st_mode), "nodup stat ino %ju", (uintmax_t)st.st_ino);
	copy = open(path, O_RDONLY);
	CHECK(copy >= 0 && lseek(copy, 0, SEEK_CUR) == 0, "nodup open: %s",
	    strerror(errno));
	if (copy >= 0)
		close(copy);
	OK(mkdir("/mnt/dir", 0755));
	put("/mnt/dir/marker", "m");
	dfd = open("/mnt/dir", O_RDONLY | O_DIRECTORY);
	CHECK(dfd >= 0, "open dir: %s", strerror(errno));
	snprintf(path, sizeof(path), "/mnt/fd/%d/marker", dfd);
	contents(path, "m");
	close(dfd);
	/* A pipe's descriptor is still a node, shared on open. */
	OK(pipe(pipes));
	snprintf(path, sizeof(path), "/mnt/fd/%d", pipes[1]);
	OK(stat(path, &st));
	CHECK(S_ISCHR(st.st_mode), "pipe mode %o", st.st_mode);
	close(pipes[0]);
	close(pipes[1]);
	OK(unmount("/mnt/fd", 0));
	close(fd);
	OK(unlink("/mnt/dir/marker"));
	OK(rmdir("/mnt/dir"));
}

int
main(void)
{
	/* What failed shows even if a check crashes. */
	setvbuf(stdout, NULL, _IONBF, 0);
	if (mount_with("tmpfs", "/mnt", 0, NULL) != 0) {
		printf("layers: cannot mount tmpfs on /mnt: %s\n",
		    strerror(errno));
		return (1);
	}
	nullfs();
	fdescfs();
	/* After fdescfs: FreeBSD knows allow.mount.fdescfs once it is loaded. */
	jailed();
	if (unmount("/mnt", 0) != 0) {
		printf("layers: unmount: %s\n", strerror(errno));
		failed = 1;
	}
	if (!failed)
		printf("layers: ok\n");
	return (failed);
}
