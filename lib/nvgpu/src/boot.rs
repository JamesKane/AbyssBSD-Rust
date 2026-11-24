// SPDX-License-Identifier: BSD-2-Clause
//! Running the firmware security microcode's FRTS command on an Ada GPU
//! (specification 02, section 5.5.4; 03, section 3.2 step 4), with each step
//! announced before it is done, so a log that stops names the last one.

use crate::falcon::{self, FRTS_ERR, FalconError, GSP, Registers};
use crate::fwsec::Fwsec;
use crate::mem::{self, Region};

/// Why the command did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootError {
    /// The protected region 2 is already set up: a GPU reset (or a power
    /// cycle) is needed before this can run.
    Wpr2Active,
    /// The engine sequence failed.
    Falcon(FalconError),
    /// The microcode reported an error code in `NV_PBUS_SW_SCRATCH[14]`.
    FrtsError(u32),
    /// The microcode ran but the protected region is not where the layout
    /// says (or not there at all).
    Wpr2Mismatch {
        /// What the registers show: `(lower, upper)`, or `None` if unset.
        found: Option<(u64, u64)>,
    },
}

impl From<FalconError> for BootError {
    fn from(e: FalconError) -> Self {
        Self::Falcon(e)
    }
}

/// The protected region 2's bounds, from the registers.
pub fn wpr2<R: Registers + ?Sized>(r: &R) -> Option<(u64, u64)> {
    mem::wpr2_bounds(
        r.read32(mem::NV_PFB_PRI_MMU_WPR2_ADDR_LO),
        r.read32(mem::NV_PFB_PRI_MMU_WPR2_ADDR_HI),
    )
}

/// Runs the prepared FRTS microcode: checks the protected region is not
/// already set up, resets the GPU system processor, loads and runs the
/// microcode from the DMA buffer at `dma`, and checks its result, which is
/// the microcode's own error code and the region it created, `frts`.
///
/// `boot_0` is the chip's identification register, recorded in the engine
/// after its reset. `log` is told each step before it is taken.
///
/// # Errors
///
/// [`BootError`], with nothing undone: a failed run leaves the engine as it
/// is for inspection.
pub fn run_frts<R: Registers + ?Sized>(
    r: &R,
    boot_0: u32,
    fw: &Fwsec,
    dma: u64,
    frts: Region,
    log: &mut dyn FnMut(&str),
) -> Result<(), BootError> {
    log("checking that protected region 2 is not set up");
    if wpr2(r).is_some() {
        return Err(BootError::Wpr2Active);
    }
    log("resetting the GPU system processor");
    falcon::reset(r, GSP, boot_0)?;
    log("loading and starting the firmware security microcode");
    falcon::run_signed(r, GSP, dma, fw.imem, fw.dmem, fw.rom, 0)?;
    log("microcode halted with status 0; reading its error code");
    let err = r.read32(FRTS_ERR) >> 16;
    if err != 0 {
        return Err(BootError::FrtsError(err));
    }
    log("checking the protected region it created");
    let found = wpr2(r);
    match found {
        Some((lo, _)) if lo == frts.start => Ok(()),
        _ => Err(BootError::Wpr2Mismatch { found }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::falcon::tests::Model;
    use crate::falcon::{Rom, Segment};
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    fn fw() -> Fwsec {
        Fwsec {
            image: alloc::vec![0; 1024],
            imem: Segment {
                src: 0,
                dst: 0,
                len: 512,
            },
            dmem: Segment {
                src: 512,
                dst: 0,
                len: 256,
            },
            rom: Rom {
                pkc_data_offset: 0x10,
                engine_id_mask: 0x400,
                ucode_id: 9,
            },
        }
    }

    const FRTS: Region = Region {
        start: 0x2_ff70_0000,
        end: 0x2_ff80_0000,
    };

    /// The region registers the microcode would leave, once it ran.
    fn after_run(m: &Model, lo: u32, hi: u32, err: u32) {
        let mut r = m.regs.borrow_mut();
        r.insert(mem::NV_PFB_PRI_MMU_WPR2_ADDR_LO, lo);
        r.insert(mem::NV_PFB_PRI_MMU_WPR2_ADDR_HI, hi);
        r.insert(FRTS_ERR, err << 16);
    }

    #[test]
    fn it_runs_in_order_and_announces_each_step() {
        let m = Model::default();
        // Registers that read as unset until the microcode ran: the model
        // returns them as written, so set them after (they read 0 first).
        let mut steps: Vec<String> = Vec::new();
        // First check passes with them unset; then set them for the check
        // after the run by hooking the last log line.
        let r = run_frts(&m, 0x1940_00a1, &fw(), 0x10_0000, FRTS, &mut |s| {
            steps.push(s.to_string());
            if s.starts_with("microcode halted") {
                let reg = |a: u64| u32::try_from((a >> 12) << 4).unwrap();
                after_run(&m, reg(FRTS.start), reg(FRTS.end), 0);
            }
        });
        assert_eq!(r, Ok(()));
        assert_eq!(steps.len(), 5);
        assert!(steps[0].contains("not set up") && steps[1].contains("resetting"));
        assert!(steps[2].contains("loading") && steps[4].contains("created"));
    }

    #[test]
    fn a_protected_region_already_there_stops_everything() {
        let m = Model::default();
        after_run(&m, 0x1000, 0x2000, 0);
        let r = run_frts(&m, 0, &fw(), 0x10_0000, FRTS, &mut |_| {});
        assert_eq!(r, Err(BootError::Wpr2Active));
        assert!(m.writes().is_empty(), "nothing was written");
    }

    #[test]
    fn the_microcodes_error_code_is_reported() {
        let m = Model::default();
        let r = run_frts(&m, 0, &fw(), 0x10_0000, FRTS, &mut |s| {
            if s.starts_with("microcode halted") {
                after_run(&m, 0, 0, 0x7);
            }
        });
        assert_eq!(r, Err(BootError::FrtsError(7)));
    }

    #[test]
    fn a_region_that_is_not_where_expected_is_reported() {
        let m = Model::default();
        let r = run_frts(&m, 0, &fw(), 0x10_0000, FRTS, &mut |s| {
            if s.starts_with("microcode halted") {
                after_run(&m, 0x0000_0010, 0x0000_0020, 0);
            }
        });
        assert_eq!(
            r,
            Err(BootError::Wpr2Mismatch {
                found: Some((0x1000, 0x2000))
            })
        );
        // Not set up at all.
        let m = Model::default();
        let r = run_frts(&m, 0, &fw(), 0x10_0000, FRTS, &mut |_| {});
        assert_eq!(r, Err(BootError::Wpr2Mismatch { found: None }));
    }

    #[test]
    fn engine_failures_are_passed_up() {
        let m = Model {
            status: 5,
            ..Model::default()
        };
        let r = run_frts(&m, 0, &fw(), 0x10_0000, FRTS, &mut |_| {});
        assert_eq!(
            r,
            Err(BootError::Falcon(FalconError::Failed { mailbox0: 5 }))
        );
    }
}
