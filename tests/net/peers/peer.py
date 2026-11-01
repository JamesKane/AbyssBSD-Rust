# SPDX-License-Identifier: BSD-2-Clause
"""A scripted TCP peer for the differential captures (xtask/src/netdiff.rs).

It runs on the peer guest, a FreeBSD 15.1 machine on the test bridge, as
10.77.0.2, whose own kernel drops what it has no socket for
(net.inet.tcp.blackhole=2). The peer answers the guest segment by segment
as a script says: dropping one, acknowledging selectively, closing at the
same time, or reporting a smaller path MTU. It never sends while the guest
is still sending: it waits until the guest has been quiet for QUIET
seconds, so the capture's order does not depend on how fast either is.
"""

import struct
import sys
import time

from scapy.all import IP, TCP, Ether, Raw, conf, get_if_hwaddr, sniff

FIN, SYN, RST, PSH, ACK = 0x01, 0x02, 0x04, 0x08, 0x10
IFACE = "vtnet0"
ME = "10.77.0.2"
QUIET = 0.05
# The peer's initial sequence number (a SYN's is blanked in the traces).
ISS = 0x10000000


def fail(why):
    print(f"peer: {why}", flush=True)
    sys.exit(1)


def flags(p):
    return int(p[TCP].flags)


def dlen(p):
    """The segment's data length (Ethernet padding aside)."""
    return p[IP].len - p[IP].ihl * 4 - p[TCP].dataofs * 4


def end(p):
    """The sequence number after the segment's data."""
    return (p[TCP].seq + dlen(p)) & 0xFFFFFFFF


def after(p, seq):
    """How far past the guest's initial sequence number `seq` is."""
    return (seq - p.irs) & 0xFFFFFFFF


def cksum(data):
    if len(data) % 2:
        data += b"\0"
    s = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return ~s & 0xFFFF


class Peer:
    """One connection, accepted on `port`; `sack` offers SACK."""

    def __init__(self, port, sack=False):
        conf.verb = 0
        self.port = port
        self.sack = sack
        self.mac = get_if_hwaddr(IFACE)
        self.sock = conf.L2socket(
            iface=IFACE, filter=f"tcp and dst host {ME} and dst port {port}"
        )
        self.pending = []
        print("peer: ready", flush=True)

    def next(self, timeout):
        """The guest's next segment, or None after `timeout` seconds."""
        if self.pending:
            return self.pending.pop(0)
        got = sniff(opened_socket=self.sock, count=1, timeout=timeout)
        return got[0] if got else None

    def expect(self, want, timeout=10.0, seq=None):
        """The guest's next segment with all of the flags `want` (and
        sequence number `seq`), skipping others."""
        deadline = time.monotonic() + timeout
        while (left := deadline - time.monotonic()) > 0:
            p = self.next(left)
            if p is None:
                break
            if flags(p) & want == want and (seq is None or p[TCP].seq == seq):
                return p
        fail(f"no segment with flags {want:#x}")

    def quiet(self):
        """What the guest sends until it has been quiet for QUIET seconds
        (perhaps nothing)."""
        out = []
        while (p := self.next(QUIET)) is not None:
            out.append(p)
        return out

    def burst(self, timeout=10.0):
        """What the guest sends from its next segment until it is quiet."""
        first = self.next(timeout)
        if first is None:
            fail("the guest sent nothing")
        return [first] + self.quiet()

    def accept(self, syn):
        """Answers `syn` and waits for the handshake's last ACK (a segment
        with data after it is kept for the script)."""
        self.guest_mac = syn[Ether].src
        self.guest = syn[IP].src
        self.gport = syn[TCP].sport
        self.irs = syn[TCP].seq
        self.rcv_nxt = (self.irs + 1) & 0xFFFFFFFF
        self.snd_nxt = ISS
        opts = [("MSS", 1460)]
        if self.sack:
            opts += [("NOP", None), ("NOP", None), ("SAckOK", b"")]
        self.send(SYN | ACK, opts=opts)
        self.snd_nxt += 1
        p = self.expect(ACK)
        if dlen(p) or flags(p) & FIN:
            self.pending.insert(0, p)

    def frame(self, payload, proto):
        ip = IP(src=ME, dst=self.guest, flags="DF", proto=proto)
        return Ether(src=self.mac, dst=self.guest_mac) / ip / payload

    def send(self, fl, ack=None, opts=(), win=65535):
        t = TCP(
            sport=self.port,
            dport=self.gport,
            seq=self.snd_nxt,
            ack=self.rcv_nxt if ack is None else ack,
            flags=fl,
            window=win,
            options=list(opts),
        )
        self.sock.send(self.frame(t, 6))

    def need_frag(self, p, mtu):
        """ICMP's fragmentation needed for the guest's packet `p`, with the
        next hop's MTU, quoting its IP header and 8 bytes after."""
        quote = bytes(p[IP])[: p[IP].ihl * 4 + 8]
        body = struct.pack("!HH", 0, mtu) + quote
        icmp = struct.pack("!BBH", 3, 4, cksum(struct.pack("!BBH", 3, 4, 0) + body)) + body
        self.sock.send(self.frame(Raw(icmp), 1))

    def close(self):
        """Our FIN, after the guest's, and the guest's ACK of it."""
        self.send(FIN | ACK)
        self.snd_nxt += 1
        self.expect(ACK)

    def done(self):
        print("peer: done", flush=True)
