# ADR-0019: Networking

- Status: Accepted
- Date: 2026-10-30

## Context

Phase 7 must give unmodified FreeBSD 15.1 a working network: `dhclient` and SLAAC configure an
interface, `sshd`, `scp`, `fetch`, and `pkg` work, on both architectures in QEMU and bhyve, before
the physical machines (P8) bring their NICs. P6 brought a slice forward (MULTIUSER.md §8.4):
`AF_INET` and `AF_INET6` sockets with FreeBSD's bind, connect, state, and option semantics, `lo0`,
interface ioctls, `net.routetable`, and `NETLINK_ROUTE`. It builds no packets: a TCP connection is
two sockets writing into each other's buffers, and the interface and route tables are one global
with `lo0` wired in. There is no packet buffer, Ethernet, ARP, ND, IP on a wire, TCP segment,
routing socket, `bpf`, virtio-net, or kernel TLS.

FreeBSD's own network tests (`tests/sys/net`, `netinet`, `netinet6`) mostly build their hosts and
routers from VNET jails joined by `epair` interfaces; without VNET they cannot run. ADR-0016
deferred VNET to the phase that needs it. ADR-0011 and ADR-0014 require a performance suite per
phase, and the roadmap requires differential tests of TCP against FreeBSD.

## Decision

Adopt the design in [NETWORKING.md](../NETWORKING.md). Its load-bearing parts:

1. **The wire is the specification.** Each protocol milestone ends with scripted scenarios
   captured against AbyssBSD and FreeBSD guests and compared field by field after normalization;
   accepted differences are recorded with reasons.
2. **Per-VNET state from the start.** All network state lives in a `Vnet` reached through an
   explicit context. VNET jails and `epair` arrive with the control plane (P7.2), before the
   protocols, and are the test bed for every later milestone and for kyua's network tests.
3. **New crates, host-tested:** `sys/mbuf` (packets), `sys/epoch` (deferred reclamation),
   `sys/net` (interfaces, the net driver class, Ethernet, routes, routing socket, netlink, `bpf`),
   `sys/tcp`, `sys/crypto`, `sys/ktls`, and `drivers/virtio-net`. `sys/inet` keeps its PCBs and
   P6 semantics and gains IPv4, IPv6, ICMP, ND, UDP on the wire; its direct delivery goes. TCP is
   tested on the host against a deterministic simulated network.
4. **One owned packet type** with FreeBSD's packet-header fields and reference-counted cluster and
   page-backed segments; one copy on each side of the kernel, none between layers. Socket buffers,
   UNIX-domain ones included, hold these chains and keep FreeBSD's accounting.
5. **Lock-free lookups in a network epoch** for interfaces, routes, next hops, and PCBs, with one
   lock per table for writers and one per connection, as FreeBSD's `net_epoch` and INP locks.
6. **Run to completion** from the driver's receive queue to the socket (`net.isr.dispatch=direct`),
   with `netisr` queues only where FreeBSD defers.
7. **FreeBSD 15.1's default TCP**: the `freebsd` stack with its defaults (SACK, window scaling and
   timestamps, ECN server mode, initial window 10, CUBIC with NewReno available, SYN cookies,
   hostcache, autotuned buffers). RACK, BBR, TCP Fast Open, and offload engines are not P7's.
8. **The net driver class** is DRIVER-MODEL.md's `EtherQueues`, with the class owning what iflib
   owns; `vtnet` is its first driver. Hardware NICs are P8's.
9. **Kernel TLS with in-tree ciphers**: AES-CBC with HMAC, AES-GCM, ChaCha20-Poly1305, constant
   time, accelerated through `arch/*`, verified against published vectors and `ktls_test`.
10. **The test network**: QEMU's user-mode networking for the QEMU cases; on the build server a
    dedicated bridge with an `xtask` responder (DHCPv4, router advertisements, DNS) and outbound
    NAT; the build server is the performance peer for both kernels.
11. **kyua target**: `sys/net`, `sys/netinet`, `sys/netinet6`, `sys/netlink`, `sbin/ifconfig`,
    `sbin/route`, `sbin/ping`, and `sys/kern/ktls_test` meet ADR-0016's target (95% of what passes
    on FreeBSD 15.1 in the same VM, every other failure listed). Tests of non-goal features
    (packet filters, tunnels, bridges, VLANs, CARP, SCTP, IPsec) are left out until their phase.
12. **Performance**: components `tcp`, `udp`, and `sendfile`, over loopback and virtio-net in
    bhyve, each gated on its geometric mean; `iperf3` from FreeBSD's packages on both sides, with
    `ubench` for latency, connection rate, and `sendfile`.

## Consequences

- `sys/inet::ifnet` and `sys/inet::netlink` move into `sys/net` and become per-VNET; their P6
  behavior is kept and their tests move with them.
- UNIX-domain socket buffers change representation in P7.1 with no user-visible effect.
- `sys/prison` gains the `vnet` jail parameter and interface moves between VNETs.
- Packet filters, tunnels, bridges, and VLANs remain absent; `rc` scripts that would use them see
  what a FreeBSD kernel without those modules shows, recorded in compat-deviations.md.
- The epoch introduced here is the mechanism a later lock-free descriptor lookup
  (`fget_unlocked`) would use.
- The build server's network setup (bridge, taps, responder, NAT) is a root step done by a script
  under `doas`, recorded in BUILD-AND-TEST.md.
