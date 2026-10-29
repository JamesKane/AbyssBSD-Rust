# GEOM's userland view (kern.geom.confxml and conftxt, through libgeom):
# a GPT that mkimg(1) writes (-y: its unit-test UUIDs, the same on every
# run), copied onto memory disk md9, which GEOM tastes again when the
# writer closes it; then as gpart(8), geom(8), and glabel(8) show it.
# Only md9's geoms are shown: the machines' own disks differ.

# md9's part of kern.geom.conftxt: its line and the ones under it.
md9txt() {
	sysctl -n kern.geom.conftxt | awk '/^0 / { on = ($3 == "md9") } on'
}

# The classes in kern.geom.confxml, in its order.
classes() {
	sysctl -n kern.geom.confxml | sed -n 's|^    <name>\(.*\)</name>$|\1|p'
}

# Every geom of md9's in kern.geom.confxml (one that names it).
md9xml() {
	sysctl -n kern.geom.confxml | awk '
		/<geom id/ { g = "" }
		{ g = g $0 "\n" }
		/<\/geom>/ { if (g ~ /md9/) printf "%s", g }'
}

mdconfig -a -t malloc -s 64m -u 9
mkimg -y -s gpt -c 64m \
    -p efi/efi9::1m \
    -p freebsd-swap/swap9::4m \
    -p 'freebsd-ufs/data disk::16m' \
    -p freebsd-ufs::8m \
    -o geom.img
dd if=geom.img of=/dev/md9 bs=1m 2> /dev/null
show gpart show md9
show gpart show -l md9
show gpart show -r md9
show gpart show -p md9
show gpart list md9
show geom part list md9
show geom md list md9
show sh -c 'glabel status | grep -e Name -e md9'
show glabel list md9p3
show sh -c 'geom -t | grep -e Geom -e md9'
show sh -c 'ls /dev | grep md9'
show ls /dev/gpt/efi9 /dev/gpt/swap9 /dev/gpt/data%20disk
show md9txt
show md9xml
show classes
show sh -c 'sysctl -n kern.geom.confdot | head -1'
# Opens, counted down the mesh: a reader of gpt/swap9 (the label takes an
# exclusive count of md9p2), and a writer of md9p3, which withers md9p3's
# labels while it is open.
spawn holder sh -c 'exec 3< /dev/gpt/swap9 4<> /dev/md9p3; : > holder.ready; exec sleep 99999'
while [ ! -e holder.ready ]; do sleep 0.1; done
show sh -c 'gpart list md9 | grep -e Name -e Mode'
show sh -c 'glabel status | grep -e Name -e md9'
show sh -c 'glabel list md9p2 | grep -e Name -e Mode'
kill $holder
wait $holder 2> /dev/null
mdconfig -d -u 9
show gpart show md9
show sh -c 'ls /dev | grep md9'
