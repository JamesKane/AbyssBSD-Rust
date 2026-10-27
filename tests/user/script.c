/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * script: checks execve of "#!" scripts: the interpreter runs with
 * arguments the interpreter, the line's one argument if it has one (the
 * rest of the line, blanks around it trimmed), the script's path as
 * given, then the caller's arguments past the first. A script whose
 * interpreter is a script, one naming no interpreter, and one whose line
 * never ends are ENOEXEC. This program is its own interpreter.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/stat.h>
#include <sys/sysctl.h>
#include <sys/wait.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failures;
static char self[1024];

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("script: " __VA_ARGS__);				\
		printf("\n");						\
		failures++;						\
	}								\
} while (0)

static void
write_file(const char *path, const char *text)
{
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0755);

	if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text))
		printf("script: cannot write %s: %s\n", path, strerror(errno));
	close(fd);
}

/* Runs `path` with `argv` in a child; its exit status, or -errno. */
static int
run(const char *path, char *const argv[])
{
	int st, fds[2];
	pid_t pid;

	pipe(fds);
	pid = fork();
	if (pid == 0) {
		int e;

		close(fds[0]);
		execv(path, argv);
		e = errno;
		write(fds[1], &e, sizeof(e));
		_exit(127);
	}
	close(fds[1]);
	int e = 0;
	ssize_t n = read(fds[0], &e, sizeof(e));
	close(fds[0]);
	waitpid(pid, &st, 0);
	if (n == sizeof(e))
		return (-e);
	return (WIFEXITED(st) ? WEXITSTATUS(st) : 200);
}

int
main(int argc, char **argv)
{
	int mib[4] = { CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, -1 };
	size_t len = sizeof(self);
	char line[1200];

	/* As the interpreter: argv[1] says what to expect. */
	if (argc > 1 && strcmp(argv[1], "with-arg") == 0)
		return (argc == 6 && strcmp(argv[2], "/tmp/script-a") == 0 &&
		    strcmp(argv[3], "x") == 0 && strcmp(argv[4], "y") == 0 &&
		    strcmp(argv[5], "z") == 0 ? 7 : 1);
	if (argc > 1 && strcmp(argv[1], "/tmp/script-b") == 0)
		return (argc == 3 && strcmp(argv[2], "y") == 0 ? 8 : 1);
	if (argc > 1 && strcmp(argv[1], "trimmed  arg") == 0)
		return (argc == 3 && strcmp(argv[2], "/tmp/script-c") == 0 ?
		    9 : 1);

	if (sysctl(mib, 4, self, &len, NULL, 0) != 0) {
		printf("script: kern.proc.pathname: %s\n", strerror(errno));
		return (1);
	}

	/* An argument on the line, and the caller's past the first. */
	snprintf(line, sizeof(line), "#! %s with-arg\n", self);
	write_file("/tmp/script-a", line);
	char *a[] = { "ignored", "x", "y", "z", NULL };
	int r = run("/tmp/script-a", a);
	CHECK(r == 7, "script with an argument: %d", r);

	/* None: the script's path comes right after the interpreter. */
	snprintf(line, sizeof(line), "#!%s\necho not this\n", self);
	write_file("/tmp/script-b", line);
	char *b[] = { "ignored", "y", NULL };
	r = run("/tmp/script-b", b);
	CHECK(r == 8, "script without an argument: %d", r);

	/* The rest of the line is one argument, blanks around it gone. */
	snprintf(line, sizeof(line), "#!\t%s \t trimmed  arg \t\n", self);
	write_file("/tmp/script-c", line);
	char *c[] = { "ignored", NULL };
	r = run("/tmp/script-c", c);
	CHECK(r == 9, "script with blanks: %d", r);

	/* A script whose interpreter is a script. */
	write_file("/tmp/script-d", "#!/tmp/script-b\n");
	r = run("/tmp/script-d", c);
	CHECK(r == -ENOEXEC, "a script for a script: %d", r);

	/* No interpreter, and a line that does not end. */
	write_file("/tmp/script-e", "#!   \n");
	r = run("/tmp/script-e", c);
	CHECK(r == -ENOEXEC, "no interpreter: %d", r);
	snprintf(line, sizeof(line), "#!%s", self);
	write_file("/tmp/script-f", line);
	r = run("/tmp/script-f", c);
	CHECK(r == -ENOEXEC, "an unended line: %d", r);

	for (char f = 'a'; f <= 'f'; f++) {
		snprintf(line, sizeof(line), "/tmp/script-%c", f);
		unlink(line);
	}
	if (failures == 0)
		printf("script: ok\n");
	return (failures != 0);
}
