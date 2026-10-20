/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * disks: checks devfs's disk nodes, read-only: the first disk found
 * (nda0, vtbd0, ada0) is a character device owned by root:operator with
 * mode 0640, answers DIOCGSECTORSIZE and DIOCGMEDIASIZE, reads whole
 * sectors at sector offsets (EINVAL otherwise), reads nothing at its end,
 * and its partitions (if it has a GPT) are nodes beside it.
 *
 * Runs on FreeBSD too (as root). Writes nothing. Exits 0 on success, or
 * when there is no disk; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/disk.h>
#include <sys/ioctl.h>
#include <sys/stat.h>

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
		printf("disks: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

int
main(void)
{
	static const char *names[] = { "nda0", "vtbd0", "ada0" };
	char path[64], buf[8192];
	const char *name = NULL;
	struct stat st;
	u_int ss = 0;
	off_t size = 0;
	int fd = -1;

	for (size_t i = 0; i < nitems(names); i++) {
		snprintf(path, sizeof(path), "/dev/%s", names[i]);
		if (stat(path, &st) == 0) {
			name = names[i];
			break;
		}
	}
	if (name == NULL) {
		printf("disks: no disk; skipped\n");
		return (0);
	}
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0640 &&
	    st.st_uid == 0 && st.st_gid == 5, "%s: mode %o owner %u:%u", path,
	    st.st_mode, st.st_uid, st.st_gid);
	fd = open(path, O_RDONLY);
	CHECK(fd >= 0, "open %s: %s", path, strerror(errno));
	if (fd < 0)
		return (1);
	CHECK(ioctl(fd, DIOCGSECTORSIZE, &ss) == 0 && (ss == 512 || ss == 4096),
	    "sector size %u: %s", ss, strerror(errno));
	CHECK(ioctl(fd, DIOCGMEDIASIZE, &size) == 0 && size > 0 &&
	    size % ss == 0, "media size %jd", (intmax_t)size);
	if (ss == 0 || size <= 0 || ss > sizeof(buf)) {
		close(fd);
		return (1);
	}
	/* Whole sectors at sector offsets; nothing else. */
	CHECK(pread(fd, buf, ss, 0) == (ssize_t)ss, "read: %s", strerror(errno));
	CHECK(pread(fd, buf, ss * 2, ss) == (ssize_t)ss * 2, "read 2: %s",
	    strerror(errno));
	errno = 0;
	CHECK(pread(fd, buf, ss, 1) == -1 && errno == EINVAL,
	    "misaligned offset: %s", strerror(errno));
	errno = 0;
	CHECK(pread(fd, buf, ss - 1, 0) == -1 && errno == EINVAL,
	    "partial sector: %s", strerror(errno));
	CHECK(pread(fd, buf, ss, size) == 0, "read at the end");
	/* read advances the offset. */
	CHECK(lseek(fd, 0, SEEK_SET) == 0 && read(fd, buf, ss) == (ssize_t)ss &&
	    lseek(fd, 0, SEEK_CUR) == (off_t)ss, "offset after read");
	/* A GPT's partitions: the first, if the disk has one, beside it. */
	if (pread(fd, buf, ss * 2, 0) == (ssize_t)ss * 2 &&
	    memcmp(buf + ss, "EFI PART", 8) == 0) {
		snprintf(path, sizeof(path), "/dev/%sp1", name);
		CHECK(stat(path, &st) == 0 && S_ISCHR(st.st_mode), "%s: %s",
		    path, strerror(errno));
	}
	close(fd);
	if (!failed)
		printf("disks: ok (%s, %jd bytes, %u byte sectors)\n", name,
		    (intmax_t)size, ss);
	return (failed);
}
