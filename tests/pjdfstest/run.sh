#!/bin/sh
# SPDX-License-Identifier: BSD-2-Clause
#
# Runs pjdfstest's TAP scripts and prints one line per script:
#
#   pjd: <group>/<script> <plan> <passed>[ not ok: <numbers>]
#
# Usage: run.sh <suite> <work> [<group>...]. Each line also goes to
# standard error, prefixed by $PJD_LABEL and the script's time, when that
# is set (progress on a console while standard output goes to a file). <suite> holds pjdfstest,
# conf, misc.sh, and <group>/NN.t as FreeBSD installs them; the scripts
# run as root with <work> (on the file system under test) as their
# directory. Needs only sh and what the scripts themselves use, so the
# same runner gives FreeBSD's results and AbyssBSD's.

suite=$1
work=$2
shift 2
[ $# -gt 0 ] || set -- chflags chmod chown ftruncate granular link mkdir \
    mkfifo mknod open rename rmdir symlink truncate unlink utimensat
out=$work/.pjd.out
for group in "$@"; do
	for t in "$suite/$group"/*.t; do
		start=$(date +%s)
		(cd "$work" && sh "$t") >"$out" 2>&1
		took=$(($(date +%s) - start))
		plan=none
		passed=0
		failed=
		while read -r a b c rest; do
			case "$a" in
			1..*)
				plan=$a
				;;
			ok)
				passed=$((passed + 1))
				;;
			not)
				[ "$b" = ok ] || continue
				case "$rest" in
				"# TODO"*) passed=$((passed + 1)) ;;
				*) failed="$failed $c" ;;
				esac
				;;
			esac
		done <"$out"
		line="pjd: $group/${t##*/} $plan $passed${failed:+ not ok:$failed}"
		echo "$line"
		[ -z "$PJD_LABEL" ] || echo "$PJD_LABEL ${took}s $line" >&2
	done
done
rm -f "$out"
echo "pjd: done"
[ -z "$PJD_LABEL" ] || echo "$PJD_LABEL pjd: done" >&2
