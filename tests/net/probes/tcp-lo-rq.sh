#!/bin/sh
# Loopback TCP for 10 s, on AbyssBSD or FreeBSD: about every 10 ms in
# seconds 2 to 8, each iperf3 thread's state and wait channel next to the
# data connection's receive and send queues (`netstat`), to see whether a
# thread sleeps with data waiting for it.
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
/tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
(/tmp/iperf3 -f m -t 10 -c 127.0.0.1 > /tmp/out) &
sleep 2
for i in $(jot 150); do
	t=$(ps -axH -o lwp,state,wchan,comm | awk '/iperf3/ && $3 != "select" && $3 != "kqread" { printf "%s:%s:%s ", $1, substr($2, 1, 1), $3 }')
	q=$(netstat -an -p tcp | awk '$1 == "tcp4" && $6 == "ESTABLISHED" && $2 + $3 > 100000 { printf "%s:%s:%s ", $4, $2, $3 }')
	echo "$t| $q"
	sleep 0.01
done > /tmp/w
wait
r=$(awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }' /tmp/out)
echo "probe tcp.lo.s1 $r Mbit/s"
sed 's/^/probe w /' /tmp/w
shutdown -p now
