#!/bin/sh
# FreeBSD's side of `tcp-lo-pin.sh --prof-thread netisr`: loopback TCP for
# 10 s, the receiver (the server) on CPU 3 and the sender on CPU 1, with
# DTrace's samples of the netisr thread by kernel function, the busiest
# first, and of every thread by name (idle shows as the idle threads).
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
cpuset -l 3 /tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
kldload dtraceall 2> /dev/null
dtrace -q -x aggsize=8m -n 'profile-4999 { @td[stringof(curthread->td_name)] = count(); }
    profile-4999 /strstr(stringof(curthread->td_name), "netisr") != NULL && arg0 != 0/ {
        @fn[func(arg0)] = count(); }
    tick-13s { trunc(@td, 20); trunc(@fn, 80);
    printa("probe fbsd td %@8d %s\n", @td);
    printa("probe fbsd netisr %@8d %a\n", @fn); exit(0); }' &
sleep 2
r=$(cpuset -l 1 /tmp/iperf3 -f m -t 10 -c 127.0.0.1 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.lo.s1 $r Mbit/s"
wait
shutdown -p now
