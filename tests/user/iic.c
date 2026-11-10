/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * iic: checks /dev/iicN against a 256-byte EEPROM at address 0x50 (a
 * write sets its pointer and stores on; a read reads on), on a controller
 * that does whole transfers only, as cdnc_i2c: the node is root's, mode
 * 0600; I2CSTART is ENODEV, I2CSTOP does nothing, I2CWRITE, I2CREAD and
 * I2CRPTSTART are EINVAL with nothing started, I2CRSTCARD succeeds;
 * read(2) is ENXIO until I2CSADDR and ENODEV after; I2CRDWR writes with
 * a NOSTART continuation and reads back after a repeated START; a target
 * that does not answer is IIC_ENOACK as it is (2); more than
 * IIC_RDRW_MAX_MSGS messages is EINVAL, a bad pointer EFAULT. Then
 * FreeBSD's i2c(8) reads the bytes back, and its scan finds 50.
 *
 * Takes the device as its argument (the kernel test attaches such a
 * controller and runs it); without one it skips, as it does on FreeBSD,
 * which has no such device in a virtual machine.
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/wait.h>

#include <dev/iicbus/iic.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define	I2C	"/usr/sbin/i2c"

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("iic: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* Runs argv; its standard output in out (NUL-terminated), and its exit
 * status, or -1. */
static int
run(char *const argv[], char *out, size_t len)
{
	pid_t pid;
	ssize_t n;
	size_t got;
	int p[2], status;

	if (pipe(p) != 0)
		return (-1);
	pid = fork();
	if (pid == 0) {
		close(p[0]);
		if (dup2(p[1], 1) < 0)
			_exit(126);
		execv(argv[0], argv);
		_exit(127);
	}
	close(p[1]);
	got = 0;
	while (got + 1 < len && (n = read(p[0], out + got, len - 1 - got)) > 0)
		got += n;
	out[got] = '\0';
	close(p[0]);
	if (pid < 0 || waitpid(pid, &status, 0) != pid || !WIFEXITED(status))
		return (-1);
	return (WEXITSTATUS(status));
}

static int
rdwr(int fd, struct iic_msg *msgs, uint32_t n)
{
	struct iic_rdwr_data d;

	d.msgs = msgs;
	d.nmsgs = n;
	return (ioctl(fd, I2CRDWR, &d));
}

int
main(int argc, char **argv)
{
	struct iic_msg m[IIC_RDRW_MAX_MSGS + 1];
	struct iiccmd cmd;
	struct stat st;
	uint8_t off, data[3] = { 0xaa, 0xbb, 0xcc }, back[4], b;
	char out[256];
	int fd, r;

	if (argc < 2) {
		printf("iic: no device given, skipped\n");
		return (0);
	}
	fd = open(argv[1], O_RDWR);
	CHECK(fd >= 0, "open %s: %s", argv[1], strerror(errno));
	if (fd < 0)
		return (1);
	CHECK(fstat(fd, &st) == 0, "%s", strerror(errno));
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 0777) == 0600 &&
	    st.st_uid == 0 && st.st_gid == 0, "mode %o uid %d gid %d",
	    st.st_mode, st.st_uid, st.st_gid);

	/* START-mode requests: the controller has no START of its own. */
	memset(&cmd, 0, sizeof(cmd));
	cmd.slave = 0x50 << 1;
	errno = 0;
	CHECK(ioctl(fd, I2CSTART, &cmd) == -1 && errno == ENODEV, "%s",
	    strerror(errno));
	CHECK(ioctl(fd, I2CSTOP) == 0, "%s", strerror(errno));
	errno = 0;
	CHECK(ioctl(fd, I2CWRITE, &cmd) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	errno = 0;
	CHECK(ioctl(fd, I2CREAD, &cmd) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	errno = 0;
	CHECK(ioctl(fd, I2CRPTSTART, &cmd) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
	CHECK(ioctl(fd, I2CRSTCARD, &cmd) == 0, "%s", strerror(errno));
	errno = 0;
	CHECK(read(fd, &b, 1) == -1 && errno == ENXIO, "%s", strerror(errno));
	b = 0x50 << 1;
	CHECK(ioctl(fd, I2CSADDR, &b) == 0, "%s", strerror(errno));
	errno = 0;
	CHECK(read(fd, &b, 1) == -1 && errno == ENODEV, "%s",
	    strerror(errno));

	/* i2c -m tr's write at an offset: the data continues the offset. */
	off = 0x10;
	m[0] = (struct iic_msg){ 0x50 << 1, IIC_M_WR | IIC_M_NOSTOP, 1, &off };
	m[1] = (struct iic_msg){ 0x50 << 1, IIC_M_WR | IIC_M_NOSTART, 3,
	    data };
	CHECK(rdwr(fd, m, 2) == 0, "write: %s", strerror(errno));
	/* And its read: a repeated START. */
	off = 0x0f;
	memset(back, 0, sizeof(back));
	m[0] = (struct iic_msg){ 0x50 << 1, IIC_M_WR | IIC_M_NOSTOP, 1, &off };
	m[1] = (struct iic_msg){ 0x50 << 1, IIC_M_RD, 4, back };
	CHECK(rdwr(fd, m, 2) == 0, "read: %s", strerror(errno));
	CHECK(back[1] == 0xaa && back[2] == 0xbb && back[3] == 0xcc,
	    "read %02x %02x %02x %02x", back[0], back[1], back[2], back[3]);

	/* Nobody at 0x51: IIC_ENOACK, returned as it is. */
	m[0] = (struct iic_msg){ 0x51 << 1, IIC_M_RD, 1, &b };
	errno = 0;
	r = rdwr(fd, m, 1);
	CHECK(r == -1 && errno == 2, "%d %s", r, strerror(errno));
	errno = 0;
	CHECK(rdwr(fd, m, IIC_RDRW_MAX_MSGS + 1) == -1 && errno == EINVAL,
	    "%s", strerror(errno));
	errno = 0;
	CHECK(rdwr(fd, (struct iic_msg *)8, 1) == -1 && errno == EFAULT,
	    "%s", strerror(errno));
	m[0] = (struct iic_msg){ 0x50 << 1, IIC_M_WR, 4, (uint8_t *)8 };
	errno = 0;
	CHECK(rdwr(fd, m, 1) == -1 && errno == EFAULT, "%s", strerror(errno));
	close(fd);

	/* FreeBSD's own tool. */
	if (access(I2C, X_OK) == 0) {
		char *rd[] = { I2C, "-a", "0x50", "-d", "r", "-o", "0x10",
		    "-c", "3", "-f", argv[1], NULL };
		char *scan[] = { I2C, "-s", "-f", argv[1], NULL };

		r = run(rd, out, sizeof(out));
		CHECK(r == 0 && strcmp(out, "aa bb cc \n") == 0,
		    "i2c -a: %d \"%s\"", r, out);
		r = run(scan, out, sizeof(out));
		CHECK(r == 0 && strcmp(out, "50\n") == 0, "i2c -s: %d \"%s\"",
		    r, out);
		if (!failed)
			printf("iic: %s, and i2c(8) on it, as expected\n",
			    argv[1]);
	} else
		printf("iic: no %s, its part skipped\n", I2C);
	return (failed);
}
