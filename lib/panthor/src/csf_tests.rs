// SPDX-License-Identifier: BSD-2-Clause
//! The interface discovery and global configuration against a shared section laid out as
//! specification 05 §5.4 describes, with the board firmware's interface version (3.13.0).

#![allow(clippy::cast_possible_truncation)] // Test images are small.

use core::cell::RefCell;
use std::vec::Vec;

use crate::Error;
use crate::csf::{self, Shared, Version};
use crate::fw::SHARED_VA;

struct Section(RefCell<Vec<u32>>);

impl Section {
    fn new(len: usize) -> Self {
        Self(RefCell::new(std::vec![0; len / 4]))
    }
    fn set(&self, off: usize, v: u32) {
        self.0.borrow_mut()[off / 4] = v;
    }
}

impl Shared for Section {
    fn len(&self) -> usize {
        self.0.borrow().len() * 4
    }
    fn read32(&self, off: usize) -> u32 {
        // As the driver's device memory: a misaligned word access is a bug.
        assert!(off.is_multiple_of(4), "misaligned read at {off:#x}");
        self.0.borrow()[off / 4]
    }
    fn write32(&self, off: usize, v: u32) {
        assert!(off.is_multiple_of(4), "misaligned write at {off:#x}");
        self.set(off, v);
    }
}

const GLB_IN: usize = 0x200;
const GLB_OUT: usize = 0x300;

/// A section with `groups` groups of `streams` streams, every block inside it.
fn board(groups: u32, streams: u32) -> Section {
    let s = Section::new(0x10000);
    s.set(0x00, 0x030D_0000);
    s.set(0x04, 0x10);
    s.set(0x08, SHARED_VA + GLB_IN as u32);
    s.set(0x0C, SHARED_VA + GLB_OUT as u32);
    s.set(0x10, groups);
    s.set(0x14, 0x400);
    s.set(0x1C, 0x71);
    for g in 0..groups as usize {
        let base = 0x1000 + g * 0x400;
        s.set(base, 0x1);
        s.set(base + 0x04, SHARED_VA + 0x8000 + (g * 0x100) as u32);
        s.set(base + 0x08, SHARED_VA + 0x9000 + (g * 0x100) as u32);
        s.set(base + 0x0C, 0x5000);
        s.set(base + 0x10, 0x5000);
        s.set(base + 0x14, streams);
        s.set(base + 0x18, 0x0C);
        for k in 0..streams as usize {
            let cs = base + 0x40 + k * 0x0C;
            s.set(cs, 0x0007_105F);
            s.set(cs + 0x04, SHARED_VA + 0xA000 + (k * 0x60) as u32);
            s.set(cs + 0x08, SHARED_VA + 0xC000 + (k * 0x100) as u32);
        }
    }
    s
}

#[test]
fn the_interface_is_discovered() {
    let s = board(8, 8);
    let i = csf::discover(&s).unwrap();
    assert_eq!(
        i.version,
        Version {
            major: 3,
            minor: 13,
            patch: 0
        }
    );
    assert_eq!(std::format!("{}", i.version), "3.13.0");
    assert!(!i.version.has_state_field());
    assert_eq!((i.glb_input, i.glb_output), (GLB_IN, GLB_OUT));
    assert_eq!((i.groups, i.streams, i.instr_features), (8, 8, 0x71));
    assert_eq!(i.work_registers(), 0x60);
}

#[test]
fn a_firmware_that_did_not_boot_or_lies_is_refused() {
    let s = board(8, 8);
    s.set(0, 0);
    assert!(matches!(csf::discover(&s), Err(Error::Bad(_))));
    // An input block outside the section.
    let s = board(8, 8);
    s.set(0x08, SHARED_VA + 0xFFF0);
    assert_eq!(
        csf::discover(&s),
        Err(Error::Bad("interface object outside the section"))
    );
    // An address below the section.
    let s = board(8, 8);
    s.set(0x0C, 0x100);
    assert!(csf::discover(&s).is_err());
    // Misaligned addresses and strides: refused, never read.
    let s = board(8, 8);
    s.set(0x08, SHARED_VA + GLB_IN as u32 + 2);
    assert_eq!(
        csf::discover(&s),
        Err(Error::Bad("interface object not word aligned"))
    );
    let s = board(8, 8);
    s.set(0x14, 0x402);
    assert_eq!(
        csf::discover(&s),
        Err(Error::Bad("control block not word aligned"))
    );
    // Too few groups, too many streams, groups that differ.
    assert!(csf::discover(&board(2, 8)).is_err());
    assert!(csf::discover(&board(8, 33)).is_err());
    let s = board(8, 8);
    s.set(0x1000 + 3 * 0x400 + 0x0C, 0x6000);
    assert_eq!(
        csf::discover(&s),
        Err(Error::Bad("command-stream groups differ"))
    );
}

#[test]
fn timers_count_in_1024_core_cycles() {
    // 10 ms at 900 MHz: ceil(10_000 × 900e6 / 1.024e9) = 8790.
    assert_eq!(csf::timer(10_000, 900_000_000), 8790 | 1 << 31);
    assert_eq!(csf::timer(800, 900_000_000), 704 | 1 << 31);
    assert_eq!(csf::timer(10_000, 0), u32::MAX);
}

#[test]
fn the_global_configuration_is_requested() {
    let s = board(8, 8);
    let i = csf::discover(&s).unwrap();
    // The firmware's acknowledge word has the configuration bits set.
    s.set(GLB_OUT, 0b1110);
    csf::configure(&s, &i, 0x0055_0555, 900_000_000);
    assert_eq!(s.read32(GLB_IN + 0x18), 0x0055_0555);
    assert_eq!(s.read32(GLB_IN + 0x1C), 0);
    assert_eq!(
        s.read32(GLB_IN + 0x04),
        0x0400_050E,
        "below 4.1: no state field"
    );
    let req = s.read32(GLB_IN);
    assert_eq!(
        req & (1 << 7 | 1 << 10),
        1 << 7 | 1 << 10,
        "counters and idle on"
    );
    assert_eq!(
        req & 0b1110,
        0,
        "each toggle is the inverse of its acknowledge"
    );
    assert!(!csf::acked(&s, &i, csf::CONFIG_BITS));
    // The firmware copies the request into the acknowledge.
    s.set(GLB_OUT, req);
    assert!(csf::acked(&s, &i, csf::CONFIG_BITS));
}

#[test]
fn a_ping_toggles_and_is_acknowledged() {
    let s = board(8, 8);
    let i = csf::discover(&s).unwrap();
    csf::ping(&s, &i);
    assert!(!csf::acked(&s, &i, csf::REQ_PING));
    s.set(GLB_OUT, s.read32(GLB_IN));
    assert!(csf::acked(&s, &i, csf::REQ_PING));
    csf::ping(&s, &i);
    assert_eq!(s.read32(GLB_IN) & csf::REQ_PING, 0, "toggled back");
}

#[test]
fn slots_resolve_and_requests_update_only_their_bits() {
    let s = board(8, 8);
    let i = csf::discover(&s).unwrap();
    let g = csf::csg(&s, &i, 3).unwrap();
    assert_eq!((g.input, g.output), (0x8300, 0x9300));
    let c = csf::cs(&s, &i, 3, 2).unwrap();
    assert_eq!((c.input, c.output), (0xA0C0, 0xC200));
    assert!(csf::csg(&s, &i, 8).is_err());
    assert!(csf::cs(&s, &i, 0, 8).is_err());
    // A toggle of the endpoint bit leaves the state field and the other bits alone.
    s.set(g.input, 0x1000_0001);
    s.set(g.output, 0x0000_0011);
    csf::toggle(&s, g.input, g.output, csf::CSG_ENDPOINT_CONFIG);
    assert_eq!(s.read32(g.input), 0x1000_0001);
    s.set(g.output, 0x0000_0001);
    csf::toggle(&s, g.input, g.output, csf::CSG_ENDPOINT_CONFIG);
    assert_eq!(s.read32(g.input), 0x1000_0011);
    // Events: the firmware flips SYNC_UPDATE; acknowledging copies it back.
    s.set(g.output, 0x1000_0011 ^ csf::CSG_SYNC_UPDATE);
    assert_eq!(
        csf::events(&s, g.input, g.output, csf::CSG_EVENTS),
        csf::CSG_SYNC_UPDATE
    );
    csf::ack_events(&s, g.input, g.output, csf::CSG_EVENTS);
    assert_eq!(csf::events(&s, g.input, g.output, csf::CSG_EVENTS), 0);
}

#[test]
fn a_group_is_programmed_and_started() {
    let s = board(8, 8);
    let i = csf::discover(&s).unwrap();
    let g = csf::csg(&s, &i, 0).unwrap();
    let c = csf::cs(&s, &i, 0, 0).unwrap();
    csf::program_stream(
        &s,
        &c,
        &csf::Stream {
            ring: 0x7F_0000_0000,
            ring_size: 0x4000,
            user_input: 0x0500_0000,
            user_output: 0x0500_1000,
            priority: 3,
            doorbell: 1,
        },
    );
    assert_eq!(s.read32(c.input + 0x10), 0);
    assert_eq!(s.read32(c.input + 0x14), 0x7F);
    assert_eq!(s.read32(c.input + 0x04), 3 | 1 << 8);
    assert_eq!(s.read32(c.input) & 0x507, 0x501);
    let grp = csf::Group {
        compute: 0x55_0555,
        fragment: 0x55_0555,
        tiler: 1,
        max_compute: 10,
        max_fragment: 10,
        max_tiler: 1,
        priority: 15,
        as_slot: 1,
        suspend: 0x0600_0000,
        protm_suspend: 0x0610_0000,
    };
    assert_eq!(csf::endpoint(&grp), 10 | 10 << 8 | 1 << 16 | 15 << 28);
    csf::start_group(&s, &i, &g, &grp, 1, csf::CSG_START);
    // Interface 3.13: the 32-bit endpoint field.
    assert_eq!(s.read32(g.input + 0x34), csf::endpoint(&grp));
    assert_eq!(s.read32(g.input + 0x38), 0);
    assert_eq!(s.read32(g.input + 0x50), 1);
    assert_eq!(s.read32(g.input) & 7, csf::CSG_START);
    assert!(!csf::csg_acked(&s, &g, csf::CSG_ENDPOINT_CONFIG));
    s.set(g.output, s.read32(g.input));
    assert!(csf::csg_acked(&s, &g, csf::CSG_ENDPOINT_CONFIG));
    assert_eq!(csf::csg_state(&s, &g), csf::CSG_START);
}
