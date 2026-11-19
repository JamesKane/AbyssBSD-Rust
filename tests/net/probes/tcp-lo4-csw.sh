#!/bin/sh
# Loopback TCP, four streams, for 10 s, on AbyssBSD or FreeBSD: each iperf3
# thread's CPU time and voluntary and involuntary context switches between
# seconds 1 and 9, the system's counters over the same span, and thirty
# looks at each thread's state and wait channel.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
(/tmp/iperf3 -f m -t 10 -P 4 -c 127.0.0.1 > /tmp/out) &
sleep 1
ps -axH -o lwp,time,nvcsw,nivcsw,comm | grep iperf3 > /tmp/ps0
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys0
sleep 4
for i in $(jot 30); do
	ps -axH -o lwp,state,wchan,comm | awk '/iperf3/ && $3 != "select" && $3 != "kqread" { printf "%s:%s ", substr($2, 1, 1), $3 }'
	echo
done > /tmp/w
sleep 3
ps -axH -o lwp,time,nvcsw,nivcsw,comm | grep iperf3 > /tmp/ps1
sysctl -n vm.stats.sys.v_swtch vm.stats.sys.v_trap vm.stats.sys.v_syscall vm.stats.sys.v_intr vm.stats.sys.v_soft > /tmp/sys1
wait
r=$(awk '/SUM.*receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s4 $r Mbit/s"
sed 's/^/probe ps0 /' /tmp/ps0
sed 's/^/probe ps1 /' /tmp/ps1
paste /tmp/sys0 /tmp/sys1 | awk 'BEGIN { split("swtch trap syscall intr soft", n) } { print "probe sys", n[NR], $2 - $1 }'
sed 's/^/probe w /' /tmp/w
shutdown -p now
