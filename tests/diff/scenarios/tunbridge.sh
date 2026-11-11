# tun(4), tap(4), and bridge(4) (docs/NETWORKING.md §5.5): two VNET jails
# on a bridge of two epairs ping each other and the bridge shows its
# members and what it learned; a tun in each jail carries IP between them
# through nc --tun, as FreeBSD's if_tun_test does; a tap is listed closed.
# Hardware addresses, and the seconds an address has left, vary; a
# failed request's extended error (FreeBSD 15's exterror) is not shown,
# AbyssBSD keeping none.
kldload -n if_epair 2> /dev/null
kldload -n if_bridge 2> /dev/null
kldload -n if_tuntap 2> /dev/null

macs() {
	sed -E -e 's/([0-9a-f]{2}:){5}[0-9a-f]{2}/<mac>/g' \
	    -e 's/fe80::[0-9a-f:]+/fe80::<iid>/g' \
	    -e 's/ 1[12][0-9][0-9] flags=/ <expire> flags=/'
}
noext() {
	sed -E 's/^(ifconfig: .*: [A-Za-z ]+) \(.*\)$/\1/'
}

a=$(ifconfig epair create)
b=${a%a}b
c=$(ifconfig epair create)
d=${c%a}b
br=$(ifconfig bridge create)
jail -c name=h1 host.hostname=h1 path=/ vnet persist vnet.interface=$b
jail -c name=h2 host.hostname=h2 path=/ vnet persist vnet.interface=$d

show ifconfig $br addm $a addm $c up
ifconfig $a up
ifconfig $c up
jexec h1 ifconfig $b inet 10.97.0.1/24 up
jexec h2 ifconfig $d inet 10.97.0.2/24 up
show sh -c "jexec h1 ping -q -c 1 -t 5 10.97.0.2 > /dev/null"
show sh -c "jexec h2 ping -q -c 1 -t 5 10.97.0.1 > /dev/null"
show ifconfig $br | macs
show ifconfig $br addr | macs
show ifconfig $a | macs
show ifconfig $br deletem $a
show ifconfig $br addm $a
show ifconfig $br addm $a 2>&1 | noext
show ifconfig $br addm nonesuch0 2>&1 | noext
show ifconfig $br static $a 02:00:00:00:00:09
show ifconfig $br maxaddr 100 timeout 600 ifmaxaddr $c 5 private $c
show ifconfig $br | macs
show ifconfig $br deladdr 02:00:00:00:00:09
show ifconfig $br flushall
show ifconfig $br addr
show sysctl net.link.bridge

# tun: each jail's end of a UDP tunnel between them.
t1=$(ifconfig tun create)
t2=$(ifconfig tun create)
show ifconfig $t1
show ls -l /dev/$t1 | awk '{ print $1, $3, $4, $NF }'
ifconfig $t1 vnet h1
ifconfig $t2 vnet h2
spawn n1 jexec h1 nc -u -l --tun /dev/$t1 10.97.0.1 2600
spawn n2 jexec h2 nc -u --tun /dev/$t2 10.97.0.1 2600
sleep 1
jexec h1 ifconfig $t1 inet 10.96.0.1/24 10.96.0.2 up
jexec h2 ifconfig $t2 inet 10.96.0.2/24 10.96.0.1 up
show sh -c "jexec h2 ping -q -c 1 -t 5 10.96.0.1 > /dev/null"
show jexec h1 ifconfig $t1
show jexec h1 netstat -rn -f inet
show sysctl net.link.tun net.link.tap

# tap: closed, no carrier.
p=$(ifconfig tap create)
show ifconfig $p | macs
show ifconfig $p destroy

kill $n1 $n2 2> /dev/null
jail -r h1 h2
ifconfig $br destroy
ifconfig $a destroy 2> /dev/null
ifconfig $c destroy 2> /dev/null
ifconfig $t1 destroy 2> /dev/null
ifconfig $t2 destroy 2> /dev/null
