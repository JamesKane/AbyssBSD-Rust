/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * modules: modstat(2) in each of the three record versions sys_modstat
 * takes (the current one, whose name is MAXPATHLEN long, and the first
 * two, with 32-byte names), modfind(2) of the name it gives back, and
 * EINVAL for a version that is none of them.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/param.h>
#include <sys/module.h>

#include <errno.h>
#include <stdio.h>
#include <string.h>

/* sys_modstat's own, in no header. */
struct module_stat_v2 {
	int		version;
	char		name[32];
	int		refs;
	int		id;
	modspecific_t	data;
};

int
main(void)
{
	struct module_stat st;
	struct module_stat_v2 v2;
	int id;

	id = modnext(0);
	if (id <= 0) {
		printf("modules: modnext(0) = %d\n", id);
		return (1);
	}
	memset(&st, 0xa5, sizeof(st));
	st.version = sizeof(st);
	if (modstat(id, &st) != 0) {
		printf("modules: modstat(v3): errno %d\n", errno);
		return (1);
	}
	if (st.version != sizeof(st) || st.id != id ||
	    memchr(st.name, '\0', sizeof(st.name)) == NULL || st.name[0] == '\0') {
		printf("modules: modstat(v3): version %d id %d (want %d)\n",
		    st.version, st.id, id);
		return (1);
	}
	if (modfind(st.name) != id) {
		printf("modules: modfind(%s) != %d\n", st.name, id);
		return (1);
	}

	memset(&v2, 0xa5, sizeof(v2));
	v2.version = sizeof(v2);
	if (modstat(id, (struct module_stat *)&v2) != 0) {
		printf("modules: modstat(v2): errno %d\n", errno);
		return (1);
	}
	if (v2.id != id || v2.refs != st.refs ||
	    strncmp(v2.name, st.name, sizeof(v2.name) - 1) != 0) {
		printf("modules: modstat(v2): id %d name %.32s (want %d %s)\n",
		    v2.id, v2.name, id, st.name);
		return (1);
	}

	v2.version = 12;
	if (modstat(id, (struct module_stat *)&v2) != -1 || errno != EINVAL) {
		printf("modules: modstat(version 12) did not fail with EINVAL\n");
		return (1);
	}
	return (0);
}
