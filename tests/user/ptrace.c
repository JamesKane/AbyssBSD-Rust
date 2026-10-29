/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ptrace: checks process tracing from user mode: PT_TRACE_ME and the
 * stop it reports, system call entry and exit stops with their
 * arguments and results, PT_IO and PT_GETREGS on a stopped child, a
 * signal handed on with PT_CONTINUE, PT_ATTACH to a sleeping process
 * (which sleeps on) and PT_DETACH, which hands it back to its real
 * parent, and PT_KILL.
 *
 * Runs on FreeBSD too. Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ptrace.h>
#include <sys/procdesc.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("ptrace: line %d: %s: ", __LINE__, #cond);	\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* What the child exits with when something fails in it. */
#define	CHILD_FAIL	77

static volatile int got_usr1;
static int peek_me = 0x5a5a1234;

static void
on_usr1(int sig)
{
	(void)sig;
	got_usr1++;
}

/* Waits for `pid` and checks it stopped with `sig`. */
static void
stopped_with(pid_t pid, int sig, int line)
{
	int status;
	pid_t w;

	w = waitpid(pid, &status, 0);
	if (w != pid || !WIFSTOPPED(status) || WSTOPSIG(status) != sig) {
		printf("ptrace: line %d: waitpid %d: got %d status %#x, want a stop with %d (%s)\n",
		    line, pid, w, status, sig, strerror(errno));
		failed = 1;
	}
}

/* Waits for `pid` and checks it exited with `code`. */
static void
exited_with(pid_t pid, int code, int line)
{
	int status;
	pid_t w;

	w = waitpid(pid, &status, 0);
	if (w != pid || !WIFEXITED(status) || WEXITSTATUS(status) != code) {
		printf("ptrace: line %d: waitpid %d: got %d status %#x, want exit %d (%s)\n",
		    line, pid, w, status, code, strerror(errno));
		failed = 1;
	}
}

/* PT_TRACE_ME, syscall stops, PT_IO, PT_GETREGS, a signal handed on. */
static void
trace_me(void)
{
	struct ptrace_lwpinfo pl;
	struct ptrace_io_desc io;
	struct ptrace_sc_ret sr;
	struct reg r;
	register_t args[8];
	pid_t child;
	int word;

	child = fork();
	if (child == 0) {
		signal(SIGUSR1, on_usr1);
		if (ptrace(PT_TRACE_ME, 0, NULL, 0) != 0)
			_exit(CHILD_FAIL);
		raise(SIGSTOP);
		(void)getppid();
		_exit(got_usr1 == 1 ? 0 : CHILD_FAIL);
	}
	CHECK(child > 0, "fork: %s", strerror(errno));
	stopped_with(child, SIGSTOP, __LINE__);
	CHECK(ptrace(PT_LWPINFO, child, (caddr_t)&pl, sizeof(pl)) == 0,
	    "PT_LWPINFO: %s", strerror(errno));
	CHECK(pl.pl_event == PL_EVENT_SIGNAL && (pl.pl_flags & PL_FLAG_SI) != 0 &&
	    pl.pl_siginfo.si_signo == SIGSTOP, "event %d flags %#x signo %d",
	    pl.pl_event, pl.pl_flags, pl.pl_siginfo.si_signo);

	/* Its memory and registers while stopped. */
	word = 0;
	CHECK((word = ptrace(PT_READ_D, child, (caddr_t)&peek_me, 0)) == 0x5a5a1234,
	    "PT_READ_D: %#x (%s)", word, strerror(errno));
	io.piod_op = PIOD_READ_D;
	io.piod_offs = &peek_me;
	io.piod_addr = &word;
	io.piod_len = sizeof(word);
	word = 0;
	CHECK(ptrace(PT_IO, child, (caddr_t)&io, 0) == 0 && io.piod_len == sizeof(word) &&
	    word == 0x5a5a1234, "PT_IO: len %zu word %#x (%s)", io.piod_len, word,
	    strerror(errno));
	CHECK(ptrace(PT_GETREGS, child, (caddr_t)&r, 0) == 0, "PT_GETREGS: %s",
	    strerror(errno));

	/* getppid's entry and exit. */
	CHECK(ptrace(PT_TO_SCE, child, (caddr_t)1, 0) == 0, "PT_TO_SCE: %s",
	    strerror(errno));
	for (;;) {
		stopped_with(child, SIGTRAP, __LINE__);
		CHECK(ptrace(PT_LWPINFO, child, (caddr_t)&pl, sizeof(pl)) == 0,
		    "PT_LWPINFO: %s", strerror(errno));
		if (failed || pl.pl_syscall_code == SYS_getppid)
			break;
		CHECK(ptrace(PT_TO_SCE, child, (caddr_t)1, 0) == 0, "PT_TO_SCE: %s",
		    strerror(errno));
	}
	CHECK((pl.pl_flags & PL_FLAG_SCE) != 0 && pl.pl_syscall_narg == 0,
	    "flags %#x narg %u", pl.pl_flags, pl.pl_syscall_narg);
	CHECK(ptrace(PT_GET_SC_ARGS, child, (caddr_t)args, sizeof(args)) == 0,
	    "PT_GET_SC_ARGS: %s", strerror(errno));
	CHECK(ptrace(PT_TO_SCX, child, (caddr_t)1, 0) == 0, "PT_TO_SCX: %s",
	    strerror(errno));
	stopped_with(child, SIGTRAP, __LINE__);
	CHECK(ptrace(PT_LWPINFO, child, (caddr_t)&pl, sizeof(pl)) == 0 &&
	    (pl.pl_flags & PL_FLAG_SCX) != 0, "flags %#x", pl.pl_flags);
	CHECK(ptrace(PT_GET_SC_RET, child, (caddr_t)&sr, sizeof(sr)) == 0 &&
	    sr.sr_error == 0 && sr.sr_retval[0] == (register_t)getpid(),
	    "PT_GET_SC_RET: error %d retval %ld (%s)", sr.sr_error,
	    (long)sr.sr_retval[0], strerror(errno));

	/* Hand it SIGUSR1 and let it finish, events off. */
	{
		int events = PTRACE_EXEC;

		CHECK(ptrace(PT_SET_EVENT_MASK, child, (caddr_t)&events,
		    sizeof(events)) == 0, "PT_SET_EVENT_MASK: %s", strerror(errno));
	}
	CHECK(ptrace(PT_CONTINUE, child, (caddr_t)1, SIGUSR1) == 0,
	    "PT_CONTINUE: %s", strerror(errno));
	exited_with(child, 0, __LINE__);
}

/*
 * PT_ATTACH to a sleeping child of ours from a debugger child, and
 * PT_DETACH: it goes back to us (ptrace_test's proc_reparent, with a
 * process descriptor).
 */
static void
attach_detach(void)
{
	pid_t traced, debugger;
	int pd, status;

	traced = pdfork(&pd, 0);
	if (traced == 0) {
		raise(SIGSTOP);
		_exit(0);
	}
	CHECK(traced > 0, "pdfork: %s", strerror(errno));
	debugger = fork();
	if (debugger == 0) {
		int step = 0;

		if (ptrace(PT_ATTACH, traced, 0, 0) != 0)
			_exit(10 + step);
		step++;
		if (waitpid(traced, &status, 0) != traced)
			_exit(10 + step);
		step++;
		if (!WIFSTOPPED(status) || WSTOPSIG(status) != SIGSTOP)
			_exit(10 + step);
		step++;
		if (close(pd) != 0)
			_exit(10 + step);
		step++;
		if (ptrace(PT_DETACH, traced, (caddr_t)1, 0) != 0)
			_exit(10 + step);
		step++;
		if (wait(&status) != -1 || errno != ECHILD)
			_exit(10 + step);
		_exit(0);
	}
	CHECK(debugger > 0, "fork: %s", strerror(errno));
	exited_with(debugger, 0, __LINE__);
	/* Ours again, and still stopped by its own SIGSTOP. */
	CHECK(kill(traced, SIGCONT) == 0, "kill: %s", strerror(errno));
	CHECK(waitpid(traced, &status, 0) == traced && WIFEXITED(status),
	    "waitpid: status %#x (%s)", status, strerror(errno));
	close(pd);
}

/* PT_KILL ends a stopped child. */
static void
kill_child(void)
{
	pid_t child;
	int status;

	child = fork();
	if (child == 0) {
		ptrace(PT_TRACE_ME, 0, NULL, 0);
		raise(SIGSTOP);
		_exit(CHILD_FAIL);
	}
	stopped_with(child, SIGSTOP, __LINE__);
	CHECK(ptrace(PT_KILL, child, NULL, 0) == 0, "PT_KILL: %s", strerror(errno));
	CHECK(waitpid(child, &status, 0) == child && WIFSIGNALED(status) &&
	    WTERMSIG(status) == SIGKILL, "status %#x", status);
}

int
main(void)
{
	trace_me();
	attach_detach();
	kill_child();
	return (failed);
}
