#!/bin/sh
# Loopback TCP, four streams, for 10 s, as suite p7's tcp.lo.s4 (unpinned),
# with a profile (`--prof`, `--prof-thread netisr`).
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
echo "kprof pause"
echo "kprof resume"
r=$(/tmp/iperf3 -f m -t 10 -P 4 -c 127.0.0.1 | awk '/SUM.*receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "kbench tcp.lo.s4 $r Mbit/s"
echo "kprof pause"
echo "probe tcp.lo.s4 $r Mbit/s"
shutdown -p now
