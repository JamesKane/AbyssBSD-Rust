# SPDX-License-Identifier: BSD-2-Clause
"""tcp-pmtu: the path's MTU is 1280. The peer drops what is larger and
answers the first such packet with ICMP's fragmentation needed; it
acknowledges what arrives in order, a round at a time, and closes after
the guest.
"""

from peer import ACK, FIN, IP, SYN, TCP, Peer, end, flags

MTU = 1280

p = Peer(7007)
p.accept(p.expect(SYN))
told = fin = False
acked = p.rcv_nxt
while not fin:
    for s in p.burst():
        if s[IP].len > MTU:
            if not told:
                p.need_frag(s, MTU)
                told = True
            continue
        if s[TCP].seq == p.rcv_nxt:
            p.rcv_nxt = end(s)
            if flags(s) & FIN:
                p.rcv_nxt = (p.rcv_nxt + 1) & 0xFFFFFFFF
                fin = True
    if p.rcv_nxt != acked:
        p.send(ACK)
        acked = p.rcv_nxt
p.close()
p.done()
