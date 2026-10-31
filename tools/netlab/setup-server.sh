#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# The build server's isolated test network for bhyve guests
# (docs/NETWORKING.md §15, ROADMAP P7.0). Run once, as root:
#
#     doas sh tools/netlab/setup-server.sh
#
# It makes, now and at every boot:
#   - abyssnet0, a bridge of its own (not bridge0, which joins the LAN),
#     with 10.77.0.1/24 and fd77:77:77::1/64; test guests' taps join it;
#   - IPv4 forwarding, and pf with outbound NAT through the LAN interface,
#     so guests reach the internet (fetch, pkg) but nothing on the LAN;
#   - dnsmasq on abyssnet0 only: DHCPv4 (10.77.0.10-250) and DNS
#     forwarded to this host's resolvers;
#   - rtadvd on abyssnet0: router advertisements for fd77:77:77::/64
#     (SLAAC). It advertises a router only when this host forwards IPv6,
#     so IPv6 forwarding is on, and pf keeps it off the test link:
#     guests get SLAAC addresses and a router, but no IPv6 leaves the
#     bridge.
#
# It changes nothing that exists: it stops if abyssnet0, /etc/pf.conf, or
# its dnsmasq configuration is already there and differs. teardown-server.sh
# undoes it.

set -eu

BRIDGE=abyssnet0
CLONE=bridge1
V4=10.77.0.1/24
V6=fd77:77:77::1/64
EXT=igc0
PF_CONF=/etc/pf.conf
DNSMASQ_CONF=/usr/local/etc/dnsmasq-abyssnet.conf

[ "$(id -u)" = 0 ] || { echo "run as root (doas)" >&2; exit 1; }
ifconfig "$EXT" >/dev/null || { echo "no $EXT" >&2; exit 1; }

# --- pf rules: written only if there are none yet ---
PF_RULES=$(cat <<EOF
# AbyssBSD test network (tools/netlab/setup-server.sh).
ext_if = "$EXT"
test_if = "$BRIDGE"
test_net = "10.77.0.0/24"

nat on \$ext_if inet from \$test_net to any -> (\$ext_if)

pass all
# Guests may reach this host on the test link (DHCP, DNS, ping) and the
# internet, never the LAN or other private networks.
pass in quick on \$test_if from \$test_net to (\$test_if)
block in quick on \$test_if inet from \$test_net to { 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 169.254.0.0/16 }
# IPv6 stays on the test link: to this host and link multicast (ND) only.
pass in quick on \$test_if inet6 to { (\$test_if), ff02::/16 }
block in quick on \$test_if inet6
EOF
)
if [ -e "$PF_CONF" ]; then
	if [ "$(cat "$PF_CONF")" != "$PF_RULES" ]; then
		echo "$PF_CONF exists and is not this script's; merge by hand:" >&2
		echo "$PF_RULES" >&2
		exit 1
	fi
fi

# --- dnsmasq configuration ---
DNSMASQ_RULES=$(cat <<EOF
# AbyssBSD test network (tools/netlab/setup-server.sh): abyssnet0 only.
interface=$BRIDGE
bind-interfaces
except-interface=lo0
# DHCPv4; this host is the router and the name server.
dhcp-range=10.77.0.10,10.77.0.250,255.255.255.0,1h
dhcp-option=option:router,10.77.0.1
dhcp-option=option:dns-server,10.77.0.1
# Forward names to this host's resolvers (/etc/resolv.conf).
domain-needed
bogus-priv
EOF
)
if [ -e "$DNSMASQ_CONF" ] && [ "$(cat "$DNSMASQ_CONF")" != "$DNSMASQ_RULES" ]; then
	echo "$DNSMASQ_CONF exists and differs; remove it or merge by hand" >&2
	exit 1
fi

# --- the bridge ---
if ifconfig "$BRIDGE" >/dev/null 2>&1; then
	echo "$BRIDGE exists; keeping it"
else
	ifconfig "$CLONE" create
	ifconfig "$CLONE" name "$BRIDGE"
fi
ifconfig "$BRIDGE" inet "$V4" up
# A bridge gets no link-local address by default, and router
# advertisements must come from one.
ifconfig "$BRIDGE" inet6 -ifdisabled auto_linklocal
ifconfig "$BRIDGE" down up
ifconfig "$BRIDGE" inet6 "$V6"
sysrc cloned_interfaces+="$CLONE"
sysrc "ifconfig_${CLONE}_name=$BRIDGE"
sysrc "ifconfig_${BRIDGE}=inet $V4 up"
sysrc "ifconfig_${BRIDGE}_ipv6=inet6 -ifdisabled auto_linklocal $V6"

# --- forwarding and NAT ---
sysrc gateway_enable=YES
sysctl net.inet.ip.forwarding=1
printf '%s\n' "$PF_RULES" > "$PF_CONF"
kldload -n pf
pfctl -nf "$PF_CONF"
sysrc pf_enable=YES pf_rules="$PF_CONF"
service pf start || service pf reload

# --- dnsmasq ---
pkg install -y dnsmasq
printf '%s\n' "$DNSMASQ_RULES" > "$DNSMASQ_CONF"
sysrc dnsmasq_enable=YES dnsmasq_conf="$DNSMASQ_CONF"
service dnsmasq restart

# --- router advertisements: base's rtadvd (dnsmasq sends none on FreeBSD) ---
sysrc ipv6_gateway_enable=YES
sysctl net.inet6.ip6.forwarding=1
sysrc rtadvd_enable=YES rtadvd_interfaces="$BRIDGE"
service rtadvd restart

echo "abyssnet0 is up: 10.77.0.1/24, fd77:77:77::1/64, NAT through $EXT"
