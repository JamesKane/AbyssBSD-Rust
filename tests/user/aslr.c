/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * aslr: checks that what the auxiliary vector reports matches where exec
 * really put the stack and the shared page, wherever randomization put
 * them: the stack base is above this frame and within the stack limit,
 * ps_strings sits at the base, and the time keeping is readable.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/auxv.h>
#include <sys/exec.h>
#include <sys/time.h>
#include <sys/vdso.h>

#include <stdint.h>
#include <stdio.h>

int
main(void)
{
	uintptr_t base = 0, lim = 0, ps = 0, here;
	struct vdso_timekeep *tk = NULL;

	if (elf_aux_info(AT_USRSTACKBASE, &base, sizeof(base)) != 0 ||
	    elf_aux_info(AT_USRSTACKLIM, &lim, sizeof(lim)) != 0 ||
	    elf_aux_info(AT_PS_STRINGS, &ps, sizeof(ps)) != 0 ||
	    elf_aux_info(AT_TIMEKEEP, &tk, sizeof(tk)) != 0) {
		printf("aslr: auxv entries missing\n");
		return (1);
	}
	here = (uintptr_t)&base;
	if (here >= base || here < base - lim) {
		printf("aslr: frame %#lx outside stack [%#lx, %#lx)\n",
		    (u_long)here, (u_long)(base - lim), (u_long)base);
		return (1);
	}
	if (ps != base - sizeof(struct ps_strings)) {
		printf("aslr: ps_strings %#lx, stack base %#lx\n", (u_long)ps,
		    (u_long)base);
		return (1);
	}
	if (tk->tk_ver != VDSO_TK_VER_CURR || !tk->tk_enabled) {
		printf("aslr: time keeping at %p unreadable\n", (void *)tk);
		return (1);
	}
	printf("aslr: ok (stack base %#lx, timekeep %p)\n", (u_long)base,
	    (void *)tk);
	return (0);
}
