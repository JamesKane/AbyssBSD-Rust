// SPDX-License-Identifier: BSD-2-Clause
//! The GPU system processor's microcontroller (a Falcon): reading its
//! state, resetting it, loading a signed microcode into it by framebuffer
//! DMA, and running that to its halt (specification 02, sections 2 and 3,
//! for the Ada generation, `GA102` on).
//!
//! The sequences run against [`Registers`], so the order of every write is
//! asserted on the host before any of it runs on a card. Every wait is
//! bounded and returns an error rather than blocking forever.

use alloc::vec::Vec;

/// What the sequences need of the GPU: its register aperture (BAR0) and a
/// busy wait.
pub trait Registers {
    /// Reads the 32-bit register at byte `offset` of BAR0.
    fn read32(&self, offset: usize) -> u32;
    /// Writes it.
    fn write32(&self, offset: usize, value: u32);
    /// Busy-waits `us` microseconds.
    fn delay_us(&self, us: u32);
}

/// A Falcon instance: the two register windows it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engine {
    /// The `PFALCON` window.
    pub base: usize,
    /// The `PFALCON2` / `PRISCV` window (`base + 0x1000`).
    pub base2: usize,
}

/// The GPU system processor.
pub const GSP: Engine = Engine {
    base: 0x0011_0000,
    base2: 0x0011_1000,
};
/// The second security engine.
pub const SEC2: Engine = Engine {
    base: 0x0084_0000,
    base2: 0x0084_1000,
};

// `PFALCON` register offsets (02, section 2.3).
const IRQSCLR: usize = 0x0004;
const MAILBOX0: usize = 0x0040;
const MAILBOX1: usize = 0x0044;
const RM: usize = 0x0084;
const HWCFG2: usize = 0x00f4;
const CPUCTL: usize = 0x0100;
const BOOTVEC: usize = 0x0104;
const DMACTL: usize = 0x010c;
const DMATRFBASE: usize = 0x0110;
const DMATRFMOFFS: usize = 0x0114;
const DMATRFCMD: usize = 0x0118;
const DMATRFFBOFFS: usize = 0x011c;
const DMATRFBASE1: usize = 0x0128;
const HWCFG1: usize = 0x012c;
const CPUCTL_ALIAS: usize = 0x0130;
const ENGINE: usize = 0x03c0;
const FBIF_TRANSCFG0: usize = 0x0600;
const FBIF_CTL: usize = 0x0624;
// `PFALCON2` register offsets (02, section 2.4).
const MOD_SEL: usize = 0x0180;
const BROM_CURR_UCODE_ID: usize = 0x0198;
const BROM_ENGIDMASK: usize = 0x019c;
const BROM_PARAADDR0: usize = 0x0210;
const BCR_CTRL: usize = 0x0668;

/// The error code register the firmware security microcode writes its
/// result to (`NV_PBUS_SW_SCRATCH[14]`; the code is bits 31:16).
pub const FRTS_ERR: usize = 0x0000_1438;

/// A block of the framebuffer DMA engine, in bytes.
const BLOCK: usize = 256;

/// Why a sequence failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FalconError {
    /// A wait ran out; says which.
    Timeout(&'static str),
    /// A DMA address that is not 256-byte aligned, or a size or offset
    /// that does not fit.
    BadArgument(&'static str),
    /// A register read as the hardware's "access blocked" pattern
    /// (`0xbadf....`): the engine is still locked down.
    Blocked(&'static str),
    /// The microcode halted with a nonzero status in `MAILBOX0`.
    Failed {
        /// The status.
        mailbox0: u32,
    },
}

/// What the engine's registers say, read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    /// `HWCFG1`: core revision, security model, subversion.
    pub hwcfg1: u32,
    /// `HWCFG2`: scrubbing, reset readiness, whether a RISC-V core exists.
    pub hwcfg2: u32,
    /// `CPUCTL`: halted, alias enable.
    pub cpuctl: u32,
    /// `BCR_CTRL`: which core is selected.
    pub bcr_ctrl: u32,
    /// `MAILBOX0`.
    pub mailbox0: u32,
    /// `MAILBOX1`.
    pub mailbox1: u32,
}

/// Reads the engine's state.
pub fn state<R: Registers + ?Sized>(r: &R, e: Engine) -> State {
    State {
        hwcfg1: r.read32(e.base + HWCFG1),
        hwcfg2: r.read32(e.base + HWCFG2),
        cpuctl: r.read32(e.base + CPUCTL),
        bcr_ctrl: r.read32(e.base2 + BCR_CTRL),
        mailbox0: r.read32(e.base + MAILBOX0),
        mailbox1: r.read32(e.base + MAILBOX1),
    }
}

/// Polls `done` every `step_us` for up to `total_us`.
fn wait<R: Registers + ?Sized>(
    r: &R,
    total_us: u32,
    step_us: u32,
    what: &'static str,
    mut done: impl FnMut(&R) -> bool,
) -> Result<(), FalconError> {
    let mut waited = 0;
    loop {
        if done(r) {
            return Ok(());
        }
        if waited >= total_us {
            return Err(FalconError::Timeout(what));
        }
        r.delay_us(step_us);
        waited += step_us;
    }
}

/// Resets the engine, waits for its memory to scrub, selects the Falcon
/// core, and records the chip's identification (`boot_0`) in its `RM`
/// register (02, section 3.1, `GA102` on).
///
/// # Errors
///
/// [`FalconError::Timeout`] if scrubbing or the core switch does not finish.
pub fn reset<R: Registers + ?Sized>(r: &R, e: Engine, boot_0: u32) -> Result<(), FalconError> {
    // Wait for reset readiness, up to 150 us; some revisions never assert
    // it, so a timeout is not an error.
    let _ = wait(r, 150, 10, "reset ready", |r| {
        r.read32(e.base + HWCFG2) >> 31 & 1 != 0
    });
    // Pulse the engine reset.
    r.write32(e.base + ENGINE, r.read32(e.base + ENGINE) | 1);
    r.delay_us(10);
    r.write32(e.base + ENGINE, r.read32(e.base + ENGINE) & !1);
    let scrubbed = |r: &R| r.read32(e.base + HWCFG2) >> 12 & 1 == 0;
    wait(r, 20_000, 100, "memory scrubbing", scrubbed)?;
    // The Falcon core, not the RISC-V one. A register that still reads as
    // blocked after the reset means the lockdown was not lifted: go no
    // further.
    let bcr = r.read32(e.base2 + BCR_CTRL);
    if bcr & 0xffff_0000 == 0xbadf_0000 {
        return Err(FalconError::Blocked("core selection"));
    }
    if bcr >> 4 & 1 != 0 {
        r.write32(e.base2 + BCR_CTRL, 0);
        wait(r, 10_000, 100, "core switch", |r| {
            r.read32(e.base2 + BCR_CTRL) & 1 != 0
        })?;
    }
    wait(r, 20_000, 100, "memory scrubbing", scrubbed)?;
    r.write32(e.base + RM, boot_0);
    Ok(())
}

/// One segment to load by DMA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Source offset in the DMA buffer.
    pub src: usize,
    /// Destination offset in the Falcon's memory.
    pub dst: usize,
    /// Length in bytes.
    pub len: usize,
}

/// What the boot ROM verifies the microcode against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rom {
    /// Where in data memory the signature was patched.
    pub pkc_data_offset: u32,
    /// The engines it may run on.
    pub engine_id_mask: u16,
    /// Its microcode ID.
    pub ucode_id: u8,
}

/// Loads `secure_imem` and `dmem` from the DMA buffer at `dma` (256-byte
/// aligned, its size rounded up to 256), programs the boot ROM, starts the
/// Falcon at `boot_addr` with the mailboxes zeroed, waits for it to halt
/// and returns its `MAILBOX0` status (02, sections 3.2.2 and 3.4).
///
/// # Errors
///
/// [`FalconError`]: a misaligned buffer, a wait that ran out, or a nonzero
/// status.
pub fn run_signed<R: Registers + ?Sized>(
    r: &R,
    e: Engine,
    dma: u64,
    secure_imem: Segment,
    dmem: Segment,
    rom: Rom,
    boot_addr: u32,
) -> Result<(), FalconError> {
    if dma & (BLOCK as u64 - 1) != 0 {
        return Err(FalconError::BadArgument("DMA address not 256-byte aligned"));
    }
    // Physical DMA, no context needed; aperture 0 is coherent system memory.
    r.write32(e.base + FBIF_CTL, r.read32(e.base + FBIF_CTL) | 1 << 7);
    r.write32(e.base + DMACTL, 0);
    let t = r.read32(e.base + FBIF_TRANSCFG0);
    r.write32(e.base + FBIF_TRANSCFG0, (t & !0b111) | 1 | 1 << 2);
    for (seg, imem) in [(secure_imem, true), (dmem, false)] {
        transfer(r, e, dma, seg, imem)?;
    }
    // The boot ROM's parameters.
    r.write32(e.base2 + BROM_PARAADDR0, rom.pkc_data_offset);
    r.write32(e.base2 + BROM_ENGIDMASK, u32::from(rom.engine_id_mask));
    r.write32(e.base2 + BROM_CURR_UCODE_ID, u32::from(rom.ucode_id));
    r.write32(e.base2 + MOD_SEL, 1);
    r.write32(e.base + BOOTVEC, boot_addr);
    r.write32(e.base + MAILBOX0, 0);
    r.write32(e.base + MAILBOX1, 0);
    // Start the CPU, through the alias register if it asks for that.
    if r.read32(e.base + CPUCTL) >> 6 & 1 != 0 {
        r.write32(e.base + CPUCTL_ALIAS, 1 << 1);
    } else {
        r.write32(e.base + CPUCTL, 1 << 1);
    }
    wait(r, 2_000_000, 1_000, "microcode halt", |r| {
        r.read32(e.base + CPUCTL) >> 4 & 1 != 0
    })?;
    let status = r.read32(e.base + MAILBOX0);
    if status != 0 {
        return Err(FalconError::Failed { mailbox0: status });
    }
    Ok(())
}

/// Transfers one segment in 256-byte blocks.
fn transfer<R: Registers + ?Sized>(
    r: &R,
    e: Engine,
    dma: u64,
    seg: Segment,
    imem: bool,
) -> Result<(), FalconError> {
    let blocks = seg.len.div_ceil(BLOCK);
    // For instruction memory the framebuffer offset also supplies the
    // virtual tag of each block, so the base is the buffer's start and the
    // offset the source; for data memory the base moves and the offset is 0.
    let (base, fb_start) = if imem {
        (dma, seg.src)
    } else {
        (
            dma.checked_add(
                u64::try_from(seg.src).map_err(|_| FalconError::BadArgument("source"))?,
            )
            .ok_or(FalconError::BadArgument("source"))?,
            0,
        )
    };
    if base & (BLOCK as u64 - 1) != 0 {
        return Err(FalconError::BadArgument("segment not 256-byte aligned"));
    }
    r.write32(
        e.base + DMATRFBASE,
        u32::try_from(base >> 8 & 0xffff_ffff).unwrap_or(0),
    );
    r.write32(
        e.base + DMATRFBASE1,
        u32::try_from(base >> 40 & 0x1ff).unwrap_or(0),
    );
    // Size 256 bytes (6), instruction memory (bit 4), secure (bits 3:2 = 1).
    let cmd = 6 << 8 | u32::from(imem) << 4 | u32::from(imem) << 2;
    for i in 0..blocks {
        let pos = i * BLOCK;
        let dst =
            u32::try_from(seg.dst + pos).map_err(|_| FalconError::BadArgument("destination"))?;
        let fb = u32::try_from(fb_start + pos).map_err(|_| FalconError::BadArgument("source"))?;
        r.write32(e.base + DMATRFMOFFS, dst);
        r.write32(e.base + DMATRFFBOFFS, fb);
        r.write32(e.base + DMATRFCMD, cmd);
        wait(r, 2_000_000, 10, "DMA transfer", |r| {
            r.read32(e.base + DMATRFCMD) >> 1 & 1 != 0
        })?;
    }
    Ok(())
}

/// Clears the engine's pending interrupts, which the sequences leave as
/// they find them: exposed for a driver that wants a clean state after a
/// halt.
pub fn clear_interrupts<R: Registers + ?Sized>(r: &R, e: Engine) {
    r.write32(e.base + IRQSCLR, u32::MAX);
}

/// Pads `bytes` with zeros to a multiple of the DMA block.
#[must_use]
pub fn pad_to_block(mut bytes: Vec<u8>) -> Vec<u8> {
    let padded = bytes.len().next_multiple_of(BLOCK);
    bytes.resize(padded, 0);
    bytes
}

#[cfg(test)]
#[path = "falcon_tests.rs"]
pub(crate) mod tests;
