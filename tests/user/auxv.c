/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * auxv: checks what the kernel tells about a program's image: its own
 * auxiliary vector through kern.proc.auxv (ending in AT_NULL, and as
 * elf_aux_info reads it), kern.ps_strings and kern.usrstack against
 * AT_PS_STRINGS and AT_USRSTACKBASE, the kern.elf64 defaults, and
 * utrace(2), which an untraced process may call.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/auxv.h>
#include <sys/ktrace.h>
#include <sys/param.h>
#include <sys/sysctl.h>
#include <sys/user.h>

#include <elf.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int failed;

static void
fail(const char *what)
{
	printf("auxv: %s\n", what);
	failed = 1;
}

static int
int_node(const char *name)
{
	int v = -2;
	size_t len = sizeof(v);

	if (sysctlbyname(name, &v, &len, NULL, 0) != 0)
		return (-3);
	return (v);
}

int
main(void)
{
	Elf_Auxinfo aux[AT_COUNT];
	int mib[4] = { CTL_KERN, KERN_PROC, KERN_PROC_AUXV, getpid() };
	size_t len = sizeof(aux), n;
	u_long ps = 0, base = 0, ps_node = 0, usrstack = 0;
	int page = 0, seen = 0;

	if (sysctl(mib, 4, aux, &len, NULL, 0) != 0 || len == 0 ||
	    len % sizeof(aux[0]) != 0) {
		fail("kern.proc.auxv");
		return (1);
	}
	n = len / sizeof(aux[0]);
	if (aux[n - 1].a_type != AT_NULL)
		fail("kern.proc.auxv does not end in AT_NULL");
	elf_aux_info(AT_PS_STRINGS, &ps, sizeof(ps));
	elf_aux_info(AT_USRSTACKBASE, &base, sizeof(base));
	elf_aux_info(AT_PAGESZ, &page, sizeof(page));
	for (size_t i = 0; i < n; i++) {
		if (aux[i].a_type == AT_PAGESZ) {
			seen |= 1;
			if (aux[i].a_un.a_val != page)
				fail("AT_PAGESZ differs from elf_aux_info");
		}
		if (aux[i].a_type == AT_PS_STRINGS) {
			seen |= 2;
			if ((u_long)aux[i].a_un.a_val != ps)
				fail("AT_PS_STRINGS differs from elf_aux_info");
		}
	}
	if (seen != 3)
		fail("kern.proc.auxv lacks AT_PAGESZ or AT_PS_STRINGS");

	len = sizeof(ps_node);
	if (sysctlbyname("kern.ps_strings", &ps_node, &len, NULL, 0) != 0 ||
	    ps_node != ps)
		fail("kern.ps_strings is not AT_PS_STRINGS");
	len = sizeof(usrstack);
	if (sysctlbyname("kern.usrstack", &usrstack, &len, NULL, 0) != 0 ||
	    usrstack != roundup2(base, (u_long)page))
		fail("kern.usrstack is not AT_USRSTACKBASE's page");

	if (int_node("kern.elf64.aslr.enable") != 1 ||
	    int_node("kern.elf64.aslr.pie_enable") != 1 ||
	    int_node("kern.elf64.aslr.stack") != 1 ||
	    int_node("kern.elf64.aslr.honor_sbrk") != 0 ||
	    int_node("kern.elf64.sigfastblock") != 1 ||
	    int_node("kern.elf64.nxstack") != 1)
		fail("kern.elf64 defaults");

	if (utrace("auxv", 4) != 0)
		fail("utrace");
	if (failed)
		return (1);
	printf("auxv: ok (%zu entries)\n", n);
	return (0);
}
