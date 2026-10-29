# ps(1): processes in known states, in several formats.
spawn sleeper sleep 77777
spawn stopped sleep 77778
helper waiter wait
helper zombie zombie
helper piperd piperd
helper selector select
kill -STOP $stopped
waitfor $sleeper nanslp
waitfor $stopped x
P=$sleeper,$stopped,$waiter,$zombie,$piperd,$selector
show ps -o pid,ppid,pgid,sid,tsid,tpgid,jobc,tt,stat,state,wchan,mwchan,nlwp,comm,ucomm,args -p $P
show ps -o pid,uid,ruid,svuid,gid,rgid,svgid,user,ruser,group,rgroup,login,jid,class,rtprio,nice,f,f2,emul,xstat -p $P
show ps -o pid,pending,blocked,ignored,caught,nsigs,inblk,oublk,majflt,nswap,cow,tracer,ktrace,lockname -p $P
show ps -l -p $P
show ps -j -p $P
sorted 2 "^USER" ps -u -p $P
sorted 1 "^ *PID" ps -v -p $P
# Every process: only the scenario's are kept.
ps -auxww | awk 'NR == 1 || $11 == "diffproc" || $11 == "sleep"' > auxww.out
sorted 2 "^USER" cat auxww.out
show ps -O wchan,nlwp -p $P
show ps -c -p $P
show ps -p $sleeper -o command=
