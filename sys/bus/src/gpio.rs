// SPDX-License-Identifier: BSD-2-Clause
//! GPIO providers (DRIVER-MODEL.md §9.3): FreeBSD's `gpio_if` and
//! `gpiobus` pin ownership.
//!
//! A GPIO controller's driver implements [`Gpio`] for its pins and
//! publishes it with `StartCx::provide_gpio`; the framework wraps it in a
//! [`GpioSource`], which owns the claims. Consumers get pins from the
//! lines their bus read from firmware (ACPI `GpioIo` and `GpioInt`):
//!
//! - `AttachCx::gpio` claims a `GpioIo` descriptor's pins as a
//!   [`GpioPins`] handle;
//! - `AttachCx::gpio_interrupt` claims a `GpioInt` pin, sets it up as an
//!   input with the descriptor's pull, and grants it as an interrupt line
//!   in the controller's cascaded domain (`irq::cascade`).
//!
//! A pin is held exclusively unless every holder's descriptor says it is
//! shared, as ACPI's `Exclusive`/`Shared` do; FreeBSD's `gpiobus` keeps
//! the same per-pin claim (`gpiobus_acquire_pin`, `EBUSY`). A pin's last
//! claim going tells the provider, which may restore what firmware left
//! (the Cadence controller's bypass bit).
//!
//! Calls take a [`Sleepable`]: a controller behind I2C sleeps in them.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use sched::Sleepable;
use sched::lock::{LockClass, Mutex};

use crate::provider::ProviderError;

static SOURCE: LockClass = LockClass::new("gpio source", 260);

/// A pin's bias.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pull {
    /// As the controller or firmware left it.
    #[default]
    Default,
    /// Pulled up.
    Up,
    /// Pulled down.
    Down,
    /// Neither.
    None,
}

/// What a pin does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinMode {
    /// Reads the pad.
    Input,
    /// Drives the pad, starting at this level (FreeBSD's
    /// `GPIO_PIN_PRESET_HIGH`/`LOW`: no glitch to a stale level).
    Output {
        /// The level to drive.
        high: bool,
    },
}

/// What a `GpioIo` descriptor allows (ACPI's `IoRestriction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Restriction {
    /// Input or output.
    #[default]
    None,
    /// Input only.
    Input,
    /// Output only.
    Output,
    /// Keep the pin's current configuration when it is claimed
    /// (`IoRestrictionNoneAndPreserve`).
    Preserve,
}

/// How a pin is used, as the consumer's firmware describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpioMode {
    /// A `GpioIo`.
    Io(Restriction),
    /// A `GpioInt`.
    Interrupt {
        /// Edge or level.
        trigger: irq::Trigger,
        /// Active level, or which edges.
        polarity: irq::Polarity,
        /// A wake source (`…AndWake`).
        wake: bool,
    },
}

/// A GPIO line a device consumes: one descriptor's pins on one
/// controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpioUse {
    /// The pins, by the controller's numbering.
    pub pins: Vec<u32>,
    /// I/O or interrupt.
    pub mode: GpioMode,
    /// Others may hold the pins too.
    pub shared: bool,
    /// The bias to set.
    pub pull: Pull,
    /// Debounce, in microseconds (0: none).
    pub debounce_us: u32,
}

/// GPIO pins, by the controller's numbering.
pub trait Gpio: Send + Sync {
    /// How many pins the controller has.
    fn pins(&self) -> u32;
    /// Sets `pin`'s direction (and, for an output, its level first) and
    /// bias. [`ProviderError::Unsupported`] for a bias the controller
    /// cannot set, which pads configured elsewhere (a pin controller)
    /// leave to firmware.
    fn configure(
        &self,
        s: &Sleepable,
        pin: u32,
        mode: PinMode,
        pull: Pull,
    ) -> Result<(), ProviderError>;
    /// The level on `pin`'s pad.
    fn get(&self, s: &Sleepable, pin: u32) -> Result<bool, ProviderError>;
    /// Drives `pin` (an output) to `high`.
    fn set(&self, s: &Sleepable, pin: u32, high: bool) -> Result<(), ProviderError>;
    /// `pin`'s last claim is gone.
    fn release(&self, s: &Sleepable, pin: u32) {
        let _ = (s, pin);
    }
    /// The domain its pins interrupt in, if they can.
    fn interrupts(&self) -> Option<&'static dyn irq::IrqDomain> {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Claim {
    holders: u32,
    /// Every holder said the pin is shared.
    shared: bool,
}

/// A GPIO controller and the claims on its pins.
pub struct GpioSource {
    imp: Arc<dyn Gpio>,
    claims: Mutex<BTreeMap<u32, Claim>>,
    revoked: AtomicBool,
}

impl core::fmt::Debug for GpioSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpioSource")
            .field("pins", &self.imp.pins())
            .finish_non_exhaustive()
    }
}

impl GpioSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn Gpio>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            claims: Mutex::new(&SOURCE, BTreeMap::new()),
            revoked: AtomicBool::new(false),
        })
    }

    /// How many pins the controller has.
    #[must_use]
    pub fn pins(&self) -> u32 {
        self.imp.pins()
    }

    /// The domain its pins interrupt in.
    #[must_use]
    pub fn interrupts(&self) -> Option<&'static dyn irq::IrqDomain> {
        self.imp.interrupts()
    }

    /// `(holders, shared)` for `pin`.
    pub fn claim_of(&self, s: &Sleepable, pin: u32) -> (u32, bool) {
        self.claims
            .lock(s)
            .get(&pin)
            .map_or((0, false), |c| (c.holders, c.shared))
    }

    /// Claims `pins` for a consumer, all or none: [`ProviderError::Claimed`]
    /// if one is held and either side wants it exclusively,
    /// [`ProviderError::NoSuchLine`] for a pin past the controller's.
    pub fn claim(
        self: &Arc<Self>,
        s: &Sleepable,
        pins: &[u32],
        restriction: Restriction,
        shared: bool,
    ) -> Result<GpioPins, ProviderError> {
        self.live()?;
        let n = self.imp.pins();
        let mut claims = self.claims.lock(s);
        for (i, &p) in pins.iter().enumerate() {
            if p >= n || pins[..i].contains(&p) {
                return Err(ProviderError::NoSuchLine);
            }
            if let Some(c) = claims.get(&p)
                && c.holders > 0
                && !(c.shared && shared)
            {
                return Err(ProviderError::Claimed);
            }
        }
        for &p in pins {
            let c = claims.entry(p).or_default();
            c.shared = if c.holders == 0 {
                shared
            } else {
                c.shared && shared
            };
            c.holders += 1;
        }
        Ok(GpioPins {
            source: Arc::clone(self),
            pins: pins.to_vec(),
            restriction,
            held: true,
        })
    }

    fn unclaim(&self, s: &Sleepable, pins: &[u32]) {
        let mut claims = self.claims.lock(s);
        for &p in pins {
            let last = match claims.get_mut(&p) {
                Some(c) => {
                    c.holders = c.holders.saturating_sub(1);
                    c.holders == 0
                }
                None => false,
            };
            if last {
                claims.remove(&p);
                if !self.is_revoked() {
                    self.imp.release(s, p);
                }
            }
        }
    }

    /// Stops every call to the provider: its driver is detaching.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    /// Whether [`Self::revoke`] has run.
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    fn live(&self) -> Result<(), ProviderError> {
        if self.is_revoked() {
            Err(ProviderError::Revoked)
        } else {
            Ok(())
        }
    }
}

/// A consumer's claim on a descriptor's pins, by their index in it.
/// Dropping it releases the claim.
pub struct GpioPins {
    source: Arc<GpioSource>,
    pins: Vec<u32>,
    restriction: Restriction,
    held: bool,
}

impl core::fmt::Debug for GpioPins {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GpioPins")
            .field("pins", &self.pins)
            .field("restriction", &self.restriction)
            .finish_non_exhaustive()
    }
}

impl GpioPins {
    /// How many pins the descriptor lists.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// Whether it lists none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// The controller's number for the `i`th pin.
    #[must_use]
    pub fn pin(&self, i: usize) -> Option<u32> {
        self.pins.get(i).copied()
    }

    fn at(&self, i: usize) -> Result<u32, ProviderError> {
        self.source.live()?;
        self.pin(i).ok_or(ProviderError::NoSuchLine)
    }

    /// Sets the `i`th pin's direction and bias, within what the
    /// descriptor allows.
    pub fn configure(
        &self,
        s: &Sleepable,
        i: usize,
        mode: PinMode,
        pull: Pull,
    ) -> Result<(), ProviderError> {
        let p = self.at(i)?;
        match (self.restriction, mode) {
            (Restriction::Input, PinMode::Output { .. })
            | (Restriction::Output, PinMode::Input) => Err(ProviderError::Unsupported),
            _ => self.source.imp.configure(s, p, mode, pull),
        }
    }

    /// The `i`th pin's level.
    pub fn get(&self, s: &Sleepable, i: usize) -> Result<bool, ProviderError> {
        let p = self.at(i)?;
        if self.restriction == Restriction::Output {
            return Err(ProviderError::Unsupported);
        }
        self.source.imp.get(s, p)
    }

    /// Drives the `i`th pin to `high`.
    pub fn set(&self, s: &Sleepable, i: usize, high: bool) -> Result<(), ProviderError> {
        let p = self.at(i)?;
        if self.restriction == Restriction::Input {
            return Err(ProviderError::Unsupported);
        }
        self.source.imp.set(s, p, high)
    }

    /// Releases the claim. Use this rather than letting the handle drop
    /// where the dropping thread may not sleep.
    pub fn release(mut self, s: &Sleepable) {
        self.leave(s);
    }

    fn leave(&mut self, s: &Sleepable) {
        if core::mem::replace(&mut self.held, false) {
            self.source.unclaim(s, &self.pins);
        }
    }
}

impl Drop for GpioPins {
    fn drop(&mut self) {
        if self.held {
            // SAFETY: claims are dropped by attach failure or detach, both
            // in thread context (DRIVER-MODEL.md §10.2).
            let s = unsafe { Sleepable::assume() };
            self.leave(&s);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use std::sync::Mutex as StdMutex;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        Configure(u32, PinMode, Pull),
        Set(u32, bool),
        Release(u32),
    }

    #[derive(Default)]
    struct Fake {
        calls: StdMutex<Vec<Call>>,
    }

    impl Fake {
        fn take(&self) -> Vec<Call> {
            core::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl Gpio for Fake {
        fn pins(&self) -> u32 {
            32
        }
        fn configure(
            &self,
            _: &Sleepable,
            pin: u32,
            mode: PinMode,
            pull: Pull,
        ) -> Result<(), ProviderError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Configure(pin, mode, pull));
            Ok(())
        }
        fn get(&self, _: &Sleepable, pin: u32) -> Result<bool, ProviderError> {
            Ok(pin % 2 == 1)
        }
        fn set(&self, _: &Sleepable, pin: u32, high: bool) -> Result<(), ProviderError> {
            self.calls.lock().unwrap().push(Call::Set(pin, high));
            Ok(())
        }
        fn release(&self, _: &Sleepable, pin: u32) {
            self.calls.lock().unwrap().push(Call::Release(pin));
        }
    }

    fn s() -> Sleepable {
        // SAFETY: host tests run on threads that may block.
        unsafe { Sleepable::assume() }
    }

    #[test]
    fn exclusive_pins_are_held_once() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = GpioSource::new(fake.clone());
        let a = src
            .claim(&s, &[12, 15], Restriction::Output, false)
            .unwrap();
        // Any overlap is refused, and nothing is claimed by the refusal.
        assert_eq!(
            src.claim(&s, &[3, 15], Restriction::None, true)
                .unwrap_err(),
            ProviderError::Claimed
        );
        assert_eq!(src.claim_of(&s, 3), (0, false));
        assert_eq!(
            src.claim(&s, &[32], Restriction::None, false).unwrap_err(),
            ProviderError::NoSuchLine
        );
        assert_eq!(
            src.claim(&s, &[4, 4], Restriction::None, false)
                .unwrap_err(),
            ProviderError::NoSuchLine
        );
        a.set(&s, 1, true).unwrap();
        assert_eq!(a.get(&s, 0), Err(ProviderError::Unsupported));
        assert_eq!(
            a.configure(&s, 0, PinMode::Input, Pull::Up),
            Err(ProviderError::Unsupported)
        );
        a.configure(&s, 0, PinMode::Output { high: false }, Pull::None)
            .unwrap();
        assert_eq!(a.set(&s, 2, true), Err(ProviderError::NoSuchLine));
        drop(a);
        assert_eq!(
            fake.take(),
            vec![
                Call::Set(15, true),
                Call::Configure(12, PinMode::Output { high: false }, Pull::None),
                Call::Release(12),
                Call::Release(15),
            ]
        );
    }

    #[test]
    fn shared_pins_are_released_by_the_last_holder() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = GpioSource::new(fake.clone());
        // The Sky1's two PD controllers share S5 GPIO 0 line 7.
        let a = src.claim(&s, &[7], Restriction::Input, true).unwrap();
        let b = src.claim(&s, &[7], Restriction::Input, true).unwrap();
        assert_eq!(src.claim_of(&s, 7), (2, true));
        // An exclusive claim on a shared pin is refused.
        assert_eq!(
            src.claim(&s, &[7], Restriction::None, false).unwrap_err(),
            ProviderError::Claimed
        );
        assert!(b.get(&s, 0).unwrap());
        a.release(&s);
        assert_eq!(fake.take(), []);
        drop(b);
        assert_eq!(fake.take(), [Call::Release(7)]);
        // A revoked provider refuses, and dropping no longer reaches it.
        let c = src.claim(&s, &[1], Restriction::None, false).unwrap();
        src.revoke();
        assert_eq!(c.get(&s, 0), Err(ProviderError::Revoked));
        drop(c);
        assert_eq!(fake.take(), []);
        assert_eq!(
            src.claim(&s, &[2], Restriction::None, false).unwrap_err(),
            ProviderError::Revoked
        );
    }
}
