/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * usbtest: the USB devices as FreeBSD's libusb sees them, through
 * /dev/usbctl and the ugen nodes: /dev/usbctl is root:operator 0644 and
 * lists every ugenB.D (USB_READ_DIR), each a link to its usb/B.D.0,
 * root:operator 0600; libusb finds the same devices and reads each one's
 * descriptors, strings, and port path; a GET_STATUS and a GET_DESCRIPTOR
 * go out as control transfers; a read-only descriptor may not send
 * requests, and what ugen does not know is ENOTTY. With a Bulk-Only disk
 * (QEMU's usb-storage), its kernel driver is detached, its interface
 * claimed, and an INQUIRY sent over its bulk endpoints (the FIFO
 * interface, completions waited for with poll); a read with nothing to
 * read times out or stalls (its halt then cleared) and the endpoint still
 * works after; then the device is enumerated again (libusb_reset_device)
 * and its driver comes back.
 *
 * With the argument "list", prints the names USB_READ_DIR gives, in its
 * order (usbconfig's), and does nothing else.
 *
 * Runs on FreeBSD too (as root). Exits 0 on success; prints what failed
 * otherwise.
 *
 * Links with: -lusb -lthr
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/stat.h>

#include <dev/usb/usb.h>
#include <dev/usb/usb_ioctl.h>
#include <dev/usb/usbdi.h>

#include <errno.h>
#include <fcntl.h>
#include <libusb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("usbtest: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	GID_OPERATOR 5
#define	MAXDEV 64

static char names[MAXDEV][32];
static int nnames;

/* USB_READ_DIR on /dev/usbctl, as libusb's ugen20 backend reads it. */
static void
read_dir(void)
{
	struct usb_read_dir urd;
	uint8_t buf[256];
	uint32_t start = 0;
	struct stat st;
	int f, more;

	CHECK(stat("/dev/usbctl", &st) == 0, "%s", strerror(errno));
	CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0644 &&
	    st.st_uid == 0 && st.st_gid == GID_OPERATOR,
	    "usbctl mode %o uid %d gid %d", st.st_mode, st.st_uid, st.st_gid);
	f = open("/dev/usbctl", O_RDONLY);
	CHECK(f >= 0, "open usbctl: %s", strerror(errno));
	if (f < 0)
		return;
	do {
		uint8_t *p;
		int parsed = 0;

		memset(buf, 0xff, sizeof(buf));
		urd.urd_data = buf;
		urd.urd_startentry = start;
		urd.urd_maxlen = sizeof(buf);
		if (ioctl(f, USB_READ_DIR, &urd) != 0) {
			CHECK(0, "USB_READ_DIR: %s", strerror(errno));
			break;
		}
		for (p = buf; p < buf + sizeof(buf) && p[0] != 0; p += p[0]) {
			const char *src = (const char *)p + 1;
			const char *dst = src + strlen(src) + 1;

			CHECK(strcmp(src, dst) == 0, "%s -> %s", src, dst);
			if (nnames < MAXDEV)
				strlcpy(names[nnames++], src, sizeof(names[0]));
			parsed++;
		}
		start += parsed;
		more = parsed != 0;
	} while (more);
	close(f);
}

/* Each listed name's link and node. */
static void
check_nodes(void)
{
	char path[64], target[64], want[64];
	unsigned bus, addr;
	struct stat st;
	ssize_t n;
	int i;

	for (i = 0; i < nnames; i++) {
		CHECK(sscanf(names[i], "ugen%u.%u", &bus, &addr) == 2, "%s",
		    names[i]);
		snprintf(path, sizeof(path), "/dev/%s", names[i]);
		n = readlink(path, target, sizeof(target) - 1);
		CHECK(n > 0, "readlink %s: %s", path, strerror(errno));
		if (n <= 0)
			continue;
		target[n] = 0;
		snprintf(want, sizeof(want), "usb/%u.%u.0", bus, addr);
		CHECK(strcmp(target, want) == 0, "%s -> %s", path, target);
		CHECK(stat(path, &st) == 0, "%s", strerror(errno));
		CHECK(S_ISCHR(st.st_mode) && (st.st_mode & 07777) == 0600 &&
		    st.st_uid == 0 && st.st_gid == GID_OPERATOR,
		    "%s: mode %o uid %d gid %d", path, st.st_mode, st.st_uid,
		    st.st_gid);
	}
}

/* The ugen requests libusb does not make, on device `name`. */
static void
check_ioctls(const char *name)
{
	struct usb_device_info di;
	struct usb_ctl_request req;
	struct usb_gen_descriptor gd;
	struct usb_fs_init fi;
	char path[64];
	uint8_t data[64];
	uint32_t plug, plug2;
	unsigned bus, addr;
	int f, g, v;

	sscanf(name, "ugen%u.%u", &bus, &addr);
	snprintf(path, sizeof(path), "/dev/%s", name);
	f = open(path, O_RDWR);
	CHECK(f >= 0, "%s: %s", path, strerror(errno));
	if (f < 0)
		return;
	CHECK(ioctl(f, USB_GET_DEVICEINFO, &di) == 0, "%s", strerror(errno));
	CHECK(di.udi_bus == bus && di.udi_index == addr, "%s: bus %u index %u",
	    name, di.udi_bus, di.udi_index);
	CHECK(di.udi_mode == USB_MODE_HOST, "mode %u", di.udi_mode);
	CHECK(ioctl(f, USB_GET_PLUGTIME, &plug) == 0, "%s", strerror(errno));
	CHECK(ioctl(f, USB_GET_PLUGTIME, &plug2) == 0 && plug == plug2,
	    "plugtime changed");
	CHECK(ioctl(f, USB_GET_CONFIG, &v) == 0 && v == 0, "config %d", v);
	/* No driver on an interface that does not exist. */
	memset(&gd, 0, sizeof(gd));
	gd.ugd_data = data;
	gd.ugd_maxlen = sizeof(data);
	gd.ugd_iface_index = 200;
	CHECK(ioctl(f, USB_GET_IFACE_DRIVER, &gd) == -1 && errno == EINVAL,
	    "USB_GET_IFACE_DRIVER 200: %s", strerror(errno));
	v = 200;
	CHECK(ioctl(f, USB_IFACE_DRIVER_ACTIVE, &v) == -1 && errno == ENXIO,
	    "USB_IFACE_DRIVER_ACTIVE 200: %s", strerror(errno));
	/* FIFO transfers want an endpoint array and a count. */
	memset(&fi, 0, sizeof(fi));
	fi.pEndpoints = NULL;
	fi.ep_index_max = 1;
	CHECK(ioctl(f, USB_FS_INIT, &fi) == -1 && errno == EINVAL,
	    "USB_FS_INIT NULL: %s", strerror(errno));
	/* A read-only open sends no requests. */
	g = open(path, O_RDONLY);
	CHECK(g >= 0, "%s read-only: %s", path, strerror(errno));
	if (g >= 0) {
		memset(&req, 0, sizeof(req));
		req.ucr_data = data;
		req.ucr_request.bmRequestType = UT_READ_DEVICE;
		req.ucr_request.bRequest = UR_GET_STATUS;
		USETW(req.ucr_request.wLength, 2);
		CHECK(ioctl(g, USB_DO_REQUEST, &req) == -1 && errno == EPERM,
		    "USB_DO_REQUEST read-only: %s", strerror(errno));
		close(g);
	}
	/* A data stage needs a buffer. */
	memset(&req, 0, sizeof(req));
	req.ucr_request.bmRequestType = UT_READ_DEVICE;
	req.ucr_request.bRequest = UR_GET_STATUS;
	USETW(req.ucr_request.wLength, 2);
	CHECK(ioctl(f, USB_DO_REQUEST, &req) == -1 && errno == EFAULT,
	    "USB_DO_REQUEST without data: %s", strerror(errno));
	/* The status itself. */
	req.ucr_data = data;
	req.ucr_flags = USB_SHORT_XFER_OK;
	CHECK(ioctl(f, USB_DO_REQUEST, &req) == 0 && req.ucr_actlen == 2,
	    "GET_STATUS: %s, %u bytes", strerror(errno), req.ucr_actlen);
	close(f);
}

/* Every device through libusb: descriptors, strings, a control transfer. */
static void
check_libusb(libusb_context *ctx)
{
	libusb_device **list;
	ssize_t n;
	int i;

	n = libusb_get_device_list(ctx, &list);
	CHECK(n == nnames, "libusb lists %zd devices, usbctl %d", n, nnames);
	for (i = 0; i < n; i++) {
		struct libusb_device_descriptor d;
		struct libusb_config_descriptor *c;
		libusb_device_handle *h;
		uint8_t ports[8], buf[64];
		unsigned char s[128];
		int r, root;

		root = libusb_get_device_address(list[i]) == 1;
		CHECK(libusb_get_device_descriptor(list[i], &d) == 0, "desc");
		CHECK(d.bLength == 18 && d.bDescriptorType == LIBUSB_DT_DEVICE,
		    "device descriptor %u/%u", d.bLength, d.bDescriptorType);
		CHECK(d.bNumConfigurations >= 1, "no configurations");
		/* A root hub has no path: libusb gives its parent's address
		 * and port instead, both 0. */
		r = libusb_get_port_numbers(list[i], ports, sizeof(ports));
		CHECK(root ? r == 2 && ports[0] == 0 && ports[1] == 0 : r >= 1,
		    "port path of %d", r);
		CHECK(libusb_get_device_speed(list[i]) != LIBUSB_SPEED_UNKNOWN,
		    "speed");
		r = libusb_open(list[i], &h);
		CHECK(r == 0, "libusb_open: %s", libusb_strerror(r));
		if (r != 0)
			continue;
		r = libusb_get_active_config_descriptor(list[i], &c);
		CHECK(r == 0, "config: %s", libusb_strerror(r));
		if (r == 0) {
			CHECK(c->bNumInterfaces >= 1, "no interfaces");
			libusb_free_config_descriptor(c);
		}
		if (d.iProduct != 0) {
			r = libusb_get_string_descriptor_ascii(h, d.iProduct,
			    s, sizeof(s));
			CHECK(r > 0, "product string: %s", libusb_strerror(r));
		}
		/* GET_STATUS: two bytes; a root hub is self-powered. */
		r = libusb_control_transfer(h, LIBUSB_ENDPOINT_IN, 0, 0, 0,
		    buf, 2, 1000);
		CHECK(r == 2, "GET_STATUS: %s", libusb_strerror(r));
		if (root && r == 2)
			CHECK(buf[0] & 1, "root hub status %02x", buf[0]);
		/* The device descriptor again, by hand. */
		r = libusb_control_transfer(h, LIBUSB_ENDPOINT_IN,
		    LIBUSB_REQUEST_GET_DESCRIPTOR, LIBUSB_DT_DEVICE << 8, 0,
		    buf, 18, 1000);
		CHECK(r == 18 && buf[1] == LIBUSB_DT_DEVICE &&
		    (buf[8] | buf[9] << 8) == d.idVendor &&
		    (buf[10] | buf[11] << 8) == d.idProduct,
		    "GET_DESCRIPTOR: %d", r);
		libusb_close(h);
	}
	libusb_free_device_list(list, 1);
}

/* An INQUIRY over a Bulk-Only disk's endpoints. */
static int
inquiry(libusb_device_handle *h, uint8_t out, uint8_t in, uint32_t tag)
{
	uint8_t cbw[31], data[36], csw[13];
	int r, n;

	memset(cbw, 0, sizeof(cbw));
	memcpy(cbw, "USBC", 4);
	memcpy(cbw + 4, &tag, 4);
	cbw[8] = sizeof(data);		/* dCBWDataTransferLength */
	cbw[12] = 0x80;			/* in */
	cbw[14] = 6;			/* CDB length */
	cbw[15] = 0x12;			/* INQUIRY */
	cbw[19] = sizeof(data);
	r = libusb_bulk_transfer(h, out, cbw, sizeof(cbw), &n, 2000);
	CHECK(r == 0 && n == sizeof(cbw), "CBW: %s, %d", libusb_strerror(r), n);
	r = libusb_bulk_transfer(h, in, data, sizeof(data), &n, 2000);
	CHECK(r == 0 && n == sizeof(data), "data: %s, %d", libusb_strerror(r),
	    n);
	if (r == 0)
		CHECK(memcmp(data + 8, "QEMU    ", 8) == 0, "vendor %.8s",
		    data + 8);
	r = libusb_bulk_transfer(h, in, csw, sizeof(csw), &n, 2000);
	CHECK(r == 0 && n == sizeof(csw), "CSW: %s, %d", libusb_strerror(r),
	    n);
	if (r == 0) {
		CHECK(memcmp(csw, "USBS", 4) == 0 && memcmp(csw + 4, &tag, 4) == 0,
		    "CSW signature or tag");
		CHECK(csw[12] == 0, "CSW status %u", csw[12]);
	}
	return (r);
}

/* The Bulk-Only disk, if there is one. */
static void
check_bulk(libusb_context *ctx)
{
	libusb_device **list;
	libusb_device *disk = NULL;
	libusb_device_handle *h;
	uint8_t in = 0, out = 0, buf[512];
	ssize_t n;
	int i, r, got, tries;

	n = libusb_get_device_list(ctx, &list);
	for (i = 0; i < n && disk == NULL; i++) {
		struct libusb_config_descriptor *c;
		const struct libusb_interface_descriptor *id;
		int e;

		if (libusb_get_active_config_descriptor(list[i], &c) != 0)
			continue;
		id = &c->interface[0].altsetting[0];
		if (id->bInterfaceClass == 8 && id->bInterfaceSubClass == 6 &&
		    id->bInterfaceProtocol == 0x50) {
			disk = libusb_ref_device(list[i]);
			for (e = 0; e < id->bNumEndpoints; e++) {
				uint8_t a = id->endpoint[e].bEndpointAddress;

				if ((id->endpoint[e].bmAttributes & 3) != 2)
					continue;
				if (a & 0x80)
					in = a;
				else
					out = a;
			}
		}
		libusb_free_config_descriptor(c);
	}
	libusb_free_device_list(list, 1);
	if (disk == NULL) {
		printf("usbtest: no Bulk-Only disk; bulk transfers not checked\n");
		return;
	}
	CHECK(in != 0 && out != 0, "bulk endpoints %02x %02x", in, out);
	r = libusb_open(disk, &h);
	CHECK(r == 0, "open disk: %s", libusb_strerror(r));
	libusb_unref_device(disk);
	if (r != 0)
		return;
	CHECK(libusb_kernel_driver_active(h, 0) == 1, "umass not attached");
	r = libusb_detach_kernel_driver(h, 0);
	CHECK(r == 0, "detach: %s", libusb_strerror(r));
	CHECK(libusb_kernel_driver_active(h, 0) == 0, "still attached");
	r = libusb_claim_interface(h, 0);
	CHECK(r == 0, "claim: %s", libusb_strerror(r));
	if (r == 0 && in != 0 && out != 0) {
		inquiry(h, out, in, 0x1234);
		/*
		 * Nothing asked: a read waits until it times out, or (QEMU's
		 * disk) the endpoint stalls, and its halt is cleared.
		 */
		r = libusb_bulk_transfer(h, in, buf, sizeof(buf), &got, 300);
		CHECK(r == LIBUSB_ERROR_TIMEOUT || r == LIBUSB_ERROR_PIPE,
		    "idle read: %s", libusb_strerror(r));
		if (r == LIBUSB_ERROR_PIPE) {
			r = libusb_clear_halt(h, in);
			CHECK(r == 0, "clear halt: %s", libusb_strerror(r));
		}
		/* And the endpoint goes on. */
		inquiry(h, out, in, 0x5678);
		libusb_release_interface(h, 0);
	}
	/* Enumerated again, the driver is back. */
	r = libusb_reset_device(h);
	CHECK(r == 0, "reset: %s", libusb_strerror(r));
	for (tries = 0; tries < 50; tries++) {
		if (libusb_kernel_driver_active(h, 0) == 1)
			break;
		usleep(100000);
	}
	CHECK(libusb_kernel_driver_active(h, 0) == 1,
	    "umass not back after reset");
	libusb_close(h);
}

int
main(int argc, char **argv)
{
	libusb_context *ctx;
	int i, r;

	read_dir();
	/* "usbtest list": the devices as USB_READ_DIR lists them. */
	if (argc > 1 && strcmp(argv[1], "list") == 0) {
		for (i = 0; i < nnames; i++)
			printf("%s\n", names[i]);
		return (failed);
	}
	if (nnames == 0) {
		/* A machine without USB (FreeBSD's usertests VM). */
		printf("usbtest: no USB devices; nothing checked\n");
		return (failed);
	}
	check_nodes();
	for (i = 0; i < nnames; i++)
		check_ioctls(names[i]);
	r = libusb_init(&ctx);
	CHECK(r == 0, "libusb_init: %s", libusb_strerror(r));
	if (r == 0) {
		check_libusb(ctx);
		check_bulk(ctx);
		libusb_exit(ctx);
	}
	return (failed);
}
