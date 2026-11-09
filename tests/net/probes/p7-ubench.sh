#!/bin/sh
# Suite p7's `ubench p7` part alone (`cargo xtask --remote net-soak --probe
# tests/net/probes/p7-ubench.sh [--freebsd]`): round trips and connects
# over loopback and to the server's peer ($1), sendfile, kernel TLS; each
# result on a `probe` line (ns/op, lower is better).
peer=$1
rm -f /tmp/ubench
cp /mnt/ubench /tmp/ && umount /mnt
for irq in $(vmstat -ai | awk '/virtio_pci|vtnet|nvme/ { sub(/^irq/, "", $1); sub(/:$/, "", $1); print $1 }'); do
	cpuset -l 0 -x "$irq"
done
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
# To a file, not a pipe: the loopback peer it forks keeps a pipe open
# after a failure, and the script would never get to power off.
/tmp/ubench p7 "$peer" > /tmp/ubench.out 2>&1
cat /tmp/ubench.out
sed -n 's/^kbench \([^ ]* [0-9.]* ns.*\)/probe \1/p' /tmp/ubench.out
shutdown -p now
