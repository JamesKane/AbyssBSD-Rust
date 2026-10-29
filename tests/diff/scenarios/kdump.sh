# ktrace(1) and kdump(1), unmodified, on a fixed run of system calls
# (diffproc syscalls): calls and returns, names, I/O, structures, a
# sysctl's name, utrace, a caught signal, the forked child's records,
# and the sleep's context switches. Only what lies between the run's
# markers is compared (what libc does to start differs with memory
# placement), the child's records apart from the parent's (they
# interleave as the two are scheduled), and what the file system
# decides (inode, device, times, block size) masked.

# Pids by name; the stat fields a file system decides masked.
names() {
	awk '{ print $2, $1 }' .pids | while read -r n p; do
		echo "s/(^|[^0-9])$p([^0-9]|\$)/\\1<$n>\\2/g"
	done > names.sed
	sed -E -f names.sed |
	    sed -E 's/(dev|ino|rdev|atime|mtime|ctime|birthtime|blksize|blocks)=[^,}]*/\1=*/g'
}

ktrace -i -f k.out diffproc syscalls -
echo "(exit $?)"
traced=$(awk '$2 == "traced" { print $1 }' .pids)
child=$(awk '$2 == "child" { print $1 }' .pids)

echo '$ kdump -f k.out -p <traced> (between the markers)'
kdump -f k.out -p "$traced" |
    awk '/"diff-begin"/ { p = 1 } p { print } /"diff-end"/ { exit }' | names
echo '$ kdump -f k.out -p <child>'
kdump -f k.out -p "$child" | names

# Context switches: the sleep's (others come with preemption).
rm -f k.out .pids
: > .pids
ktrace -t w -f k.out diffproc syscalls -
echo '$ kdump -t w | grep nanslp'
kdump -f k.out -t w | grep -c 'CSW.*"nanslp"'
