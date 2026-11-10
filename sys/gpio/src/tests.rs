// SPDX-License-Identifier: BSD-2-Clause
//! `gpioc`'s requests against a controller model.

extern crate std;

use alloc::vec;
use std::sync::Mutex as StdMutex;

use bus::gpio::{Gpio, PinCaps};

use super::*;

/// Eight pins: 0-3 inputs, 4-6 outputs, 7 a peripheral's; levels kept.
struct Fake {
    levels: StdMutex<[bool; 8]>,
    calls: StdMutex<Vec<(u32, PinMode, Pull)>>,
}

impl Gpio for Fake {
    fn pins(&self) -> u32 {
        8
    }
    fn configure(
        &self,
        _: &Sleepable,
        pin: u32,
        mode: PinMode,
        pull: Pull,
    ) -> Result<(), ProviderError> {
        if pull != Pull::Default {
            return Err(ProviderError::Unsupported);
        }
        self.calls.lock().unwrap().push((pin, mode, pull));
        Ok(())
    }
    fn get(&self, _: &Sleepable, pin: u32) -> Result<bool, ProviderError> {
        Ok(self.levels.lock().unwrap()[pin as usize])
    }
    fn set(&self, _: &Sleepable, pin: u32, high: bool) -> Result<(), ProviderError> {
        self.levels.lock().unwrap()[pin as usize] = high;
        Ok(())
    }
    fn state(&self, _: &Sleepable, pin: u32) -> Result<PinState, ProviderError> {
        Ok(match pin {
            0..=3 => PinState::Input,
            4..=6 => PinState::Output,
            _ => PinState::Peripheral,
        })
    }
    fn caps(&self, _: u32) -> PinCaps {
        PinCaps::IN_OUT
    }
    fn name(&self, pin: u32) -> String {
        format!("gpio0.{pin}")
    }
    fn toggle(&self, _: &Sleepable, pin: u32) -> Result<(), ProviderError> {
        let mut l = self.levels.lock().unwrap();
        l[pin as usize] = !l[pin as usize];
        Ok(())
    }
}

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    unsafe { Sleepable::assume() }
}

fn node() -> (Node, Arc<Fake>) {
    let fake = Arc::new(Fake {
        levels: StdMutex::new([false, true, false, false, true, false, false, false]),
        calls: StdMutex::new(Vec::new()),
    });
    let node = Node {
        bus: Arc::new(Bus {
            source: GpioSource::new(Arc::clone(&fake) as Arc<dyn Gpio>),
            unit: 0,
            names: SpinLock::new(BTreeMap::new()),
        }),
    };
    (node, fake)
}

fn pin_arg(p: u32, flags: u32, name: &[u8]) -> Vec<u8> {
    let mut gp = GpioPin {
        gp_pin: p,
        gp_name: [0; g::GPIOMAXNAME],
        gp_caps: 0,
        gp_flags: flags,
    };
    gp.gp_name[..name.len()].copy_from_slice(name);
    let mut b = vec![0u8; g::GPIO_PIN_LEN];
    gp.store(&mut b);
    b
}

fn req(p: u32, v: u32) -> Vec<u8> {
    let mut b = vec![0u8; g::GPIO_REQ_LEN];
    GpioReq {
        gp_pin: p,
        gp_value: v,
    }
    .store(&mut b);
    b
}

#[test]
fn pins_report_as_gpioctl_lists_them() {
    let (n, _) = node();
    let s = s();
    let mut max = [0u8; 4];
    n.ioctl(&s, g::GPIOMAXPIN, &mut max).unwrap();
    assert_eq!(u32::from_ne_bytes(max), 7);
    for (p, flags) in [(1, pin::INPUT), (5, pin::OUTPUT), (7, 0)] {
        let mut b = pin_arg(p, 0, b"");
        n.ioctl(&s, g::GPIOGETCONFIG, &mut b).unwrap();
        let gp = GpioPin::from_bytes(&b).unwrap();
        assert_eq!(gp.gp_flags, flags, "pin {p}");
        assert_eq!(gp.gp_caps, pin::INPUT | pin::OUTPUT);
        assert_eq!(gp.name(), format!("gpio0.{p}").as_bytes());
    }
    let mut b = pin_arg(8, 0, b"");
    assert_eq!(n.ioctl(&s, g::GPIOGETCONFIG, &mut b), Err(Errno::EINVAL));
}

#[test]
fn configuration_is_checked_as_freebsd_checks_it() {
    let (n, fake) = node();
    let s = s();
    let mut b = pin_arg(1, pin::OUTPUT, b"");
    n.ioctl(&s, g::GPIOSETCONFIG, &mut b).unwrap();
    // It starts driving what the pad showed (high).
    assert_eq!(
        fake.calls.lock().unwrap().pop(),
        Some((1, PinMode::Output { high: true }, Pull::Default))
    );
    for (flags, e) in [
        (pin::INPUT | pin::OUTPUT, Errno::EINVAL),
        // Not a capability of the pin: the controller refuses it.
        (pin::INPUT | pin::PULLUP, Errno::EINVAL),
        (pin::PRESET_HIGH | pin::OUTPUT, Errno::EINVAL),
        // gpio_check_flags drops what caps lack, so the interrupt passes
        // it and then finds no pin offering one.
        (pin::INPUT | intr::EDGE_RISING, Errno::EOPNOTSUPP),
        (pin::INPUT | intr::LEVEL_LOW, Errno::EINVAL),
    ] {
        let mut b = pin_arg(2, flags, b"");
        assert_eq!(n.ioctl(&s, g::GPIOSETCONFIG, &mut b), Err(e), "{flags:#x}");
    }
    // Neither direction changes nothing.
    let mut b = pin_arg(2, 0, b"");
    n.ioctl(&s, g::GPIOSETCONFIG, &mut b).unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn values_and_names_go_through() {
    let (n, fake) = node();
    let s = s();
    let mut b = req(4, 0);
    n.ioctl(&s, g::GPIOGET, &mut b).unwrap();
    assert_eq!(GpioReq::from_bytes(&b).unwrap().gp_value, 1);
    n.ioctl(&s, g::GPIOSET, &mut req(4, 0)).unwrap();
    assert!(!fake.levels.lock().unwrap()[4]);
    n.ioctl(&s, g::GPIOTOGGLE, &mut req(4, 0)).unwrap();
    assert!(fake.levels.lock().unwrap()[4]);
    assert_eq!(n.ioctl(&s, g::GPIOSET, &mut req(9, 1)), Err(Errno::EINVAL));
    n.ioctl(&s, g::GPIOSETNAME, &mut pin_arg(3, 0, b"led"))
        .unwrap();
    let mut b = pin_arg(3, 0, b"");
    n.ioctl(&s, g::GPIOGETCONFIG, &mut b).unwrap();
    assert_eq!(GpioPin::from_bytes(&b).unwrap().name(), b"led");
    let mut a = vec![0u8; g::GPIO_ACCESS_32_LEN];
    assert_eq!(n.ioctl(&s, g::GPIOACCESS32, &mut a), Err(Errno::EOPNOTSUPP));
}
