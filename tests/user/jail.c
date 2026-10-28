/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * jail: checks jails from user mode (as root). jail_set(2) creates a jail
 * that must persist or be entered, names it, refuses a second of the same
 * name and a parameter it does not know (saying so in "errmsg"), and
 * jail_get(2) reads its parameters back. A process attached to it is
 * rooted and working at its path, sees itself as jailed, sees only the
 * jail's processes, has a hostname of the jail's own, may not lower its
 * securelevel or make jails below it, and keeps its jail alive; once it
 * exits, a jail that does not persist is gone. jail_remove(2) ends one
 * that does, and the processes in it.
 *
 * Links with: -ljail
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/jail.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/wait.h>

#include <errno.h>
#include <stdarg.h>
#include <jail.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failed;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("jail: line %d: %s: ", __LINE__, #cond);		\
		printf(__VA_ARGS__);					\
		printf("\n");						\
		failed = 1;						\
	}								\
} while (0)

static char dir[] = "/tmp/jail.XXXXXX";

/* jail_set(2) with `n` name-value pairs, the last an "errmsg" buffer. */
static int
set(int flags, char *errmsg, size_t len, int n, ...)
{
	struct iovec iov[32];
	va_list ap;
	int i;

	va_start(ap, n);
	for (i = 0; i < 2 * n; i += 2) {
		iov[i].iov_base = va_arg(ap, char *);
		iov[i].iov_len = strlen(iov[i].iov_base) + 1;
		iov[i + 1].iov_base = va_arg(ap, void *);
		iov[i + 1].iov_len = va_arg(ap, size_t);
	}
	va_end(ap);
	iov[i].iov_base = __DECONST(char *, "errmsg");
	iov[i].iov_len = sizeof("errmsg");
	iov[i + 1].iov_base = errmsg;
	iov[i + 1].iov_len = len;
	errmsg[0] = '\0';
	return (jail_set(iov, i + 2, flags));
}

/* An integer sysctl. */
static int
intsysctl(const char *name)
{
	int v = -100;
	size_t len = sizeof(v);

	if (sysctlbyname(name, &v, &len, NULL, 0) != 0)
		return (-100);
	return (v);
}

/* How many processes kern.proc.proc lists (its size alone overestimates). */
static int
nprocs(void)
{
	int mib[3] = { CTL_KERN, KERN_PROC, KERN_PROC_PROC };
	struct kinfo_proc *kp;
	size_t len = 0;
	int n;

	if (sysctl(mib, 3, NULL, &len, NULL, 0) != 0 ||
	    (kp = malloc(len)) == NULL)
		return (-1);
	n = sysctl(mib, 3, kp, &len, NULL, 0) == 0 ?
	    (int)(len / sizeof(*kp)) : -1;
	free(kp);
	return (n);
}

/* In the jail, as `jid`: what it may and may not do. */
static void
inside(int jid)
{
	char buf[MAXPATHLEN], err[256];
	int level;

	CHECK(intsysctl("security.jail.jailed") == 1, "not jailed");
	CHECK(getcwd(buf, sizeof(buf)) != NULL && strcmp(buf, "/") == 0,
	    "cwd %s", buf);
	CHECK(access("/inside", F_OK) == 0, "the root is the jail's path");
	CHECK(gethostname(buf, sizeof(buf)) == 0 &&
	    strcmp(buf, "inside.test") == 0, "hostname %s", buf);
	CHECK(sethostname("renamed", 7) == 0, "%s", strerror(errno));
	CHECK(gethostname(buf, sizeof(buf)) == 0 &&
	    strcmp(buf, "renamed") == 0, "hostname %s", buf);
	/* init and its parent are out of sight: it is the jail's only one. */
	CHECK(kill(1, 0) == -1 && errno == ESRCH, "%s", strerror(errno));
	CHECK(kill(getppid(), 0) == -1 && errno == ESRCH, "%s",
	    strerror(errno));
	CHECK(nprocs() == 1, "%d processes", nprocs());
	/* Its securelevel only rises. */
	level = intsysctl("kern.securelevel");
	CHECK(level == 1, "securelevel %d", level);
	level = 0;
	CHECK(sysctlbyname("kern.securelevel", NULL, NULL, &level,
	    sizeof(level)) == -1 && errno == EPERM, "%s", strerror(errno));
	/* children.max is 0: no jails below. */
	CHECK(set(JAIL_CREATE, err, sizeof(err), 1, "persist", NULL,
	    (size_t)0) == -1 && errno == EPERM, "%s", strerror(errno));
	/* Not even its own jail is a child to remove or attach to. */
	CHECK(jail_remove(jid) == -1 && errno == EINVAL, "%s",
	    strerror(errno));
}

int
main(void)
{
	char name[64], err[256], buf[MAXPATHLEN], path[MAXPATHLEN];
	char hostname[MAXHOSTNAMELEN];
	int jid, jid2, one = 1, level = 1, status;
	pid_t pid;

	if (mkdtemp(dir) == NULL) {
		printf("jail: mkdtemp: %s\n", strerror(errno));
		return (1);
	}
	snprintf(path, sizeof(path), "%s/inside", dir);
	if (mkdir(path, 0755) != 0) {
		printf("jail: mkdir: %s\n", strerror(errno));
		return (1);
	}
	snprintf(name, sizeof(name), "abyss-%d", (int)getpid());
	(void)gethostname(hostname, sizeof(hostname));

	/* A new jail must persist or be entered. */
	CHECK(set(JAIL_CREATE, err, sizeof(err), 2, "name", name,
	    strlen(name) + 1, "path", dir, strlen(dir) + 1) == -1 &&
	    errno == EINVAL && strcmp(err, "new jail must persist or attach") == 0,
	    "%s: %s", strerror(errno), err);
	jid = set(JAIL_CREATE, err, sizeof(err), 5, "name", name,
	    strlen(name) + 1, "path", dir, strlen(dir) + 1, "persist", NULL,
	    (size_t)0, "host.hostname", "inside.test", sizeof("inside.test"),
	    "securelevel", &level, sizeof(level));
	CHECK(jid > 0, "%s: %s", strerror(errno), err);
	if (jid <= 0)
		return (1);
	/* Its name is its own. */
	CHECK(set(JAIL_CREATE, err, sizeof(err), 2, "name", name,
	    strlen(name) + 1, "persist", NULL, (size_t)0) == -1 &&
	    errno == EEXIST, "%s: %s", strerror(errno), err);
	CHECK(set(JAIL_UPDATE, err, sizeof(err), 2, "jid", &jid, sizeof(jid),
	    "bogus", &one, sizeof(one)) == -1 && errno == EINVAL &&
	    strcmp(err, "unknown parameter: bogus") == 0, "%s: %s",
	    strerror(errno), err);
	CHECK(set(JAIL_UPDATE, err, sizeof(err), 2, "jid", &jid, sizeof(jid),
	    "path", "/", sizeof("/")) == -1 && errno == EINVAL &&
	    strcmp(err, "path cannot be changed after creation") == 0, "%s: %s",
	    strerror(errno), err);
	/* What libjail reads back. */
	CHECK(jail_getid(name) == jid, "jail_getid %d", jail_getid(name));
	CHECK(jail_getv(0, "jid", (snprintf(buf, sizeof(buf), "%d", jid),
	    buf), "path", path, NULL) == jid && strcmp(path, dir) == 0,
	    "%s: %s", jail_errmsg, path);
	CHECK(jail_getv(0, "name", name, "host.hostname", buf, NULL) == jid &&
	    strcmp(buf, "inside.test") == 0, "%s: %s", jail_errmsg, buf);

	/* Inside it. */
	fflush(stdout);
	pid = fork();
	if (pid == 0) {
		failed = 0;
		if (jail_attach(jid) != 0) {
			printf("jail: jail_attach: %s\n", strerror(errno));
			_exit(1);
		}
		inside(jid);
		fflush(stdout);
		_exit(failed);
	}
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0, "child status %#x", status);
	/* The jail's hostname is its own; the host's stays. */
	CHECK(gethostname(buf, sizeof(buf)) == 0 &&
	    strcmp(buf, hostname) == 0, "host's hostname %s", buf);
	CHECK(jail_getv(0, "name", name, "host.hostname", buf, NULL) == jid &&
	    strcmp(buf, "renamed") == 0, "%s: %s", jail_errmsg, buf);

	/* jail_remove ends it, and what runs in it. */
	fflush(stdout);
	pid = fork();
	if (pid == 0) {
		if (jail_attach(jid) != 0)
			_exit(1);
		for (;;)
			pause();
	}
	usleep(100000);
	CHECK(jail_remove(jid) == 0, "%s", strerror(errno));
	CHECK(waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) &&
	    WTERMSIG(status) == SIGKILL, "child status %#x", status);
	CHECK(jail_getid(name) == -1, "still there");

	/* One that only its process keeps goes with it. */
	snprintf(name, sizeof(name), "abyss-%d-b", (int)getpid());
	fflush(stdout);
	pid = fork();
	if (pid == 0) {
		jid2 = set(JAIL_CREATE | JAIL_ATTACH, err, sizeof(err), 2,
		    "name", name, strlen(name) + 1, "path", dir,
		    strlen(dir) + 1);
		_exit(jid2 > 0 ? 0 : 1);
	}
	CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
	    WEXITSTATUS(status) == 0, "child status %#x", status);
	CHECK(jail_getid(name) == -1, "a jail with nothing in it lives on");

	(void)rmdir(path);
	(void)rmdir(dir);
	if (failed)
		return (1);
	printf("jail: ok\n");
	return (0);
}
