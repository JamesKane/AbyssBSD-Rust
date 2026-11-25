// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The power management firmware, SMU 13.0.5 (specification 04): its mailbox
//! (three registers beyond the register window, reached through the
//! index/data pair of the bus interface) and the messages. A message is how
//! the driver asks for a graphics reset (message 10, section 10 of the
//! specification) or reads the firmware's version.

use crate::regs::Window;

/// `PCIE_INDEX2` and `PCIE_DATA2` (01 section 3.2): the byte address of the
/// register, read back, then its data.
pub const PCIE_INDEX2: usize = 0x38;
pub const PCIE_DATA2: usize = 0x3c;

/// `MP1_FIRMWARE_FLAGS` (bit 0: the firmware's interrupts are on, so it is
/// running), and the mailbox: the message, the response and the argument
/// (04 section 1.1), as byte addresses.
pub const FIRMWARE_FLAGS: u32 = 0x3b1_0024;
pub const MESSAGE: u32 = 0x3b1_0508;
pub const RESPONSE: u32 = 0x3b1_0984;
pub const ARGUMENT: u32 = 0x3b1_0988;

/// The messages (04 section 3).
pub mod message {
    pub const TEST: u32 = 1;
    pub const GET_VERSION: u32 = 2;
    pub const GFX_DEVICE_RESET: u32 = 10;
    pub const GET_DRIVER_INTERFACE_VERSION: u32 = 25;
}

/// How the firmware answered (04 section 1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The firmware is not running (the flag is clear).
    NotRunning,
    /// No answer in the time allowed.
    Timeout,
    /// `0xff`: the message completed and failed.
    Failed,
    /// `0xfe`: not a message it knows.
    Unknown,
    /// `0xfd`: a valid message whose prerequisites are not met.
    Prerequisites,
    /// `0xfc`: busy with other work after every retry.
    Busy,
    /// Any other response: the firmware is treated as hung.
    Hung(u32),
}

/// Reads the register at byte address `addr`.
pub fn read<W: Window + ?Sized>(w: &W, addr: u32) -> u32 {
    w.write32(PCIE_INDEX2, addr);
    // Read back, so the index has landed before the data access.
    let _ = w.read32(PCIE_INDEX2);
    w.read32(PCIE_DATA2)
}

/// Writes the register at byte address `addr`.
pub fn write<W: Window + ?Sized>(w: &W, addr: u32, value: u32) {
    w.write32(PCIE_INDEX2, addr);
    let _ = w.read32(PCIE_INDEX2);
    w.write32(PCIE_DATA2, value);
}

/// Whether the firmware is running.
pub fn running<W: Window + ?Sized>(w: &W) -> bool {
    read(w, FIRMWARE_FLAGS) & 1 != 0
}

/// Sends message `msg` with argument `arg` and returns what the firmware
/// gives back in the argument register (04 section 2). `polls` looks at the
/// response at most, `pause_us` between them; a busy answer is retried after
/// 10 microseconds.
///
/// # Errors
///
/// [`Error`] for a firmware that is not running, does not answer, or answers
/// anything but OK.
pub fn send<W: Window + ?Sized>(
    w: &W,
    msg: u32,
    arg: u32,
    polls: u32,
    pause_us: &dyn Fn(u32),
) -> Result<u32, Error> {
    if !running(w) {
        return Err(Error::NotRunning);
    }
    let mut busy = 0;
    loop {
        // 1. The previous message is done when the response is not zero.
        let mut ready = false;
        for _ in 0..polls {
            if read(w, RESPONSE) != 0 {
                ready = true;
                break;
            }
            pause_us(1);
        }
        if !ready {
            return Err(Error::Timeout);
        }
        // 2. to 4.: the response cleared, the argument, the message.
        write(w, RESPONSE, 0);
        write(w, ARGUMENT, arg);
        write(w, MESSAGE, msg);
        // 5. The answer.
        let mut answer = 0;
        for _ in 0..polls {
            answer = read(w, RESPONSE);
            if answer != 0 {
                break;
            }
            pause_us(1);
        }
        match answer {
            0 => return Err(Error::Timeout),
            0x01 => return Ok(read(w, ARGUMENT)),
            0xff => return Err(Error::Failed),
            0xfe => return Err(Error::Unknown),
            0xfd => return Err(Error::Prerequisites),
            // Busy: again after a short wait, a few times.
            0xfc if busy < 100 => {
                busy += 1;
                pause_us(10);
            }
            0xfc => return Err(Error::Busy),
            other => return Err(Error::Hung(other)),
        }
    }
}

/// Splits the version message's answer: program, major, minor, debug.
#[must_use]
pub const fn version_fields(v: u32) -> (u32, u32, u32, u32) {
    (v >> 24, (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// The index/data pair and a firmware behind it: messages answered from
    /// a script.
    struct Smu {
        index: RefCell<u32>,
        regs: RefCell<std::collections::BTreeMap<u32, u32>>,
        /// (message, response, argument returned).
        answers: RefCell<Vec<(u32, u32, u32)>>,
        log: RefCell<Vec<(u32, u32)>>,
        flags: u32,
    }

    impl Smu {
        fn new(flags: u32, answers: &[(u32, u32, u32)]) -> Self {
            let mut r = std::collections::BTreeMap::new();
            r.insert(RESPONSE, 1);
            Self {
                index: RefCell::new(0),
                regs: RefCell::new(r),
                answers: RefCell::new(answers.to_vec()),
                log: RefCell::new(Vec::new()),
                flags,
            }
        }
    }

    impl Window for Smu {
        fn read32(&self, o: usize) -> u32 {
            match o {
                PCIE_INDEX2 => *self.index.borrow(),
                PCIE_DATA2 => {
                    let a = *self.index.borrow();
                    if a == FIRMWARE_FLAGS {
                        self.flags
                    } else {
                        self.regs.borrow().get(&a).copied().unwrap_or(0)
                    }
                }
                _ => 0,
            }
        }
        fn write32(&self, o: usize, v: u32) {
            match o {
                PCIE_INDEX2 => *self.index.borrow_mut() = v,
                PCIE_DATA2 => {
                    let a = *self.index.borrow();
                    self.log.borrow_mut().push((a, v));
                    self.regs.borrow_mut().insert(a, v);
                    if a == MESSAGE {
                        // The firmware answers the message in the script.
                        let mut ans = self.answers.borrow_mut();
                        if !ans.is_empty() {
                            let (m, resp, ret) = ans.remove(0);
                            assert_eq!(m, v, "the message sent");
                            self.regs.borrow_mut().insert(ARGUMENT, ret);
                            self.regs.borrow_mut().insert(RESPONSE, resp);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn a_message_goes_through_the_mailbox_in_the_specifications_order() {
        let s = Smu::new(1, &[(message::GET_VERSION, 1, 0x0054_4fe1)]);
        let v = send(&s, message::GET_VERSION, 0, 10, &|_| {}).unwrap();
        assert_eq!(v, 0x0054_4fe1);
        assert_eq!(version_fields(v), (0, 0x54, 0x4f, 0xe1));
        // The response cleared, then the argument, then the message.
        let order: Vec<u32> = s
            .log
            .borrow()
            .iter()
            .filter(|(a, _)| [RESPONSE, ARGUMENT, MESSAGE].contains(a))
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(order, [RESPONSE, ARGUMENT, MESSAGE]);
    }

    #[test]
    fn the_firmware_must_be_running_and_must_answer() {
        let s = Smu::new(0, &[]);
        assert_eq!(send(&s, 1, 0, 10, &|_| {}), Err(Error::NotRunning));
        // No scripted answer: the response stays 0 after it is cleared.
        let s = Smu::new(1, &[]);
        // The message leaves the response at 0 (cleared, never answered).
        assert_eq!(send(&s, 1, 0, 5, &|_| {}), Err(Error::Timeout));
    }

    #[test]
    fn the_responses_map_to_errors() {
        for (resp, want) in [
            (0xff, Error::Failed),
            (0xfe, Error::Unknown),
            (0xfd, Error::Prerequisites),
            (0x77, Error::Hung(0x77)),
        ] {
            let s = Smu::new(1, &[(10, resp, 0)]);
            assert_eq!(send(&s, 10, 2, 5, &|_| {}), Err(want));
        }
    }

    #[test]
    fn a_busy_firmware_is_asked_again() {
        let s = Smu::new(1, &[(2, 0xfc, 0), (2, 0xfc, 0), (2, 1, 7)]);
        assert_eq!(send(&s, 2, 0, 5, &|_| {}), Ok(7));
    }

    #[test]
    fn the_graphics_reset_message_carries_mode_two() {
        let s = Smu::new(1, &[(message::GFX_DEVICE_RESET, 1, 0)]);
        send(&s, message::GFX_DEVICE_RESET, 2, 5, &|_| {}).unwrap();
        assert!(s.log.borrow().contains(&(ARGUMENT, 2)));
        assert_eq!(MESSAGE, 0x3b1_0508);
    }
}
