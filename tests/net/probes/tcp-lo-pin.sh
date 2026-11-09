#!/bin/sh
# Loopback TCP for 10 s with the receiver (the server) on CPU 3 and the
# sender on CPU 1, to profile one side alone (`--prof-cpu 3`, or 1).
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
cpuset -l 3 /tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
echo "kprof pause"
echo "kprof resume"
r=$(cpuset -l 1 /tmp/iperf3 -f m -t 10 -c 127.0.0.1 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "kbench tcp.lo.s1 $r Mbit/s"
echo "kprof pause"
echo "probe tcp.lo.s1 $r Mbit/s"
shutdown -p now
