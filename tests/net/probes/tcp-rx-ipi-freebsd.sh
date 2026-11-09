#!/bin/sh
# FreeBSD's wakeups during 10 s of `iperf3 -R` (TCP received from the
# peer, $1): scheduler notifications, IPIs sent, and wakeups an idle CPU
# took without one (`cpu_idle_wakeup`), counted with DTrace.
peer=$1
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
kldload dtraceall 2> /dev/null
dtrace -Z -q -n 'fbt::tdq_notify:entry { @n = count(); }
    fbt::ipi_cpu:entry { @i = count(); }
    fbt::ipi_selected:entry { @s = count(); }
    fbt::cpu_idle_wakeup:return /arg1 != 0/ { @w = count(); }
    fbt::sched_idletd:entry { @d = count(); }
    fbt::sched_wakeup:entry { @k = count(); }
    tick-13s { printa("probe fbsd tdq_notify %@d\n", @n); printa("probe fbsd ipi_cpu %@d\n", @i);
    printa("probe fbsd ipi_selected %@d\n", @s); printa("probe fbsd idle_wakeup_noipi %@d\n", @w);
    printa("probe fbsd sched_wakeup %@d\n", @k); exit(0); }' &
sleep 2
r=$(/tmp/iperf3 -f m -t 10 -c "$peer" -R | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.rx.s1 $r Mbit/s"
wait
shutdown -p now
