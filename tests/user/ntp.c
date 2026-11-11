/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ntp: checks the kernel's NTP interface and clock discipline from user
 * mode, as FreeBSD behaves: ntp_gettime and ntp_adjtime (the state they
 * return, the frequency and status they take and report, the privilege
 * they need to change anything), adjtime slewing the clock over time
 * rather than stepping it, and its old delta normalized; CLOCK_TAI, and
 * the kern.ntp_pll, kern.timecounter, and kern.eventtimer nodes ntpd,
 * chrony, and their tools read.
 *
 * Run as root. Everything it changes it puts back.
 *
 * Exits 0 when every check holds; prints each one that fails.
 */

#include <sys/types.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <sys/timex.h>
#include <sys/wait.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int failures;

#define CHECK(cond, ...) do {						\
	if (!(cond)) {							\
		printf("ntp: " __VA_ARGS__);				\
		printf(" (line %d)\n", __LINE__);			\
		failures++;						\
	}								\
} while (0)

static long long
ns(struct timespec t)
{
	return (t.tv_sec * 1000000000LL + t.tv_nsec);
}

static long long
now(clockid_t c)
{
	struct timespec t;

	clock_gettime(c, &t);
	return (ns(t));
}

static void
gettime(void)
{
	struct ntptimeval ntv;
	struct timespec a, b;
	int st;

	clock_gettime(CLOCK_REALTIME, &a);
	st = ntp_gettime(&ntv);
	clock_gettime(CLOCK_REALTIME, &b);
	CHECK(st >= TIME_OK && st <= TIME_ERROR, "ntp_gettime returned %d", st);
	CHECK(st == ntv.time_state, "ntp_gettime returned %d, state %d", st,
	    ntv.time_state);
	CHECK(ns(ntv.time) >= ns(a) && ns(ntv.time) <= ns(b),
	    "ntp_gettime's time %lld not within [%lld, %lld]", ns(ntv.time),
	    ns(a), ns(b));
	CHECK(ntv.maxerror >= 0 && ntv.esterror >= 0,
	    "error bounds %ld, %ld", ntv.maxerror, ntv.esterror);
}

static void
adjtime_ntp(void)
{
	struct timex orig, t;
	int st;

	memset(&orig, 0, sizeof(orig));
	st = ntp_adjtime(&orig);
	CHECK(st >= TIME_OK && st <= TIME_ERROR, "ntp_adjtime returned %d", st);
	CHECK(orig.tolerance == MAXFREQ * SCALE_PPM, "tolerance %ld",
	    orig.tolerance);
	CHECK(orig.constant >= 0 && orig.constant <= MAXTC, "constant %ld",
	    orig.constant);
	if ((orig.status & STA_UNSYNC) != 0)
		CHECK(st == TIME_ERROR, "unsynchronized but state %d", st);

	/* 12.5 ppm in, 12.5 ppm out; then too much, clamped to 500 ppm. */
	memset(&t, 0, sizeof(t));
	t.modes = MOD_FREQUENCY;
	t.freq = 12 * 65536 + 32768;
	CHECK(ntp_adjtime(&t) != -1, "MOD_FREQUENCY: %s", strerror(errno));
	CHECK(t.freq == 12 * 65536 + 32768, "frequency read back %ld", t.freq);
	memset(&t, 0, sizeof(t));
	t.modes = MOD_FREQUENCY;
	t.freq = -600L * 65536;
	ntp_adjtime(&t);
	CHECK(t.freq == -500L * 65536, "frequency clamped to %ld", t.freq);

	/* Read-only status bits are not taken; STA_NANO follows MOD_NANO. */
	memset(&t, 0, sizeof(t));
	t.modes = MOD_STATUS | MOD_NANO;
	t.status = STA_PLL | STA_UNSYNC | STA_MODE | STA_PPSSIGNAL;
	st = ntp_adjtime(&t);
	CHECK((t.status & (STA_MODE | STA_PPSSIGNAL)) == 0,
	    "read-only bits taken: %#x", t.status);
	CHECK((t.status & STA_NANO) != 0, "MOD_NANO: status %#x", t.status);
	CHECK((t.status & (STA_PLL | STA_UNSYNC)) == (STA_PLL | STA_UNSYNC),
	    "status %#x", t.status);
	CHECK(st == TIME_ERROR, "STA_UNSYNC but state %d", st);
	memset(&t, 0, sizeof(t));
	t.modes = MOD_MICRO;
	ntp_adjtime(&t);
	CHECK((t.status & STA_NANO) == 0, "MOD_MICRO: status %#x", t.status);

	/* The time constant is clamped to [0, MAXTC]. */
	memset(&t, 0, sizeof(t));
	t.modes = MOD_TIMECONST;
	t.constant = 99;
	ntp_adjtime(&t);
	CHECK(t.constant == MAXTC, "time constant %ld", t.constant);

	/* Back as it was: dropping STA_PLL resets the state. */
	memset(&t, 0, sizeof(t));
	t.modes = MOD_STATUS;
	t.status = 0;
	ntp_adjtime(&t);
	memset(&t, 0, sizeof(t));
	t.modes = MOD_STATUS | MOD_FREQUENCY | MOD_TIMECONST |
	    MOD_MAXERROR | MOD_ESTERROR |
	    ((orig.status & STA_NANO) != 0 ? MOD_NANO : MOD_MICRO);
	t.status = orig.status & ~STA_RONLY;
	t.freq = orig.freq;
	t.constant = orig.constant;
	t.maxerror = orig.maxerror;
	t.esterror = orig.esterror;
	ntp_adjtime(&t);
	CHECK(t.freq == orig.freq, "frequency restored as %ld, was %ld",
	    t.freq, orig.freq);
}

static void
unprivileged(void)
{
	struct timex t;
	struct timeval d;
	pid_t pid;
	int status;

	pid = fork();
	if (pid == 0) {
		int bad = 0;

		if (setuid(65534) != 0)
			_exit(2);
		memset(&t, 0, sizeof(t));
		if (ntp_adjtime(&t) == -1)
			bad |= 4;
		t.modes = MOD_FREQUENCY;
		if (ntp_adjtime(&t) != -1 || errno != EPERM)
			bad |= 8;
		d.tv_sec = 0;
		d.tv_usec = 1000;
		if (adjtime(&d, NULL) != -1 || errno != EPERM)
			bad |= 16;
		if (adjtime(NULL, &d) != 0)
			bad |= 32;
		_exit(bad);
	}
	CHECK(pid > 0, "fork: %s", strerror(errno));
	waitpid(pid, &status, 0);
	CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
	    "unprivileged checks: status %#x", status);
}

static void
slew(void)
{
	struct timeval d, old;
	long long t0, t1, secs, left, before, after;
	int tries;

	/* Nothing in progress to start with. */
	d.tv_sec = 0;
	d.tv_usec = 0;
	CHECK(adjtime(&d, &old) == 0, "adjtime: %s", strerror(errno));

	/*
	 * A 200 ms correction is slewed, not stepped: the time of day does
	 * not jump, and what is left of the correction shrinks, 500 us for
	 * each second the clock passes. Only the shape is checked here, not
	 * the rate: the wait is generous and bounded only by the seconds
	 * that actually passed.
	 */
	before = now(CLOCK_REALTIME) - now(CLOCK_MONOTONIC);
	t0 = now(CLOCK_MONOTONIC);
	d.tv_sec = 0;
	d.tv_usec = 200000;
	CHECK(adjtime(&d, &old) == 0, "adjtime: %s", strerror(errno));
	CHECK(old.tv_sec == 0 && old.tv_usec == 0, "old delta %ld.%06ld",
	    (long)old.tv_sec, old.tv_usec);
	after = now(CLOCK_REALTIME) - now(CLOCK_MONOTONIC);
	CHECK(after - before < 100000000, "the clock was stepped");
	left = 200000;
	for (tries = 0; tries < 300 && left == 200000; tries++) {
		usleep(100000);
		CHECK(adjtime(NULL, &old) == 0, "adjtime: %s", strerror(errno));
		left = old.tv_sec * 1000000LL + old.tv_usec;
	}
	t1 = now(CLOCK_MONOTONIC);
	secs = (t1 - t0) / 1000000000LL;
	CHECK(left < 200000, "no slew after %lld s", secs);
	CHECK(left >= 200000 - 500 * (secs + 2) && left % 500 == 0,
	    "%lld us left after %lld s", left, secs);

	/* A negative delta's remainder comes back normalized. */
	d.tv_sec = -1;
	d.tv_usec = -500000;
	CHECK(adjtime(&d, NULL) == 0, "adjtime: %s", strerror(errno));
	d.tv_sec = 0;
	d.tv_usec = 0;
	CHECK(adjtime(&d, &old) == 0, "adjtime: %s", strerror(errno));
	left = old.tv_sec * 1000000LL + old.tv_usec;
	CHECK(old.tv_usec >= 0 && old.tv_usec < 1000000,
	    "old delta's microseconds %ld", old.tv_usec);
	CHECK(left <= -1490000 && left >= -1500000,
	    "old delta %ld.%06ld", (long)old.tv_sec, old.tv_usec);
}

static void
tai(void)
{
	struct ntptimeval ntv;
	struct timespec t;
	int r;

	ntp_gettime(&ntv);
	r = clock_gettime(CLOCK_TAI, &t);
	if (ntv.tai == 0)
		CHECK(r == -1 && errno == EINVAL,
		    "CLOCK_TAI with no offset: %d (%s)", r, strerror(errno));
	else
		CHECK(r == 0 && t.tv_sec - time(NULL) - ntv.tai <= 1,
		    "CLOCK_TAI %lld with offset %ld", (long long)t.tv_sec,
		    ntv.tai);
}

static void
sysctls(void)
{
	struct ntptimeval ntv;
	char hw[64], choice[256], name[128], et[64];
	uint64_t freq;
	size_t len;
	int v;

	len = sizeof(ntv);
	CHECK(sysctlbyname("kern.ntp_pll.gettime", &ntv, &len, NULL, 0) == 0 &&
	    len == sizeof(ntv), "kern.ntp_pll.gettime: %s", strerror(errno));
	CHECK(ntv.time.tv_sec > 0, "kern.ntp_pll.gettime's time");
	len = sizeof(int64_t);
	CHECK(sysctlbyname("kern.ntp_pll.time_freq", &freq, &len, NULL, 0) == 0,
	    "kern.ntp_pll.time_freq: %s", strerror(errno));
	len = sizeof(v);
	CHECK(sysctlbyname("kern.ntp_pll.pps_shiftmax", &v, &len, NULL, 0) == 0,
	    "kern.ntp_pll.pps_shiftmax: %s", strerror(errno));

	len = sizeof(hw);
	CHECK(sysctlbyname("kern.timecounter.hardware", hw, &len, NULL, 0) == 0,
	    "kern.timecounter.hardware: %s", strerror(errno));
	len = sizeof(choice);
	CHECK(sysctlbyname("kern.timecounter.choice", choice, &len, NULL, 0) == 0,
	    "kern.timecounter.choice: %s", strerror(errno));
	CHECK(strstr(choice, hw) != NULL, "%s not in %s", hw, choice);
	CHECK(strstr(choice, "dummy(-1000000)") != NULL, "choice %s", choice);
	snprintf(name, sizeof(name), "kern.timecounter.tc.%s.frequency", hw);
	len = sizeof(freq);
	CHECK(sysctlbyname(name, &freq, &len, NULL, 0) == 0 && freq > 0,
	    "%s: %s", name, strerror(errno));
	snprintf(name, sizeof(name), "kern.timecounter.tc.%s.mask", hw);
	len = sizeof(v);
	CHECK(sysctlbyname(name, &v, &len, NULL, 0) == 0, "%s: %s", name,
	    strerror(errno));
	CHECK(sysctlbyname("kern.timecounter.hardware", NULL, NULL, hw,
	    strlen(hw) + 1) == 0, "selecting %s again: %s", hw,
	    strerror(errno));
	CHECK(sysctlbyname("kern.timecounter.hardware", NULL, NULL,
	    "no-such-counter", 16) == -1 && errno == EINVAL,
	    "selecting a counter that is not there: %s", strerror(errno));
	len = sizeof(v);
	CHECK(sysctlbyname("kern.timecounter.fast_gettime", &v, &len, NULL,
	    0) == 0 && v == 1, "kern.timecounter.fast_gettime %d", v);
	len = sizeof(v);
	CHECK(sysctlbyname("kern.timecounter.stepwarnings", &v, &len, NULL,
	    0) == 0, "kern.timecounter.stepwarnings: %s", strerror(errno));
	len = sizeof(v);
	CHECK(sysctlbyname("kern.timecounter.alloweddeviation", &v, &len, NULL,
	    0) == 0 && v == 5, "kern.timecounter.alloweddeviation %d", v);

	len = sizeof(et);
	CHECK(sysctlbyname("kern.eventtimer.timer", et, &len, NULL, 0) == 0,
	    "kern.eventtimer.timer: %s", strerror(errno));
	len = sizeof(choice);
	CHECK(sysctlbyname("kern.eventtimer.choice", choice, &len, NULL, 0) == 0,
	    "kern.eventtimer.choice: %s", strerror(errno));
	CHECK(strstr(choice, et) != NULL, "%s not in %s", et, choice);
	snprintf(name, sizeof(name), "kern.eventtimer.et.%s.frequency", et);
	len = sizeof(freq);
	CHECK(sysctlbyname(name, &freq, &len, NULL, 0) == 0, "%s: %s", name,
	    strerror(errno));
	snprintf(name, sizeof(name), "kern.eventtimer.et.%s.flags", et);
	len = sizeof(v);
	CHECK(sysctlbyname(name, &v, &len, NULL, 0) == 0 && (v & 2) != 0,
	    "%s: %d", name, v);
	CHECK(sysctlbyname("kern.eventtimer.timer", NULL, NULL, "no-such-timer",
	    14) == -1 && errno == ENOENT,
	    "selecting a timer that is not there: %s", strerror(errno));
	len = sizeof(v);
	CHECK(sysctlbyname("kern.eventtimer.periodic", &v, &len, NULL, 0) == 0,
	    "kern.eventtimer.periodic: %s", strerror(errno));
}

int
main(void)
{
	gettime();
	adjtime_ntp();
	unprivileged();
	slew();
	tai();
	sysctls();
	return (failures != 0);
}
