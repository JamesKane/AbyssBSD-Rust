// SPDX-License-Identifier: BSD-2-Clause
//! Realtek's fixed register programs ([`crate::generated`]), and the
//! interpreter that replays them on a chip.

/// One step of a PHY configuration program (`tools/rgegen`'s reading of
/// FreeBSD's `rge_phy_config_mac_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Writes a PHY OCP register.
    Write(u16, u16),
    /// Sets bits of a PHY OCP register.
    Set(u16, u16),
    /// Clears bits of a PHY OCP register.
    Clear(u16, u16),
    /// Clears the mask, then sets the bits.
    Modify(u16, u16, u16),
    /// Starts (`true`) or ends patching the PHY's microcontroller.
    PatchMcu(bool),
    /// Loads the PHY microcode, unless the PHY already runs this version.
    LoadMcu(u16),
}

/// What a program runs on.
pub trait PhyTarget {
    /// Reads a PHY OCP register.
    fn read_phy_ocp(&mut self, reg: u16) -> u16;
    /// Writes a PHY OCP register.
    fn write_phy_ocp(&mut self, reg: u16, val: u16);
    /// `rge_patch_phy_mcu`.
    fn patch_phy_mcu(&mut self, on: bool);
    /// `rge_phy_config_mcu`.
    fn load_phy_mcu(&mut self, version: u16);
}

/// Replays `ops` on `t`.
pub fn run(ops: &[Op], t: &mut impl PhyTarget) {
    for &op in ops {
        match op {
            Op::Write(r, v) => t.write_phy_ocp(r, v),
            Op::Set(r, b) => {
                let v = t.read_phy_ocp(r);
                t.write_phy_ocp(r, v | b);
            }
            Op::Clear(r, b) => {
                let v = t.read_phy_ocp(r);
                t.write_phy_ocp(r, v & !b);
            }
            Op::Modify(r, m, b) => {
                let v = t.read_phy_ocp(r);
                t.write_phy_ocp(r, v & !m | b);
            }
            Op::PatchMcu(on) => t.patch_phy_mcu(on),
            Op::LoadMcu(v) => t.load_phy_mcu(v),
        }
    }
}
