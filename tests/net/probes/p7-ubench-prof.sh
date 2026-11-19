#!/bin/sh
# Suite p7's `ubench p7` part with its lines on the console as it runs,
# so that the kernel's profiler (`--prof`) reports each benchmark.
peer=$1
rm -f /tmp/ubench
cp /mnt/ubench /tmp/ && umount /mnt
for i in $(jot 120); do
	ifconfig vtnet0 | grep -q 'inet 10\.77\.0\.' && break
	sleep 1
done
# To the console, not a pipe: the loopback peer it forks keeps a pipe
# open after a failure, and the script would never get to power off.
/tmp/ubench p7 "$peer" > /dev/console 2>&1
shutdown -p now
