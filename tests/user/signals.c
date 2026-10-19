/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * signals: checks signal delivery from user mode, as FreeBSD behaves:
 * handlers and their arguments, blocking, SA_RESETHAND and SA_NODEFER,
 * the alternate stack, faults, default actions seen through wait4, stop
 * and continue, interrupted and restarted system calls, sigsuspend, the
 * sigwait family, sigqueue, user contexts, thr_kill, and SIGSYS.
 *
 * Exits 0 when every check holds; prints each one that fails.
 */

#include <sys/types.h>
#include <sys/syscall.h>
#include <sys/thr.h>
#include <sys/wait.h>

#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

static int failures;

#define	CHECK(cond) do {						\
	if (!(cond)) {							\
		printf("signals: line %d: %s\n", __LINE__, #cond);	\
		failures++;						\
	}								\
} while (0)

static volatile sig_atomic_t hits;
static volatile int last_sig, last_code, last_pid;
static volatile intptr_t last_value;
static volatile void *last_addr;
static volatile uintptr_t handler_sp;
static sigjmp_buf jump;

static void
on_info(int sig, siginfo_t *si, void *uc)
{
	(void)uc;
	hits++;
	last_sig = sig;
	last_code = si->si_code;
	last_pid = si->si_pid;
	last_value = (intptr_t)si->si_value.sival_ptr;
	last_addr = si->si_addr;
	handler_sp = (uintptr_t)&sig;
}

static void
on_plain(int sig)
{
	hits++;
	last_sig = sig;
}

static void
on_fault(int sig, siginfo_t *si, void *uc)
{
	(void)uc;
	last_sig = sig;
	last_code = si->si_code;
	last_addr = si->si_addr;
	siglongjmp(jump, 1);
}

/* While inside a handler, is the signal blocked? */
static volatile int blocked_inside;

static void
on_check_mask(int sig)
{
	sigset_t cur;

	sigprocmask(SIG_BLOCK, NULL, &cur);
	blocked_inside = sigismember(&cur, sig);
	hits++;
}

static void
catch(int sig, void (*h)(int), int flags)
{
	struct sigaction sa;

	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = h;
	sa.sa_flags = flags;
	sigemptyset(&sa.sa_mask);
	sigaction(sig, &sa, NULL);
}

static void
catch_info(int sig, void (*h)(int, siginfo_t *, void *), int flags)
{
	struct sigaction sa;

	memset(&sa, 0, sizeof(sa));
	sa.sa_sigaction = h;
	sa.sa_flags = flags | SA_SIGINFO;
	sigemptyset(&sa.sa_mask);
	sigaction(sig, &sa, NULL);
}

static void
handlers(void)
{
	sigset_t set, pend;

	/* A handler with siginfo, for a signal sent to ourselves. */
	catch_info(SIGUSR1, on_info, 0);
	hits = 0;
	kill(getpid(), SIGUSR1);
	CHECK(hits == 1);
	CHECK(last_sig == SIGUSR1);
	CHECK(last_code == SI_USER);
	CHECK(last_pid == getpid());

	/* Blocked: pending until unblocked, then delivered at once. */
	sigemptyset(&set);
	sigaddset(&set, SIGUSR1);
	sigprocmask(SIG_BLOCK, &set, NULL);
	hits = 0;
	kill(getpid(), SIGUSR1);
	CHECK(hits == 0);
	sigpending(&pend);
	CHECK(sigismember(&pend, SIGUSR1));
	sigprocmask(SIG_UNBLOCK, &set, NULL);
	CHECK(hits == 1);
	sigpending(&pend);
	CHECK(!sigismember(&pend, SIGUSR1));

	/* The signal is blocked while its handler runs, unless SA_NODEFER. */
	catch(SIGUSR2, on_check_mask, 0);
	raise(SIGUSR2);
	CHECK(blocked_inside == 1);
	catch(SIGUSR2, on_check_mask, SA_NODEFER);
	raise(SIGUSR2);
	CHECK(blocked_inside == 0);

	/* SA_RESETHAND: the handler runs once, then the default is back. */
	catch(SIGURG, on_plain, SA_RESETHAND);
	hits = 0;
	raise(SIGURG);
	CHECK(hits == 1);
	{
		struct sigaction old;

		sigaction(SIGURG, NULL, &old);
		CHECK(old.sa_handler == SIG_DFL);
	}
	/* SIGURG's default is to ignore it. */
	raise(SIGURG);
	CHECK(hits == 1);

	/* Ignored signals are dropped, even while blocked. */
	signal(SIGUSR1, SIG_IGN);
	sigprocmask(SIG_BLOCK, &set, NULL);
	kill(getpid(), SIGUSR1);
	sigpending(&pend);
	CHECK(!sigismember(&pend, SIGUSR1));
	sigprocmask(SIG_UNBLOCK, &set, NULL);
	signal(SIGUSR1, SIG_DFL);
}

static void
altstack(void)
{
	static char stack[65536];
	stack_t ss, old;

	ss.ss_sp = stack;
	ss.ss_size = sizeof(stack);
	ss.ss_flags = 0;
	CHECK(sigaltstack(&ss, NULL) == 0);
	catch_info(SIGUSR1, on_info, SA_ONSTACK);
	raise(SIGUSR1);
	CHECK(handler_sp >= (uintptr_t)stack &&
	    handler_sp < (uintptr_t)stack + sizeof(stack));
	/* Without SA_ONSTACK, the normal stack. */
	catch_info(SIGUSR1, on_info, 0);
	raise(SIGUSR1);
	CHECK(handler_sp < (uintptr_t)stack ||
	    handler_sp >= (uintptr_t)stack + sizeof(stack));
	ss.ss_flags = SS_DISABLE;
	CHECK(sigaltstack(&ss, &old) == 0);
	CHECK(old.ss_sp == stack && old.ss_flags == 0);
	ss.ss_size = 16;
	ss.ss_flags = 0;
	CHECK(sigaltstack(&ss, NULL) == -1 && errno == ENOMEM);
	signal(SIGUSR1, SIG_DFL);
}

static void
faults(void)
{
	volatile int *bad = (volatile int *)(uintptr_t)16;

	catch_info(SIGSEGV, on_fault, 0);
	if (sigsetjmp(jump, 1) == 0) {
		(void)*bad;
		CHECK(!"no fault");
	}
	CHECK(last_sig == SIGSEGV);
	CHECK(last_code == SEGV_MAPERR);
	CHECK(last_addr == (void *)bad);
	signal(SIGSEGV, SIG_DFL);
}

static void
on_chld(int sig)
{
	(void)sig;
	hits++;
}

static void
children(void)
{
	pid_t pid;
	int st;

	/* A default action kills, and the parent hears of it. */
	catch(SIGCHLD, on_chld, 0);
	hits = 0;
	pid = fork();
	if (pid == 0) {
		kill(getpid(), SIGTERM);
		_exit(99);
	}
	CHECK(waitpid(pid, &st, 0) == pid);
	CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
	CHECK(hits >= 1);

	/* Stop, continue, kill, each reported to waitpid. */
	pid = fork();
	if (pid == 0) {
		for (;;)
			usleep(1000);
	}
	kill(pid, SIGSTOP);
	CHECK(waitpid(pid, &st, WUNTRACED) == pid);
	CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP);
	/* Reported once. */
	CHECK(waitpid(pid, &st, WUNTRACED | WNOHANG) == 0);
	kill(pid, SIGCONT);
	CHECK(waitpid(pid, &st, WCONTINUED) == pid);
	CHECK(WIFCONTINUED(st));
	kill(pid, SIGKILL);
	CHECK(waitpid(pid, &st, 0) == pid);
	CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);

	/* SIGKILL ends a stop too. */
	pid = fork();
	if (pid == 0) {
		for (;;)
			usleep(1000);
	}
	kill(pid, SIGSTOP);
	CHECK(waitpid(pid, &st, WUNTRACED) == pid);
	kill(pid, SIGKILL);
	CHECK(waitpid(pid, &st, 0) == pid);
	CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
	signal(SIGCHLD, SIG_DFL);
}

static void
interrupted(void)
{
	struct timespec ts = { 5, 0 }, left;
	pid_t pid, parent = getpid();
	int st;

	/* nanosleep ends with EINTR and the time left. */
	catch(SIGUSR1, on_plain, 0);
	pid = fork();
	if (pid == 0) {
		usleep(50000);
		kill(parent, SIGUSR1);
		_exit(0);
	}
	errno = 0;
	CHECK(nanosleep(&ts, &left) == -1 && errno == EINTR);
	CHECK(left.tv_sec < 5 && left.tv_sec >= 3);
	waitpid(pid, &st, 0);

	/* With SA_RESTART, waitpid goes on after the handler. */
	catch(SIGUSR2, on_plain, SA_RESTART);
	hits = 0;
	pid = fork();
	if (pid == 0) {
		/* Once the parent is waiting. */
		usleep(50000);
		kill(parent, SIGUSR2);
		usleep(50000);
		_exit(7);
	}
	CHECK(waitpid(pid, &st, 0) == pid);
	CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 7);
	CHECK(hits == 1);

	/* Without it, EINTR. */
	catch(SIGUSR2, on_plain, 0);
	pid = fork();
	if (pid == 0) {
		usleep(50000);
		kill(parent, SIGUSR2);
		usleep(50000);
		_exit(7);
	}
	errno = 0;
	CHECK(waitpid(pid, &st, 0) == -1 && errno == EINTR);
	CHECK(waitpid(pid, &st, 0) == pid);

	/* sigsuspend waits for a signal, with a temporary mask. */
	{
		sigset_t block, empty, cur;

		sigemptyset(&block);
		sigaddset(&block, SIGUSR1);
		sigprocmask(SIG_BLOCK, &block, NULL);
		hits = 0;
		pid = fork();
		if (pid == 0) {
			kill(parent, SIGUSR1);
			_exit(0);
		}
		sigemptyset(&empty);
		CHECK(sigsuspend(&empty) == -1 && errno == EINTR);
		CHECK(hits == 1);
		sigprocmask(SIG_BLOCK, NULL, &cur);
		CHECK(sigismember(&cur, SIGUSR1));
		sigprocmask(SIG_UNBLOCK, &block, NULL);
		waitpid(pid, &st, 0);
	}
	signal(SIGUSR1, SIG_DFL);
	signal(SIGUSR2, SIG_DFL);
}

static void
waiting(void)
{
	sigset_t set;
	siginfo_t si;
	struct timespec zero = { 0, 10000000 };
	union sigval v;
	int sig;

	sigemptyset(&set);
	sigaddset(&set, SIGUSR1);
	sigaddset(&set, SIGRTMIN);
	sigprocmask(SIG_BLOCK, &set, NULL);

	kill(getpid(), SIGUSR1);
	CHECK(sigwait(&set, &sig) == 0 && sig == SIGUSR1);

	errno = 0;
	CHECK(sigtimedwait(&set, &si, &zero) == -1 && errno == EAGAIN);

	/* Real-time signals queue, each with its value. */
	v.sival_ptr = (void *)0x1234;
	CHECK(sigqueue(getpid(), SIGRTMIN, v) == 0);
	v.sival_ptr = (void *)0x5678;
	CHECK(sigqueue(getpid(), SIGRTMIN, v) == 0);
	CHECK(sigwaitinfo(&set, &si) == SIGRTMIN);
	CHECK(si.si_code == SI_QUEUE && si.si_value.sival_ptr == (void *)0x1234);
	CHECK(sigwaitinfo(&set, &si) == SIGRTMIN);
	CHECK(si.si_value.sival_ptr == (void *)0x5678);

	sigprocmask(SIG_UNBLOCK, &set, NULL);
}

static ucontext_t main_uc, other_uc;
static volatile int other_ran;

static void
other(void)
{
	other_ran++;
	swapcontext(&other_uc, &main_uc);
}

static void
contexts(void)
{
	static char stack[65536];
	ucontext_t uc;
	volatile int loops = 0;

	/* setcontext returns to getcontext's return, with 0 again. */
	CHECK(getcontext(&uc) == 0);
	if (++loops < 3)
		setcontext(&uc);
	CHECK(loops == 3);

	/* swapcontext to a function on its own stack and back. */
	CHECK(getcontext(&other_uc) == 0);
	other_uc.uc_stack.ss_sp = stack;
	other_uc.uc_stack.ss_size = sizeof(stack);
	other_uc.uc_link = &main_uc;
	makecontext(&other_uc, other, 0);
	CHECK(swapcontext(&main_uc, &other_uc) == 0);
	CHECK(other_ran == 1);
}

static void
misc(void)
{
	/* thr_kill to our own thread. */
	{
		long tid;

		catch(SIGUSR1, on_plain, 0);
		hits = 0;
		CHECK(thr_self(&tid) == 0);
		CHECK(thr_kill(tid, SIGUSR1) == 0);
		CHECK(hits == 1);
		CHECK(thr_kill(tid + 1, SIGUSR1) == -1 && errno == ESRCH);
		signal(SIGUSR1, SIG_DFL);
	}

	/* A number that is no system call: SIGSYS, then ENOSYS. */
	catch(SIGSYS, on_plain, 0);
	hits = 0;
	errno = 0;
	CHECK(syscall(5000) == -1 && errno == ENOSYS);
	CHECK(hits == 1 && last_sig == SIGSYS);
	signal(SIGSYS, SIG_DFL);

	/* Bad arguments. */
	CHECK(kill(getpid(), 1000) == -1 && errno == EINVAL);
	CHECK(kill(99998, 0) == -1 && errno == ESRCH);
	CHECK(signal(SIGKILL, SIG_IGN) == SIG_ERR);
}

#define	RUN(f) do {							\
	printf("signals: %s\n", #f);					\
	f();								\
} while (0)

int
main(void)
{
	/* The console is not a tty to stdio yet (no fstat): no buffering. */
	setvbuf(stdout, NULL, _IONBF, 0);
	RUN(handlers);
	RUN(altstack);
	RUN(faults);
	RUN(children);
	RUN(interrupted);
	RUN(waiting);
	RUN(contexts);
	RUN(misc);
	if (failures != 0) {
		printf("signals: %d failed\n", failures);
		return (1);
	}
	printf("signals: ok\n");
	return (0);
}
