#!/bin/sh
# Loopback TCP for 10 s, on AbyssBSD or FreeBSD: iperf3's whole report
# (each second's rate, retransmits, and the sender's congestion window,
# from `TCP_INFO`), and the TCP counters that moved.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
netstat -s -p tcp > /tmp/s0 2>&1
/tmp/iperf3 -f m -t 10 -c 127.0.0.1 > /tmp/out 2>&1
netstat -s -p tcp > /tmp/s1 2>&1
sed 's/^/probe out /' /tmp/out
r=$(awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s1 $r Mbit/s"
diff /tmp/s0 /tmp/s1 | sed -n 's/^> /probe tcp /p'
shutdown -p now
