/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * hcons: hcons(4) and hsctrl(4), FreeBSD's consumer-control and
 * system-control HID drivers, through their evdev nodes. With "inject",
 * the kernel test has attached a keyboard's media-key interface (report 1:
 * Consumer Control, two 16-bit slots indexing usages 0 to 0x3ff; report
 * 2: System Control's Power Down, Sleep, and Wake Up bits) with the
 * Orange Pi 6 Plus keyboard's identity, Microdia 0c45:0133, and sends
 * the reports listed below once both nodes are open. This finds the nodes
 * by EVIOCGPHYS and checks each one's name, uniq (none), and id, its event
 * types and keys (all 148 keys hcons_map has for usages up to 0x3ff;
 * hsctrl's three), the attach lines in the message buffer, the devices'
 * dev.hcons.N and dev.hsctrl.N sysctls and kern.evdev.input.M, and then
 * reads the events the reports become, as FreeBSD's hidmap pushes them.
 *
 * Without "inject" it skips, as it does on FreeBSD, which has no such
 * device in a virtual machine.
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/sysctl.h>

#include <dev/evdev/input.h>

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
		printf("hcons: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	NAME	"vendor 0x0c45 product 0x0133"

struct ev {
	uint16_t type;
	uint16_t code;
	int32_t value;
};

#define	SYN	{ EV_SYN, SYN_REPORT, 1 }

/*
 * Consumer Control's reports (slot 0, slot 1):
 *   Mute, -; Mute, Volume Up; Volume Up, -; -, -; Sleep, Sleep Mode;
 *   Brightness Up, Play/Pause; -, -.
 * Two usages of one key merge; a report's keys go out in code order.
 */
static const struct ev hcons_events[] = {
	{ EV_KEY, KEY_MUTE, 1 }, SYN,
	{ EV_KEY, KEY_VOLUMEUP, 1 }, SYN,
	{ EV_KEY, KEY_MUTE, 0 }, SYN,
	{ EV_KEY, KEY_VOLUMEUP, 0 }, SYN,
	{ EV_KEY, KEY_SLEEP, 1 }, SYN,
	{ EV_KEY, KEY_SLEEP, 0 }, { EV_KEY, KEY_PLAYPAUSE, 1 },
	    { EV_KEY, KEY_BRIGHTNESSUP, 1 }, SYN,
	{ EV_KEY, KEY_PLAYPAUSE, 0 }, { EV_KEY, KEY_BRIGHTNESSUP, 0 }, SYN,
};

/* System Control's: Power Down; none; Sleep and Wake Up; none. */
static const struct ev hsctrl_events[] = {
	{ EV_KEY, KEY_POWER, 1 }, SYN,
	{ EV_KEY, KEY_POWER, 0 }, SYN,
	{ EV_KEY, KEY_SLEEP, 1 }, { EV_KEY, KEY_WAKEUP, 1 }, SYN,
	{ EV_KEY, KEY_SLEEP, 0 }, { EV_KEY, KEY_WAKEUP, 0 }, SYN,
};

#define	NBITS(n)	(((n) + 7) / 8)

static int
bit(const unsigned char *b, int i)
{
	return ((b[i / 8] >> (i % 8)) & 1);
}

static int
count(const unsigned char *b, int n)
{
	int i, c;

	for (i = c = 0; i < n; i++)
		c += bit(b, i);
	return (c);
}

/* The evdev node whose phys starts with driver, open; its unit in *unit. */
static int
find(const char *driver, int *unit, int *event)
{
	char path[64], phys[64];
	int fd, i;

	for (i = 0; i < 64; i++) {
		snprintf(path, sizeof(path), "/dev/input/event%d", i);
		fd = open(path, O_RDONLY | O_NONBLOCK);
		if (fd < 0)
			continue;
		memset(phys, 0, sizeof(phys));
		if (ioctl(fd, EVIOCGPHYS(sizeof(phys) - 1), phys) >= 0 &&
		    strncmp(phys, driver, strlen(driver)) == 0 &&
		    sscanf(phys + strlen(driver), "%d", unit) == 1) {
			*event = i;
			return (fd);
		}
		close(fd);
	}
	return (-1);
}

static void
sysctl_str(const char *name, char *buf, size_t len)
{
	size_t n = len - 1;

	memset(buf, 0, len);
	CHECK(sysctlbyname(name, buf, &n, NULL, 0) == 0, "%s: %s", name,
	    strerror(errno));
}

/* Its identity, its bits, its device's sysctls and attach line. */
static void
check_node(int fd, const char *driver, int unit, int event,
    const char *suffix, const char *usage, int index)
{
	char want[128], got[256], name[96], *msgbuf;
	unsigned char types[NBITS(EV_CNT)], keys[NBITS(KEY_CNT)];
	struct input_id id;
	size_t len;
	int debug;

	snprintf(want, sizeof(want), "%s %s", NAME, suffix);
	memset(got, 0, sizeof(got));
	CHECK(ioctl(fd, EVIOCGNAME(sizeof(got) - 1), got) >= 0, "%s",
	    strerror(errno));
	CHECK(strcmp(got, want) == 0, "%s name \"%s\"", driver, got);
	/* No serial number: ENOENT, as evdev answers for an empty one. */
	CHECK(ioctl(fd, EVIOCGUNIQ(sizeof(got) - 1), got) == -1 &&
	    errno == ENOENT, "%s uniq: %s", driver, strerror(errno));
	CHECK(ioctl(fd, EVIOCGID, &id) == 0, "%s", strerror(errno));
	CHECK(id.bustype == BUS_USB && id.vendor == 0x0c45 &&
	    id.product == 0x0133 && id.version == 0x0200,
	    "%s id %04x %04x %04x %04x", driver, id.bustype, id.vendor,
	    id.product, id.version);
	memset(types, 0, sizeof(types));
	CHECK(ioctl(fd, EVIOCGBIT(0, sizeof(types)), types) >= 0, "%s",
	    strerror(errno));
	CHECK(count(types, EV_CNT) == 2 && bit(types, EV_SYN) &&
	    bit(types, EV_KEY), "%s types %02x", driver, types[0]);
	memset(keys, 0, sizeof(keys));
	CHECK(ioctl(fd, EVIOCGBIT(EV_KEY, sizeof(keys)), keys) >= 0, "%s",
	    strerror(errno));
	if (strcmp(driver, "hcons") == 0) {
		CHECK(count(keys, KEY_CNT) == 148, "hcons has %d keys",
		    count(keys, KEY_CNT));
		CHECK(bit(keys, KEY_MUTE) && bit(keys, KEY_VOLUMEUP) &&
		    bit(keys, KEY_VOLUMEDOWN) && bit(keys, KEY_BRIGHTNESSUP) &&
		    bit(keys, KEY_BRIGHTNESSDOWN) && bit(keys, KEY_PLAYPAUSE) &&
		    bit(keys, KEY_NEXTSONG) && bit(keys, KEY_CALC) &&
		    bit(keys, KEY_SLEEP) && bit(keys, KEY_POWER) &&
		    bit(keys, BTN_MISC) && bit(keys, KEY_SCALE) &&
		    !bit(keys, KEY_A), "hcons keys");
	} else {
		CHECK(count(keys, KEY_CNT) == 3 && bit(keys, KEY_POWER) &&
		    bit(keys, KEY_SLEEP) && bit(keys, KEY_WAKEUP),
		    "hsctrl has %d keys", count(keys, KEY_CNT));
	}

	snprintf(name, sizeof(name), "dev.%s.%d.%%desc", driver, unit);
	sysctl_str(name, got, sizeof(got));
	CHECK(strcmp(got, want) == 0, "%s: \"%s\"", name, got);
	snprintf(name, sizeof(name), "dev.%s.%d.%%driver", driver, unit);
	sysctl_str(name, got, sizeof(got));
	CHECK(strcmp(got, driver) == 0, "%s: \"%s\"", name, got);
	snprintf(name, sizeof(name), "dev.%s.%d.%%location", driver, unit);
	sysctl_str(name, got, sizeof(got));
	snprintf(want, sizeof(want), "index=%d", index);
	CHECK(strcmp(got, want) == 0, "%s: \"%s\"", name, got);
	snprintf(name, sizeof(name), "dev.%s.%d.%%pnpinfo", driver, unit);
	sysctl_str(name, got, sizeof(got));
	snprintf(want, sizeof(want),
	    "%s bus=0x03 vendor=0x0c45 product=0x0133 version=0x0200", usage);
	CHECK(strcmp(got, want) == 0, "%s: \"%s\"", name, got);
	snprintf(name, sizeof(name), "dev.%s.%d.%%parent", driver, unit);
	sysctl_str(name, got, sizeof(got));
	CHECK(strncmp(got, "hidbus", 6) == 0, "%s: \"%s\"", name, got);
	/* HID_DEBUG's knob, as GENERIC has it. */
	snprintf(name, sizeof(name), "dev.%s.%d.debug", driver, unit);
	len = sizeof(debug);
	CHECK(sysctlbyname(name, &debug, &len, NULL, 0) == 0 &&
	    len == sizeof(debug) && debug == 0, "%s: %s", name,
	    strerror(errno));
	snprintf(name, sizeof(name), "kern.evdev.input.%d.phys", event);
	sysctl_str(name, got, sizeof(got));
	snprintf(want, sizeof(want), "%s%d", driver, unit);
	CHECK(strcmp(got, want) == 0, "%s: \"%s\"", name, got);

	/* "hcons0: <... Consumer Control> on hidbusN" */
	snprintf(want, sizeof(want), "%s%d: <%s %s> on hidbus", driver, unit,
	    NAME, suffix);
	len = 0;
	CHECK(sysctlbyname("kern.msgbuf", NULL, &len, NULL, 0) == 0, "%s",
	    strerror(errno));
	len += 4096;
	msgbuf = calloc(1, len + 1);
	if (msgbuf != NULL &&
	    sysctlbyname("kern.msgbuf", msgbuf, &len, NULL, 0) == 0)
		CHECK(strstr(msgbuf, want) != NULL, "no \"%s\" in dmesg", want);
	free(msgbuf);
}

/* Reads n events from fd, as they come, and compares them. */
static void
read_events(int fd, const char *driver, const struct ev *want, int n)
{
	struct input_event ie[16];
	struct pollfd p;
	ssize_t r;
	int got = 0, i;

	while (got < n) {
		p.fd = fd;
		p.events = POLLIN;
		p.revents = 0;
		/* Generous: only a failure waits this long. */
		if (poll(&p, 1, 60000) != 1) {
			CHECK(0, "%s: %d of %d events came", driver, got, n);
			return;
		}
		r = read(fd, ie, sizeof(ie));
		if (r < 0 && errno == EAGAIN)
			continue;
		CHECK(r > 0 && r % sizeof(ie[0]) == 0, "%s: read %zd: %s",
		    driver, r, strerror(errno));
		if (r <= 0)
			return;
		for (i = 0; i < (int)(r / sizeof(ie[0])); i++, got++) {
			if (got >= n) {
				CHECK(0, "%s: extra event %u %u %d", driver,
				    ie[i].type, ie[i].code, ie[i].value);
				continue;
			}
			CHECK(ie[i].type == want[got].type &&
			    ie[i].code == want[got].code &&
			    ie[i].value == want[got].value,
			    "%s: event %d is %u %u %d, not %u %u %d", driver,
			    got, ie[i].type, ie[i].code, ie[i].value,
			    want[got].type, want[got].code, want[got].value);
		}
	}
}

int
main(int argc, char **argv)
{
	unsigned char keys[NBITS(KEY_CNT)];
	int c, s, cu, su, ce, se;

	if (argc < 2 || strcmp(argv[1], "inject") != 0) {
		printf("hcons: no media-key device to drive, skipped\n");
		return (0);
	}
	/* Kept open from here: the kernel test sends once both are. */
	c = find("hcons", &cu, &ce);
	s = find("hsctrl", &su, &se);
	CHECK(c >= 0, "no hcons evdev node");
	CHECK(s >= 0, "no hsctrl evdev node");
	if (c < 0 || s < 0)
		return (1);
	check_node(c, "hcons", cu, ce, "Consumer Control",
	    "page=0x000c usage=0x0001", 0);
	check_node(s, "hsctrl", su, se, "System Control",
	    "page=0x0001 usage=0x0080", 1);
	read_events(c, "hcons", hcons_events,
	    sizeof(hcons_events) / sizeof(hcons_events[0]));
	read_events(s, "hsctrl", hsctrl_events,
	    sizeof(hsctrl_events) / sizeof(hsctrl_events[0]));
	/* Every key is up again. */
	memset(keys, 0, sizeof(keys));
	CHECK(ioctl(c, EVIOCGKEY(sizeof(keys)), keys) >= 0 &&
	    count(keys, KEY_CNT) == 0, "hcons keys still down");
	memset(keys, 0, sizeof(keys));
	CHECK(ioctl(s, EVIOCGKEY(sizeof(keys)), keys) >= 0 &&
	    count(keys, KEY_CNT) == 0, "hsctrl keys still down");
	close(c);
	close(s);
	return (failed);
}
