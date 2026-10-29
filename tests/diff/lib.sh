# SPDX-License-Identifier: BSD-2-Clause
#
# What every differential scenario sources (tests/diff/run.sh runs each
# one in an empty directory of its own, under script(1), so it leads a
# session on a pseudo-terminal of its own). See docs/BUILD-AND-TEST.md.
#
#   spawn NAME CMD...    runs CMD in the background as NAME: the normalizer
#                        shows its pid as <NAME>, and $NAME holds it
#   helper NAME MODE     spawn NAME diffproc MODE, and waits until its
#                        state is set up
#   waitfor PID WCHAN    waits (up to 30 s) until PID's first thread
#                        sleeps on WCHAN, or is stopped (T) or a zombie (Z)
#   show CMD...          runs CMD, its output going to the normalizer
#                        (everything a scenario prints does)

DIFF=/usr/tests/diff
DIFFBIN=$DIFF/bin
PATH=/bin:/usr/bin:/sbin:/usr/sbin:$DIFFBIN
export PATH

: > .pids

spawn() {
	_name=$1
	shift
	"$@" &
	eval "$_name=$!"
	echo "$! $_name" >> .pids
}

helper() {
	rm -f "$1.ready"
	spawn "$1" diffproc "$2" "$1.ready"
	_i=0
	while [ ! -e "$1.ready" ]; do
		_i=$((_i + 1))
		if [ $_i -gt 300 ]; then
			echo "diff: $1 ($2) never got ready" >&2
			return 1
		fi
		sleep 0.1
	done
}

waitfor() {
	_i=0
	while :; do
		_w=$(ps -o state= -o wchan= -p "$1" 2> /dev/null | head -1)
		case "$_w" in
		T*|Z*) return 0 ;;
		*" $2") return 0 ;;
		esac
		_i=$((_i + 1))
		if [ $_i -gt 300 ]; then
			echo "diff: pid $1 not in $2 (it is: $_w)" >&2
			return 1
		fi
		sleep 0.1
	done
}

show() {
	echo "\$ $*"
	"$@" 2>&1
	echo "(exit $?)"
}

# sorted FIELD HEADER_RE command...: like show, but the lines after the
# first one matching HEADER_RE are sorted by FIELD (numerically; 0 sorts
# by the last field, as text). For displays sorted by a key the scenario
# leaves tied (ps -u by %CPU, top by WCPU): FreeBSD leaves ties in the
# order it found them, which varies from run to run.
sorted() {
	_f=$1 _re=$2
	shift 2
	echo "\$ $*"
	"$@" > sorted.out 2>&1
	_st=$?
	awk -v re="$_re" '{ print } $0 ~ re { exit }' sorted.out
	awk -v re="$_re" -v f="$_f" '
		# Blank lines (which end a table) stay after the rest.
		h { k = NF == 0 ? 2 : 1 (f == 0 ? $NF : sprintf("%012d", $f)); print k "\t" $0 }
		$0 ~ re { h = 1 }' sorted.out | sort | cut -f 2-
	echo "(exit $_st)"
}
