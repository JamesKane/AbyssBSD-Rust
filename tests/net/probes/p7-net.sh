#!/bin/sh
# A short look at suite p7's network benchmarks (`cargo xtask --remote
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
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
run() {
	name=$1
	shift
	echo "kprof resume"
	r=$(/tmp/iperf3 -f m -t 5 "$@" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
	echo "kbench $name $r Mbit/s"
	echo "kprof pause"
	echo "probe $name $r Mbit/s"
}
echo "kprof pause"
run tcp.lo.s1 -c 127.0.0.1
run tcp.tx.s1 -c "$peer"
run tcp.tx.s4 -c "$peer" -P 4
run tcp.rx.s1 -c "$peer" -R
run udp.tx.bulk -c "$peer" -u -b 0 -l 1448
run udp.tx.small -c "$peer" -u -b 0 -l 64
run udp.rx.bulk -c "$peer" -u -b 0 -l 1448 -R
shutdown -p now
