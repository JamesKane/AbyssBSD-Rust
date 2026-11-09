#!/bin/sh
# What vtnet0 offloads, on either system: its capabilities and options.
umount /mnt 2>/dev/null
ifconfig -m vtnet0 | sed 's/^/probe /'
sysctl dev.vtnet.0 2>/dev/null | grep -i -E 'feature|csum|tso' | sed 's/^/probe /'
shutdown -p now
