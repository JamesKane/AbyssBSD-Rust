#!/bin/sh
# 10 s of `iperf3` sending TCP to the peer ($1), device interrupts on
# CPU 0 as suite p7 has them: the system counters (vm.stats.sys, where a
# temporary counter can stand in), the interrupts, and TCP's counters
# the run changed; under `--prof` its `kbench` line ends the profile.
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
netstat -s -p tcp > /tmp/tcp0
vmstat -i > /tmp/intr0
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys0
r=$(/tmp/iperf3 -f m -t 10 -c "$peer" | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "kbench tcp.tx.s1 $r Mbit/s"
echo "probe tcp.tx.s1 $r Mbit/s"
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys1
paste /tmp/sys0 /tmp/sys1 | awk 'BEGIN { split("swtch trap syscall intr soft", n) } { print "probe sys", n[NR], $2 - $1 }'
netstat -s -p tcp > /tmp/tcp1
vmstat -i > /tmp/intr1
sed 's/^/probe intr0 /' /tmp/intr0
sed 's/^/probe intr1 /' /tmp/intr1
paste -d '|' /tmp/tcp0 /tmp/tcp1 | awk -F'|' '$1 != $2 { sub(/^[ \t]+/, "", $2); print "probe tcp", $2, "(was", $1 ")" }' |
    sed 's/(was[ \t]*/(was /'
shutdown -p now
