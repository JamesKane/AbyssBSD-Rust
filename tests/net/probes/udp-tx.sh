#!/bin/sh
# UDP bulk sends alone, for 10 s: a short look at suite p7's (`cargo xtask --remote
# net-soak --probe tests/net/probes/p7-net.sh [--prof]`): each `iperf3` run
# for 5 seconds against the server's peer ($1), its receiver's rate on a
# `probe` line, then a `kbench` line, which ends that run's profile when
# the kernel profiles (`abyss.prof`).
peer=$1
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
# Device interrupts to CPU 0, as suite p7 sets them.
for irq in $(vmstat -ai | awk '/virtio_pci|vtnet|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
run() {
	name=$1
	shift
	echo "kprof resume"
	r=$(/tmp/iperf3 -f m -t 10 "$@" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
	echo "kbench $name $r Mbit/s"
	echo "kprof pause"
	echo "probe $name $r Mbit/s"
}
echo "kprof pause"




sysctl -n vfs.heapx > /dev/null 2>&1
run udp.tx.bulk -c "$peer" -u -b 0 -l 1448
sysctl -n vfs.heapx > /dev/null 2>&1


shutdown -p now
