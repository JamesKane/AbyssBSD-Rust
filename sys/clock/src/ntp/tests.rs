// SPDX-License-Identifier: BSD-2-Clause
//! The discipline's arithmetic against values worked out from FreeBSD's
//! `kern_ntptime.c`, and the loop closed around a simulated clock.

use super::*;
use abi::timex::status::{STA_DEL, STA_INS};

const FRAC: i64 = 1 << 32;

fn tx(modes: u32) -> Timex {
    Timex {
        modes,
        ..Timex::default()
    }
}

/// `ntpd`'s first call: PLL on, nanoseconds, synchronized.
fn synced(d: &mut Discipline, constant: i64) {
    let mut t = tx(modes::MOD_STATUS | modes::MOD_NANO | modes::MOD_TIMECONST);
    t.status = STA_PLL;
    t.constant = constant;
    assert_eq!(d.adjust(&mut t, 1), state::TIME_OK);
}

#[test]
fn boot_state_is_unsynchronized() {
    let d = Discipline::new();
    let r = d.reading();
    assert_eq!(r.state, state::TIME_ERROR);
    assert_eq!(r.maxerror, 500_000);
    assert_eq!(r.esterror, 500_000);
    let mut d = d;
    let mut t = tx(0);
    assert_eq!(d.adjust(&mut t, 0), state::TIME_ERROR);
    assert_eq!(t.status, STA_UNSYNC);
    assert_eq!(t.tolerance, 500_000 * 65);
    assert_eq!(t.precision, 0, "1 ns in microseconds");
    assert_eq!(t.shift, PPS_FAVG);
}

#[test]
fn read_only_status_bits_stay() {
    let mut d = Discipline::new();
    let mut t = tx(modes::MOD_STATUS);
    t.status = STA_PLL | STA_NANO | STA_MODE | STA_PPSSIGNAL;
    d.adjust(&mut t, 0);
    assert_eq!(t.status, STA_PLL, "the read-only bits were not taken");
    let mut t = tx(modes::MOD_NANO);
    d.adjust(&mut t, 0);
    assert_eq!(t.status, STA_PLL | STA_NANO);
    assert_eq!(t.precision, 1);
    let mut t = tx(modes::MOD_MICRO);
    d.adjust(&mut t, 0);
    assert_eq!(t.status, STA_PLL);
}

#[test]
fn clearing_pll_resets_the_state() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    let mut t = tx(modes::MOD_STATUS);
    t.status = STA_INS | STA_PLL;
    d.adjust(&mut t, 2);
    let mut s = 86_399;
    d.update_second(&mut s);
    assert_eq!(d.leap(), LeapState::Insert);
    let mut t = tx(modes::MOD_STATUS);
    t.status = 0;
    // Back to boot's `STA_UNSYNC`, which the new status (none) then
    // replaces, as in FreeBSD.
    assert_eq!(d.adjust(&mut t, 3), state::TIME_OK);
    assert_eq!(d.leap(), LeapState::Ok);
    assert_eq!(t.status, 0);
}

#[test]
fn frequency_round_trips_and_clamps() {
    let mut d = Discipline::new();
    // 12.5 ppm.
    let mut t = tx(modes::MOD_FREQUENCY);
    t.freq = 12 * 65536 + 32768;
    d.adjust(&mut t, 0);
    assert_eq!(t.freq, 12 * 65536 + 32768);
    assert_eq!(d.time_freq(), 12_500 * FRAC);
    assert_eq!(t.ppsfreq, t.freq);
    // -3 ppm.
    let mut t = tx(modes::MOD_FREQUENCY);
    t.freq = -3 * 65536;
    d.adjust(&mut t, 0);
    assert_eq!(t.freq, -3 * 65536);
    assert_eq!(d.time_freq(), -3000 * FRAC);
    // 600 ppm is more than the 500 allowed.
    let mut t = tx(modes::MOD_FREQUENCY);
    t.freq = 600 * 65536;
    d.adjust(&mut t, 0);
    assert_eq!(d.time_freq(), MAXFREQ * FRAC);
    assert_eq!(t.freq, 500 * 65536);
    let mut t = tx(modes::MOD_FREQUENCY);
    t.freq = -600 * 65536;
    d.adjust(&mut t, 0);
    assert_eq!(t.freq, -500 * 65536);
}

#[test]
fn time_constant_and_tai() {
    let mut d = Discipline::new();
    let mut t = tx(modes::MOD_TIMECONST);
    t.constant = 15;
    d.adjust(&mut t, 0);
    assert_eq!(t.constant, MAXTC);
    let mut t = tx(modes::MOD_TIMECONST);
    t.constant = -2;
    d.adjust(&mut t, 0);
    assert_eq!(t.constant, 0);
    let mut t = tx(modes::MOD_TAI);
    t.constant = 37;
    d.adjust(&mut t, 0);
    assert_eq!(d.tai(), 37);
    assert_eq!(
        t.constant, 0,
        "the time constant is reported, not the offset"
    );
    let mut t = tx(modes::MOD_TAI);
    t.constant = 0;
    d.adjust(&mut t, 0);
    assert_eq!(d.tai(), 37, "zero is ignored");
    let mut t = tx(modes::MOD_PPSMAX);
    t.shift = 40;
    d.adjust(&mut t, 0);
    assert_eq!(d.pps_shiftmax(), PPS_FAVGMAX);
}

#[test]
fn offset_without_pll_is_ignored() {
    let mut d = Discipline::new();
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 1000;
    d.adjust(&mut t, 5);
    assert_eq!(t.offset, 0);
}

#[test]
fn pll_slews_the_phase_and_steers_the_frequency() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    // A first offset of 1 ms: no interval yet, so phase only.
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 1_000_000;
    d.adjust(&mut t, 100);
    assert_eq!(t.offset, 1_000_000);
    assert_eq!(d.time_freq(), 0);
    // Each second takes 1/16 of what is left (SHIFT_PLL + 0).
    let mut s = 1000;
    let adj = d.update_second(&mut s);
    assert_eq!(adj, (1_000_000 * FRAC) >> 4);
    let mut t = tx(0);
    d.adjust(&mut t, 101);
    assert_eq!(t.offset, 1_000_000 - 62_500);
    // 16 s later the same offset again: the frequency moves by
    // offset / 2^12 per second of interval, times 16.
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 1_000_000;
    d.adjust(&mut t, 116);
    assert_eq!(d.time_freq(), ((1_000_000 * FRAC) >> 12) * 16);
    assert_eq!(t.status & STA_MODE, 0, "PLL mode");
    // In scaled ppm: 3.90625 ppm.
    assert_eq!(t.freq, 256_000);
    // Microseconds.
    let mut t = tx(modes::MOD_MICRO | modes::MOD_OFFSET);
    t.offset = -250;
    d.adjust(&mut t, 132);
    assert_eq!(t.offset, -250);
    assert_eq!(t.precision, 0);
}

#[test]
fn long_intervals_add_the_fll() {
    let mut d = Discipline::new();
    synced(&mut d, 4);
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 4_000_000;
    d.adjust(&mut t, 10);
    let f0 = d.time_freq();
    // 3000 s later: past MAXSEC, so the FLL adds its share.
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 4_000_000;
    d.adjust(&mut t, 3010);
    let pll = rshift(4_000_000 * FRAC, (SHIFT_PLL + 2 + 4) << 1) * 3000;
    let fll = rshift(((4_000_000i64 << 4) / 3000) * FRAC, SHIFT_FLL + 4);
    assert_eq!(d.time_freq(), f0 + pll + fll);
    assert_ne!(t.status & STA_MODE, 0, "FLL mode");
    // A short interval goes back to PLL mode.
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 0;
    d.adjust(&mut t, 3020);
    assert_eq!(t.status & STA_MODE, 0);
}

#[test]
fn freqhold_keeps_the_frequency() {
    let mut d = Discipline::new();
    let mut t = tx(modes::MOD_STATUS | modes::MOD_NANO);
    t.status = STA_PLL | STA_FREQHOLD;
    d.adjust(&mut t, 1);
    for up in [10, 100, 5000] {
        let mut t = tx(modes::MOD_OFFSET);
        t.offset = 30_000_000;
        d.adjust(&mut t, up);
    }
    assert_eq!(d.time_freq(), 0);
}

#[test]
fn offsets_clamp_and_frequency_saturates() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 2_000_000_000;
    d.adjust(&mut t, 10);
    assert_eq!(t.offset, MAXPHASE);
    assert_eq!(d.time_monitor(), MAXPHASE);
    let mut t = tx(modes::MOD_OFFSET);
    t.offset = 2_000_000_000;
    d.adjust(&mut t, 10_000);
    assert_eq!(d.time_freq(), MAXFREQ * FRAC);
}

#[test]
fn adjtime_slews_at_freebsds_rates() {
    let mut d = Discipline::new();
    // 1.5 s: 5000 us/s until a second is left, then 500 us/s.
    assert_eq!(d.set_adjtime(Some(1_500_000)), 0);
    let mut s = 10;
    let adj = d.update_second(&mut s);
    assert_eq!(adj, 5_000_000 * FRAC);
    let mut seconds = 1;
    while d.adjtime_left() > 1_000_000 {
        d.update_second(&mut s);
        seconds += 1;
    }
    assert_eq!(seconds, 100);
    assert_eq!(d.adjtime_left(), 1_000_000);
    let adj = d.update_second(&mut s);
    assert_eq!(adj, 500_000 * FRAC);
    seconds += 1;
    while d.adjtime_left() != 0 {
        d.update_second(&mut s);
        seconds += 1;
    }
    assert_eq!(seconds, 100 + 2000);
    // A remainder under 500 us goes in one second.
    d.set_adjtime(Some(-123));
    assert_eq!(d.update_second(&mut s), -123_000 * FRAC);
    assert_eq!(d.update_second(&mut s), 0);
    // The old correction comes back and is replaced.
    d.set_adjtime(Some(7_000));
    assert_eq!(d.set_adjtime(None), 7_000);
    assert_eq!(d.set_adjtime(Some(0)), 7_000);
    assert_eq!(d.adjtime_left(), 0);
}

#[test]
fn maxerror_grows_by_the_tolerance() {
    let mut d = Discipline::new();
    let mut t = tx(modes::MOD_MAXERROR | modes::MOD_ESTERROR);
    t.maxerror = 1000;
    t.esterror = 20;
    d.adjust(&mut t, 0);
    let mut s = 5;
    for _ in 0..10 {
        d.update_second(&mut s);
    }
    let r = d.reading();
    assert_eq!(r.maxerror, 1000 + 10 * 500);
    assert_eq!(r.esterror, 20);
}

#[test]
fn leap_second_insertion() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    let mut t = tx(modes::MOD_STATUS | modes::MOD_TAI);
    t.status = STA_PLL | STA_INS;
    t.constant = 37;
    d.adjust(&mut t, 2);
    // 2016-12-31 23:59:58 UTC.
    let day_end = 1_483_228_800;
    let mut s = day_end - 2;
    d.update_second(&mut s);
    assert_eq!(d.reading().state, state::TIME_INS);
    let mut s = day_end - 1;
    d.update_second(&mut s);
    assert_eq!(s, day_end - 1);
    // Midnight: the clock goes back to 23:59:59 (shown as :60).
    let mut s = day_end;
    d.update_second(&mut s);
    assert_eq!(s, day_end - 1);
    assert_eq!(d.reading().state, state::TIME_OOP);
    assert_eq!(d.tai(), 38);
    let mut s = day_end;
    d.update_second(&mut s);
    assert_eq!(s, day_end);
    assert_eq!(d.reading().state, state::TIME_WAIT);
    let mut s = day_end + 1;
    d.update_second(&mut s);
    assert_eq!(d.reading().state, state::TIME_WAIT);
    let mut t = tx(modes::MOD_STATUS);
    t.status = STA_PLL;
    d.adjust(&mut t, 5);
    let mut s = day_end + 2;
    d.update_second(&mut s);
    assert_eq!(d.reading().state, state::TIME_OK);
}

#[test]
fn leap_second_deletion() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    let mut t = tx(modes::MOD_STATUS | modes::MOD_TAI);
    t.status = STA_PLL | STA_DEL;
    t.constant = 37;
    d.adjust(&mut t, 2);
    let day_end = 86_400 * 20_000;
    let mut s = day_end - 3;
    d.update_second(&mut s);
    assert_eq!(d.leap(), LeapState::Delete);
    let mut s = day_end - 2;
    d.update_second(&mut s);
    assert_eq!(s, day_end - 2);
    // 23:59:59 is skipped.
    let mut s = day_end - 1;
    d.update_second(&mut s);
    assert_eq!(s, day_end);
    assert_eq!(d.tai(), 36);
    assert_eq!(d.leap(), LeapState::Wait);
}

#[test]
fn a_request_withdrawn_before_midnight() {
    let mut d = Discipline::new();
    synced(&mut d, 0);
    let mut t = tx(modes::MOD_STATUS);
    t.status = STA_PLL | STA_INS;
    d.adjust(&mut t, 2);
    let mut s = 100;
    d.update_second(&mut s);
    assert_eq!(d.leap(), LeapState::Insert);
    let mut t = tx(modes::MOD_STATUS);
    t.status = STA_PLL;
    d.adjust(&mut t, 3);
    let mut s = 86_400;
    d.update_second(&mut s);
    assert_eq!(s, 86_400, "no second inserted");
    assert_eq!(d.leap(), LeapState::Ok);
}

#[test]
fn time_error_decoding() {
    assert!(Discipline::is_time_error(STA_UNSYNC));
    assert!(Discipline::is_time_error(STA_CLOCKERR));
    assert!(Discipline::is_time_error(STA_PPSFREQ));
    assert!(!Discipline::is_time_error(STA_PPSFREQ | STA_PPSSIGNAL));
    assert!(Discipline::is_time_error(
        STA_PPSTIME | STA_PPSSIGNAL | STA_PPSJITTER
    ));
    assert!(Discipline::is_time_error(
        STA_PPSFREQ | STA_PPSSIGNAL | STA_PPSWANDER
    ));
    assert!(!Discipline::is_time_error(STA_PLL | STA_NANO));
}

#[test]
fn scale_matches_freebsds_within_its_error() {
    let f = 3_000_000_000u64 >> 1;
    assert_eq!(
        scale(f, 0, 0),
        u64::try_from((1u128 << 64) / u128::from(f)).unwrap()
    );
    // The shifted counter: a 3 GHz count read >> 1 has twice the scale.
    let unshifted = 3_000_000_000u64;
    let s = scale(unshifted, 1, 0);
    assert_eq!(s, u64::try_from((1u128 << 65) / 3_000_000_000).unwrap());
    // +100 ppm.
    let adj = 100_000 * FRAC;
    let fast = scale(unshifted, 1, adj);
    let want = f64::from(u32::try_from(s >> 20).unwrap()) * 1.0001;
    let got = f64::from(u32::try_from(fast >> 20).unwrap());
    assert!((got - want).abs() <= 1.0, "{got} vs {want}");
    // FreeBSD's divide-before-multiply form, for comparison.
    let fbsd = {
        let mut sc: u64 = 1 << 63;
        sc = sc.wrapping_add_signed((adj / 1024) * 2199);
        sc /= f;
        sc * 2
    };
    let diff = fbsd.abs_diff(fast);
    // Its error is about 10 ppm of the adjustment: 1e-9 of the scale.
    assert!(diff < fast / 100_000_000, "{diff}");
    // -500 ppm.
    let slow = scale(unshifted, 1, -500_000 * FRAC);
    assert!(slow < s);
}

#[test]
fn slow_pad_rounds_up() {
    assert_eq!(slow_pad(0), 0);
    assert_eq!(slow_pad(1000 * FRAC), 0);
    // 500 ppm slow: 0.0005 / 0.9995 more.
    let p = slow_pad(-500_000 * FRAC);
    let exact = 0.0005 / 0.9995 * 4_294_967_296.0;
    assert!((f64::from(u32::try_from(p).unwrap()) - exact).abs() < 2.0);
    // A 1 s sleep on the counter, padded, is at least 1 s on the clock.
    let ticks: u128 = 1_000_000_000;
    let padded = ticks + ((ticks * u128::from(p)) >> 32) + 1;
    let on_clock = padded as f64 * (1.0 - 0.0005);
    assert!(on_clock >= 1e9);
}

/// The loop closed: a counter 50 ppm fast, an ideal server polled every
/// 64 s, and the offsets `ntpd` would hand the kernel. The frequency must
/// settle on -50 ppm and the offset near zero, as FreeBSD's loop does.
#[test]
fn the_loop_converges_on_the_counters_error() {
    let err_ppb: i128 = 50_000; // the counter runs 50 ppm fast
    let mut d = Discipline::new();
    // Poll 64 s: ntpd passes the poll exponent minus 4 as the constant.
    synced(&mut d, 2);
    // Local clock minus true time, in ns × 2^32.
    let mut lag: i128 = 3_000_000i128 << 32; // 3 ms ahead to start
    let mut adj: i64 = 0;
    let mut sec = 1_700_000_000i64;
    for up in 2i64..(3 * 3600) {
        // One second: the counter's error, plus the discipline's.
        lag += (err_ppb << 32) + i128::from(adj);
        sec += 1;
        adj = d.update_second(&mut sec);
        if up % 64 == 0 {
            let mut t = tx(modes::MOD_OFFSET);
            // The offset is true minus local.
            t.offset = i64::try_from(-(lag >> 32)).unwrap();
            d.adjust(&mut t, up);
        }
    }
    let freq_ppb = gint(d.time_freq());
    assert!(
        (freq_ppb + 50_000).abs() < 500,
        "frequency {freq_ppb} ns/s, expected about -50000"
    );
    let off_us = (lag >> 32) / 1000;
    assert!(off_us.abs() < 100, "offset {off_us} us");
}
