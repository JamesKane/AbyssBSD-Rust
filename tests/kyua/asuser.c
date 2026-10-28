/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * asuser: runs a program as another user, as kyua runs a test case that
 * requires an unprivileged one (its `unprivileged_user`): "asuser user
 * program args...", or "asuser - program args..." to stay who it is,
 * with the groups a login session would give it (kyua runs from one).
 * Either way the program starts with SIGINT and SIGQUIT at their defaults,
 * as kyua's do (sh ignores them in the background jobs that run-atf's
 * cases are). run-atf's helper, built static.
 */

#include <sys/types.h>

#include <err.h>
#include <grp.h>
#include <pwd.h>
#include <signal.h>
#include <string.h>
#include <unistd.h>

int
main(int argc, char **argv)
{
	struct passwd *pw;

	if (argc < 3)
		errx(2, "usage: asuser user|- program [args...]");
	if (strcmp(argv[1], "-") != 0) {
		if ((pw = getpwnam(argv[1])) == NULL)
			errx(1, "no user %s", argv[1]);
		if (setgroups(1, &pw->pw_gid) != 0 ||
		    setgid(pw->pw_gid) != 0 || setuid(pw->pw_uid) != 0)
			err(1, "becoming %s", argv[1]);
	} else if ((pw = getpwuid(getuid())) != NULL &&
	    initgroups(pw->pw_name, pw->pw_gid) != 0)
		err(1, "initgroups %s", pw->pw_name);
	signal(SIGINT, SIG_DFL);
	signal(SIGQUIT, SIG_DFL);
	execv(argv[2], argv + 2);
	err(127, "%s", argv[2]);
}
