# SPDX-License-Identifier: BSD-2-Clause
"""tcp-sack: the guest's second segment is lost. The peer acknowledges the
first, then sends a duplicate ACK for each segment after the hole, each
with a SACK block for what arrived past it, until the guest resends the
lost one; then everything is acknowledged at once, and the peer closes
after the guest.
"""

from peer import ACK, FIN, SYN, TCP, Peer, after, dlen, end, fail, flags

p = Peer(7006, sack=True)
p.accept(p.expect(SYN))
got = p.burst()
data = [s for s in got if dlen(s)]
if len(data) < 5:
    fail(f"{len(data)} segments in the first window")
lost = data[1]


def resent(segs):
    return any(dlen(s) and s[TCP].seq == lost[TCP].seq for s in segs)


p.rcv_nxt = end(data[0])
p.send(ACK)
later = p.quiet()
sack = [("NOP", None), ("NOP", None)]
for s in data[2:]:
    p.send(ACK, opts=sack + [("SAck", (end(lost), end(s)))])
    later += p.quiet()
    if resent(later):
        break
else:
    later += p.burst()
    if not resent(later):
        fail("the lost segment was not sent again")
got += later
top = max((end(s) for s in got if dlen(s)), key=lambda e: after(p, e))
p.rcv_nxt = top
if any(flags(s) & FIN for s in got):
    p.rcv_nxt = (top + 1) & 0xFFFFFFFF
    p.send(ACK)
else:
    p.send(ACK)
    p.expect(FIN)
    p.rcv_nxt = (top + 1) & 0xFFFFFFFF
    p.send(ACK)
p.quiet()
p.close()
p.done()
