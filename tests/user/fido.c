/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * fido: /dev/fido's requests as FreeBSD's watchdog(9) answers them, then
 * watchdogd(8) and watchdog(8) on it when the memory disk has them:
 * - the node is root:wheel 0600;
 * - WDIOC_SETTIMEOUT arms (a hardware watchdog, or the software one) and
 *   pats, 0 or WDIOC_CONTROL WD_CTRL_DISABLE disarms, WDIOC_GETTIMEOUT
 *   reads the timeout back, WDIOC_GETTIMELEFT the seconds since the pat;
 * - WDIOC_PATPAT is no request (ENOTTY);
 * - a pre-timeout must be under the timeout, and a shorter timeout drops
 *   it; WD_SOFT_* actions are checked; WDIOC_SETSOFT cannot switch while
 *   armed;
 * - FreeBSD 14's WDIOCPATPAT refuses WD_PASSIVE (ENOSYS) and an interval
 *   without a mode (EINVAL);
 * - watchdogd -t 30 -s 1 keeps the 30 s timeout patted and disarms it
 *   when it exits on SIGTERM; watchdog -t 60 arms, watchdog -t 0 disarms.
 *
 * Timeouts stay long (30 s and up), and everything is disarmed at the
 * end, so a slow run never trips the watchdog.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <sys/watchdog.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* FreeBSD 14's request, which COMPAT_FREEBSD14 keeps (watchdog.c). */
#define	WDIOCPATPAT_14	_IOW('W', 42, u_int)

static int failed;
static int fd;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("fido: line %d: ", __LINE__);			\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

/* An ioctl's errno, 0 for success. */
static int
req(unsigned long r, void *arg)
{
	return (ioctl(fd, r, arg) == 0 ? 0 : errno);
}

static int
set_timeout(sbintime_t sbt)
{
	return (req(WDIOC_SETTIMEOUT, &sbt));
}

static sbintime_t
get(unsigned long r)
{
	sbintime_t v = -1;
	int e;

	e = req(r, &v);
	CHECK(e == 0, "request %#lx: errno %d", r, e);
	return (v);
}

static void
control(int ctrl)
{
	int e = req(WDIOC_CONTROL, &ctrl);

	CHECK(e == 0, "WDIOC_CONTROL %d: errno %d", ctrl, e);
}

/* Runs `argv` to its end; its exit status, or -1. */
static int
run(char *const argv[])
{
	pid_t pid;
	int st;

	pid = fork();
	if (pid == 0) {
		execv(argv[0], argv);
		_exit(127);
	}
	if (pid < 0 || waitpid(pid, &st, 0) != pid || !WIFEXITED(st))
		return (-1);
	return (WEXITSTATUS(st));
}

static void
daemon_and_command(void)
{
	char *wd[] = { "/usr/sbin/watchdogd", "-d", "-S", "-t", "30", "-s",
	    "1", "-I", "/tmp/fido-watchdogd.pid", NULL };
	char *on[] = { "/usr/sbin/watchdog", "-t", "60", NULL };
	char *off[] = { "/usr/sbin/watchdog", "-t", "0", NULL };
	sbintime_t left;
	pid_t pid;
	int st;

	if (access(wd[0], X_OK) != 0)
		return;
	pid = fork();
	if (pid == 0) {
		/* Its -d chatter is not ours. */
		freopen("/dev/null", "w", stdout);
		execv(wd[0], wd);
		_exit(127);
	}
	CHECK(pid > 0, "fork: %s", strerror(errno));
	if (pid <= 0)
		return;
	sleep(3);
	CHECK(get(WDIOC_GETTIMEOUT) == 30 * SBT_1S,
	    "watchdogd -t 30: timeout %jd", (intmax_t)get(WDIOC_GETTIMEOUT));
	left = get(WDIOC_GETTIMELEFT);
	CHECK(left >= 0 && left <= 2 * SBT_1S,
	    "watchdogd -s 1: %jd since the last pat", (intmax_t)left);
	kill(pid, SIGTERM);
	CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) &&
	    WEXITSTATUS(st) == 0, "watchdogd on SIGTERM: status %#x", st);
	CHECK(get(WDIOC_GETTIMEOUT) == 0, "watchdogd left %jd armed",
	    (intmax_t)get(WDIOC_GETTIMEOUT));

	CHECK(run(on) == 0, "watchdog -t 60 failed");
	CHECK(get(WDIOC_GETTIMEOUT) == 60 * SBT_1S, "watchdog -t 60: %jd",
	    (intmax_t)get(WDIOC_GETTIMEOUT));
	CHECK(run(off) == 0, "watchdog -t 0 failed");
	CHECK(get(WDIOC_GETTIMEOUT) == 0, "watchdog -t 0: %jd",
	    (intmax_t)get(WDIOC_GETTIMEOUT));
}

int
main(void)
{
	struct stat sb;
	sbintime_t sbt;
	u_int u;
	int i, e;

	if (stat("/dev/" _PATH_WATCHDOG, &sb) != 0) {
		printf("fido: /dev/fido: %s\n", strerror(errno));
		return (1);
	}
	CHECK(S_ISCHR(sb.st_mode) && (sb.st_mode & 07777) == 0600 &&
	    sb.st_uid == 0 && sb.st_gid == 0, "/dev/fido is %o %d:%d",
	    sb.st_mode, sb.st_uid, sb.st_gid);
	fd = open("/dev/" _PATH_WATCHDOG, O_RDWR);
	if (fd < 0) {
		printf("fido: open: %s\n", strerror(errno));
		return (1);
	}

	/*
	 * As watchdogd starts: the hardware, not the soft timer, then the
	 * timeout. No disarming first: the first pat no hardware watchdog
	 * takes is what attaches the software one, and a disarm would be
	 * that first pat.
	 */
	i = 0;
	CHECK(req(WDIOC_SETSOFT, &i) == 0, "WDIOC_SETSOFT 0");
	sbt = SBT_1S;
	e = req(WDIOC_PATPAT, &sbt);
	CHECK(e == ENOTTY, "WDIOC_PATPAT: errno %d, not ENOTTY", e);

	e = set_timeout(128 * SBT_1S);
	CHECK(e == 0, "WDIOC_SETTIMEOUT 128 s: errno %d", e);
	CHECK(get(WDIOC_GETTIMEOUT) == 128 * SBT_1S, "timeout %jd",
	    (intmax_t)get(WDIOC_GETTIMEOUT));
	sbt = get(WDIOC_GETTIMELEFT);
	CHECK(sbt == 0 || sbt == SBT_1S, "%jd since the pat", (intmax_t)sbt);
	i = 1;
	e = req(WDIOC_SETSOFT, &i);
	CHECK(e == EINVAL, "WDIOC_SETSOFT while armed: errno %d", e);

	i = 0x10;
	CHECK(req(WDIOC_SETPRETIMEOUTACT, &i) == EINVAL, "bad pre action");
	CHECK(req(WDIOC_SETSOFTTIMEOUTACT, &i) == EINVAL, "bad soft action");
	i = WD_SOFT_LOG;
	CHECK(req(WDIOC_SETPRETIMEOUTACT, &i) == 0, "WD_SOFT_LOG");
	CHECK(req(WDIOC_SETSOFTTIMEOUTACT, &i) == 0, "WD_SOFT_LOG (soft)");

	sbt = 200 * SBT_1S;
	e = req(WDIOC_SETPRETIMEOUT, &sbt);
	CHECK(e == EINVAL, "a pre-timeout past the timeout: errno %d", e);
	sbt = 100 * SBT_1S;
	CHECK(req(WDIOC_SETPRETIMEOUT, &sbt) == 0, "pre-timeout 100 s");
	CHECK(get(WDIOC_GETPRETIMEOUT) == 100 * SBT_1S, "pre-timeout %jd",
	    (intmax_t)get(WDIOC_GETPRETIMEOUT));
	CHECK(set_timeout(64 * SBT_1S) == 0, "WDIOC_SETTIMEOUT 64 s");
	CHECK(get(WDIOC_GETPRETIMEOUT) == 0,
	    "a shorter timeout kept the pre-timeout (%jd)",
	    (intmax_t)get(WDIOC_GETPRETIMEOUT));

	u = WD_PASSIVE | WD_TO_8SEC;
	e = req(WDIOCPATPAT_14, &u);
	CHECK(e == ENOSYS, "WD_PASSIVE: errno %d, not ENOSYS", e);
	u = WD_TO_8SEC;
	e = req(WDIOCPATPAT_14, &u);
	CHECK(e == EINVAL, "an interval without a mode: errno %d", e);
	u = WD_ACTIVE | WD_LASTVAL | WD_TO_8SEC;
	e = req(WDIOCPATPAT_14, &u);
	CHECK(e == EINVAL, "WD_LASTVAL with an interval: errno %d", e);
	u = WD_ACTIVE | WD_LASTVAL;
	CHECK(req(WDIOCPATPAT_14, &u) == 0, "WD_LASTVAL");
	CHECK(get(WDIOC_GETTIMEOUT) == 64 * SBT_1S, "WD_LASTVAL changed it");
	u = WD_ACTIVE | WD_TO_NEVER;
	CHECK(req(WDIOCPATPAT_14, &u) == 0, "WD_TO_NEVER");
	CHECK(get(WDIOC_GETTIMEOUT) == 0, "WD_TO_NEVER left %jd",
	    (intmax_t)get(WDIOC_GETTIMEOUT));

	sbt = SBT_1S;
	e = req(WDIOC_SETPRETIMEOUT, &sbt);
	CHECK(e == EINVAL, "a pre-timeout with no timeout: errno %d", e);

	CHECK(set_timeout(64 * SBT_1S) == 0, "WDIOC_SETTIMEOUT 64 s");
	CHECK(set_timeout(0) == 0, "WDIOC_SETTIMEOUT 0");
	CHECK(get(WDIOC_GETTIMEOUT) == 0, "0 left it armed");
	control(WD_CTRL_ENABLE);
	CHECK(get(WDIOC_GETTIMEOUT) == 0, "WD_CTRL_ENABLE after a disarm");

	daemon_and_command();

	control(WD_CTRL_DISABLE);
	close(fd);
	return (failed);
}
