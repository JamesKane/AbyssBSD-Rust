# devfs(8)'s rules and rulesets on a devfs mount of the scenario's own:
# adding rules (numbered and not, includes), showing them, applying a
# ruleset and single rules, what they hide and the modes they set (on
# that mount only), and the errors of each request.
d() {
	devfs -m dev "$@"
	echo "(exit $?)"
}
mkdir dev
mount -t devfs devfs dev
echo "(mount $?)"
d ruleset 100
d rule add hide
d rule add path null unhide
d rule add path zero unhide mode 0600 group operator
d rule -s 101 add path random unhide
d rule -s 101 add 150 path 'pts/*' unhide
d rule -s 101 add 150 path urandom unhide
d rule -s 102 add include 101
d rule -s 102 add include 100
d rule -s 103 add include 99
d rule -s 100 show
d rule -s 101 show
d rule -s 102 show
d rule -s 102 show 200
d rule -s 102 show 150
d rule applyset
ls -a dev
stat -f '%Sp %Su:%Sg %N' dev/null dev/zero /dev/zero
d rule -s 101 applyset
ls -a dev
d rule apply path random hide
ls -a dev
d rule apply 100
ls -a dev
d rule -s 104 applyset
d rule -s 101 del 150
d rule -s 101 show
d rule -s 100 delset
d rule -s 100 show
d rule -s 102 show
d ruleset 0
d rule -s 102 delset
d rule -s 101 delset
umount dev
echo "(umount $?)"
