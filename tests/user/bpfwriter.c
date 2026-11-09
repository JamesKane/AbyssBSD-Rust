/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * bpfwriter: checks that a /dev/bpf opened for writing only is a writer,
 * which the interface's taps pass over (bpfopen's bd_writer, as dhclient's
 * sending descriptor is): set to lo0 beside a descriptor opened for
 * reading too, it counts none of the loopback datagrams the reader sees,
 * until it sets a read filter of its own.
 *
 * Runs on FreeBSD too (as root, with lo0 configured as rc.d/netif does).
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/socket.h>

#include <net/bpf.h>
#include <net/if.h>
#include <netinet/in.h>

#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define CHECK(cond, ...)                                     \
	do {                                                 \
		if (!(cond)) {                               \
			printf("bpfwriter: FAIL: " __VA_ARGS__); \
			printf("\n");                        \
			failed = 1;                          \
		}                                            \
	} while (0)

static int
tap(int flags)
{
	struct ifreq ifr;
	int fd = open("/dev/bpf", flags);

	if (fd < 0) {
		perror("bpfwriter: open /dev/bpf");
		return (-1);
	}
	memset(&ifr, 0, sizeof(ifr));
	strlcpy(ifr.ifr_name, "lo0", sizeof(ifr.ifr_name));
	if (ioctl(fd, BIOCSETIF, &ifr) != 0) {
		perror("bpfwriter: BIOCSETIF lo0");
		close(fd);
		return (-1);
	}
	return (fd);
}

static unsigned
received(int fd)
{
	struct bpf_stat st;

	if (ioctl(fd, BIOCGSTATS, &st) != 0) {
		perror("bpfwriter: BIOCGSTATS");
		return (~0U);
	}
	return (st.bs_recv);
}

/* Datagrams from 127.0.0.1 to itself, on lo0. */
static void
traffic(int n)
{
	struct sockaddr_in a;
	int s = socket(AF_INET, SOCK_DGRAM, 0);

	memset(&a, 0, sizeof(a));
	a.sin_family = AF_INET;
	a.sin_len = sizeof(a);
	a.sin_port = htons(9);
	a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	for (int i = 0; i < n; i++)
		(void)sendto(s, "x", 1, 0, (struct sockaddr *)&a, sizeof(a));
	close(s);
	/* lo0 hands datagrams on through netisr: let them pass. */
	usleep(200000);
}

int
main(void)
{
	struct bpf_insn all = BPF_STMT(BPF_RET | BPF_K, (u_int)-1);
	struct bpf_insn none = BPF_STMT(BPF_RET | BPF_K, 0);
	struct bpf_program p;
	int w = tap(O_WRONLY), r = tap(O_RDWR);

	if (w < 0 || r < 0)
		return (1);
	traffic(10);
	CHECK(received(r) >= 10, "the reader saw %u of 10", received(r));
	CHECK(received(w) == 0, "the writer saw %u", received(w));

	/* A write filter leaves it a writer. */
	p.bf_len = 1;
	p.bf_insns = &none;
	CHECK(ioctl(w, BIOCSETWF, &p) == 0, "BIOCSETWF");
	traffic(5);
	CHECK(received(w) == 0, "after BIOCSETWF the writer saw %u", received(w));

	/* A read filter other than pcap's lone snapshot makes it read. */
	p.bf_insns = &all;
	CHECK(ioctl(w, BIOCSETF, &p) == 0, "BIOCSETF");
	p.bf_insns = &none;
	CHECK(ioctl(w, BIOCSETF, &p) == 0, "BIOCSETF again");
	p.bf_insns = &all;
	CHECK(ioctl(w, BIOCSETF, &p) == 0, "BIOCSETF a third time");
	traffic(5);
	CHECK(received(w) >= 5, "once reading, it saw %u of 5", received(w));

	close(w);
	close(r);
	if (failed)
		return (1);
	printf("bpfwriter: ok\n");
	return (0);
}
