# top(1) in batch mode: one display, of the scenario's processes only.
spawn sleeper sleep 77777
helper threads threads
helper piperd piperd
waitfor $sleeper nanslp
show top -b -d 1 -p $sleeper
sorted 0 "^ *PID" top -b -d 1 -H -p $threads
show top -b -d 1 -o pid -p $piperd
