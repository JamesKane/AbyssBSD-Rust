// SPDX-License-Identifier: BSD-2-Clause
//! Finding out what the display processor has: identification, the soft
//! reset, and the walk over its self-describing blocks (specification
//! `linlon-dpu` 03 §3.1, §3.2).

use alloc::vec::Vec;

use crate::regs::{self, Words, common, gcu, kind, layer};

/// Waits the hardware's protocols need.
pub trait Delay: Send + Sync {
    /// Busy-waits `us` microseconds.
    fn delay_us(&self, us: u64);
}

/// Why the device is not one this driver drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A product other than the three of this register model; the value
    /// is `GLB_CORE_ID[31:16]`.
    Product(u32),
    /// The soft reset did not finish.
    ResetTimeout,
    /// More pipelines than the register model has (two), or none.
    Pipelines(u32),
    /// A block of one pipeline appears twice, or a pipeline's layers or
    /// scalers are not numbered from zero without a gap.
    Layout,
    /// A pipeline lacks a block that it cannot work without.
    Incomplete,
    /// The window is too small for the blocks the device announces.
    Window,
}

/// A block found in the walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// Byte offset in the window.
    pub offset: usize,
    /// `BLOCK_INFO`, from which input IDs are made.
    pub info: u32,
    /// `MAX_LINE_SIZE` of the block: the longest line in pixels.
    pub max_line: u32,
}

impl Block {
    /// The input ID that connects to this block's output `port`.
    #[must_use]
    pub fn input_id(&self, port: u32) -> u32 {
        regs::input_id(self.info, port)
    }
}

/// A read layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer {
    pub block: Block,
    /// Takes YUV (has chroma upsampling).
    pub rich: bool,
    /// The longest line of a chroma-subsampled format.
    pub max_line_yuv: u32,
}

/// What one pipeline is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipe {
    pub lpu: Block,
    pub layers: Vec<Layer>,
    pub writeback: Option<Block>,
    pub cu: Block,
    pub scalers: Vec<Block>,
    pub splitter: Option<Block>,
    pub merger: Option<Block>,
    pub dou: Block,
    pub ips: Block,
    pub bs: Block,
    /// The forward-transform (gamma) table.
    pub ft: Option<Block>,
}

/// What the device is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// `GLB_CORE_ID`.
    pub core_id: u32,
    pub arch_id: u32,
    /// Longest line any block takes, and the most lines.
    pub max_line: u32,
    pub max_lines: u32,
    /// Two links of one pipeline can carry the halves of a line.
    pub dual_link: bool,
    /// The device has translation units of its own (section 03 §3.7).
    pub integrated_tbu: bool,
    pub pipes: Vec<Pipe>,
}

impl Layout {
    /// The product, `GLB_CORE_ID[31:16]`.
    #[must_use]
    pub fn product(&self) -> u32 {
        self.core_id >> 16
    }
}

const fn accepts(product: u32) -> bool {
    matches!(
        product,
        gcu::PRODUCT_D6 | gcu::PRODUCT_D71 | gcu::PRODUCT_D32
    )
}

/// Checks the product (section 03 §3.1): the identification registers
/// need the main clock running.
///
/// # Errors
///
/// [`Error::Product`] for a device of another register model.
pub fn identify(w: &dyn Words) -> Result<u32, Error> {
    let id = w.read(gcu::CORE_ID);
    if accepts(id >> 16) {
        Ok(id)
    } else {
        Err(Error::Product(id >> 16))
    }
}

/// Soft reset (section 03 §3.2 step 1): `SRST` alone, which also asks for
/// mode 0, until it clears. Bounded: 100 polls of 1 to 10 ms.
///
/// # Errors
///
/// [`Error::ResetTimeout`] when `SRST` stays set.
pub fn soft_reset(w: &dyn Words, d: &dyn Delay) -> Result<(), Error> {
    w.write(gcu::CONTROL, gcu::SRST);
    for _ in 0..100 {
        if w.read(gcu::CONTROL) & gcu::SRST == 0 {
            return Ok(());
        }
        d.delay_us(1000);
    }
    Err(Error::ResetTimeout)
}

/// What the block list says before the pipelines are assembled.
struct Global {
    legacy: bool,
    max_line: u32,
    max_lines: u32,
    dual_link: bool,
    integrated_tbu: bool,
}

/// Reads the global capabilities: from the legacy block at the end of
/// the window if there is one, from the control unit otherwise (section
/// 03 §3.2 step 3).
fn globals(w: &dyn Words, window: usize) -> Global {
    let legacy = window > 0xFE00 + common::BLOCK_INFO
        && (w.read(0xFE00 + common::BLOCK_INFO) >> 8) & 0xFF == u32::from(kind::PERIPH);
    if legacy {
        let c = w.read(0xFE00 + 0x1D4);
        Global {
            legacy: true,
            max_line: if c & 1 != 0 { 4096 } else { 2048 },
            max_lines: 4096,
            dual_link: c & (1 << 8) != 0,
            integrated_tbu: c & (1 << 12) != 0,
        }
    } else {
        let a = w.read(gcu::CONFIG_ID0);
        let b = w.read(gcu::CONFIG_ID1);
        Global {
            legacy: false,
            max_line: a & 0xFFFF,
            max_lines: a >> 16,
            dual_link: b & (1 << 16) != 0,
            integrated_tbu: b & (1 << 17) != 0,
        }
    }
}

/// A pipeline's blocks as the walk finds them.
#[derive(Default)]
struct Draft {
    lpu: Option<Block>,
    layers: [Option<Layer>; 4],
    writeback: Option<Block>,
    cu: Option<Block>,
    scalers: [Option<Block>; 2],
    splitter: Option<Block>,
    merger: Option<Block>,
    dou: Option<Block>,
    ips: Option<Block>,
    bs: Option<Block>,
    ft: Option<Block>,
}

fn put<T>(slot: &mut Option<T>, v: T) -> Result<(), Error> {
    if slot.replace(v).is_some() {
        Err(Error::Layout)
    } else {
        Ok(())
    }
}

/// The blocks of a dense list: `[a, b, c, none, ...]` gives `[a, b, c]`.
fn dense<T: Copy>(slots: &[Option<T>]) -> Result<Vec<T>, Error> {
    let n = slots.iter().take_while(|s| s.is_some()).count();
    if slots[n..].iter().any(Option::is_some) {
        return Err(Error::Layout);
    }
    Ok(slots[..n].iter().flatten().copied().collect())
}

/// Walks the blocks (section 03 §3.2 steps 2, 4 and 5). `window` is the
/// size of the register window in bytes. Blocks of a type this driver
/// does not know are skipped, so a part with more blocks than the
/// specification describes still probes.
///
/// # Errors
///
/// An [`Error`] saying what is wrong with the device.
pub fn walk(w: &dyn Words, window: usize) -> Result<Layout, Error> {
    let core_id = identify(w)?;
    let arch_id = w.read(0x000);
    let info = w.read(gcu::CORE_INFO);
    let slots = (info & 0xFF) as usize;
    let npipes = (info >> 8) & 7;
    if npipes == 0 || npipes > 2 {
        return Err(Error::Pipelines(npipes));
    }
    if slots.saturating_mul(regs::BLOCK) > window {
        return Err(Error::Window);
    }
    let g = globals(w, window);
    let legacy_lines = g.legacy;
    let mut drafts: [Draft; 2] = [Draft::default(), Draft::default()];
    for k in 1..slots {
        let at = k * regs::BLOCK;
        let bi = w.read(at + common::BLOCK_INFO);
        let ty = u8::try_from((bi >> 8) & 0xFF).unwrap_or(kind::RESERVED);
        if ty == kind::RESERVED {
            continue;
        }
        let inst = ((bi >> 4) & 0xF) as usize;
        // The global line size stands in for every block's. Reading a
        // block's own `MAX_LINE_SIZE` (offset 8) is refused on the board:
        // the interconnect monitor reports a non-secure access error at
        // `0x208` of each unit's window (2026-11-23).
        let max_line = g.max_line;
        let block = Block {
            offset: at,
            info: bi,
            max_line,
        };
        let pipe = |p: usize| drafts_index(p, npipes);
        match ty {
            kind::LPU => put(&mut drafts[pipe(inst)?].lpu, block)?,
            kind::CU => put(&mut drafts[pipe(inst)?].cu, block)?,
            kind::DOU => put(&mut drafts[pipe(inst)?].dou, block)?,
            kind::LAYER => {
                let li = w.read(at + layer::LAYER_INFO);
                let rich = li & layer::INFO_RICH != 0;
                let yuv = if legacy_lines { max_line / 2 } else { li >> 16 };
                let l = Layer {
                    block,
                    rich,
                    max_line_yuv: yuv,
                };
                let d = &mut drafts[pipe(inst / 4)?];
                put(&mut d.layers[inst % 4], l)?;
            }
            kind::WRITEBACK => put(&mut drafts[pipe(inst)?].writeback, block)?,
            kind::SPLITTER => put(&mut drafts[pipe(inst)?].splitter, block)?,
            kind::SCALER => {
                let d = &mut drafts[pipe(inst / 2)?];
                put(&mut d.scalers[inst % 2], block)?;
            }
            kind::MERGER => put(&mut drafts[pipe(inst)?].merger, block)?,
            kind::IPS => put(&mut drafts[pipe(inst)?].ips, block)?,
            kind::BS => put(&mut drafts[pipe(inst)?].bs, block)?,
            kind::FT_TABLE => put(&mut drafts[pipe(inst)?].ft, block)?,
            // The GCU appears once, as block 0; the rest are not driven.
            _ => {}
        }
    }
    let mut pipes = Vec::new();
    for d in drafts.into_iter().take(npipes as usize) {
        let (Some(lpu), Some(cu), Some(dou), Some(ips), Some(bs)) =
            (d.lpu, d.cu, d.dou, d.ips, d.bs)
        else {
            return Err(Error::Incomplete);
        };
        let layers = dense(&d.layers)?;
        if layers.is_empty() {
            return Err(Error::Incomplete);
        }
        pipes.push(Pipe {
            lpu,
            layers,
            writeback: d.writeback,
            cu,
            scalers: dense(&d.scalers)?,
            splitter: d.splitter,
            merger: d.merger,
            dou,
            ips,
            bs,
            ft: d.ft,
        });
    }
    Ok(Layout {
        core_id,
        arch_id,
        max_line: g.max_line,
        max_lines: g.max_lines,
        dual_link: g.dual_link,
        integrated_tbu: g.integrated_tbu,
        pipes,
    })
}

fn drafts_index(p: usize, npipes: u32) -> Result<usize, Error> {
    if p < npipes as usize {
        Ok(p)
    } else {
        Err(Error::Layout)
    }
}
