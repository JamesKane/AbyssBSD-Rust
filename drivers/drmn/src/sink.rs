// SPDX-License-Identifier: BSD-2-Clause
//! The monitor on the HDMI connector (the "sink"): which video modes the card
//! offers for it. The card can set a fixed table of CTA-861 modes (the ones
//! the pixel-clock path has been run for); what the monitor's EDID lists
//! selects among them and says which is preferred. A monitor that gives no
//! usable EDID, or none of those modes, gets the whole table.

use alloc::vec::Vec;

use abi::drm::DRM_MODE_TYPE_PREFERRED;
use kms_check::Mode;

/// The CTA-861 codes the card can set, 1080p60 first: the table the pixel
/// clock path was run for on the board (1080p60 and 50, 720p60 and 50, 480p
/// and 576p in 4:3 or 16:9, and 640 by 480).
pub(crate) const SETTABLE: [u8; 7] = [16, 31, 4, 19, 3, 18, 1];

/// The whole table, 1080p60 preferred.
pub(crate) fn settable() -> Vec<(u8, Mode)> {
    SETTABLE
        .into_iter()
        .filter_map(|vic| {
            let mut m = edid::mode_for_vic(vic)?;
            // The generator runs 640 by 480 at the VGA clock.
            if vic == 1 {
                m.clock_khz = 25_175;
            }
            if vic == 16 {
                m.kind |= DRM_MODE_TYPE_PREFERRED;
            }
            Some((vic, m))
        })
        .collect()
}

/// Whether two timings are the same mode: the same pictures, totals and
/// (within 1%: the VGA mode is 25.175 or 25.2 megahertz) pixel clock. The
/// clock matters: 1080p at 25 and at 50 hertz have the same totals.
fn same_timing(a: &Mode, b: &Mode) -> bool {
    let close = u64::from(a.clock_khz) * 100 / u64::from(b.clock_khz.max(1));
    (a.hdisplay, a.vdisplay, a.htotal, a.vtotal) == (b.hdisplay, b.vdisplay, b.htotal, b.vtotal)
        && (99..=101).contains(&close)
}

/// The modes to offer for a monitor whose EDID is `edid`: those of the table
/// that the EDID lists, the monitor's preferred first among them if it is
/// one, otherwise 1080p60 if offered, otherwise the first.
pub(crate) fn offered(edid: Option<&edid::Edid>) -> Vec<(u8, Mode)> {
    let all = settable();
    let Some(e) = edid else {
        return all;
    };
    let mut modes: Vec<(u8, Mode)> = all
        .iter()
        .filter(|(_, m)| e.modes.iter().any(|l| same_timing(l, m)))
        .map(|(v, m)| {
            let mut m = *m;
            m.kind &= !DRM_MODE_TYPE_PREFERRED;
            (*v, m)
        })
        .collect();
    if modes.is_empty() {
        return all;
    }
    // The preferred one: the monitor's first (native) timing if the table
    // has it, else 1080p60, else the first offered.
    let native = e.modes.first();
    let at = native
        .and_then(|n| modes.iter().position(|(_, m)| same_timing(n, m)))
        .or_else(|| modes.iter().position(|(v, _)| *v == 16))
        .unwrap_or(0);
    modes[at].1.kind |= DRM_MODE_TYPE_PREFERRED;
    // The preferred first, the rest in the table's order.
    let first = modes.remove(at);
    modes.insert(0, first);
    modes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EDID that lists the given CTA codes, the first its native timing.
    fn edid_with(vics: &[u8]) -> edid::Edid {
        edid::Edid {
            name: None,
            size_mm: (0, 0),
            modes: vics.iter().filter_map(|v| edid::mode_for_vic(*v)).collect(),
            extensions: 1,
            bpc: None,
            manufacturer: 0,
            product: 0,
            audio: edid::Audio::default(),
        }
    }

    fn vics(m: &[(u8, Mode)]) -> Vec<u8> {
        m.iter().map(|(v, _)| *v).collect()
    }

    fn preferred(m: &[(u8, Mode)]) -> Vec<u8> {
        m.iter()
            .filter(|(_, m)| m.kind & DRM_MODE_TYPE_PREFERRED != 0)
            .map(|(v, _)| *v)
            .collect()
    }

    #[test]
    fn without_an_edid_the_whole_table_is_offered_with_1080p_preferred() {
        let m = offered(None);
        assert_eq!(vics(&m), SETTABLE);
        assert_eq!(preferred(&m), [16]);
        // The VGA mode runs at the VGA clock.
        assert_eq!(m.last().unwrap().1.clock_khz, 25_175);
    }

    #[test]
    fn a_monitor_selects_among_the_modes_the_card_can_set() {
        // A monitor with 1080p60, 720p60 and 1080p24 (which the card cannot set).
        let e = edid_with(&[16, 4, 32]);
        let m = offered(Some(&e));
        assert_eq!(vics(&m), [16, 4]);
        assert_eq!(preferred(&m), [16]);
    }

    #[test]
    fn the_monitors_native_mode_is_preferred_when_the_card_can_set_it() {
        // 720p60 first (its native timing), then 1080p60.
        let e = edid_with(&[4, 16]);
        let m = offered(Some(&e));
        assert_eq!(vics(&m), [4, 16]);
        assert_eq!(preferred(&m), [4]);
    }

    #[test]
    fn a_native_mode_the_card_cannot_set_leaves_1080p_preferred() {
        let e = edid_with(&[32, 16, 31]);
        let m = offered(Some(&e));
        assert_eq!(vics(&m), [16, 31]);
        assert_eq!(preferred(&m), [16]);
    }

    #[test]
    fn without_1080p60_the_first_offered_is_preferred() {
        let e = edid_with(&[31, 4]);
        let m = offered(Some(&e));
        // The table's order after the preferred: 1080p50 (31), 720p60 (4).
        assert_eq!(vics(&m), [31, 4]);
        assert_eq!(preferred(&m), [31]);
    }

    #[test]
    fn a_mode_with_the_same_totals_but_another_rate_is_not_the_same_mode() {
        // 1080p at 25 hertz has the totals of 1080p at 50 hertz.
        let e = edid_with(&[33]);
        assert_eq!(vics(&offered(Some(&e))), SETTABLE, "25 hertz is not 50");
        let e = edid_with(&[33, 31]);
        assert_eq!(vics(&offered(Some(&e))), [31]);
    }

    #[test]
    fn a_monitor_with_none_of_them_gets_the_whole_table() {
        let e = edid_with(&[32, 33]);
        assert_eq!(vics(&offered(Some(&e))), SETTABLE);
        let none = edid_with(&[]);
        assert_eq!(vics(&offered(Some(&none))), SETTABLE);
    }
}
