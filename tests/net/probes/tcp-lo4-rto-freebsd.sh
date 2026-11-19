#!/bin/sh
# FreeBSD, loopback TCP, four streams, for 10 s: the connection's state at
# each retransmission timeout (what is in flight, the window, duplicate
# ACKs, whether in fast recovery, bytes SACKed).
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
kldload dtraceall 2> /dev/null
dtrace -q -n 'fbt::tcp_timer_rexmt:entry { this->tp = (struct tcpcb *)arg0;
    printf("probe rto flight %d cwnd %d ssth %d dup %d fr %d sacked %d rxtcur %d shift %d state %d\n",
    this->tp->snd_max - this->tp->snd_una, this->tp->snd_cwnd, this->tp->snd_ssthresh, this->tp->t_dupacks,
    (this->tp->t_flags & 0x00100000) != 0, this->tp->sackhint.sacked_bytes, this->tp->t_rxtcur,
    this->tp->t_rxtshift, this->tp->t_state); }
    tick-14s { exit(0); }' &
sleep 3
r=$(/tmp/iperf3 -f m -t 10 -P 4 -c 127.0.0.1 | awk '/SUM.*receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.lo.s4 $r Mbit/s"
wait
shutdown -p now
