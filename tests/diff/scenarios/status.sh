# The terminal status line (^T, VSTATUS, and TIOCSTAT): the load average,
# the foreground process, its wait channel, times, CPU share, and size.
helper status status
wait $status
echo "(exit $?)"
