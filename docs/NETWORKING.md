# Networking

The design for Phase 7 ([ROADMAP.md](ROADMAP.md#phase-7-networking-xl)): what it takes for an
unmodified FreeBSD 15.1 system to configure itself with `dhclient` and SLAAC, accept `ssh` logins,
and run `fetch` and `pkg` over virtio-net, on both architectures, in QEMU and bhyve, before the
physical machines bring their own NICs in P8. [ADR-0019](adr/0019-networking.md) records the
decision.

Behavior reference: sys/kern (kern_mbuf.c, uipc_mbuf.c, uipc_mbuf2.c, uipc_sockbuf.c,
kern_sendfile.c, uipc_ktls.c, subr_epoch.c), sys/net (if.c, if_ethersubr.c, if_loop.c, if_epair.c,
if_clone.c, bpf.c, bpf_filter.c, netisr.c, vnet.c, rtsock.c, route/*.c), sys/netlink, sys/netinet
(in.c, in_pcb.c, ip_input.c, ip_output.c, ip_reass.c, ip_icmp.c, raw_ip.c, if_ether.c, igmp.c,
udp_usrreq.c, tcp_*.c, cc/cc_newreno.c, cc/cc_cubic.c), sys/netinet6 (ip6_*.c, icmp6.c, nd6*.c,
in6_ifattach.c, mld6.c, frag6.c), sys/dev/virtio/network/if_vtnet.c, sbin/dhclient,
usr.sbin/rtsold, sbin/ifconfig, sbin/route, sbin/ping. FreeBSD is the specification of what
programs and peers see, not the source of the code
([ADR-0007](adr/0007-license-and-clean-implementation.md)). The RFCs are cited where FreeBSD
follows them; where it departs from them, FreeBSD's behavior is the target.

## 1. Goals and non-goals

### 1.1 Goals

- **A networked base system, unmodified.** The P6 reference disk, with the `rc.conf` lines
  `bsdinstall` writes for a DHCP interface (`ifconfig_vtnet0="DHCP"`,
  `ifconfig_vtnet0_ipv6="inet6 accept_rtadv"`), boots, gets an IPv4 address from `dhclient` and an
  IPv6 one by SLAAC (`rtsold` soliciting), and resolves names. `sshd` accepts logins, `scp` copies
  both ways, and `fetch` and `pkg update && pkg install` work against the real package mirrors.
  `ifconfig`, `route`, `netstat`, `sockstat`, `ping`, `ping6`, `traceroute`, and `tcpdump` print
  what they print on FreeBSD.
- **FreeBSD on the wire.** A peer sees what FreeBSD 15.1 sends: the same TCP options negotiated,
  the same window and acknowledgement behavior, retransmission and congestion response, the same
  ARP, ND, and ICMP answers. This is checked by packet captures of scripted scenarios on both
  systems (§16.3), not by reading RFCs alone.
- **The P6 loopback slice keeps its semantics.** What `sys/inet` decided for sockets (bind and
  connect rules, lookups, states, options, errors; MULTIUSER.md §8.4) stays; its data path is
  replaced by packets.
- **Conformance.** kyua's `sys/net`, `sys/netinet`, `sys/netinet6`, `sys/netlink`, and
  `sys/kern/ktls_test` meet the P6 target (§16.2). Most of FreeBSD's network tests build their
  topologies from VNET jails and `epair` interfaces, so those are part of the goal (§10).
- **Performance parity** for the P7 components (§17), over loopback and virtio-net in bhyve, and
  over the physical NICs from P8.

### 1.2 Non-goals for Phase 7

- Packet filters and NAT (`pf`, `ipfw`, `ipf`, `divert`), IPsec, SCTP (`sctp_*` stay `ENOSYS`,
  [COMPATIBILITY.md §2.1](COMPATIBILITY.md#21-system-calls)), CARP, `pfsync`, and MAC.
- Pseudo-interfaces other than `lo`, `epair`, and those `rc` creates by default: `bridge`,
  `vlan`, `lagg`, `gif`, `gre`, `stf`, `tun`, `tap`, `wg`, `ovpn`, `geneve`. kyua's tests for them
  are left out until the phase that brings them, as `ktls_test` was in P6.
- TCP stacks other than the default (`net.inet.tcp.functions_default` is `freebsd`): RACK and BBR,
  TCP offload engines, TCP Fast Open (off by default in 15.1), and the `tcp_log` black box.
  Congestion control is NewReno and CUBIC (15.1's default; `net.inet.tcp.cc.available` lists
  only `cubic` on a stock 15.1 system).
- `netmap`, Wi-Fi (`net80211`), InfiniBand, `ng_*` (netgraph), and multipath routing beyond
  `net.fibs` tables.
- Hardware NIC drivers. The class and its first driver (virtio-net) are P7's; the reference
  machines' NICs arrive in P8 with the machines.

## 2. Principles

1. **The wire is the specification.** Every protocol milestone ends with differential captures:
   the same scenario run against AbyssBSD and FreeBSD, the captures normalized (addresses, ports,
   initial sequence numbers, timestamps) and compared segment by segment (§16.3).
2. **Per-VNET state from the first line.** Every piece of network state (interfaces, addresses,
   routes, neighbor caches, PCB tables, statistics, the `net.*` sysctls marked `VNET` in FreeBSD)
   lives in a `Vnet` that code reaches through an explicit context, never a global. VNET jails then
   cost a constructor and a teardown, not a retrofit (§10).
3. **Protocols are host-tested crates.** Parsers, the TCP state machine, congestion control, the
   reassembly queues, the BPF interpreter, and the ciphers have no kernel dependencies and run
   under `cargo test`, TCP against a deterministic simulated network (§16.1). The kernel supplies
   time, timers, and packet I/O through traits.
4. **Untrusted input never panics.** Every header from the wire, every filter program, every
   `sockaddr` and option is parsed into a typed value or rejected. Parsers are fuzzed on the host.
5. **One packet type, owned.** A packet is a `Packet` with FreeBSD's `m_pkthdr` fields and a
   chain of buffer segments (§4). It moves by value from driver to socket and back; a transmit
   packet is completed exactly once (DRIVER-MODEL.md §9.1). Data is copied once on each side
   (user to buffer, buffer to user) and never between layers.
6. **Read-mostly state is read without locks.** Interface lists, routes and next hops, and PCB
   lookups are read inside a network epoch and reclaimed after it, as FreeBSD's `net_epoch` (§6).
   Writers serialize on a lock per table.
7. **Run to completion.** A received packet is processed from the driver's queue to the socket in
   the receiving thread, as `net.isr.dispatch=direct`; `netisr` queues exist only where FreeBSD
   defers (loopback output, ARP and ND resolution queues).
8. **First-use telemetry continues.** An unimplemented socket option, ioctl, sysctl, routing
   message, or netlink message logs once per program (MULTIUSER.md §2), and the network boot cases
   fail on any such line.

## 3. Crates

| Crate | Holds | Notes |
|---|---|---|
| `sys/mbuf` (new) | `Packet`, buffer segments, cluster and page-backed buffers, the zones, checksum and TSO metadata, `m_*`-style operations (prepend, pullup, split, copy by reference) | Host-tested; replaces the accounting-only `socket::Mbuf` |
| `sys/epoch` (new) | A preemptible network epoch: enter and exit, deferred reclamation | Also the base a lock-free descriptor lookup can use later |
| `sys/net` (new) | `Vnet`, `ifnet` and the net driver class (`EtherQueues`), Ethernet, `lo`, `epair`, cloners, `bpf` and its filter interpreter, routes, next hops and FIBs, the link-layer table (`lltable`) and ARP, which fills it under the same lock, the routing socket, netlink | Takes over `sys/inet::ifnet` and `sys/inet::netlink` |
| `sys/inet` (grows) | PCBs (as today), IPv4, ICMP, IGMP, raw IP, UDP, IPv6, ICMPv6, ND and SLAAC, MLD, fragmentation and reassembly, forwarding | Its direct delivery between sockets goes |
| `sys/tcp` (new) | TCP: segments, the state machine, timers, SACK, reassembly, syncache and cookies, hostcache, congestion control (NewReno, CUBIC), ECN, LRO, TSO sizing | Host-tested against a simulated network; takes over `sys/inet::tcp`'s states and errors |
| `sys/crypto` (new) | AES (ECB core, CBC, GCM), ChaCha20-Poly1305, SHA-1, SHA-256, SHA-384, HMAC | Constant-time software; AES-NI, PCLMULQDQ, and the ARMv8 cryptographic extensions through `arch/*` |
| `sys/ktls` (new) | TLS 1.0–1.3 record framing, encryption on send, decryption on receive | Uses `sys/crypto` and `sys/mbuf` |
| `drivers/virtio-net` (new) | `vtnet`: multiple queue pairs, control queue, checksum offload, TSO and receive coalescing, mergeable receive buffers | On `sys/virtio` (PCI; split rings) |
| `kernel/src/net/` (grows) | System calls, ioctls, sysctls, `/dev/bpf`, `sendfile`, the glue between crates | |

`sys/socket` stays protocol-neutral; its socket buffers hold `sys/mbuf` chains, and UNIX-domain
sockets move to them in P7.1 (they already account as FreeBSD's do, so nothing user-visible
changes).

## 4. Packet buffers

FreeBSD's mbufs are the model for what a packet carries and costs; the representation is Rust's.

- **`Packet`.** The packet header (`m_pkthdr`): total length, receiving interface, flow ID and
  hash type, checksum flags (`CSUM_IP`, `CSUM_*_VALID`, `CSUM_DATA_VALID`, the transmit requests),
  TSO segment size, VLAN tag, the `M_BCAST`/`M_MCAST`/`M_EOR`-style flags, and the VNET. Then a
  chain of segments, each a reference to a buffer with an offset and length.
- **Buffers.** Inline small buffers (FreeBSD's `MLEN` and `MHLEN`), clusters of 2 KiB, 4 KiB
  (page), 9 KiB, and 16 KiB from per-CPU cached zones, and page-backed buffers (FreeBSD's
  `M_EXTPG`) referencing page-cache pages for `sendfile` and TLS. Clusters are reference-counted,
  so a TCP send buffer and the segment built from it share data, as `m_copym` shares clusters.
- **Headroom.** A packet built for output reserves `max_linkhdr + max_protohdr`, so headers are
  prepended without copying; `m_pullup`'s contract (the first segment holds at least *n*
  contiguous bytes) is a typed operation that returns the header view or fails.
- **Accounting.** Socket buffers charge `MSIZE` and the cluster size per segment against
  `sb_mbmax`, as the UNIX-domain sockets already do, so buffers fill when FreeBSD's would.
- **Drivers.** A transmit packet is mapped segment by segment through the driver's DMA tag
  (`sys/vm::dma`), the mapping released on completion. Receive buffers come from a per-queue pool
  of mapped clusters, refilled in batches, and become a `Packet` without a copy.
- **Zones and limits.** `kern.ipc.nmbclusters` and its siblings bound the zones;
  `netstat -m` reads their counts.

## 5. Interfaces and drivers

### 5.1 `ifnet`

An interface is an `Ifnet` in a VNET's list: name and unit, index, flags, capabilities enabled and
available (`IFCAP_*`), MTU, link-layer address, link state, media, groups, counters (`if_data`),
its addresses, multicast filter, and its driver. Drivers are reached through the net class trait;
`lo` and `epair` implement it in software. Link-state changes go to `devd` as `IFNET` devctl
events and to routing-socket and netlink listeners.

Interface cloning (`SIOCIFCREATE2`, `ifconfig epair create`) creates `epair` pairs and further
`lo` units; `ifconfig -C` lists the cloners.

### 5.2 The net class (`EtherQueues`)

As DRIVER-MODEL.md §9.1 plans: a driver exposes transmit and receive queue pairs, its offloads,
link state and media, and receive filters (unicast address, multicast list, promiscuous,
all-multicast). The class owns everything iflib owns in FreeBSD: per-queue transmit rings (a
packet is completed exactly once), budgeted receive processing in the queue's interrupt thread,
interrupt moderation and busy-polling, receive-side flow hashing to pick a queue, software LRO,
and the `ifnet` the driver appears as. Drivers never see sockets or protocol state.

### 5.3 virtio-net

`vtnet` as FreeBSD's `if_vtnet.c` negotiates it: `MAC`, `STATUS`, `MRG_RXBUF`, `CSUM` and
`GUEST_CSUM`, `HOST_TSO4`/`TSO6` and `GUEST_TSO4`/`TSO6`, `CTRL_VQ`, `CTRL_RX`, `MQ`, `EVENT_IDX`.
It appears as `vtnet0` with the media and options `ifconfig` shows on FreeBSD. On QEMU's `q35`
and `virt` machines and on bhyve it is a PCI function, the same transport `vtblk` uses.

### 5.4 Loopback and `epair`

`lo0` keeps its P6 configuration and gains a data path: output queues the packet to the input
side through `netisr` (`if_simloop`), so a send never recurses into a receive. `epair` pairs
deliver each other's frames, and either end can move into a VNET jail (§10).

## 6. Concurrency

- **Receive.** A driver's receive queue runs in its interrupt thread with a budget per pass;
  packets go up through Ethernet, IP, and the protocol to the socket buffer without changing
  thread. Flows hash to queues, so one connection's segments arrive on one CPU.
- **Transmit.** A sender builds the packet in its own thread and enqueues it on the transmit
  queue its flow hashes to; the queue drains without a global lock.
- **Epoch.** Lookups of interfaces, addresses, routes, next hops, and PCBs happen inside a network
  epoch section, as FreeBSD's `NET_EPOCH_ENTER`; objects removed during a section are freed after
  every CPU has left it. Nothing sleeps inside a section.
- **Locks.** One lock per table for writers (interfaces, routes, each protocol's PCB hash), one
  per connection (FreeBSD's INP lock), one per socket buffer, taken in FreeBSD's order (the INP
  lock, then a socket buffer's). Timers run under their connection's lock.
- **Timers.** TCP's retransmission, persist, keepalive, delayed-acknowledgement, and 2MSL timers
  are one callout per connection, rescheduled to the earliest deadline, as FreeBSD 15's
  `tcp_timer.c` does. ARP and ND entries expire through their table's callouts.

## 7. The link layer

- **Ethernet.** Frame input with the destination filter, VLAN tag stripping when offloaded,
  `bpf` taps on input and output, demultiplexing by EtherType; output resolves the next hop's
  link-layer address and prepends the header.
- **ARP.** `if_ether.c`'s behavior: the `lltable`, requests with FreeBSD's retransmission and hold
  queue limits, replies, gratuitous ARP when an address is added, entry expiry
  (`net.link.ether.inet.max_age`), `arp -a` through `NET_RT_FLAGS`.
- **ND.** Neighbor solicitation and advertisement, neighbor unreachability detection, duplicate
  address detection, redirects, and the neighbor cache (`ndp -a`).
- **Multicast.** Interface multicast filters, IGMPv3 and MLDv2 as far as joining groups requires
  (the solicited-node groups ND needs, and `IP_ADD_MEMBERSHIP`/`IPV6_JOIN_GROUP` for programs).

## 8. IPv4 and IPv6

- **Input** validates lengths, header checksum (unless offloaded), and options, and delivers to
  the protocol or forwards (`net.inet.ip.forwarding`, `net.inet6.ip6.forwarding`, off by default
  and on in tests' routers).
- **Output** chooses the route and source address (IPv6 by the RFC 6724 policy P6 implemented),
  fragments where allowed, honors path MTU, and hands the packet to the interface with the
  checksum work the interface's capabilities do not take.
- **Reassembly** with FreeBSD's limits (`maxfragpackets`, `maxfragsperpacket`, the IPv6
  `frag6` limits) and timeouts.
- **ICMP and ICMPv6.** Echo, errors with FreeBSD's rate limit (`net.inet.icmp.icmplim`),
  unreachable for closed UDP ports, fragmentation-needed and packet-too-big feeding path MTU, and
  ICMPv6's ND and MLD messages.
- **SLAAC.** Router advertisements accepted on interfaces with `accept_rtadv` (`rtsold` sends the
  solicitations), prefixes and default routers recorded, addresses formed as 15.1's defaults form
  them, with DAD and the valid and preferred lifetimes.
- **Raw sockets.** `SOCK_RAW` for `IPPROTO_ICMP`, `IPPROTO_ICMPV6`, and others, with `IP_HDRINCL`,
  so `ping`, `ping6`, `traceroute`, and `rtsold` run unchanged.

## 9. Transport

### 9.1 UDP

The P6 semantics on real packets: checksums (offloaded where possible), the receive control
messages already implemented, broadcast and multicast delivery, `ICMP port unreachable` both ways.

### 9.2 TCP

FreeBSD 15.1's default stack, with its defaults (`net.inet.tcp.*` as GENERIC sets them):

- The RFC 9293 state machine on segments, with P6's states and errors preserved; `TIME_WAIT`,
  half-close, resets, urgent data, keepalives (`TCP_KEEP*`), and `TCP_NODELAY` with Nagle.
- Options: MSS (`mssdflt` 536), window scaling and timestamps (`rfc1323`), SACK (`sack.enable`)
  with FreeBSD's scoreboard and loss recovery, ECN in its default server mode (`ecn.enable` 2).
- Retransmission: RTT estimation and RTO as `tcp_timer.c` computes them, with FreeBSD's minimum
  and slop; persist; and the proportional rate reduction and limited transmit FreeBSD applies.
- Congestion control: NewReno and CUBIC (default) behind FreeBSD's `cc` interface, the initial
  window of 10 segments (`initcwnd_segments`), and the `net.inet.tcp.cc.*` sysctls.
- The SYN cache with SYN cookies (`syncookies` 1), the host cache (`netstat` and
  `net.inet.tcp.hostcache.list`), path MTU discovery, and delayed acknowledgements.
- Socket buffer autotuning (`sendbuf_auto`, `recvbuf_auto`; send space 32 KiB, receive 64 KiB
  initially), TSO sizing against the interface's limit, and software LRO on receive.
- Observability: `TCP_INFO`, `net.inet.tcp.stats` (`netstat -s -p tcp`), the `xtcpcb` records P6
  exports, `tcp_functions` names (`freebsd`), and `sockstat`'s connection states.

## 10. VNET

A VNET jail (`jail -c vnet ...`) has its own `Vnet`: its own `lo0`, interfaces moved into it
(`ifconfig epair0b vnet j1`, returned to the parent when the jail goes), routes, neighbor caches,
PCB tables, statistics, and per-VNET sysctls. `sys/prison` gains the `vnet` parameter, as
ADR-0016 anticipated. This is what FreeBSD's network tests use to build hosts and routers in one
machine (`vnet_mkjail`, `vnet_mkepair` in `tests/sys/common/vnet.subr`), so it arrives with the
control plane in P7.2, before the protocols, and every protocol milestone is tested on it.

## 11. Control plane

- **Routes.** Per-VNET FIBs (`net.fibs`, default 1; `setfib`, `SO_SETFIB`), next-hop objects
  shared by routes, longest-prefix lookup inside the epoch, interface and gateway routes, reject
  and blackhole routes, and the route attributes `route` and `netstat -r` show.
- **Routing socket.** `PF_ROUTE` sockets: `RTM_ADD`, `RTM_DELETE`, `RTM_CHANGE`, `RTM_GET`, and
  the announcements (`RTM_IFINFO`, `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_IFANNOUNCE`, route changes)
  that `dhclient`, `route monitor`, and `rtsold` listen for.
- **Netlink.** The P6 `NETLINK_ROUTE` handlers move to `sys/net` and become per-VNET; group
  notifications are delivered (P6 records subscriptions but sends nothing); `RTM_*NEIGH` reads the
  neighbor caches; `NETLINK_GENERIC` gets its controller and the families 15.1's tools open.
- **Interface ioctls** on real interfaces: media (`SIOCGIFMEDIA`, `SIOCGIFXMEDIA`), capabilities
  (`SIOCSIFCAP`), link-layer address (`SIOCSIFLLADDR`), multicast (`SIOCADDMULTI`), and the
  cloning ones.
- **Sysctls.** The `net.*` nodes `rc`, `netstat`, and `sysctl -a` read, the statistics nodes
  (`net.inet.*.stats`, `net.inet6.*.stats`), and `netstat -m`'s zone counts.

## 12. `bpf` and configuration daemons

- **`/dev/bpf`** as a cloning device with the `BIOC*` ioctls `dhclient`, `tcpdump`, and `rtsold`
  use, buffer sizes and the double-buffer read, immediate mode, direction filters, timestamps, and
  data-link types. Filter programs are validated as `bpf_validate` does and interpreted by a
  host-tested classic BPF machine.
- **`dhclient`** reads and writes raw frames through `bpf`, configures the address and default
  route through ioctls and the routing socket, and writes `resolv.conf`; nothing in it is
  special-cased.
- **`rtsold`** sends router solicitations on a raw ICMPv6 socket; the kernel does SLAAC.

## 13. `sendfile` and kernel TLS

- **`sendfile`** sends page-cache pages as page-backed buffers without copying, with headers and
  trailers, `SF_NODISKIO`, and read-ahead (`SF_FLAGS` readahead), waiting for pages being read as
  `kern_sendfile.c` does.
- **Kernel TLS.** `TCP_TXTLS_ENABLE` and `TCP_RXTLS_ENABLE` with TLS 1.0–1.3 records: AES-CBC with
  HMAC-SHA1/SHA-256/SHA-384, AES-GCM, and ChaCha20-Poly1305, written in-tree (`sys/crypto`).
  `sendfile` over a TLS socket encrypts the pages into new buffers. `kern.ipc.tls.*` sysctls and
  statistics as FreeBSD has them.
- **Ciphers** are constant-time in software, use the CPU's instructions where present (AES-NI and
  PCLMULQDQ, the ARMv8 AES and PMULL instructions) behind `arch/*`, and are tested against the
  published vectors (NIST CAVP, RFC 8439, RFC 4231) on the host. `ktls_test` is the end-to-end
  check (566 cases pass on FreeBSD 15.1 in bhyve).

## 14. The socket API

What remains of [COMPATIBILITY.md](COMPATIBILITY.md)'s Tier C for INET and INET6: multicast
options, `IP_BINDANY` and `IP_BINDMULTI`, `SO_REUSEPORT_LB`, `SO_SETFIB`, `IP_RECVIF` on real
interfaces, accept filters only if a base daemon enables one, and `sendfile`. Every option P6 accepted keeps its behavior.

## 15. The test network

- **QEMU** (both architectures, CI's network cases): user-mode networking, which provides DHCPv4,
  router advertisements, DNS, and outbound NAT, so `dhclient`, SLAAC, `fetch`, and `pkg` work with
  nothing on the host.
- **bhyve** (the build server): a dedicated bridge, `abyssnet0`, with a tap per guest, dnsmasq
  on it for DHCPv4 and DNS, base's `rtadvd` for router advertisements, and outbound IPv4 NAT
  through `pf`, IPv6 kept on the link; set up by `tools/netlab/setup-server.sh` under `doas`
  (BUILD-AND-TEST.md §1). AbyssBSD and FreeBSD guests can share the bridge for interoperability
  and differential captures, which `tcpdump` on the host's tap records.
- **The performance peer** is the build server itself, over each guest's virtio-net, the same for
  both kernels.

## 16. Testing

### 16.1 Host

- Parsers (Ethernet, ARP, IPv4, IPv6 and extension headers, ICMP, UDP, TCP options, ND options,
  DHCP-relevant frames, routing and netlink messages, BPF programs), each with unit tests and a
  fuzzing harness (an in-tree `std` harness; no external crates).
- TCP on a simulated network: two endpoints and a link with configurable delay, bandwidth, loss,
  reordering, duplication, and MTU, driven by a virtual clock, so a scenario (a lost SYN-ACK, a
  zero window, a SACK hole, a CUBIC epoch) replays exactly.
- The ciphers against their published vectors, and Miri for the unsafe parts of `sys/mbuf` and
  `sys/epoch`.

### 16.2 kyua

`sys/net`, `sys/netinet`, `sys/netinet6`, `sys/netlink`, `sbin/ifconfig`, `sbin/route`,
`sbin/ping`, and `sys/kern/ktls_test` join the suite, each against FreeBSD 15.1's results in the
same VM with ADR-0016's target (95% of what passes on FreeBSD, every other failure listed with its
reason). Test programs for P7's non-goals (§1.2) are left out until their phase. Many of FreeBSD's
network tests skip without `scapy`; they are compared as FreeBSD runs them, skipped on both.

### 16.3 Differential captures

`tests/net/scenarios/` holds scripted exchanges (a handshake and transfer, a lost segment, a zero
window and persist, a reset to a closed port, a path MTU change, a SACK recovery, DHCP, SLAAC,
ARP, ND with DAD, ICMP errors). Each runs against AbyssBSD and FreeBSD guests from the same peer;
the captures are normalized and compared field by field, and an accepted difference is recorded
with its reason, as `compat-deviations.md` does for user-visible ones.

### 16.4 Boot cases

`<arch>-net` (QEMU) and `x86_64-bhyve-net` boot the reference disk with networking: `dhclient`
and SLAAC configure `vtnet0`, `ping` and `ping6` reach the router, `fetch` downloads a file,
`ssh` logs in from the host, and `scp` copies a file both ways and compares it. A soak case runs
`iperf3` for an hour with the kernel's leak and zone checks before and after.

## 17. Performance

The P7 components ([ADR-0014](adr/0014-performance-parity-per-component.md)), each gated on its
geometric mean, over loopback and over virtio-net in bhyve with the build server as the peer
(physical NICs from P8):

| Component | Benchmarks |
|---|---|
| `tcp` | `iperf3` throughput, one stream and four, each direction; request/response latency (p50 and p99) and connection-establishment rate in `ubench`; CPU time per gigabit at a fixed rate |
| `udp` | `iperf3` UDP throughput at a fixed packet size, and small-datagram rate |
| `sendfile` | A file from the page cache over TCP, plain and over kernel TLS (AES-GCM) |

`iperf3` comes from FreeBSD's packages, the same binary on both systems, installed into the
reference disk's test overlay; it is not part of base. Both systems negotiate the same TCP
options and offloads, checked from a capture of each run (PERFORMANCE.md's "same semantics").
The workloads are in [tests/perf/README.md](../tests/perf/README.md) (suite `p7`).

## 18. Hardware

The X870E's and the Orange Pi 6 Plus's NICs ([HARDWARE.md](HARDWARE.md) §4, §5) are P8's
drivers on the P7 class. Nothing in P7 touches the physical machines.

## 19. Risks

- **TCP correctness and interoperability** is the largest risk. The simulated network and the
  differential captures exist to find divergences before real peers do.
- **Cryptography written in-tree** must be correct and constant-time; the published vectors,
  differential `ktls_test` runs, and review of the timing-sensitive code are the defense.
- **VNET breadth.** Every new piece of state has to be per-VNET; a global slipped in is a bug that
  the jail tests catch only if a test exercises it.
- **Virtualized interrupt cost.** Under bhyve every IPI and many interrupts exit to the host
  (the P6 `unix.rights` work measured about 1 µs per IPI). Queue and wakeup design should minimize
  cross-CPU wakeups per packet, as FreeBSD's does.
