#!/bin/sh
# FreeBSD's CPU 0 during 10 s of `iperf3 -R` (TCP received from the peer,
# $1), device interrupts on CPU 0 as suite p7 has them: DTrace's samples
# of CPU 0 by kernel function, the busiest first.
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
dtrace -q -x aggsize=8m -n 'profile-4999 /cpu == 0/ { @all = count(); }
    profile-4999 /cpu == 0 && arg0 != 0/ { @fn[func(arg0)] = count(); }
    tick-13s { trunc(@fn, 45); printa("probe fbsd cpu0 samples %@d\n", @all);
    printa("probe fbsd %@8d %a\n", @fn); exit(0); }' &
sleep 2
r=$(/tmp/iperf3 -f m -t 10 -c "$peer" -R | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.rx.s1 $r Mbit/s"
wait
shutdown -p now
