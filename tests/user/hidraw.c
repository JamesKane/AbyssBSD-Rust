/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * hidraw: HID devices through hidraw(4), as hidapi and libfido2 use them.
 * hidraw is loaded (kldload; it is a module on FreeBSD) and each
 * /dev/hidrawN checked: root:operator 0600, one open at a time; its
 * report descriptor by both interfaces (HIDIOCGRDESC and the uhid-style
 * HIDRAW_GET_REPORT_DESC) and its size, its identity (HIDIOCGRAWINFO,
 * HIDIOCGRAWNAME, HIDIOCGRAWPHYS, HIDRAW_GET_DEVICEINFO); a pointing
 * device's input report read three ways (HIDRAW_GET_REPORT,
 * HIDIOCGINPUT, and read(2) in immediate mode); a non-blocking read with
 * nothing queued, poll and kqueue; writes to a device without output
 * reports refused, and a keyboard's LED report written. hidraw is unloaded
 * again if this loaded it.
 *
 * Runs on FreeBSD too (as root, with USB HID devices). Exits 0 on success;
 * prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/event.h>
#include <sys/ioctl.h>
#include <sys/linker.h>
#include <sys/module.h>
#include <sys/stat.h>

#include <dev/hid/hid.h>
#include <dev/hid/hidraw.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("hidraw: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	GID_OPERATOR 5

static int pointers, keyboards;

/* Whether descriptor `d` starts a Generic Desktop collection of `usage`. */
static int
is_desktop(const uint8_t *d, int len, int usage)
{
	return (len >= 4 && d[0] == 0x05 && d[1] == 0x01 && d[2] == 0x09 &&
	    d[3] == usage);
}

static void
check_pointer(int f)
{
	struct hidraw_gen_descriptor hgd;
	uint8_t a[64], b[65], c[64];
	struct pollfd pfd;
	struct kevent kev;
	int isize, n, on, kq;

	/* The current input report, through GET_REPORT. */
	memset(&hgd, 0, sizeof(hgd));
	memset(a, 0, sizeof(a));
	hgd.hgd_data = a;
	hgd.hgd_maxlen = sizeof(a);
	hgd.hgd_report_type = HID_INPUT_REPORT;
	CHECK(ioctl(f, HIDRAW_GET_REPORT, &hgd) == 0, "GET_REPORT: %s",
	    strerror(errno));
	isize = hgd.hgd_actlen;
	CHECK(isize > 0 && isize < 64, "input report of %d bytes", isize);
	if (isize <= 0 || isize >= 64)
		return;
	/* The same through the Linux request: an ID byte of 0 first. */
	memset(b, 0, sizeof(b));
	n = ioctl(f, HIDIOCGINPUT(isize + 1), b);
	CHECK(n == isize + 1, "HIDIOCGINPUT: %d, %s", n, strerror(errno));
	/* Nothing is queued: a non-blocking read waits for nothing. */
	CHECK(fcntl(f, F_SETFL, O_NONBLOCK) == 0, "%s", strerror(errno));
	CHECK(read(f, c, sizeof(c)) == -1 && errno == EAGAIN,
	    "read without input: %s", strerror(errno));
	pfd.fd = f;
	pfd.events = POLLIN | POLLOUT;
	CHECK(poll(&pfd, 1, 0) == 1 && pfd.revents == POLLOUT,
	    "poll: revents %#x", pfd.revents);
	kq = kqueue();
	EV_SET(&kev, f, EVFILT_READ, EV_ADD, 0, 0, NULL);
	CHECK(kevent(kq, &kev, 1, NULL, 0, NULL) == 0, "EVFILT_READ: %s",
	    strerror(errno));
	EV_SET(&kev, f, EVFILT_WRITE, EV_ADD, 0, 0, NULL);
	CHECK(kevent(kq, &kev, 1, NULL, 0, NULL) == -1 && errno == EINVAL,
	    "EVFILT_WRITE: %s", strerror(errno));
	close(kq);
	/* Immediate mode: read(2) asks the device. */
	on = 1;
	CHECK(ioctl(f, HIDRAW_SET_IMMED, &on) == 0, "SET_IMMED: %s",
	    strerror(errno));
	n = read(f, c, sizeof(c));
	CHECK(n == isize, "immediate read: %d, %s", n, strerror(errno));
	on = 0;
	CHECK(ioctl(f, HIDRAW_SET_IMMED, &on) == 0, "SET_IMMED 0: %s",
	    strerror(errno));
	/* No output reports: writing is not supported. */
	c[0] = 0;
	c[1] = 0;
	CHECK(write(f, c, 2) == -1 && errno == EOPNOTSUPP, "write: %s",
	    strerror(errno));
	pointers++;
}

static void
check_keyboard(int f)
{
	uint8_t leds[2] = { 0, 0 };

	/* No report ID: a leading 0, then the LED byte. */
	CHECK(write(f, leds, sizeof(leds)) == 2, "LED report: %s",
	    strerror(errno));
	CHECK(write(f, leds, 1) == -1 && errno == EINVAL, "short write: %s",
	    strerror(errno));
	keyboards++;
}

static void
check_device(int unit)
{
	struct hidraw_report_descriptor *rd;
	struct hidraw_gen_descriptor hgd;
	struct hidraw_device_info hdi;
	struct hidraw_devinfo info;
	char path[32], name[128], phys[64], want[32];
	uint8_t uhid[4096];
	struct stat st;
	int f, g, size, n;

	snprintf(path, sizeof(path), "/dev/hidraw%d", unit);
	if (stat(path, &st) != 0)
		return;
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0600 &&
	    st.st_uid == 0 && st.st_gid == GID_OPERATOR,
	    "%s: mode %o uid %d gid %d", path, st.st_mode, st.st_uid,
	    st.st_gid);
	f = open(path, O_RDWR);
	CHECK(f >= 0, "%s: %s", path, strerror(errno));
	if (f < 0)
		return;
	g = open(path, O_RDWR);
	CHECK(g == -1 && errno == EBUSY, "second open: %s", strerror(errno));
	if (g >= 0)
		close(g);
	CHECK(ioctl(f, HIDIOCGRDESCSIZE, &size) == 0 && size > 0 &&
	    size < HID_MAX_DESCRIPTOR_SIZE, "HIDIOCGRDESCSIZE: %d", size);
	memset(&hgd, 0, sizeof(hgd));
	hgd.hgd_data = uhid;
	hgd.hgd_maxlen = sizeof(uhid);
	CHECK(ioctl(f, HIDRAW_GET_REPORT_DESC, &hgd) == 0,
	    "HIDRAW_GET_REPORT_DESC: %s", strerror(errno));
	/* Its length alone. */
	hgd.hgd_data = NULL;
	hgd.hgd_actlen = 0;
	CHECK(ioctl(f, HIDRAW_GET_REPORT_DESC, &hgd) == 0 &&
	    hgd.hgd_actlen == size, "length only: %u", hgd.hgd_actlen);
	/* The Linux request last: it leaves the open in hidraw mode. */
	rd = calloc(1, sizeof(*rd));
	rd->size = size;
	CHECK(ioctl(f, HIDIOCGRDESC, rd) == 0, "HIDIOCGRDESC: %s",
	    strerror(errno));
	CHECK(hgd.hgd_actlen == size && memcmp(uhid, rd->value, size) == 0,
	    "the two descriptors differ (%u, %d bytes)", hgd.hgd_actlen, size);
	CHECK(ioctl(f, HIDIOCGRAWINFO, &info) == 0 && info.bustype == 3,
	    "HIDIOCGRAWINFO: bus %u", info.bustype);
	memset(name, 0xff, sizeof(name));
	n = ioctl(f, HIDIOCGRAWNAME(sizeof(name)), name);
	CHECK(n > 1 && n == (int)strlen(name) + 1, "HIDIOCGRAWNAME: %d", n);
	n = ioctl(f, HIDIOCGRAWPHYS(sizeof(phys)), phys);
	snprintf(want, sizeof(want), "hidraw%d", unit);
	CHECK(n == (int)strlen(want) + 1 && strcmp(phys, want) == 0,
	    "HIDIOCGRAWPHYS: %d %s", n, phys);
	CHECK(ioctl(f, HIDRAW_GET_DEVICEINFO, &hdi) == 0, "%s",
	    strerror(errno));
	CHECK(hdi.hdi_vendor == (uint16_t)info.vendor &&
	    hdi.hdi_product == (uint16_t)info.product &&
	    hdi.hdi_bustype == 3 && strcmp(hdi.hdi_name, name) == 0 &&
	    strcmp(hdi.hdi_phys, want) == 0, "GET_DEVICEINFO: %s %s",
	    hdi.hdi_name, hdi.hdi_phys);
	n = 0;
	CHECK(ioctl(f, HIDRAW_GET_REPORT_ID, &n) == 0 && n == 0,
	    "GET_REPORT_ID %d", n);
	/* What hidraw does not know. */
	CHECK(ioctl(f, FIONREAD, &n) == -1 && errno == EINVAL,
	    "FIONREAD: %s", strerror(errno));
	if (is_desktop(rd->value, size, 0x02))
		check_pointer(f);
	else if (is_desktop(rd->value, size, 0x06))
		check_keyboard(f);
	free(rd);
	close(f);
}

int
main(void)
{
	int fileid, loaded = 0, unit;

	fileid = kldload("hidraw");
	if (fileid >= 0)
		loaded = 1;
	else
		CHECK(errno == EEXIST, "kldload: %s", strerror(errno));
	if (!loaded)
		fileid = kldfind("hidraw.ko");
	CHECK(fileid > 0, "kldfind: %s", strerror(errno));
	CHECK(modfind("hidbus/hidraw") > 0, "modfind: %s", strerror(errno));
	for (unit = 0; unit < 16; unit++)
		check_device(unit);
	if (pointers == 0)
		printf("hidraw: no pointing device; input reports not checked\n");
	if (keyboards == 0)
		printf("hidraw: no keyboard; output reports not checked\n");
	if (loaded) {
		CHECK(kldunload(fileid) == 0, "kldunload: %s", strerror(errno));
		CHECK(access("/dev/hidraw0", F_OK) == -1 && errno == ENOENT,
		    "/dev/hidraw0 still there");
	}
	return (failed);
}
