#!/bin/sh
# FreeBSD's side of tcp-lo-pin.sh: loopback TCP for 10 s, the receiver
# (the server) on CPU 3 and the sender on CPU 1, with DTrace's samples of
# both CPUs by kernel function, the busiest first (user time counted
# apart; idle shows as cpu_idle_acpi, including the 3 s around the run).
rm -f /tmp/iperf3 /tmp/libiperf.so.0
cp /mnt/iperf3 /mnt/libiperf.so.0 /tmp/ && umount /mnt
export LD_LIBRARY_PATH=/tmp
cpuset -l 3 /tmp/iperf3 -s -D -B 127.0.0.1
sleep 1
kldload dtraceall 2> /dev/null
dtrace -q -x aggsize=8m -n 'profile-4999 /cpu == 1 || cpu == 3/ { @all[cpu] = count(); }
    profile-4999 /(cpu == 1 || cpu == 3) && arg0 == 0/ { @user[cpu] = count(); }
    profile-4999 /(cpu == 1 || cpu == 3) && arg0 != 0/ { @fn[cpu, func(arg0)] = count(); }
    tick-13s { trunc(@fn, 80); printa("probe fbsd cpu%d samples %@d\n", @all);
    printa("probe fbsd cpu%d user %@d\n", @user);
    printa("probe fbsd cpu%d %@8d %a\n", @fn); exit(0); }' &
sleep 2
r=$(cpuset -l 1 /tmp/iperf3 -f m -t 10 -c 127.0.0.1 | awk '/receiver/ { for (i = 2; i <= NF; i++) if ($i == "Mbits/sec") v = $(i - 1) } END { print v + 0 }')
echo "probe tcp.lo.s1 $r Mbit/s"
wait
shutdown -p now
