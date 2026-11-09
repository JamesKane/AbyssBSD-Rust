#!/bin/sh
# Suite p7's iperf3 benchmarks the other probes leave out (`cargo xtask
# --remote net-soak --probe tests/net/probes/p7-rest.sh [--freebsd]`):
# four streams over loopback and received, the CPU a byte costs sending and
# receiving, small datagrams received; as the suite measures them (ns/op).
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
rate() {
	awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Kbits/sec") v = $(i - 1) } END { print v + 0 }'
}
ip() {
	name=$1 scale=$2
	shift 2
	/tmp/iperf3 -f k "$@" | rate | awk -v n="$name" -v s="$scale" \
	    '$1 > 0 { printf "probe %s %.3f ns/op\n", n, s / $1 }'
}
cpu() {
	name=$1
	shift
	a=$(sysctl -n kern.cp_time)
	r=$(/tmp/iperf3 -f k -t 20 -b 1000M "$@" | rate)
	b=$(sysctl -n kern.cp_time)
	hz=$(sysctl -n kern.clockrate | sed 's/.*stathz = \([0-9]*\).*/\1/')
	echo $a $b $hz $r | awk -v n="$name" '$12 > 0 {
		busy = 0
		for (i = 1; i <= 4; i++)
			busy += $(i + 5) - $i
		kib = $12 * 1000 / 8 * 20 / 1024
		printf "probe %s %.3f ns/op\n", n, busy / $11 * 1e9 / kib
	}'
}
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
ip tcp.lo.s4 8.192e9 -c 127.0.0.1 -t 10 -O 2 -P 4
ip tcp.rx.s4 8.192e9 -c "$peer" -t 10 -O 2 -R -P 4
cpu tcp.cpu.tx -c "$peer"
cpu tcp.cpu.rx -c "$peer" -R
ip udp.rx.small 5.12e8 -c "$peer" -t 10 -O 2 -u -b 0 -l 64 -R
shutdown -p now
