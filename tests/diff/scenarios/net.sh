# The network's commands, on two VNET jails joined by an epair: an address
# of each family on each end, a route, neighbors from traffic and by hand,
# a listening socket and a connection. Nothing of the machine's own network
# shows. Hardware addresses and what is made from them vary by machine, as
# do ephemeral ports, and counters of anything TCP's or the memory
# allocator's timing decides. A table in the order of a hash or of kernel
# addresses is sorted.
kldload -n if_epair 2> /dev/null
a=$(ifconfig epair create)
b=${a%a}b
jail -c name=n1 host.hostname=n1 path=/ vnet persist vnet.interface=$a
jail -c name=n2 host.hostname=n2 path=/ vnet persist vnet.interface=$b

macs() {
	sed -E -e 's/([0-9a-f]{2}:){5}[0-9a-f]{2}/<mac>/g' \
	    -e 's/fe80::[0-9a-f:]+/fe80::<iid>/g' \
	    -e 's/expires in [0-9]+ seconds/expires in <n> seconds/' \
	    -e 's/ +[0-9]+[smh]( |$)/ <t>\1/'
}
counts() {
	sed -E 's/[0-9]+/<n>/g'
}
ports() {
	sed -E -e 's/\.[0-9]{4,5}( |$)/.<port>\1/g' -e 's/:[0-9]{4,5}( |$)/:<port>\1/g'
}

for j in n1 n2; do
	jexec $j ifconfig lo0 inet 127.0.0.1/8 up
done
jexec n1 ifconfig $a inet 10.99.0.1/24 up
jexec n1 ifconfig $a inet6 fd99::1/64
jexec n2 ifconfig $b inet 10.99.0.2/24 up
jexec n2 ifconfig $b inet6 fd99::2/64
jexec n1 route -q add -net 10.98.0.0/24 10.99.0.2
jexec n1 route -q -6 add fd98::/64 fd99::2
# Duplicate address detection ends.
sleep 3
jexec n1 ping -q -c 1 10.99.0.2 > /dev/null
jexec n1 ping -q -6 -c 1 fd99::2 > /dev/null

show jexec n1 ifconfig -l
show jexec n1 ifconfig -a | macs
show jexec n1 ifconfig $a inet
show jexec n1 netstat -rn | macs
show jexec n1 netstat -rn -f inet
show jexec n1 netstat -rn -f inet6 | macs
show jexec n1 route -n get 10.98.0.5 | macs
show jexec n1 route -n get -inet6 fd98::5 | macs
show jexec n1 arp -an | macs | sort
show jexec n1 arp -s 10.99.0.9 02:00:00:00:00:09
show jexec n1 arp -an | macs | sort
show jexec n1 arp -d 10.99.0.9
show jexec n1 ndp -an | macs | sort
show jexec n1 ndp -p
show jexec n1 ndp -r
show jexec n1 netstat -in | macs | counts
show jexec n1 netstat -s -p icmp
show jexec n1 netstat -s -p udp
show jexec n1 netstat -s -p arp
show jexec n1 netstat -s -p tcp | counts
show jexec n1 netstat -s -p ip | counts
# FreeBSD 15.1 counts every histogram and scope in slot 0 (IP6STAT_INC2).
show jexec n1 netstat -s -p ip6 | counts |
    grep -v -E 'hop by hop|ICMP<n>|two or more mbuf|\(null\)|scope=|globals'
show netstat -m | counts

spawn lsn jexec n1 nc -l 10.99.0.1 5000
spawn lsn6 jexec n1 nc -6 -l fd99::1 5001
spawn udp jexec n1 nc -u -l 10.99.0.1 5002
sleep 1
show jexec n1 sockstat -4 -l | ports | sort
show jexec n1 sockstat -6 -l | ports | sort
show jexec n1 netstat -an -f inet | ports
show jexec n1 netstat -an -f inet6 | ports
spawn cli sh -c 'sleep 60 | jexec n2 nc 10.99.0.1 5000'
sleep 1
show jexec n1 sockstat -4 -c | ports | sort
show jexec n1 netstat -an -p tcp -f inet | ports
show jexec n2 netstat -an -p tcp -f inet | ports

kill $lsn $lsn6 $udp $cli 2> /dev/null
pkill -j n1 nc
pkill -j n2 nc
jail -r n1 n2
# The interfaces go home in the background on FreeBSD.
ifconfig $a destroy 2> /dev/null
