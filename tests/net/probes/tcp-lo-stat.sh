#!/bin/sh
# Loopback TCP for 10 s, on AbyssBSD or FreeBSD: the connection's buffers
# halfway (`netstat -x`), and what the run changed in TCP's counters.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
netstat -s -p tcp > /tmp/tcp0
(/tmp/iperf3 -f m -t 10 -c 127.0.0.1 > /tmp/out) &
sleep 5
netstat -anx -p tcp | sed 's/^/probe x /'
wait
r=$(awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s1 $r Mbit/s"
netstat -s -p tcp > /tmp/tcp1
paste -d '|' /tmp/tcp0 /tmp/tcp1 | awk -F'|' '$1 != $2 { sub(/^[ \t]+/, "", $2); print "probe tcp", $2 }'
shutdown -p now
