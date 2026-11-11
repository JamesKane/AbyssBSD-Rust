/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * sysinfo: what system-information tools (fastfetch, dmidecode, iostat,
 * gstat) read of the kernel and the loader:
 *
 * - hw.model names the CPU;
 * - the loader's hint.smbios.0.mem is an SMBIOS entry point that
 *   /dev/mem maps read-only, its structure table maps too, and the
 *   table's system information (type 1) names the product the loader put
 *   in smbios.system.product;
 * - kern.devstat: version 6, the device list as kern.devstat.all returns
 *   it (the generation, then a struct devstat per device), and reading a
 *   disk counts there;
 * - /dev/devstat: DIOCGMEDIASIZE whole pages, which map read-only and
 *   hold GEOM's provider entries (unit -1, an id);
 * - the routing table's NET_RT_FLAGS dump with RTF_GATEWAY, as fastfetch
 *   finds the default route's interface: a default route through
 *   127.0.0.1 (added for the test, unless there is one) comes back with
 *   its interface's sockaddr_dl (RTA_IFP) and source address (RTA_IFA).
 *
 * Runs on FreeBSD too (as root, with lo0 configured as rc.d/netif does).
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/socket.h>
#include <sys/devicestat.h>
#include <sys/disk.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/sysctl.h>

#include <net/if.h>
#include <net/if_dl.h>
#include <net/route.h>
#include <netinet/in.h>
#include <arpa/inet.h>

#include <errno.h>
#include <fcntl.h>
#include <kenv.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define	FAIL(...) do { printf("sysinfo: " __VA_ARGS__); printf("\n"); return (1); } while (0)

/* Maps len bytes of physical memory at pa (any alignment) from fd. */
static const uint8_t *
map_phys(int fd, uint64_t pa, size_t len, void **base, size_t *maplen)
{
	uint64_t page = pa & ~(uint64_t)(getpagesize() - 1);
	void *p;

	*maplen = (size_t)(pa - page) + len;
	p = mmap(NULL, *maplen, PROT_READ, MAP_SHARED, fd, (off_t)page);
	if (p == MAP_FAILED)
		return (NULL);
	*base = p;
	return ((const uint8_t *)p + (pa - page));
}

static int
smbios(void)
{
	char buf[128], product[128];
	const uint8_t *ep, *t, *s, *end;
	void *ebase, *tbase;
	size_t elen, tlen;
	uint64_t pa, table;
	uint32_t len;
	int fd, found = 0;

	if (kenv(KENV_GET, "hint.smbios.0.mem", buf, sizeof(buf)) < 0)
		FAIL("no hint.smbios.0.mem: errno %d", errno);
	pa = strtoull(buf, NULL, 16);
	if (kenv(KENV_GET, "smbios.system.product", product,
	    sizeof(product)) < 0)
		FAIL("no smbios.system.product: errno %d", errno);
	fd = open("/dev/mem", O_RDONLY);
	if (fd < 0)
		FAIL("open /dev/mem: errno %d", errno);
	ep = map_phys(fd, pa, 0x20, &ebase, &elen);
	if (ep == NULL)
		FAIL("mmap of the entry point at %#jx: errno %d",
		    (uintmax_t)pa, errno);
	if (memcmp(ep, "_SM3_", 5) == 0) {
		memcpy(&len, ep + 0x0c, 4);
		memcpy(&table, ep + 0x10, 8);
	} else if (memcmp(ep, "_SM_", 4) == 0) {
		uint16_t l16;
		uint32_t a32;

		memcpy(&l16, ep + 0x16, 2);
		memcpy(&a32, ep + 0x18, 4);
		len = l16;
		table = a32;
	} else
		FAIL("no SMBIOS anchor at %#jx", (uintmax_t)pa);
	munmap(ebase, elen);
	t = map_phys(fd, table, len, &tbase, &tlen);
	if (t == NULL)
		FAIL("mmap of the table at %#jx: errno %d", (uintmax_t)table,
		    errno);
	/* Walk to the system information; its product is string 0x05. */
	end = t + len;
	for (s = t; s + 4 <= end && s[0] != 127; ) {
		const uint8_t *str = s + s[1];

		if (s[0] == 1 && s[1] > 5 && s[5] != 0) {
			const uint8_t *p = str;
			int i;

			for (i = 1; i < s[5] && p < end; i++)
				p += strnlen((const char *)p, end - p) + 1;
			if (p < end && strcmp((const char *)p, product) == 0)
				found = 1;
			else
				FAIL("type 1's product is \"%.64s\", the "
				    "loader's \"%s\"", p, product);
		}
		while (str + 1 < end && (str[0] != 0 || str[1] != 0))
			str++;
		s = str + 2;
	}
	munmap(tbase, tlen);
	close(fd);
	if (!found)
		FAIL("no system information in the table");
	return (0);
}

/* The device list: kern.devstat.all into *list, *n entries. */
static int
devlist(struct devstat **list, int *n)
{
	size_t sz = 0;
	long gen;
	char *b;

	if (sysctlbyname("kern.devstat.all", NULL, &sz, NULL, 0) != 0)
		FAIL("kern.devstat.all: errno %d", errno);
	b = malloc(sz);
	if (b == NULL || sysctlbyname("kern.devstat.all", b, &sz, NULL, 0) != 0)
		FAIL("kern.devstat.all read: errno %d", errno);
	if ((sz - sizeof(gen)) % sizeof(struct devstat) != 0)
		FAIL("kern.devstat.all: %zu bytes", sz);
	*n = (int)((sz - sizeof(gen)) / sizeof(struct devstat));
	*list = (struct devstat *)(void *)(b + sizeof(gen));
	return (0);
}

static int
devstats(void)
{
	struct devstat *list, *after;
	char path[64], buf[65536];
	off_t size;
	size_t sz;
	void *p;
	uint64_t reads;
	int version, numdevs, n, i, fd, providers = 0;

	sz = sizeof(version);
	if (sysctlbyname("kern.devstat.version", &version, &sz, NULL, 0) != 0 ||
	    version != DEVSTAT_VERSION)
		FAIL("kern.devstat.version %d, errno %d", version, errno);
	sz = sizeof(numdevs);
	if (sysctlbyname("kern.devstat.numdevs", &numdevs, &sz, NULL, 0) != 0)
		FAIL("kern.devstat.numdevs: errno %d", errno);
	if (devlist(&list, &n) != 0)
		return (1);
	if (n != numdevs || n == 0)
		FAIL("%d entries in kern.devstat.all, numdevs %d", n, numdevs);
	for (i = 0; i < n; i++)
		if (list[i].unit_number < 0 || list[i].device_name[0] == 0)
			FAIL("entry %d: unit %d", i, list[i].unit_number);

	/* Reading the first disk counts there. */
	snprintf(path, sizeof(path), "/dev/%s%d", list[0].device_name,
	    list[0].unit_number);
	reads = list[0].operations[DEVSTAT_READ];
	fd = open(path, O_RDONLY);
	if (fd < 0)
		FAIL("open %s: errno %d", path, errno);
	if (pread(fd, buf, sizeof(buf), 0) != (ssize_t)sizeof(buf))
		FAIL("read %s: errno %d", path, errno);
	close(fd);
	if (devlist(&after, &n) != 0)
		return (1);
	if (after[0].operations[DEVSTAT_READ] <= reads ||
	    after[0].bytes[DEVSTAT_READ] < sizeof(buf))
		FAIL("%s: reads %ju -> %ju, %ju bytes", path, (uintmax_t)reads,
		    (uintmax_t)after[0].operations[DEVSTAT_READ],
		    (uintmax_t)after[0].bytes[DEVSTAT_READ]);

	/* The pages, as libgeom's geom_stats_open maps them. */
	fd = open("/dev/" DEVSTAT_DEVICE_NAME, O_RDONLY);
	if (fd < 0)
		FAIL("open /dev/devstat: errno %d", errno);
	if (ioctl(fd, DIOCGMEDIASIZE, &size) != 0 || size <= 0 ||
	    size % getpagesize() != 0)
		FAIL("DIOCGMEDIASIZE: %jd, errno %d", (intmax_t)size, errno);
	p = mmap(NULL, size, PROT_READ, MAP_SHARED, fd, 0);
	if (p == MAP_FAILED)
		FAIL("mmap /dev/devstat: errno %d", errno);
	for (i = 0; i < size / getpagesize(); i++) {
		struct devstat *d = (struct devstat *)(void *)
		    ((char *)p + i * getpagesize());
		int j;

		for (j = 0; j < getpagesize() / (int)sizeof(*d); j++)
			if (d[j].allocated && d[j].unit_number == -1 &&
			    d[j].id != NULL)
				providers++;
	}
	munmap(p, size);
	close(fd);
	if (providers == 0)
		FAIL("no GEOM provider entries in /dev/devstat");
	return (0);
}

/* RTM_ADD or RTM_DELETE of 0.0.0.0/0 through 127.0.0.1. */
static int
default_route(int type)
{
	struct {
		struct rt_msghdr h;
		struct sockaddr_in dst, gw, mask;
	} m;
	int s, r;

	memset(&m, 0, sizeof(m));
	m.h.rtm_msglen = sizeof(m);
	m.h.rtm_version = RTM_VERSION;
	m.h.rtm_type = type;
	m.h.rtm_flags = RTF_UP | RTF_GATEWAY | RTF_STATIC;
	m.h.rtm_addrs = RTA_DST | RTA_GATEWAY | RTA_NETMASK;
	m.h.rtm_seq = 1;
	m.dst.sin_len = m.gw.sin_len = m.mask.sin_len = sizeof(m.dst);
	m.dst.sin_family = m.gw.sin_family = m.mask.sin_family = AF_INET;
	m.gw.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	s = socket(PF_ROUTE, SOCK_RAW, AF_INET);
	if (s < 0)
		return (errno);
	r = write(s, &m, sizeof(m)) == (ssize_t)sizeof(m) ? 0 : errno;
	close(s);
	return (r);
}

#define	SA_ROUND(n) ((n) > 0 ? (1 + (((n) - 1) | (sizeof(long) - 1))) : sizeof(long))

static int
routes(void)
{
	int mib[6] = { CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_FLAGS, RTF_GATEWAY };
	char *b, *at, ifname[IF_NAMESIZE + 1] = "";
	struct in_addr src = { 0 };
	size_t sz = 0;
	int added, found = 0;

	added = default_route(RTM_ADD);
	if (added != 0 && added != EEXIST)
		FAIL("adding a default route: errno %d", added);
	if (sysctl(mib, 6, NULL, &sz, NULL, 0) != 0 || sz == 0)
		FAIL("NET_RT_FLAGS RTF_GATEWAY: errno %d, %zu bytes", errno, sz);
	b = malloc(sz);
	if (b == NULL || sysctl(mib, 6, b, &sz, NULL, 0) != 0)
		FAIL("NET_RT_FLAGS read: errno %d", errno);
	for (at = b; at < b + sz && !found;
	    at += ((struct rt_msghdr *)(void *)at)->rtm_msglen) {
		struct rt_msghdr *h = (struct rt_msghdr *)(void *)at;
		struct sockaddr *sa = (struct sockaddr *)(h + 1);
		struct sockaddr_in *dst = (struct sockaddr_in *)(void *)sa;
		int i;

		if (h->rtm_msglen == 0)
			FAIL("a record of length 0");
		if (!(h->rtm_flags & RTF_GATEWAY))
			FAIL("a route without RTF_GATEWAY: %#x", h->rtm_flags);
		if (sa->sa_family != AF_INET || dst->sin_addr.s_addr != 0)
			continue;
		for (i = 0; i < RTAX_MAX; i++) {
			if (!(h->rtm_addrs & (1 << i)))
				continue;
			if (i == RTAX_IFP && sa->sa_family == AF_LINK) {
				struct sockaddr_dl *sdl = (struct sockaddr_dl *)
				    (void *)sa;

				if (sdl->sdl_nlen <= IF_NAMESIZE) {
					memcpy(ifname, sdl->sdl_data, sdl->sdl_nlen);
					ifname[sdl->sdl_nlen] = '\0';
				}
			}
			if (i == RTAX_IFA && sa->sa_family == AF_INET)
				src = ((struct sockaddr_in *)(void *)sa)->sin_addr;
			sa = (struct sockaddr *)(void *)
			    ((char *)sa + SA_ROUND(sa->sa_len));
		}
		found = 1;
	}
	if (added == 0)
		(void)default_route(RTM_DELETE);
	if (!found || ifname[0] == '\0' || src.s_addr == 0)
		FAIL("default route: found %d, interface \"%s\", source %s",
		    found, ifname, inet_ntoa(src));
	if (added == 0 && (strcmp(ifname, "lo0") != 0 ||
	    src.s_addr != htonl(INADDR_LOOPBACK)))
		FAIL("default route through lo0: interface %s, source %s",
		    ifname, inet_ntoa(src));
	free(b);
	return (0);
}

int
main(void)
{
	char model[128];
	size_t sz = sizeof(model);

	if (sysctlbyname("hw.model", model, &sz, NULL, 0) != 0 || sz < 2)
		FAIL("hw.model: errno %d, %zu bytes", errno, sz);
	if (smbios() != 0)
		return (1);
	if (devstats() != 0)
		return (1);
	if (routes() != 0)
		return (1);
	return (0);
}
