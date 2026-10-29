# procstat(1): every mode, on processes in known states.
helper files files
helper threads threads
helper kq kqueue
spawn sleeper sleep 77777
waitfor $sleeper nanslp
waitfor $threads sigsusp
P="$files $threads $sleeper"
show procstat $P
show procstat -b $P
show procstat -c $P
show procstat -e $P
show procstat -f $files
show procstat -fC $files
show procstat -i $sleeper
show procstat -j $threads
show procstat -k $sleeper $threads
show procstat -kk $sleeper
show procstat -l $sleeper
show procstat -r $sleeper
show procstat -s $P
show procstat -S $threads
show procstat -t $threads
show procstat -x $sleeper
show procstat kqueue $kq
# The daemons' pid files are locked too; only the scenario's are kept.
procstat advlock | grep -v /var/run/ > advlock.out
show cat advlock.out
show procstat pwdx $files
show procstat pargs $sleeper
show procstat penv $sleeper
show procstat sigfastblock $threads
show procstat rlimitusage $sleeper
show procstat binary $sleeper
# -a: every process; only the scenario's are shown.
procstat -ab | awk 'NR == 1 || $2 == "diffproc" || $2 == "sleep"' > ab.out
show cat ab.out
