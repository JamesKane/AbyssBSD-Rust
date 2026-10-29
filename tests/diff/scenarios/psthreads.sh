# ps(1) of a threaded process: one line per thread (-H), and the
# process's own line.
helper threads threads
waitfor $threads sigsusp
show ps -H -o pid,lwp,nlwp,tdname,stat,wchan,comm -p $threads
show ps -H -o pid,tid,tdnam,state,mwchan,command -p $threads
show ps -o pid,nlwp,stat,wchan,comm -p $threads
show ps -Hl -p $threads
