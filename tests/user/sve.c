/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * sve: checks arm64's signal frame on a CPU with SVE. A handler's context
 * lists a struct sve_context (Z0-Z31, P0-P15, FFR, FPSR, FPCR at the
 * thread's vector length) and then the end marker, and sigreturn loads
 * the Z registers' bits past the V registers from it, while mc_fpregs
 * still sets their low 128 bits. Without SVE, the list is the end marker
 * alone. Elsewhere there is nothing to check.
 *
 * The signal comes from another thread while this one runs in user mode
 * holding a pattern in Z0, which mc_fpregs shows in V0. What the SVE
 * context holds of the interrupted registers is not checked: FreeBSD 15.1 writes the state it last saved,
 * which a signal taken from an interrupt (whose saved ESR is the last
 * system call's) does not bring up to date.
 *
 * Links with: -lthr
 *
 * Exits 0 on success; prints what failed otherwise.
 */

#include <stdio.h>

#if defined(__aarch64__)

#include <sys/types.h>
#include <sys/auxv.h>
#include <sys/ucontext.h>

#include <machine/elf.h>

#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <string.h>
#include <unistd.h>

#define	MAXVL	256

static int failures;

#define	CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("sve: " __VA_ARGS__);				\
		printf("\n");						\
		failures++;						\
	}								\
} while (0)

static size_t vl;
static _Atomic(int) released;
static _Atomic(int) handled;
/* Signals taken; one that came before the thread held the pattern is
 * let go, and the sender tries again. */
static int signals;
static unsigned char after[MAXVL];
/* What the handler saw, checked outside it. */
static int saw_sve, saw_len, saw_end, saw_v0;
static unsigned int saw_size;

static size_t
vector_length(void)
{
	size_t n;

	__asm__ volatile(".arch_extension sve\n\trdvl %0, #1" : "=r"(n));
	return (n);
}

/* What Z0 holds while the thread waits: byte i is i. */
static unsigned char
pattern(size_t i)
{
	return ((unsigned char)i);
}

static void
handler(int sig, siginfo_t *si, void *ctx)
{
	ucontext_t *uc = ctx;
	struct arm64_reg_context *rc;
	unsigned char *z0, v0[16];

	(void)sig;
	(void)si;
	/* The sender may get one more in before it sees this. */
	if (atomic_load(&handled))
		return;
	signals++;
	/* V0, Z0's low 128 bits: the pattern once the thread holds it. */
	memcpy(v0, &uc->uc_mcontext.mc_fpregs.fp_q[0], sizeof(v0));
	saw_v0 = 1;
	for (size_t i = 0; i < sizeof(v0); i++)
		if (v0[i] != pattern(i))
			saw_v0 = 0;
	if (!saw_v0 && signals < 100)
		return;
	rc = (void *)uc->uc_mcontext.mc_ptr;
	if (rc != NULL && rc->ctx_id == ARM64_CTX_SVE) {
		struct sve_context *sc = (void *)rc;

		saw_sve = 1;
		saw_len = sc->sve_vector_len;
		saw_size = rc->ctx_size;
		z0 = (unsigned char *)(sc + 1);
		/* Past the V register: sigreturn takes this. */
		for (size_t i = 16; i < vl; i++)
			z0[i] = 0xa5;
		rc = (void *)((char *)rc + rc->ctx_size);
	}
	saw_end = rc != NULL && rc->ctx_id == ARM64_CTX_END &&
	    rc->ctx_size == sizeof(*rc);
	atomic_store(&released, 1);
	atomic_store(&handled, 1);
}

/* Holds the pattern in Z0 until released, then stores Z0. */
static void
wait_holding_z0(void)
{
	__asm__ volatile(
	    ".arch_extension sve\n\t"
	    "index z0.b, #0, #1\n"
	    "1:\tldr w9, [%[flag]]\n\t"
	    "cbz w9, 1b\n\t"
	    "str z0, [%[out]]\n"
	    :
	    : [flag] "r"(&released), [out] "r"(after)
	    : "x9", "v0", "memory");
}

static void *
sender(void *arg)
{
	pthread_t target = *(pthread_t *)arg;

	/* Until it lands while the target spins in user mode. */
	while (!atomic_load(&handled)) {
		usleep(10000);
		pthread_kill(target, SIGUSR1);
	}
	return (NULL);
}

static void
check_sve(void)
{
	pthread_t self = pthread_self(), t;
	size_t want;

	vl = vector_length();
	if (vl > MAXVL) {
		printf("sve: vector length %zu\n", vl);
		failures++;
		return;
	}
	pthread_create(&t, NULL, sender, &self);
	wait_holding_z0();
	pthread_join(t, NULL);
	want = (sizeof(struct sve_context) + 32 * vl + 17 * vl / 8 + 16 + 3) &
	    ~(size_t)3;
	CHECK(saw_sve, "no SVE context in the signal frame");
	CHECK(saw_len == (int)vl, "context vector length %d, not %zu", saw_len,
	    vl);
	CHECK(saw_size == want, "context size %u, not %zu", saw_size, want);
	CHECK(saw_v0, "V0 in the context is not what the thread held");
	CHECK(saw_end, "no end marker after the SVE context");
	for (size_t i = 0; i < vl; i++) {
		unsigned char w = i < 16 ? pattern(i) : 0xa5;

		if (after[i] != w) {
			printf("sve: after sigreturn Z0 byte %zu is %#x, "
			    "not %#x\n", i, after[i], w);
			failures++;
			break;
		}
	}
}

static void
end_only_handler(int sig, siginfo_t *si, void *ctx)
{
	ucontext_t *uc = ctx;
	struct arm64_reg_context *rc = (void *)uc->uc_mcontext.mc_ptr;

	(void)sig;
	(void)si;
	saw_end = rc != NULL && rc->ctx_id == ARM64_CTX_END &&
	    rc->ctx_size == sizeof(*rc);
}

int
main(void)
{
	struct sigaction sa;
	unsigned long hwcap = 0;

	elf_aux_info(AT_HWCAP, &hwcap, sizeof(hwcap));
	memset(&sa, 0, sizeof(sa));
	sa.sa_flags = SA_SIGINFO;
	if ((hwcap & HWCAP_SVE) == 0) {
		sa.sa_sigaction = end_only_handler;
		sigaction(SIGUSR1, &sa, NULL);
		raise(SIGUSR1);
		CHECK(saw_end, "the context list is not just its end");
	} else {
		sa.sa_sigaction = handler;
		sigaction(SIGUSR1, &sa, NULL);
		check_sve();
	}
	if (failures == 0)
		printf("sve: ok (%s)\n", (hwcap & HWCAP_SVE) != 0 ?
		    "SVE" : "no SVE");
	return (failures != 0);
}

#else

int
main(void)
{
	printf("sve: ok (not arm64)\n");
	return (0);
}

#endif
