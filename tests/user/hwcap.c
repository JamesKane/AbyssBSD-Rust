/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * hwcap: checks that AT_HWCAP and AT_HWCAP2 agree with the ID registers
 * user mode reads (the kernel emulates the mrs), on arm64: each feature
 * bit is set exactly when its field reaches it, the fields the kernel
 * hides read as their safe values, and the CPU's own MIDR is readable.
 * On amd64 there is no AT_HWCAP (programs ask CPUID).
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <sys/types.h>
#include <sys/auxv.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>

#ifdef __aarch64__
#include <machine/armreg.h>
#include <machine/elf.h>

static int failed;

/* A 4-bit field of a register; signed fields sign-extend. */
static int
field(uint64_t reg, int shift, int is_signed)
{
	int v = (reg >> shift) & 0xf;

	return (is_signed && v >= 8 ? v - 16 : v);
}

static void
check(const char *what, int has, u_long caps, u_long bit)
{
	if (has != ((caps & bit) != 0)) {
		printf("hwcap: %s: field says %d, hwcap %#lx\n", what, has,
		    caps & bit);
		failed = 1;
	}
}

int
main(void)
{
	u_long hwcap = 0, hwcap2 = 0;
	uint64_t isar0, isar1, pfr0, mmfr0, midr;

	if (elf_aux_info(AT_HWCAP, &hwcap, sizeof(hwcap)) != 0 ||
	    elf_aux_info(AT_HWCAP2, &hwcap2, sizeof(hwcap2)) != 0) {
		printf("hwcap: no AT_HWCAP or AT_HWCAP2\n");
		return (1);
	}
	if ((hwcap & HWCAP_CPUID) == 0) {
		printf("hwcap: no HWCAP_CPUID (%#lx)\n", hwcap);
		return (1);
	}
	isar0 = READ_SPECIALREG(id_aa64isar0_el1);
	isar1 = READ_SPECIALREG(id_aa64isar1_el1);
	pfr0 = READ_SPECIALREG(id_aa64pfr0_el1);
	mmfr0 = READ_SPECIALREG(id_aa64mmfr0_el1);
	midr = READ_SPECIALREG(midr_el1);

	check("FP", field(pfr0, 16, 1) >= 0, hwcap, HWCAP_FP);
	check("FPHP", field(pfr0, 16, 1) >= 1, hwcap, HWCAP_FPHP);
	check("AdvSIMD", field(pfr0, 20, 1) >= 0, hwcap, HWCAP_ASIMD);
	check("SVE", field(pfr0, 32, 0) >= 1, hwcap, HWCAP_SVE);
	check("AES", field(isar0, 4, 0) >= 1, hwcap, HWCAP_AES);
	check("PMULL", field(isar0, 4, 0) >= 2, hwcap, HWCAP_PMULL);
	check("SHA1", field(isar0, 8, 0) >= 1, hwcap, HWCAP_SHA1);
	check("SHA2", field(isar0, 12, 0) >= 1, hwcap, HWCAP_SHA2);
	check("SHA512", field(isar0, 12, 0) >= 2, hwcap, HWCAP_SHA512);
	check("CRC32", field(isar0, 16, 0) >= 1, hwcap, HWCAP_CRC32);
	check("Atomic", field(isar0, 20, 0) >= 2, hwcap, HWCAP_ATOMICS);
	check("RNDR", field(isar0, 60, 0) >= 1, hwcap2, HWCAP2_RNG);
	check("JSCVT", field(isar1, 12, 0) >= 1, hwcap, HWCAP_JSCVT);
	check("LRCPC", field(isar1, 20, 0) >= 1, hwcap, HWCAP_LRCPC);
	check("PAC", field(isar1, 4, 0) >= 1 || field(isar1, 8, 0) >= 1,
	    hwcap, HWCAP_PACA);
	/* The memory model is the kernel's business: all hidden. */
	if (mmfr0 != 0) {
		printf("hwcap: ID_AA64MMFR0_EL1 %#jx, not hidden\n",
		    (uintmax_t)mmfr0);
		failed = 1;
	}
	if (midr == 0) {
		printf("hwcap: MIDR_EL1 reads 0\n");
		failed = 1;
	}
	if (failed)
		return (1);
	printf("hwcap: ok (%#lx %#lx)\n", hwcap, hwcap2);
	return (0);
}
#else
int
main(void)
{
	u_long hwcap;

	if (elf_aux_info(AT_HWCAP, &hwcap, sizeof(hwcap)) != ENOENT) {
		printf("hwcap: AT_HWCAP present on amd64\n");
		return (1);
	}
	printf("hwcap: ok (no AT_HWCAP)\n");
	return (0);
}
#endif
