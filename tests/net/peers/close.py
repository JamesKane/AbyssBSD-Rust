# SPDX-License-Identifier: BSD-2-Clause
"""tcp-simultaneous-close: the peer's FIN crosses the guest's. It answers
the guest's FIN with its own, acknowledging only what came before that
FIN, so the guest goes from FIN_WAIT_1 to CLOSING; then it acknowledges
the guest's FIN, and the guest waits in TIME_WAIT.
"""

from peer import ACK, FIN, SYN, Peer

p = Peer(7005)
p.accept(p.expect(SYN))
p.expect(FIN)
p.send(FIN | ACK)
p.snd_nxt += 1
p.expect(ACK)
p.rcv_nxt = (p.rcv_nxt + 1) & 0xFFFFFFFF
p.send(ACK)
p.done()
