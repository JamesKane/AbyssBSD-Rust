// SPDX-License-Identifier: BSD-2-Clause
//! AMD's Collaborative Processor Performance Control registers and the
//! policy `hwpstate_amd` applies to them: the fields of the capability and
//! request MSRs, and the request FreeBSD programs when it enables CPPC.
//!
//! The registers are per core. `CPPC_CAPS_1` says what the core can do
//! (the performance scale is the hardware's own, 0 to 255, and differs
//! from core to core on a processor with preferred cores); `CPPC_REQUEST`
//! says what the OS wants within that: a minimum and maximum performance,
//! a desired performance (0 leaves the choice to the hardware, which is
//! autonomous mode), and an energy/performance preference (0 favours
//! performance, 255 efficiency).
//!
//! Behavior reference: sys/x86/cpufreq/hwpstate_amd.c.

/// `MSR_AMD_CPPC_CAPS_1`: what the core can do (read-only).
pub const MSR_CAPS_1: u32 = 0xc001_02b0;
/// `MSR_AMD_CPPC_ENABLE`: bit 0 turns CPPC on, and it stays on.
pub const MSR_ENABLE: u32 = 0xc001_02b1;
/// `MSR_AMD_CPPC_CAPS_2`.
pub const MSR_CAPS_2: u32 = 0xc001_02b2;
/// `MSR_AMD_CPPC_REQUEST`: what the OS wants.
pub const MSR_REQUEST: u32 = 0xc001_02b3;
/// `MSR_AMD_CPPC_STATUS`.
pub const MSR_STATUS: u32 = 0xc001_02b4;
/// The maximum-performance counter, which counts at the TSC's rate while
/// the core runs.
pub const MSR_MPERF: u32 = 0xe7;
/// The actual-performance counter, which counts at the core's own rate.
pub const MSR_APERF: u32 = 0xe8;

/// The bits of `CPPC_CAPS_1`.
pub mod caps {
    /// Highest performance.
    pub const HIGHEST: u64 = 0xff00_0000;
    /// Guaranteed (nominal) performance.
    pub const NOMINAL: u64 = 0x00ff_0000;
    /// Most efficient performance.
    pub const EFFICIENT: u64 = 0x0000_ff00;
    /// Lowest performance.
    pub const LOWEST: u64 = 0x0000_00ff;
}

/// The bits of `CPPC_REQUEST`.
pub mod request {
    /// Efficiency/performance preference.
    pub const EPP: u64 = 0xff00_0000;
    /// Desired performance.
    pub const DESIRED: u64 = 0x00ff_0000;
    /// Minimum performance.
    pub const MIN: u64 = 0x0000_ff00;
    /// Maximum performance.
    pub const MAX: u64 = 0x0000_00ff;
}

/// The value of the field `bits` in `reg` (`BITS_VALUE`).
#[must_use]
pub const fn field(bits: u64, reg: u64) -> u64 {
    (reg & bits) >> bits.trailing_zeros()
}

/// `value` placed in the field `bits` (`BITS_WITH_VALUE`); what does not
/// fit is cut off.
#[must_use]
pub const fn in_field(bits: u64, value: u64) -> u64 {
    (value << bits.trailing_zeros()) & bits
}

/// `reg` with the field `bits` set to `value` (`SET_BITS_VALUE`).
#[must_use]
pub const fn with_field(reg: u64, bits: u64, value: u64) -> u64 {
    (reg & !bits) | in_field(bits, value)
}

/// What a write of `value` to one request field changes: the register's
/// new content, if `value` fits the field (`sysctl_cppc_request_field_handler`
/// refuses what is above the field's maximum).
#[must_use]
pub const fn updated(current: u64, bits: u64, value: u64) -> Option<u64> {
    if value > field(bits, u64::MAX) {
        None
    } else {
        Some(with_field(current, bits, value))
    }
}

/// The request FreeBSD writes when it enables CPPC on a core, from the
/// request the core holds and its capabilities (`None` if they could not
/// be read).
///
/// The minimum and maximum are the core's lowest and highest performance;
/// when they are missing or not in order (a reset value of 0 is seen in
/// the field), ACPI's defaults stand: all zeros for the minimum, all ones
/// for the maximum. The preference is 0, the most performant, and the
/// desired performance is the highest one, so that the core runs flat out
/// as it did without CPPC. (A desired performance of 0 would be
/// autonomous mode.)
#[must_use]
pub const fn initial_request(held: u64, capabilities: Option<u64>) -> u64 {
    let (mut lowest, mut highest) = (0, 0xff);
    if let Some(c) = capabilities {
        let (l, h) = (field(caps::LOWEST, c), field(caps::HIGHEST, c));
        if l < h {
            lowest = l;
            highest = h;
        }
    }
    let mut r = with_field(held, request::MIN, lowest);
    r = with_field(r, request::MAX, highest);
    r = with_field(r, request::EPP, 0);
    with_field(r, request::DESIRED, highest)
}

/// A core's clock rate in Hz from two samples of the performance
/// counters and the time-stamp counter, taken `tsc` ticks apart over a
/// busy interval (`cpu_est_clockrate`): the TSC rate times the ratio the
/// core's own counter ran at the reference counter's. `tsc_hz` is the
/// TSC's frequency. `None` if the reference counter did not move (the
/// core was halted, or the counters do not exist).
#[must_use]
pub fn clock_rate_hz(tsc_hz: u64, mperf: u64, aperf: u64) -> Option<u64> {
    if mperf == 0 {
        return None;
    }
    let hz = u128::from(tsc_hz) * u128::from(aperf) / u128::from(mperf);
    u64::try_from(hz).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        assert_eq!(field(request::DESIRED, 0x1234_5678), 0x34);
        assert_eq!(field(caps::HIGHEST, 0xa6_00_00_00), 0xa6);
        assert_eq!(in_field(request::MIN, 0x12), 0x1200);
        // What does not fit is cut off, not spilled into a neighbour.
        assert_eq!(in_field(request::MIN, 0x1ff), 0xff00);
        assert_eq!(with_field(0xffff_ffff, request::DESIRED, 0), 0xff00_ffff);
    }

    #[test]
    fn a_write_must_fit_its_field() {
        let held = 0x0000_c0de;
        assert_eq!(updated(held, request::EPP, 0x40), Some(0x4000_c0de));
        assert_eq!(updated(held, request::MAX, 255), Some(0x0000_c0ff));
        assert_eq!(updated(held, request::MAX, 256), None);
    }

    /// A Zen 4 core: highest 166, nominal 118, efficient 75, lowest 25
    /// (Linux's amd-pstate on the X870E reports a highest performance of
    /// 166).
    const ZEN4: u64 = 166 << 24 | 118 << 16 | 75 << 8 | 25;

    #[test]
    fn the_request_runs_the_core_flat_out_within_its_capabilities() {
        let r = initial_request(0, Some(ZEN4));
        assert_eq!(field(request::MIN, r), 25);
        assert_eq!(field(request::MAX, r), 166);
        assert_eq!(field(request::DESIRED, r), 166);
        assert_eq!(field(request::EPP, r), 0);
    }

    #[test]
    fn missing_or_unsorted_capabilities_fall_back_to_acpis_defaults() {
        for caps in [None, Some(0), Some(50 << 24 | 50), Some(10 << 24 | 90)] {
            let r = initial_request(0, caps);
            assert_eq!(field(request::MIN, r), 0, "{caps:?}");
            assert_eq!(field(request::MAX, r), 255, "{caps:?}");
            assert_eq!(field(request::DESIRED, r), 255, "{caps:?}");
        }
    }

    #[test]
    fn the_clock_rate_is_the_tsc_rate_scaled_by_the_counters() {
        // 4.69 GHz TSC; the core ran at 5.4 GHz: the actual counter
        // counted 1.151 times faster.
        let hz = clock_rate_hz(4_691_215_700, 4_691_000, 5_400_000).unwrap();
        assert!((5_400_000_000..=5_401_000_000).contains(&hz), "{hz}");
        assert_eq!(
            clock_rate_hz(4_000_000_000, 1000, 1000),
            Some(4_000_000_000)
        );
        assert_eq!(clock_rate_hz(4_000_000_000, 0, 1000), None);
    }
}
