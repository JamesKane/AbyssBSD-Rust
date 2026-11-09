#!/bin/sh
# Loopback TCP for 10 s, on AbyssBSD or FreeBSD: each thread's CPU time
# before and after (`ps -H`), for the ones that ran.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
ps -axH -o lwp,time,comm,tdname > /tmp/ps0 2>&1
(/tmp/iperf3 -f m -t 10 -c 127.0.0.1 > /tmp/out) &
sleep 9
ps -axH -o lwp,time,comm,tdname > /tmp/ps1 2>&1
wait
r=$(awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s1 $r Mbit/s"
sed 's/^/probe ps0 /' /tmp/ps0 | grep -E 'iperf|netisr|swi|intr|idle|softclock|LWP'
sed 's/^/probe ps1 /' /tmp/ps1 | grep -E 'iperf|netisr|swi|intr|idle|softclock|LWP'
shutdown -p now
