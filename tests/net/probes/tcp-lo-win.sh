#!/bin/sh
# Loopback TCP for 5 s at several socket buffer sizes (`iperf3 -w`), on
# AbyssBSD or FreeBSD: whether less data in flight goes faster.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
for w in 256K 512K 1M 2M 4M 0; do
	if [ "$w" = 0 ]; then a=""; else a="-w $w"; fi
	r=$(/tmp/iperf3 -f m -t 5 $a -c 127.0.0.1 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
	echo "probe tcp.lo.w$w $r Mbit/s"
done
shutdown -p now
