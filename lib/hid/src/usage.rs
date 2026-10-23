// SPDX-License-Identifier: BSD-2-Clause
//! The usages this stack acts on (HID Usage Tables 1.4), extended: page
//! in the high 16 bits.

/// Builds an extended usage.
#[must_use]
pub fn usage(page: u16, id: u16) -> u32 {
    (u32::from(page) << 16) | u32::from(id)
}

/// The page of an extended usage.
#[must_use]
pub fn page(u: u32) -> u16 {
    u16::try_from(u >> 16).unwrap_or(0)
}

/// The ID of an extended usage.
#[must_use]
pub fn id(u: u32) -> u16 {
    u16::try_from(u & 0xffff).unwrap_or(0)
}

/// Generic Desktop page.
pub const GENERIC_DESKTOP: u16 = 0x01;
/// Keyboard/Keypad page.
pub const KEYBOARD: u16 = 0x07;
/// LED page.
pub const LED: u16 = 0x08;
/// Button page.
pub const BUTTON: u16 = 0x09;
/// Consumer page.
pub const CONSUMER: u16 = 0x0c;

/// Generic Desktop: pointer.
pub const POINTER: u32 = 0x0001_0001;
/// Generic Desktop: mouse.
pub const MOUSE: u32 = 0x0001_0002;
/// Generic Desktop: keyboard.
pub const KEYBOARD_APP: u32 = 0x0001_0006;
/// Generic Desktop: X.
pub const X: u32 = 0x0001_0030;
/// Generic Desktop: Y.
pub const Y: u32 = 0x0001_0031;
/// Generic Desktop: Z.
pub const Z: u32 = 0x0001_0032;
/// Generic Desktop: wheel.
pub const WHEEL: u32 = 0x0001_0038;
/// Consumer: AC Pan (the horizontal wheel).
pub const AC_PAN: u32 = 0x000c_0238;

/// Keyboard: the first modifier (Left Control); the eight run to 0xe7.
pub const KEY_LEFT_CONTROL: u16 = 0xe0;
/// Keyboard: `ErrorRollOver`, reported in every slot when too many keys
/// are down.
pub const KEY_ROLLOVER: u16 = 0x01;

/// LED: Num Lock.
pub const LED_NUM_LOCK: u32 = 0x0008_0001;
/// LED: Caps Lock.
pub const LED_CAPS_LOCK: u32 = 0x0008_0002;
/// LED: Scroll Lock.
pub const LED_SCROLL_LOCK: u32 = 0x0008_0003;
