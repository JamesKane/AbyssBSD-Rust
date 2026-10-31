#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# Undoes setup-server.sh: the test bridge, its rc.conf lines, pf and its
# rules (only if they are setup-server.sh's), forwarding, dnsmasq, and
# rtadvd.
#
#     doas sh tools/netlab/teardown-server.sh

set -eu

BRIDGE=abyssnet0
CLONE=bridge1
PF_CONF=/etc/pf.conf
DNSMASQ_CONF=/usr/local/etc/dnsmasq-abyssnet.conf

[ "$(id -u)" = 0 ] || { echo "run as root (doas)" >&2; exit 1; }

service rtadvd onestop || true
sysrc -x rtadvd_enable rtadvd_interfaces ipv6_gateway_enable || true
sysctl net.inet6.ip6.forwarding=0

if [ -e "$DNSMASQ_CONF" ]; then
	service dnsmasq onestop || true
	sysrc -x dnsmasq_enable dnsmasq_conf || true
	rm -f "$DNSMASQ_CONF"
fi

if head -1 "$PF_CONF" 2>/dev/null | grep -q 'tools/netlab/setup-server.sh'; then
	service pf onestop || true
	sysrc -x pf_enable pf_rules || true
	rm -f "$PF_CONF"
fi

sysrc -x gateway_enable || true
sysctl net.inet.ip.forwarding=0

ifconfig "$BRIDGE" destroy 2>/dev/null || true
sysrc cloned_interfaces-="$CLONE" || true
sysrc -x "ifconfig_${CLONE}_name" "ifconfig_${BRIDGE}" "ifconfig_${BRIDGE}_ipv6" || true

echo "abyssnet0 removed"
