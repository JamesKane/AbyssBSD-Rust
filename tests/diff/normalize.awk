# SPDX-License-Identifier: BSD-2-Clause
#
# The differential tests' normalizer (docs/BUILD-AND-TEST.md): what two
# machines legitimately print differently is masked, and nothing else.
# Both sides run it with FreeBSD's own awk, so their outputs compare line
# for line.
#
# Masked: pids (a scenario's own processes by name, <NAME>; others by
# first appearance, <p1>, <p2>, ...), thread ids (<t1>, ...), addresses,
# times and dates, durations, load averages, CPU percentages, memory and
# file sizes the kernel decides (VSZ, RSS, RES, ...), inode numbers, CPU
# numbers, scheduling priorities, kernel stacks, reference counts, which
# pseudo-terminal was free, idle (asleep past 20 s) against asleep, top's
# summary of the other processes and memory, and the login class (rc's
# `daemon` there, the boot shell's `default` here). Kept: everything
# else, structure, columns, flags, names, states, and wait channels.
#
# A table is recognized by its header line (column names); a row's
# fields are matched to the header's columns from the left, the last
# column taking the rest of the line (a command, a path), so a column to
# mask is named, not guessed.
#
# Input: -v pids=FILE, lines "PID NAME" (tests/diff/lib.sh's spawn).
# Runs of blanks become one blank: column widths follow the masked values.

BEGIN {
	while (pids != "" && (getline line < pids) > 0) {
		split(line, f, " ")
		name[f[1]] = "<" f[2] ">"
	}
	# Columns holding pids, thread ids, and values to mask.
	split("PID PPID PGID SID TSID TPGID", a, " ")
	for (i in a)
		pidcol[a[i]] = 1
	split("TID LWP", a, " ")
	for (i in a)
		tidcol[a[i]] = 1
	split("VSZ RSS %CPU %MEM TIME STARTED START END ELAPSED SL RE PRI UPR " \
	    "C WCPU SIZE RES PRES REF SHD INUM CSID KSTACK PAGEIN TSIZ " \
	    "LOGIN@ IDLE OFFSET DEV COW CLASS RTPRIO FSID RDEV INO USAGE", a, " ")
	for (i in a)
		maskcol[a[i]] = 1
	# Words that make a line a table header (two or more of them).
	split("PID PPID PGID SID TID LWP USER UID COMM COMMAND TT STAT STATE " \
	    "WCHAN MWCHAN NLWP THR FD MOUNT INUM PRT TP PATH AUXV VALUE RLIMIT " \
	    "SOFT HARD RESOURCE KSTACK TDNAME EMUL CMD MODE R/W NAME USERNAME " \
	    "NICE TTY FROM WHAT ARGS ENVIRONMENT OSREL EUID RUID SIG FLAGS " \
	    "KQFD FILTER IDENT DATA CPU CSID PRI UPR VSZ RSS ELAPSED START " \
	    "LIMIT RW TYPE SYSID LEN FSID RDEV INO USAGE ID", a, " ")
	for (i in a)
		hdrword[a[i]] = 1
	ncols = 0
	npid = 0
	ntid = 0
}

# A pid as shown: a scenario's name, or a number by first appearance.
function pidname(v) {
	if (v in name)
		return name[v]
	if (v !~ /^[0-9]+$/ || v + 0 == 0)
		return v
	if (!(v in seenpid))
		seenpid[v] = "<p" (++npid) ">"
	return seenpid[v]
}

function tidname(v) {
	if (v !~ /^[0-9]+$/)
		return v
	if (!(v in seentid))
		seentid[v] = "<t" (++ntid) ">"
	return seentid[v]
}

# Free text: addresses, times, and numbers that are pids or tids.
function text(s,    out, w, n, i, t, pre, post) {
	# The terminal status line: `cmd: NAME PID [WCHAN] ... RSSk`.
	if (match(s, /cmd: [^ ]+ [0-9]+ \[/)) {
		t = substr(s, RSTART, RLENGTH)
		sub(/ [0-9]+ \[$/, " <pid> [", t)
		s = substr(s, 1, RSTART - 1) t substr(s, RSTART + RLENGTH)
		sub(/ [0-9]+k$/, " <k>k", s)
	}
	gsub(/last pid: *[0-9]+/, "last pid: <p>", s)
	# top's counts: the other processes on the machine differ.
	if (s ~ /^[0-9]+ (processes|threads): /) {
		sub(/^[0-9]+ /, "<n> ", s)
		sub(/: .*/, ": <states>", s)
	}
	if (s ~ /^Mem: /)
		s = "Mem: <memory>"
	gsub(/0x[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]+/, "<addr>", s)
	# Kernel addresses as fstat prints them, without 0x.
	gsub(/[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]+/, "<addr>", s)
	gsub(/pts\/[0-9]+/, "pts/<n>", s)
	# File system and device numbers (`fstat -n`).
	gsub(/0x[0-9a-f][0-9a-f]+/, "<hex>", s)
	gsub(/[0-9]+:[0-9][0-9]:[0-9][0-9]\.[0-9]+/, "<time>", s)
	gsub(/[0-9]+:[0-9][0-9]\.[0-9][0-9]/, "<time>", s)
	gsub(/[0-9]+\+[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/, "<time>", s)
	gsub(/[0-9]+:[0-9][0-9]:[0-9][0-9]/, "<time>", s)
	gsub(/[0-9]?[0-9]:[0-9][0-9][AP]M/, "<clock>", s)
	gsub(/[0-9]?[0-9][AP]M/, "<clock>", s)
	gsub(/load averages?: *[0-9.]+,? *[0-9.]+,? *[0-9.]+/, "load averages: <l>, <l>, <l>", s)
	gsub(/load: [0-9.]+/, "load: <l>", s)
	gsub(/about [0-9]+ second/, "about <n> second", s)
	gsub(/ up .*, [0-9]+ users?/, " up <uptime>, N users", s)
	gsub(/[0-9]+\.[0-9][0-9][rus] /, "<t> ", s)
	gsub(/[0-9.]+%/, "<p>%", s)
	# In a table, only its pid columns hold pids (a signal's number
	# may be one's).
	if (inrow)
		return s
	n = split(s, w, " ")
	out = ""
	for (i = 1; i <= n; i++) {
		t = w[i]
		if (t ~ /^[0-9]+(,[0-9]+)+$/) {
			# A list of pids (`ps -p 1,2`).
			m = split(t, l, ",")
			t = ""
			for (j = 1; j <= m; j++)
				t = t (j > 1 ? "," : "") ((l[j] in name) ? name[l[j]] : l[j])
		} else if (t ~ /^[0-9]+$/ && (t in name))
			t = name[t]
		else if (t ~ /^\(?[0-9]+[,;:)]?$/) {
			# A pid inside parentheses or before punctuation.
			pre = ""; post = ""
			if (substr(t, 1, 1) == "(") { pre = "("; t = substr(t, 2) }
			if (t ~ /[,;:)]$/) { post = substr(t, length(t)); t = substr(t, 1, length(t) - 1) }
			if (t in name)
				t = name[t]
			t = pre t post
		}
		out = out (i > 1 ? " " : "") t
	}
	return out
}

function isheader(n, w,    i, hits) {
	# Column names: capitals (and `%@|/`), one of them known.
	hits = 0
	if (n < 2)
		return 0
	for (i = 1; i <= n; i++) {
		if (w[i] !~ /^[A-Z%@|\/][A-Z0-9%@|\/]*$/)
			return 0
		if (w[i] in hdrword)
			hits++
	}
	return hits >= 1
}

{
	sub(/\r$/, "")
	line = $0
	if (line ~ /^\$ / || line ~ /^\(exit [0-9]+\)$/) {
		# A command, as `show` echoes it, or its status: no table.
		ncols = 0
		print text(line)
		next
	}
	n = split(line, w, " ")
	if (n == 0) {
		ncols = 0
		print ""
		next
	}
	if (isheader(n, w)) {
		ncols = n
		for (i = 1; i <= n; i++)
			col[i] = w[i]
		# `procstat -r`'s values are counts and times.
		rusage = (line ~ /RESOURCE/)
		out = w[1]
		for (i = 2; i <= n; i++)
			out = out " " w[i]
		print out
		next
	}
	if (ncols == 0) {
		print text(line)
		next
	}
	out = ""
	limits = (line ~ / (maxprocesses|openfiles) /)
	for (i = 1; i <= n; i++) {
		v = w[i]
		c = (i < ncols) ? col[i] : col[ncols]
		if (limits && v ~ /^[0-9]+$/)
			# Limits the kernel sizes from memory (`maxproc`,
			# `maxfilesperproc`).
			v = "*"
		else if (rusage && i > 2 && v ~ /^[0-9.:]+$/)
			# `procstat -r`'s counts and times.
			v = "*"
		else if (c == "SZ|DV" && v ~ /^0x[0-9a-f]+$/)
			# A device's number.
			v = "<dev>"
		else if (i <= ncols && (c in pidcol))
			v = pidname(v)
		else if (i <= ncols && (c in tidcol))
			v = tidname(v)
		else if (i < ncols && (c in maskcol))
			v = (v == "-") ? v : "*"
		else if (i >= ncols && (c in maskcol)) {
			# The last column (a kernel stack): masked as a whole.
			if (i > ncols)
				continue
			v = "*"
		} else if (c == "STATE" && v ~ /^CPU[0-9]+$/)
			v = "CPU*"
		else if (c == "TT" && v ~ /^[0-9]+$/)
			# A pseudo-terminal's number: whichever was free.
			v = "<n>"
		else if ((c == "STAT" || c == "STATE") && v ~ /^I/)
			# Idle is asleep past 20 seconds (`MAXSLP`): timing.
			v = "S" substr(v, 2)
		else {
			inrow = 1
			v = text(v)
			inrow = 0
		}
		out = out (out == "" ? "" : " ") v
	}
	print out
}
