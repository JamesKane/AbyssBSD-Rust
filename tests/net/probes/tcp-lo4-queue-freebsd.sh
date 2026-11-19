#!/bin/sh
# FreeBSD, loopback TCP, four streams, for 10 s: who queues to netisr (the
# netisr thread itself, from the segments its input sends, or another
# thread, from a send call) and whose packets a full queue drops.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
kldload dtraceall 2> /dev/null
dtrace -q -n 'fbt::netisr_queue:entry { @q[strstr(stringof(curthread->td_name), "netisr") != NULL ? "self" : "other"] = count(); }
    fbt::netisr_queue_src:return /arg1 != 0/ { @d[strstr(stringof(curthread->td_name), "netisr") != NULL ? "self" : "other"] = count(); }
    tick-14s { printa("probe fbsd queued %s %@d\n", @q); printa("probe fbsd dropped %s %@d\n", @d); exit(0); }' &
sleep 3
r=$(/tmp/iperf3 -f m -t 10 -P 4 -c 127.0.0.1 | awk '/SUM.*receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.lo.s4 $r Mbit/s"
wait
shutdown -p now
