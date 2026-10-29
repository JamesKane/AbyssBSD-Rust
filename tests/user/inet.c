/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * inet: checks internet sockets over loopback from user mode: UDP on
 * 127.0.0.1 and ::1 (bind, ephemeral ports, EADDRINUSE and SO_REUSEPORT,
 * a connected socket's ECONNREFUSED, a dual-stack socket's mapped names),
 * TCP (ECONNREFUSED to a closed port, connect and accept, getsockname
 * and getpeername, a large transfer between processes, half-close,
 * EPIPE after the peer's reset, a non-blocking connect with poll and
 * SO_ERROR, TIME_WAIT holding a port, out-of-band data with SIOCATMARK),
 * getaddrinfo's passive wildcard for syslogd's "syslog" service, and lo0
 * as getifaddrs, if_nametoindex, SIOCGIFFLAGS, netlink, the
 * net.inet.*.pcblist sysctls, and kern.features show it.
 *
 * Runs on FreeBSD too (as root, with lo0 configured as rc.d/netif does).
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/sockio.h>
#include <sys/sysctl.h>
#include <sys/wait.h>

#include <net/if.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <netlink/netlink.h>
#include <netlink/netlink_route.h>
#include <arpa/inet.h>

#include <errno.h>
#include <fcntl.h>
#include <ifaddrs.h>
#include <netdb.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("inet: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

#define	FAILS(r, want) do {						\
	errno = 0;							\
	long r_ = (long)(r);						\
	int e_ = errno;							\
	CHECK(r_ == -1 && e_ == (want), "got %ld (%s), want %s", r_,	\
	    strerror(e_), strerror(want));				\
} while (0)

static struct sockaddr_in
sin4(in_port_t port)
{
	struct sockaddr_in s;

	memset(&s, 0, sizeof(s));
	s.sin_len = sizeof(s);
	s.sin_family = AF_INET;
	s.sin_port = htons(port);
	s.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	return (s);
}

static struct sockaddr_in6
sin6(in_port_t port)
{
	struct sockaddr_in6 s;

	memset(&s, 0, sizeof(s));
	s.sin6_len = sizeof(s);
	s.sin6_family = AF_INET6;
	s.sin6_port = htons(port);
	s.sin6_addr = in6addr_loopback;
	return (s);
}

static in_port_t
port_of(int fd)
{
	struct sockaddr_storage ss;
	socklen_t len = sizeof(ss);

	if (getsockname(fd, (struct sockaddr *)&ss, &len) != 0)
		return (0);
	if (ss.ss_family == AF_INET)
		return (ntohs(((struct sockaddr_in *)&ss)->sin_port));
	return (ntohs(((struct sockaddr_in6 *)&ss)->sin6_port));
}

static int
exited_ok(pid_t pid)
{
	int status;

	return (waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0);
}

static void
udp(void)
{
	int a, b, c, one = 1;
	struct sockaddr_in to, from;
	socklen_t len;
	char buf[64];

	a = socket(AF_INET, SOCK_DGRAM, 0);
	b = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
	CHECK(a >= 0 && b >= 0, "socket: %s", strerror(errno));
	to = sin4(0);
	CHECK(bind(b, (struct sockaddr *)&to, sizeof(to)) == 0, "bind: %s",
	    strerror(errno));
	in_port_t port = port_of(b);
	CHECK(port >= 10000, "ephemeral port %u", port);
	to = sin4(port);
	CHECK(sendto(a, "ping", 4, 0, (struct sockaddr *)&to, sizeof(to)) == 4,
	    "sendto: %s", strerror(errno));
	len = sizeof(from);
	CHECK(recvfrom(b, buf, sizeof(buf), 0, (struct sockaddr *)&from,
	    &len) == 4 && memcmp(buf, "ping", 4) == 0, "recvfrom");
	CHECK(len == sizeof(from) && from.sin_family == AF_INET &&
	    from.sin_addr.s_addr == htonl(INADDR_LOOPBACK) &&
	    ntohs(from.sin_port) == port_of(a), "from %u", ntohs(from.sin_port));
	/* The port is taken; with SO_REUSEPORT on both, shared. */
	c = socket(AF_INET, SOCK_DGRAM, 0);
	FAILS(bind(c, (struct sockaddr *)&to, sizeof(to)), EADDRINUSE);
	close(c);
	/* A connected socket learns of an ICMP port unreachable. */
	close(b);
	CHECK(connect(a, (struct sockaddr *)&to, sizeof(to)) == 0,
	    "connect: %s", strerror(errno));
	CHECK(send(a, "x", 1, 0) == 1, "send: %s", strerror(errno));
	usleep(100000);
	FAILS(recv(a, buf, sizeof(buf), MSG_DONTWAIT), ECONNREFUSED);
	FAILS(sendto(a, "x", 1, 0, (struct sockaddr *)&to, sizeof(to)), EISCONN);
	close(a);

	a = socket(AF_INET, SOCK_DGRAM, 0);
	b = socket(AF_INET, SOCK_DGRAM, 0);
	setsockopt(a, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one));
	setsockopt(b, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one));
	to = sin4(0);
	CHECK(bind(a, (struct sockaddr *)&to, sizeof(to)) == 0, "bind a");
	to = sin4(port_of(a));
	CHECK(bind(b, (struct sockaddr *)&to, sizeof(to)) == 0,
	    "SO_REUSEPORT bind: %s", strerror(errno));
	close(a);
	close(b);
}

static void
udp6(void)
{
	int six, four, dual, zero = 0;
	struct sockaddr_in6 s6, from6;
	struct sockaddr_in s4;
	socklen_t len;
	int v6only = -1;
	char buf[16];

	six = socket(AF_INET6, SOCK_DGRAM, 0);
	CHECK(six >= 0, "socket(AF_INET6): %s", strerror(errno));
	len = sizeof(v6only);
	CHECK(getsockopt(six, IPPROTO_IPV6, IPV6_V6ONLY, &v6only, &len) == 0 &&
	    v6only == 1, "IPV6_V6ONLY default %d", v6only);
	s6 = sin6(0);
	CHECK(bind(six, (struct sockaddr *)&s6, sizeof(s6)) == 0, "bind ::1: %s",
	    strerror(errno));
	s6.sin6_port = htons(port_of(six));
	four = socket(AF_INET6, SOCK_DGRAM, 0);
	CHECK(sendto(four, "six", 3, 0, (struct sockaddr *)&s6, sizeof(s6)) == 3,
	    "sendto ::1: %s", strerror(errno));
	len = sizeof(from6);
	CHECK(recvfrom(six, buf, sizeof(buf), 0, (struct sockaddr *)&from6,
	    &len) == 3 && IN6_IS_ADDR_LOOPBACK(&from6.sin6_addr), "recv ::1");
	close(four);

	/* Without IPV6_V6ONLY, IPv4 comes from a mapped address. */
	dual = socket(AF_INET6, SOCK_DGRAM, 0);
	CHECK(setsockopt(dual, IPPROTO_IPV6, IPV6_V6ONLY, &zero,
	    sizeof(zero)) == 0, "IPV6_V6ONLY 0");
	memset(&s6, 0, sizeof(s6));
	s6.sin6_len = sizeof(s6);
	s6.sin6_family = AF_INET6;
	CHECK(bind(dual, (struct sockaddr *)&s6, sizeof(s6)) == 0, "bind ::");
	four = socket(AF_INET, SOCK_DGRAM, 0);
	s4 = sin4(port_of(dual));
	CHECK(sendto(four, "m", 1, 0, (struct sockaddr *)&s4, sizeof(s4)) == 1,
	    "sendto mapped: %s", strerror(errno));
	len = sizeof(from6);
	CHECK(recvfrom(dual, buf, sizeof(buf), 0, (struct sockaddr *)&from6,
	    &len) == 1 && IN6_IS_ADDR_V4MAPPED(&from6.sin6_addr), "recv mapped");
	/* An IPv4 socket cannot bind an IPv6 address. */
	FAILS(bind(four, (struct sockaddr *)&s6, sizeof(s6)), EAFNOSUPPORT);
	close(four);
	close(dual);
	close(six);
}

static void
tcp_basics(void)
{
	int l, c, s, one = 1, n;
	struct sockaddr_in a, peer;
	socklen_t len;
	char buf[64];

	/* Nothing listens: refused. */
	c = socket(AF_INET, SOCK_STREAM, 0);
	l = socket(AF_INET, SOCK_STREAM, 0);
	a = sin4(0);
	CHECK(bind(l, (struct sockaddr *)&a, sizeof(a)) == 0, "bind");
	a = sin4(port_of(l));
	close(l);
	FAILS(connect(c, (struct sockaddr *)&a, sizeof(a)), ECONNREFUSED);
	close(c);

	l = socket(AF_INET, SOCK_STREAM, 0);
	setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
	CHECK(bind(l, (struct sockaddr *)&a, sizeof(a)) == 0, "bind listener: %s",
	    strerror(errno));
	CHECK(listen(l, 5) == 0, "listen: %s", strerror(errno));
	len = sizeof(n);
	CHECK(getsockopt(l, SOL_SOCKET, SO_ACCEPTCONN, &n, &len) == 0 && n != 0,
	    "SO_ACCEPTCONN");
	c = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(connect(c, (struct sockaddr *)&a, sizeof(a)) == 0, "connect: %s",
	    strerror(errno));
	len = sizeof(peer);
	s = accept(l, (struct sockaddr *)&peer, &len);
	CHECK(s >= 0, "accept: %s", strerror(errno));
	CHECK(ntohs(peer.sin_port) == port_of(c) &&
	    peer.sin_addr.s_addr == htonl(INADDR_LOOPBACK), "accepted peer");
	CHECK(port_of(s) == port_of(l), "accepted socket's port");
	len = sizeof(peer);
	CHECK(getpeername(c, (struct sockaddr *)&peer, &len) == 0 &&
	    ntohs(peer.sin_port) == port_of(l), "getpeername");
	len = sizeof(n);
	CHECK(getsockopt(c, IPPROTO_TCP, TCP_NODELAY, &n, &len) == 0 && n == 0,
	    "TCP_NODELAY");
	CHECK(write(c, "hello", 5) == 5, "write");
	/* Loopback delivers in its own time on FreeBSD (netisr). */
	for (int tries = 0; tries < 100; tries++) {
		if (ioctl(s, FIONREAD, &n) == 0 && n == 5)
			break;
		usleep(10000);
	}
	CHECK(ioctl(s, FIONREAD, &n) == 0 && n == 5, "FIONREAD %d", n);
	CHECK(read(s, buf, sizeof(buf)) == 5 && memcmp(buf, "hello", 5) == 0,
	    "read");
	/* Half-close: the end comes after the data, the other way works. */
	CHECK(write(c, "bye", 3) == 3, "write");
	CHECK(shutdown(c, SHUT_WR) == 0, "shutdown");
	CHECK(read(s, buf, sizeof(buf)) == 3, "read before the end");
	CHECK(read(s, buf, sizeof(buf)) == 0, "the end");
	CHECK(write(s, "back", 4) == 4, "write back");
	CHECK(read(c, buf, sizeof(buf)) == 4, "read back");
	/*
	 * The client, which closed first, waits in TIME_WAIT (20 ms between
	 * local addresses, net.inet.tcp.msl_local); SO_REUSEADDR binds its
	 * port all the same.
	 */
	in_port_t cport = port_of(c);
	close(c);
	close(s);
	signal(SIGPIPE, SIG_IGN);
	c = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in again = sin4(cport);
	setsockopt(c, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
	CHECK(bind(c, (struct sockaddr *)&again, sizeof(again)) == 0,
	    "SO_REUSEADDR over TIME_WAIT: %s", strerror(errno));
	close(c);

	c = socket(AF_INET, SOCK_STREAM, 0);
	CHECK(connect(c, (struct sockaddr *)&a, sizeof(a)) == 0, "connect 2");
	s = accept(l, NULL, NULL);
	close(s);
	usleep(100000);
	CHECK(write(c, "a", 1) == 1, "first write after the peer closed");
	usleep(100000);
	FAILS(write(c, "b", 1), EPIPE);
	close(c);
	close(l);
}

static void
tcp_nonblock_and_oob(void)
{
	int l, c, s, err, one = 1, mark;
	struct sockaddr_in a;
	struct pollfd p;
	socklen_t len;
	char buf[8];

	l = socket(AF_INET, SOCK_STREAM, 0);
	a = sin4(0);
	bind(l, (struct sockaddr *)&a, sizeof(a));
	listen(l, -1);
	a = sin4(port_of(l));
	c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
	int r = connect(c, (struct sockaddr *)&a, sizeof(a));
	CHECK(r == 0 || errno == EINPROGRESS, "non-blocking connect: %s",
	    strerror(errno));
	p.fd = c;
	p.events = POLLOUT;
	CHECK(poll(&p, 1, 5000) == 1 && (p.revents & POLLOUT), "poll %x",
	    p.revents);
	len = sizeof(err);
	CHECK(getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len) == 0 && err == 0,
	    "SO_ERROR %d", err);
	FAILS(connect(c, (struct sockaddr *)&a, sizeof(a)), EISCONN);
	s = accept(l, NULL, NULL);
	CHECK(s >= 0, "accept");
	/* Urgent data: the byte is out of band, the mark before it. */
	CHECK(send(c, "ab", 2, 0) == 2, "send");
	CHECK(send(c, "!", 1, MSG_OOB) == 1, "send MSG_OOB: %s",
	    strerror(errno));
	usleep(100000);
	CHECK(ioctl(s, SIOCATMARK, &mark) == 0 && mark == 0, "not at mark");
	CHECK(recv(s, buf, sizeof(buf), 0) == 2, "data before the mark");
	CHECK(ioctl(s, SIOCATMARK, &mark) == 0 && mark == 1, "at mark");
	CHECK(recv(s, buf, 1, MSG_OOB) == 1 && buf[0] == '!', "MSG_OOB");
	close(s);
	close(c);
	/* Refused, non-blocking: the error is in SO_ERROR. */
	close(l);
	c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
	r = connect(c, (struct sockaddr *)&a, sizeof(a));
	if (r == -1 && errno == EINPROGRESS) {
		p.fd = c;
		p.events = POLLOUT;
		poll(&p, 1, 5000);
		len = sizeof(err);
		getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len);
		CHECK(err == ECONNREFUSED, "SO_ERROR %d", err);
	} else
		CHECK(r == -1 && errno == ECONNREFUSED, "refused: %s",
		    strerror(errno));
	close(c);
	(void)one;
}

/* A transfer bigger than both buffers, between processes, over ::1. */
static void
tcp_transfer(void)
{
	int l, c, s;
	struct sockaddr_in6 a;
	static char big[1 << 20];
	pid_t pid;

	l = socket(AF_INET6, SOCK_STREAM, 0);
	a = sin6(0);
	CHECK(bind(l, (struct sockaddr *)&a, sizeof(a)) == 0, "bind ::1");
	listen(l, 1);
	a.sin6_port = htons(port_of(l));
	pid = fork();
	if (pid == 0) {
		c = socket(AF_INET6, SOCK_STREAM, 0);
		if (connect(c, (struct sockaddr *)&a, sizeof(a)) != 0)
			_exit(1);
		for (size_t i = 0; i < sizeof(big); i++)
			big[i] = (char)(i * 7);
		size_t at = 0;
		while (at < sizeof(big)) {
			ssize_t n = write(c, big + at, sizeof(big) - at);
			if (n <= 0)
				_exit(2);
			at += n;
		}
		_exit(0);
	}
	s = accept(l, NULL, NULL);
	CHECK(s >= 0, "accept ::1");
	size_t got = 0;
	int bad = 0;
	char buf[8192];
	for (;;) {
		ssize_t n = read(s, buf, sizeof(buf));
		if (n <= 0)
			break;
		for (ssize_t i = 0; i < n; i++)
			if (buf[i] != (char)((got + i) * 7))
				bad = 1;
		got += n;
	}
	CHECK(got == sizeof(big) && !bad, "got %zu bytes, bad %d", got, bad);
	CHECK(exited_ok(pid), "writer");
	close(s);
	close(l);
}

/* syslogd's addsock(NULL, "syslog", 0): both wildcards, UDP port 514. */
static void
syslog_wildcard(void)
{
	struct addrinfo hints, *res, *r;
	int n4 = 0, n6 = 0, one = 1;

	memset(&hints, 0, sizeof(hints));
	hints.ai_family = AF_UNSPEC;
	hints.ai_socktype = SOCK_DGRAM;
	hints.ai_flags = AI_PASSIVE | AI_NUMERICSERV;
	int e = getaddrinfo(NULL, "514", &hints, &res);
	CHECK(e == 0, "getaddrinfo: %s", gai_strerror(e));
	if (e != 0)
		return;
	for (r = res; r != NULL; r = r->ai_next) {
		int s = socket(r->ai_family, r->ai_socktype, r->ai_protocol);
		CHECK(s >= 0, "socket(%d): %s", r->ai_family, strerror(errno));
		if (r->ai_family == AF_INET6) {
			n6++;
			CHECK(setsockopt(s, IPPROTO_IPV6, IPV6_V6ONLY, &one,
			    sizeof(one)) == 0, "IPV6_V6ONLY");
		} else
			n4++;
		CHECK(setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one,
		    sizeof(one)) == 0, "SO_REUSEADDR");
		CHECK(setsockopt(s, SOL_SOCKET, SO_REUSEPORT, &one,
		    sizeof(one)) == 0, "SO_REUSEPORT");
		CHECK(bind(s, r->ai_addr, r->ai_addrlen) == 0, "bind %d: %s",
		    r->ai_family, strerror(errno));
		/* syslogd -s: no input from the network. */
		int sr = shutdown(s, SHUT_RD);
		CHECK(sr == 0 || errno == ENOTCONN, "shutdown: %s",
		    strerror(errno));
		close(s);
	}
	CHECK(n4 == 1 && n6 == 1, "%d IPv4 and %d IPv6 wildcards", n4, n6);
	freeaddrinfo(res);
}

static void
lo0(void)
{
	struct ifaddrs *ifa, *i;
	int have4 = 0, have6 = 0, s, on;
	struct ifreq ifr;
	size_t len;

	unsigned idx = if_nametoindex("lo0");
	CHECK(idx >= 1, "if_nametoindex(lo0) %u", idx);
	CHECK(getifaddrs(&ifa) == 0, "getifaddrs: %s", strerror(errno));
	for (i = ifa; i != NULL; i = i->ifa_next) {
		if (strcmp(i->ifa_name, "lo0") != 0 || i->ifa_addr == NULL)
			continue;
		CHECK(i->ifa_flags & IFF_LOOPBACK && i->ifa_flags & IFF_UP,
		    "lo0 flags %x", i->ifa_flags);
		if (i->ifa_addr->sa_family == AF_INET &&
		    ((struct sockaddr_in *)i->ifa_addr)->sin_addr.s_addr ==
		    htonl(INADDR_LOOPBACK))
			have4 = 1;
		if (i->ifa_addr->sa_family == AF_INET6 && IN6_IS_ADDR_LOOPBACK(
		    &((struct sockaddr_in6 *)i->ifa_addr)->sin6_addr))
			have6 = 1;
	}
	freeifaddrs(ifa);
	CHECK(have4 && have6, "lo0 has 127.0.0.1 %d and ::1 %d", have4, have6);

	s = socket(AF_INET, SOCK_DGRAM, 0);
	memset(&ifr, 0, sizeof(ifr));
	strlcpy(ifr.ifr_name, "lo0", sizeof(ifr.ifr_name));
	CHECK(ioctl(s, SIOCGIFFLAGS, &ifr) == 0 &&
	    (ifr.ifr_flags & (IFF_UP | IFF_LOOPBACK)) == (IFF_UP | IFF_LOOPBACK),
	    "SIOCGIFFLAGS %x", ifr.ifr_flags);
	CHECK(ioctl(s, SIOCGIFMTU, &ifr) == 0 && ifr.ifr_mtu == 16384,
	    "SIOCGIFMTU %d", ifr.ifr_mtu);
	CHECK(ioctl(s, SIOCGIFADDR, &ifr) == 0 &&
	    ((struct sockaddr_in *)&ifr.ifr_addr)->sin_addr.s_addr ==
	    htonl(INADDR_LOOPBACK), "SIOCGIFADDR");
	strlcpy(ifr.ifr_name, "nonesuch0", sizeof(ifr.ifr_name));
	FAILS(ioctl(s, SIOCGIFFLAGS, &ifr), ENXIO);
	close(s);

	len = sizeof(on);
	CHECK(sysctlbyname("kern.features.inet", &on, &len, NULL, 0) == 0 &&
	    on == 1, "kern.features.inet");
	len = sizeof(on);
	CHECK(sysctlbyname("kern.features.inet6", &on, &len, NULL, 0) == 0 &&
	    on == 1, "kern.features.inet6");
	len = sizeof(on);
	CHECK(sysctlbyname("net.inet.ip.portrange.first", &on, &len, NULL,
	    0) == 0 && on == 10000, "portrange.first %d", on);
}

/* netstat's records: a connection shows in net.inet.tcp.pcblist. */
static void
pcblist(void)
{
	int l, c, s;
	struct sockaddr_in a;
	size_t len = 0;

	l = socket(AF_INET, SOCK_STREAM, 0);
	a = sin4(0);
	bind(l, (struct sockaddr *)&a, sizeof(a));
	listen(l, 1);
	a = sin4(port_of(l));
	c = socket(AF_INET, SOCK_STREAM, 0);
	connect(c, (struct sockaddr *)&a, sizeof(a));
	s = accept(l, NULL, NULL);
	CHECK(sysctlbyname("net.inet.tcp.pcblist", NULL, &len, NULL, 0) == 0,
	    "pcblist size: %s", strerror(errno));
	char *buf = malloc(len + 4096);
	len += 4096;
	CHECK(buf != NULL && sysctlbyname("net.inet.tcp.pcblist", buf, &len,
	    NULL, 0) == 0, "pcblist: %s", strerror(errno));
	if (buf != NULL && len > sizeof(uint64_t) * 2) {
		/* An xinpgen, then records each starting with its length. */
		uint64_t first = *(uint64_t *)buf;
		size_t at = first, recs = 0, est = 0;
		while (at + sizeof(uint64_t) <= len) {
			uint64_t rl = *(uint64_t *)(buf + at);
			if (rl <= first || at + rl > len)
				break;
			/* xt_inp's xi_socket's state, and t_state at the end. */
			recs++;
			at += rl;
		}
		CHECK(recs >= 3, "%zu TCP records", recs);
		(void)est;
	}
	free(buf);
	len = 0;
	CHECK(sysctlbyname("net.inet.udp.pcblist", NULL, &len, NULL, 0) == 0,
	    "udp pcblist: %s", strerror(errno));
	close(s);
	close(c);
	close(l);
}

/* ifconfig's first steps: a NETLINK_ROUTE socket, and lo0's link. */
static void
netlink(void)
{
	int s = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
	int one = 1;

	CHECK(s >= 0, "socket(AF_NETLINK): %s", strerror(errno));
	if (s < 0)
		return;
	CHECK(setsockopt(s, SOL_NETLINK, NETLINK_EXT_ACK, &one,
	    sizeof(one)) == 0, "NETLINK_EXT_ACK: %s", strerror(errno));
	struct {
		struct nlmsghdr hdr;
		struct ifinfomsg ifi;
	} req;
	memset(&req, 0, sizeof(req));
	req.hdr.nlmsg_len = sizeof(req);
	req.hdr.nlmsg_type = NL_RTM_GETLINK;
	req.hdr.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
	req.hdr.nlmsg_seq = 42;
	CHECK(send(s, &req, sizeof(req), 0) == sizeof(req), "send: %s",
	    strerror(errno));
	static char buf[65536];
	int found = 0, done = 0;
	for (int tries = 0; tries < 8 && !done; tries++) {
		ssize_t n = recv(s, buf, sizeof(buf), 0);
		CHECK(n > 0, "recv: %s", strerror(errno));
		if (n <= 0)
			break;
		size_t left = (size_t)n;
		for (struct nlmsghdr *h = (struct nlmsghdr *)buf;
		    NLMSG_OK(h, left); h = NLMSG_NEXT(h, left)) {
			if (h->nlmsg_type == NLMSG_DONE) {
				done = 1;
				break;
			}
			CHECK(h->nlmsg_seq == 42, "seq %u", h->nlmsg_seq);
			if (h->nlmsg_type != NL_RTM_NEWLINK)
				continue;
			struct ifinfomsg *ifi = NLMSG_DATA(h);
			int len = h->nlmsg_len - NLMSG_LENGTH(sizeof(*ifi));
			for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len);
			    a = RTA_NEXT(a, len))
				if (a->rta_type == IFLA_IFNAME &&
				    strcmp(RTA_DATA(a), "lo0") == 0)
					found = (ifi->ifi_flags & IFF_LOOPBACK) != 0;
		}
	}
	CHECK(done && found, "done %d, lo0 %d", done, found);
	close(s);
}

int
main(void)
{
	udp();
	udp6();
	tcp_basics();
	tcp_nonblock_and_oob();
	tcp_transfer();
	syslog_wildcard();
	lo0();
	pcblist();
	netlink();
	return (failed);
}
