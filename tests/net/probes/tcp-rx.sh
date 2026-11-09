#!/bin/sh
# 10 s of `iperf3 -R` (TCP received from the peer, $1), its device
# interrupts on CPU 0 as suite p7 has them, between two reads of
# `vfs.heapx` (a temporary counter dump when one is installed); under
# `--prof` its `kbench` line ends the profile.
peer=$1
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
export LD_LIBRARY_PATH=/tmp
# Device interrupts to CPU 0, as suite p7 sets them.
for irq in $(vmstat -ai | awk '/virtio_pci|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
sysctl -n vfs.heapx > /dev/null 2>&1
netstat -s -p tcp > /tmp/tcp0
vmstat -i > /tmp/intr0
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys0
r=$(/tmp/iperf3 -f m -t 10 -c "$peer" -R | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "kbench tcp.rx.s1 $r Mbit/s"
sysctl -n vfs.heapx > /dev/null 2>&1
echo "probe tcp.rx.s1 $r Mbit/s"
vmstat -i > /tmp/intr1
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys1
paste /tmp/sys0 /tmp/sys1 | awk 'BEGIN { split("swtch intr soft", n) } { print "probe sys", n[NR], $2 - $1 }'
sed 's/^/probe intr0 /' /tmp/intr0
sed 's/^/probe intr1 /' /tmp/intr1
# What the run changed in TCP's counters (lines start with a tab).
netstat -s -p tcp > /tmp/tcp1
paste -d '|' /tmp/tcp0 /tmp/tcp1 | awk -F'|' '$1 != $2 { sub(/^[ \t]+/, "", $2); print "probe tcp", $2, "(was", $1 ")" }' |
    sed 's/(was[ \t]*/(was /'
shutdown -p now
