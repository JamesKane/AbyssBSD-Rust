#!/bin/sh
# Suite p7's tcp.cpu.tx alone, with a profile (`--prof`): TCP to the
# server's peer ($1) at 1 Gbit/s for 20 s, and the CPU a KiB costs.
peer=$1
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
for irq in $(vmstat -ai | awk '/virtio_pci|vtnet|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
echo "kprof pause"
a=$(sysctl -n kern.cp_time)
echo "kprof resume"
r=$(/tmp/iperf3 -f k -t 20 -b 1000M -c "$peer" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Kbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "kbench tcp.cpu.tx $r"
echo "kprof pause"
b=$(sysctl -n kern.cp_time)
hz=$(sysctl -n kern.clockrate | sed 's/.*stathz = \([0-9]*\).*/\1/')
echo $a $b $hz $r | awk '$12 > 0 {
	busy = 0
	for (i = 1; i <= 4; i++)
		busy += $(i + 5) - $i
	kib = $12 * 1000 / 8 * 20 / 1024
	printf "probe tcp.cpu.tx %.3f ns/op\n", busy / $11 * 1e9 / kib
	printf "probe cp_time user %d nice %d sys %d intr %d idle %d\n", $6-$1, $7-$2, $8-$3, $9-$4, $10-$5
}'
shutdown -p now
