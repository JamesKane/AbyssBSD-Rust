/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * kpreload: the vdso. On amd64 the kernel tells rtld where it is in the
 * shared page (AT_KPRELOAD, while kern.elf64.vdso is on); rtld lists it
 * as "[vdso]", with its own PT_GNU_EH_FRAME and the signal trampoline,
 * __vdso_sigcode, which a handler returns into; and its call frame
 * information lets an unwinder walk from a handler back to the code the
 * signal interrupted. arm64 has none.
 *
 * Built dynamic, all of it is checked; built static, rtld is absent, so
 * only the auxiliary vector is.
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/user.h>

#include <dlfcn.h>
#include <elf.h>
#include <link.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <unwind.h>

static int failed;

static void
fail(const char *what)
{
	printf("kpreload: %s\n", what);
	failed = 1;
}

static uintptr_t kpreload;
static int vdsos;
static uintptr_t vdso_base;
static int vdso_eh_frame;

static int
each_object(struct dl_phdr_info *info, size_t size __unused, void *arg __unused)
{
	if (info->dlpi_name == NULL || strcmp(info->dlpi_name, "[vdso]") != 0)
		return (0);
	vdsos++;
	vdso_base = info->dlpi_addr;
	for (int i = 0; i < info->dlpi_phnum; i++)
		if (info->dlpi_phdr[i].p_type == PT_GNU_EH_FRAME)
			vdso_eh_frame = 1;
	return (0);
}

#define FRAMES	32
static uintptr_t frames[FRAMES];
static int nframes;
static uintptr_t returned_to;

static _Unwind_Reason_Code
each_frame(struct _Unwind_Context *ctx, void *arg __unused)
{
	if (nframes == FRAMES)
		return (_URC_END_OF_STACK);
	frames[nframes++] = _Unwind_GetIP(ctx);
	return (_URC_NO_REASON);
}

static void
handler(int sig __unused)
{
	returned_to = (uintptr_t)__builtin_return_address(0);
	_Unwind_Backtrace(each_frame, NULL);
}

/* The interrupted code: the unwinder must reach it from the handler. */
static __attribute__((noinline)) void
interrupted(void)
{
	raise(SIGUSR1);
	__asm__ volatile("" ::: "memory");
}

int
main(void)
{
	Elf_Auxinfo aux[AT_COUNT];
	int mib[4] = { CTL_KERN, KERN_PROC, KERN_PROC_AUXV, getpid() };
	size_t len = sizeof(aux);
	int vdso = -1, dynamic;

	if (sysctl(mib, 4, aux, &len, NULL, 0) != 0) {
		fail("kern.proc.auxv");
		return (1);
	}
	for (size_t i = 0; i < len / sizeof(aux[0]); i++)
		if (aux[i].a_type == AT_KPRELOAD)
			kpreload = (uintptr_t)aux[i].a_un.a_ptr;
	len = sizeof(vdso);
	if (sysctlbyname("kern.elf64.vdso", &vdso, &len, NULL, 0) != 0)
		vdso = -1;
#ifdef __amd64__
	if (vdso != 1)
		fail("kern.elf64.vdso is not 1");
	if (kpreload == 0 || (kpreload & (getpagesize() - 1)) != 0 ||
	    memcmp((void *)kpreload, ELFMAG, SELFMAG) != 0)
		fail("AT_KPRELOAD is not an ELF image at a page");
#else
	if (vdso != -1)
		fail("kern.elf64.vdso exists");
	if (kpreload != 0)
		fail("AT_KPRELOAD on an architecture without a vdso");
#endif

	/* A static program's dlsym finds nothing. */
	dynamic = dlsym(RTLD_DEFAULT, "dlopen") != NULL;
	dl_iterate_phdr(each_object, NULL);
	if (kpreload == 0 || !dynamic) {
		if (vdsos != 0)
			fail("a [vdso] without rtld or AT_KPRELOAD");
		goto out;
	}

	if (vdsos != 1 || vdso_base != kpreload)
		fail("rtld does not list [vdso] at AT_KPRELOAD");
	if (!vdso_eh_frame)
		fail("[vdso] has no PT_GNU_EH_FRAME");
	void *sigcode = dlsym(RTLD_DEFAULT, "__vdso_sigcode");
	Dl_info di;
	if (sigcode == NULL || dladdr(sigcode, &di) == 0 ||
	    strcmp(di.dli_fname, "[vdso]") != 0 ||
	    (uintptr_t)di.dli_fbase != kpreload)
		fail("__vdso_sigcode is not in [vdso]");
	signal(SIGUSR1, handler);
	interrupted();
	if (sigcode != NULL &&
	    (returned_to <= (uintptr_t)sigcode ||
	    returned_to >= (uintptr_t)sigcode + 64))
		fail("the handler does not return into __vdso_sigcode");
	int through_trampoline = 0, reached = 0;
	for (int i = 0; i < nframes; i++) {
		if (frames[i] == returned_to)
			through_trampoline = 1;
		if (through_trampoline && frames[i] > (uintptr_t)interrupted &&
		    frames[i] < (uintptr_t)interrupted + 64)
			reached = 1;
	}
	if (!through_trampoline)
		fail("the unwinder does not see the trampoline's frame");
	else if (!reached)
		fail("the unwinder does not reach the interrupted code");

out:
	if (failed)
		return (1);
	printf("kpreload: ok (%s%s)\n", dynamic ? "dynamic" : "static",
	    kpreload != 0 ? ", vdso" : "");
	return (0);
}
