#!/bin/sh
# FreeBSD's wakeups during 10 s of `iperf3` (TCP sent to the
# peer, $1): scheduler notifications, IPIs sent, and wakeups an idle CPU
# took without one (`cpu_idle_wakeup`), counted with DTrace, device
# interrupts on CPU 0 as suite p7 has them; and TCP's counters.
peer=$1
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
for irq in $(vmstat -ai | awk '/virtio_pci|vtnet|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
kldload dtraceall 2> /dev/null
dtrace -Z -q -n 'fbt::tdq_notify:entry { @n = count(); }
    fbt::ipi_cpu:entry { @i = count(); }
    fbt::ipi_selected:entry { @s = count(); }
    fbt::cpu_idle_wakeup:return /arg1 != 0/ { @w = count(); }
    fbt::sched_idletd:entry { @d = count(); }
    sched:::wakeup { @k = count(); }
    sched:::on-cpu /execname == "iperf3"/ { @o = count(); }
    tick-13s { printa("probe fbsd tdq_notify %@d\n", @n); printa("probe fbsd ipi_cpu %@d\n", @i);
    printa("probe fbsd ipi_selected %@d\n", @s); printa("probe fbsd idle_wakeup_noipi %@d\n", @w);
    printa("probe fbsd sched_wakeup %@d\n", @k); printa("probe fbsd iperf_oncpu %@d\n", @o); exit(0); }' &
netstat -s -p tcp > /tmp/tcp0
vmstat -i > /tmp/intr0
sleep 2
r=$(/tmp/iperf3 -f m -t 10 -c "$peer" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.tx.s1 $r Mbit/s"
wait
netstat -s -p tcp > /tmp/tcp1
vmstat -i > /tmp/intr1
sed 's/^/probe intr0 /' /tmp/intr0
sed 's/^/probe intr1 /' /tmp/intr1
paste -d "|" /tmp/tcp0 /tmp/tcp1 | awk -F"|" '$1 != $2 { sub(/^[ \t]+/, "", $2); print "probe tcp", $2 }'
shutdown -p now
