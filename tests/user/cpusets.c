/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * cpusets: a new numbered set (cpuset(2)) narrowed to CPU 0 at
 * CPU_LEVEL_CPUSET, as ptrace_test pins its children; a CPU that does not
 * exist refused with EINVAL; and a child setting its parent's affinity,
 * as ktrace_test's cap_cpuset does.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/cpuset.h>
#include <sys/wait.h>

#include <errno.h>
#include <stdio.h>
#include <unistd.h>

int
main(void)
{
	cpuset_t mask;
	cpusetid_t setid;
	pid_t pid;
	int status;

	if (cpuset(&setid) != 0) {
		printf("cpusets: cpuset: errno %d\n", errno);
		return (1);
	}
	CPU_ZERO(&mask);
	CPU_SET(0, &mask);
	if (cpuset_setaffinity(CPU_LEVEL_CPUSET, CPU_WHICH_CPUSET, setid,
	    sizeof(mask), &mask) != 0) {
		printf("cpusets: narrowing set %d: errno %d\n", setid, errno);
		return (1);
	}
	CPU_ZERO(&mask);
	CPU_SET(CPU_SETSIZE - 1, &mask);
	if (cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_PID, -1,
	    sizeof(mask), &mask) != -1 || errno != EINVAL) {
		printf("cpusets: an absent CPU: errno %d, not EINVAL\n", errno);
		return (1);
	}

	pid = fork();
	if (pid == -1) {
		printf("cpusets: fork: errno %d\n", errno);
		return (1);
	}
	if (pid == 0) {
		CPU_ZERO(&mask);
		CPU_SET(0, &mask);
		_exit(cpuset_setaffinity(CPU_LEVEL_WHICH, CPU_WHICH_PID,
		    getppid(), sizeof(mask), &mask) == 0 ? 0 : errno);
	}
	if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) ||
	    WEXITSTATUS(status) != 0) {
		printf("cpusets: the parent's affinity: status %#x\n", status);
		return (1);
	}
	return (0);
}
