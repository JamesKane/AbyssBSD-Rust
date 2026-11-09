#!/bin/sh
# Loopback TCP for 10 s: the system counters (vm.stats.sys, where a
# temporary counter can stand in) the run changed.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys0
r=$(/tmp/iperf3 -f m -t 10 -c 127.0.0.1 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.lo.s1 $r Mbit/s"
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys1
paste /tmp/sys0 /tmp/sys1 | awk 'BEGIN { split("swtch trap syscall intr soft", n) } { print "probe sys", n[NR], $2 - $1 }'
shutdown -p now
