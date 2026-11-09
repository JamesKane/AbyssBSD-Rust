#!/bin/sh
# Suite p7's udp.tx.small five times (5 s each) to the server's peer ($1):
# its rate flips between two regimes under bhyve, so one run says little.
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
for i in 1 2 3 4 5; do
	r=$(/tmp/iperf3 -f m -t 5 -c "$peer" -u -b 0 -l 64 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
	echo "probe udp.tx.small $r Mbit/s"
	r=$(/tmp/iperf3 -f m -t 5 -c "$peer" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
	echo "probe tcp.tx.s1 $r Mbit/s"
done
shutdown -p now
