# truss(1), following forks, on a fixed run of system calls (diffproc
# syscalls): its calls with their arguments decoded, results and errors,
# the caught signal, and the child's fork return and exit. As in kdump.sh,
# only what lies between the markers is compared, the child's lines
# apart, and what the file system decides masked.

names() {
	awk '{ print $2, $1 }' .pids | while read -r n p; do
		echo "s/(^|[^0-9])$p([^0-9]|\$)/\\1<$n>\\2/g"
	done > names.sed
	sed -E -f names.sed |
	    sed -E 's/(inode|dev|blksize)=[0-9]+/\1=*/g'
}

truss -f -o t.out diffproc syscalls -
echo "(exit $?)"
traced=$(awk '$2 == "traced" { print $1 }' .pids)
child=$(awk '$2 == "child" { print $1 }' .pids)

echo '$ truss -f (the traced process, between the markers)'
awk -v p="$traced:" '$1 == p' t.out |
    awk '/"diff-begin"/ { p = 1 } p { print } /"diff-end"/ { exit }' | names
echo '$ truss -f (the child)'
awk -v p="$child:" '$1 == p' t.out | names
