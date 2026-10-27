/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * asuser: runs a program as another user, as kyua runs a test case that
 * requires an unprivileged one (its `unprivileged_user`): "asuser user
 * program args...". run-atf's helper, built static.
 */

#include <sys/types.h>

#include <err.h>
#include <grp.h>
#include <pwd.h>
#include <unistd.h>

int
main(int argc, char **argv)
{
	struct passwd *pw;

	if (argc < 3)
		errx(2, "usage: asuser user program [args...]");
	if ((pw = getpwnam(argv[1])) == NULL)
		errx(1, "no user %s", argv[1]);
	if (setgroups(1, &pw->pw_gid) != 0 || setgid(pw->pw_gid) != 0 ||
	    setuid(pw->pw_uid) != 0)
		err(1, "becoming %s", argv[1]);
	execv(argv[2], argv + 2);
	err(127, "%s", argv[2]);
}
