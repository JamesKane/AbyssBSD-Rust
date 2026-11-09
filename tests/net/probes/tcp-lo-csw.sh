#!/bin/sh
# Loopback TCP for 10 s, on AbyssBSD or FreeBSD: each iperf3 thread's
# CPU time and voluntary and involuntary context switches between
# seconds 1 and 9, and the netisr and loopback drop counts.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
(/tmp/iperf3 -f m -t 10 -c 127.0.0.1 > /tmp/out) &
sleep 1
ps -axH -o lwp,time,nvcsw,nivcsw,comm | grep iperf3 > /tmp/ps0
netstat -i -d -I lo0 > /tmp/n0 2>&1
sleep 8
ps -axH -o lwp,time,nvcsw,nivcsw,comm | grep iperf3 > /tmp/ps1
netstat -i -d -I lo0 > /tmp/n1 2>&1
wait
r=$(awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s1 $r Mbit/s"
sed 's/^/probe ps0 /' /tmp/ps0
sed 's/^/probe ps1 /' /tmp/ps1
sed 's/^/probe n0 /' /tmp/n0
sed 's/^/probe n1 /' /tmp/n1
netstat -Q 2>&1 | sed 's/^/probe q /'
shutdown -p now
