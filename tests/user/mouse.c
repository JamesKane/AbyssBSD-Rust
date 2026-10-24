/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * mouse: checks /dev/sysmouse through /dev/consolectl, as moused feeds
 * it: both are root's character devices with mode 0600; consolectl
 * answers CONS_GETVERS; sysmouse describes itself (MOUSE_GETHWINFO,
 * MOUSE_GETMODE) and reads nothing until moused reports, then reads each
 * report as a MouseSystems packet at level 0 and a sysmouse packet at
 * level 1, whole packets only; MOUSE_GETSTATUS sums the motion and
 * clears it; and the level goes back to 0 with the last close.
 *
 * Runs on FreeBSD too (as root, under vt, with the mouse left alone).
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/consio.h>
#include <sys/ioctl.h>
#include <sys/kbio.h>
#include <sys/mouse.h>
#include <sys/stat.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("mouse: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

static int ctl;

/* Reports through consolectl, as moused does. */
static void
report(int operation, int a, int b, int c, int d)
{
	mouse_info_t mi;

	memset(&mi, 0, sizeof(mi));
	mi.operation = operation;
	if (operation == MOUSE_BUTTON_EVENT) {
		mi.u.event.id = a;
		mi.u.event.value = b;
	} else {
		mi.u.data.x = a;
		mi.u.data.y = b;
		mi.u.data.z = c;
		mi.u.data.buttons = d;
	}
	CHECK(ioctl(ctl, CONS_MOUSECTL, &mi) == 0, "%s", strerror(errno));
}

static void
node(const char *path)
{
	struct stat st;

	CHECK(stat(path, &st) == 0, "%s: %s", path, strerror(errno));
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0600 &&
	    st.st_uid == 0 && st.st_gid == 0, "%s: mode %o owner %d:%d",
	    path, st.st_mode, st.st_uid, st.st_gid);
}

int
main(void)
{
	static const unsigned char level0[] = { 0x83, 1, 2, 2, 3 };
	static const unsigned char level1[] = {
	    0x87, 0xfe, 0x80, 0xff, 0x81, 0x7f, 0, 0x7e };
	unsigned char buf[32];
	mousestatus_t status;
	mousemode_t mode;
	mousehw_t hw;
	ssize_t n;
	int fd, level, vers;

	node("/dev/sysmouse");
	node("/dev/consolectl");
	ctl = open("/dev/consolectl", O_RDWR);
	CHECK(ctl >= 0, "consolectl: %s", strerror(errno));
	CHECK(ioctl(ctl, CONS_GETVERS, &vers) == 0 && vers == 0x200,
	    "version %#x", vers);
	CHECK(ioctl(ctl, KDGKBMODE, &level) == -1 && errno == ENOTTY,
	    "%s", strerror(errno));
	CHECK(read(ctl, buf, 1) == -1 && errno == ENODEV, "%s",
	    strerror(errno));

	fd = open("/dev/sysmouse", O_RDONLY | O_NONBLOCK);
	CHECK(fd >= 0, "sysmouse: %s", strerror(errno));
	CHECK(ioctl(fd, MOUSE_GETHWINFO, &hw) == 0 && hw.buttons == 10 &&
	    hw.iftype == MOUSE_IF_SYSMOUSE && hw.type == MOUSE_MOUSE &&
	    hw.model == MOUSE_MODEL_GENERIC, "buttons %d iftype %d",
	    hw.buttons, hw.iftype);
	CHECK(ioctl(fd, MOUSE_GETLEVEL, &level) == 0 && level == 0,
	    "level %d", level);
	CHECK(ioctl(fd, MOUSE_GETMODE, &mode) == 0 &&
	    mode.protocol == MOUSE_PROTO_MSC && mode.packetsize == 5 &&
	    mode.level == 0 && mode.rate == -1 &&
	    mode.syncmask[0] == MOUSE_MSC_SYNCMASK &&
	    mode.syncmask[1] == MOUSE_MSC_SYNC, "protocol %d packetsize %d",
	    mode.protocol, mode.packetsize);
	CHECK(read(fd, buf, sizeof(buf)) == -1 && errno == EAGAIN,
	    "empty: %s", strerror(errno));
	(void)ioctl(fd, MOUSE_GETSTATUS, &status);

	/* Left down, right 3, up 5, the wheel 1 toward the user. */
	report(MOUSE_ACTION, 3, -5, 1, MOUSE_BUTTON1DOWN);
	n = read(fd, buf, sizeof(buf));
	CHECK(n == 5 && memcmp(buf, level0, 5) == 0,
	    "level 0: %zd bytes, %02x %02x %02x %02x %02x", n, buf[0],
	    buf[1], buf[2], buf[3], buf[4]);

	/* Level 1: eight bytes, the wheel and buttons 4 to 10 after. */
	level = 1;
	CHECK(ioctl(fd, MOUSE_SETLEVEL, &level) == 0, "%s", strerror(errno));
	CHECK(ioctl(fd, MOUSE_GETMODE, &mode) == 0 &&
	    mode.protocol == MOUSE_PROTO_SYSMOUSE && mode.packetsize == 8 &&
	    mode.syncmask[0] == MOUSE_SYS_SYNCMASK, "protocol %d",
	    mode.protocol);
	report(MOUSE_ACTION, -3, 1000, -1, MOUSE_BUTTON4DOWN);
	CHECK(read(fd, buf, 7) == 0, "a short read reads nothing");
	n = read(fd, buf, sizeof(buf));
	CHECK(n == 8 && memcmp(buf, level1, 8) == 0,
	    "level 1: %zd bytes, %02x %02x %02x %02x %02x %02x %02x %02x",
	    n, buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6],
	    buf[7]);

	CHECK(ioctl(fd, MOUSE_GETSTATUS, &status) == 0 &&
	    status.dx == 0 && status.dy == 995 && status.dz == 0 &&
	    status.button == MOUSE_BUTTON4DOWN &&
	    (status.flags & MOUSE_POSCHANGED),
	    "status: flags %#x button %#x dx %d dy %d dz %d", status.flags,
	    status.button, status.dx, status.dy, status.dz);
	CHECK(ioctl(fd, MOUSE_GETSTATUS, &status) == 0 &&
	    status.flags == 0 && status.dx == 0 &&
	    status.obutton == MOUSE_BUTTON4DOWN, "cleared: flags %#x dx %d",
	    status.flags, status.dx);
	/*
	 * With the status cleared: no change, no packet; a button alone is
	 * one. (Not the middle button, which would paste into vt.)
	 */
	report(MOUSE_MOTION_EVENT, 0, 0, 0, 0);
	report(MOUSE_BUTTON_EVENT, MOUSE_BUTTON3DOWN, 1, 0, 0);
	report(MOUSE_BUTTON_EVENT, MOUSE_BUTTON3DOWN, 0, 0, 0);
	n = read(fd, buf, sizeof(buf));
	CHECK(n == 16 && buf[0] == 0x86 && buf[8] == 0x87,
	    "buttons: %zd bytes, %02x %02x", n, buf[0], buf[8]);

	report(MOUSE_ACTION, 0, 0, 0, 0);

	level = 2;
	CHECK(ioctl(fd, MOUSE_SETLEVEL, &level) == -1 && errno == EINVAL,
	    "level 2: %s", strerror(errno));
	mode.level = -1;
	CHECK(ioctl(fd, MOUSE_SETMODE, &mode) == 0 &&
	    ioctl(fd, MOUSE_GETLEVEL, &level) == 0 && level == 1,
	    "SETMODE -1 keeps level 1: %d", level);
	CHECK(ioctl(fd, MOUSE_READDATA, buf) == -1 && errno == ENOTTY,
	    "READDATA: %s", strerror(errno));

	/* The last close drops the packets and the level. */
	close(fd);
	report(MOUSE_ACTION, 1, 1, 0, 0);
	fd = open("/dev/sysmouse", O_RDONLY | O_NONBLOCK);
	CHECK(fd >= 0, "reopen: %s", strerror(errno));
	CHECK(ioctl(fd, MOUSE_GETLEVEL, &level) == 0 && level == 0,
	    "level after reopen %d", level);
	CHECK(read(fd, buf, sizeof(buf)) == -1 && errno == EAGAIN,
	    "packets before the open: %s", strerror(errno));
	close(fd);
	close(ctl);
	return (failed);
}
