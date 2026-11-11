// SPDX-License-Identifier: BSD-2-Clause
//! The `GeneralPurposeIo` handler against a model controller and the
//! corpus: the Radxa Q8B's GPIO fields (the only ones in the corpus a
//! bus driver serves; the Orange Pi 6 Plus has none), and a connection's
//! restrictions and bounds.

extern crate std;

use alloc::string::String;
use alloc::vec;
use std::sync::Mutex as StdMutex;

use acpi::opregion::RegionHandler;
use aml::{BufferTransfer, Host, Interpreter, NodeId, RegionAccess, RegionSpace, Value};
use bus::gpio::Gpio;
use bus::provider::ProviderError;

use super::*;

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    unsafe { Sleepable::assume() }
}

/// 256 pins whose levels are kept; every get and set is logged.
struct Model {
    levels: StdMutex<[bool; 256]>,
    log: StdMutex<Vec<(&'static str, u32, bool)>>,
}

impl Model {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            levels: StdMutex::new([false; 256]),
            log: StdMutex::new(Vec::new()),
        })
    }
}

impl Gpio for Model {
    fn pins(&self) -> u32 {
        256
    }
    fn configure(
        &self,
        _: &Sleepable,
        _: u32,
        _: bus::gpio::PinMode,
        _: bus::gpio::Pull,
    ) -> Result<(), ProviderError> {
        Ok(())
    }
    fn get(&self, _: &Sleepable, pin: u32) -> Result<bool, ProviderError> {
        let v = self.levels.lock().unwrap()[usize::try_from(pin).unwrap()];
        self.log.lock().unwrap().push(("get", pin, v));
        Ok(v)
    }
    fn set(&self, _: &Sleepable, pin: u32, high: bool) -> Result<(), ProviderError> {
        self.levels.lock().unwrap()[usize::try_from(pin).unwrap()] = high;
        self.log.lock().unwrap().push(("set", pin, high));
        Ok(())
    }
}

/// Memory as the corpus machine serves it; GPIO through the handler on
/// the node the interpreter names, as the kernel's host dispatches.
struct Dispatch<'a, H: Host> {
    inner: H,
    on: NodeId,
    handler: &'a SpaceHandler,
}

impl<H: Host> Host for Dispatch<'_, H> {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        self.inner.read(a)
    }
    fn write(&mut self, a: &RegionAccess, v: u64) -> Result<(), HostError> {
        self.inner.write(a, v)
    }
    fn transfer(&mut self, _: &BufferTransfer<'_>) -> Result<Vec<u8>, HostError> {
        Err(HostError::Unsupported)
    }
    fn gpio_read(&mut self, a: &GpioAccess<'_>) -> Result<u64, HostError> {
        if a.handler != Some(self.on) {
            return Err(HostError::Unsupported);
        }
        self.handler.gpio_read(&s(), a)
    }
    fn gpio_write(&mut self, a: &GpioAccess<'_>, v: u64) -> Result<(), HostError> {
        if a.handler != Some(self.on) {
            return Err(HostError::Unsupported);
        }
        self.handler.gpio_write(&s(), a, v)
    }
}

/// The GPIO fields of a namespace: each one's path and the controller
/// pins of its connection.
fn gpio_fields(i: &Interpreter) -> Vec<(String, Vec<u32>)> {
    let mut out = Vec::new();
    for n in i.nodes() {
        let Some(f) = i.field_info(n) else { continue };
        let Some(r) = f.region.and_then(|r| i.region_info(r)) else {
            continue;
        };
        if r.space != RegionSpace::GeneralPurposeIo {
            continue;
        }
        let Some(Ok(Resource::Gpio(g))) = f.connection.as_deref().map(aml::resource::connection)
        else {
            continue;
        };
        out.push((
            i.path(n).to_text(),
            g.pins.iter().map(|&p| u32::from(p)).collect(),
        ));
    }
    out
}

/// Each of the Q8B's GPIO fields reads the pin its connection names, once
/// the handler is on `GIO0`, whose scope declares both regions.
#[test]
fn q8b_gpio_fields_reach_their_pins() {
    let (mut i, machine) = acpi_corpus::load("radxa-q8b");
    let gio0 = i
        .lookup(&aml::AmlPath::parse("\\_SB.GIO0").unwrap())
        .unwrap();
    let model = Model::new();
    let handler = SpaceHandler::new(GpioSource::new(Arc::clone(&model) as Arc<dyn Gpio>));
    let mut h = Dispatch {
        inner: machine,
        on: gio0,
        handler: &handler,
    };
    let fields = gpio_fields(&i);
    assert_eq!(fields.len(), 2, "{fields:?}");
    let _ = i.install_handler(gio0, RegionSpace::GeneralPurposeIo, &mut h);
    for (path, pins) in &fields {
        assert_eq!(pins.len(), 1, "{path}");
        let pin = pins[0];
        model.levels.lock().unwrap()[usize::try_from(pin).unwrap()] = true;
        model.log.lock().unwrap().clear();
        let v = i.evaluate_path(path, Vec::new(), &mut h).unwrap();
        assert_eq!(v, Value::Integer(1), "{path}");
        assert_eq!(*model.log.lock().unwrap(), [("get", pin, true)], "{path}");
    }
    // With the handler elsewhere, the fields cannot be read.
    h.on = i.root();
    for (path, _) in &fields {
        assert!(i.evaluate_path(path, Vec::new(), &mut h).is_err(), "{path}");
    }
}

/// `GpioIo` with `restriction` on pins 5, 6, 7 of `\_SB.GPI0`, without
/// its end tag.
fn gpio_io(restriction: u8) -> Vec<u8> {
    let mut d = vec![0x8c, 0, 0, 1, 1, 0, 0, restriction, 0, 0, 0, 0, 0, 0];
    let pin_off = 23u16;
    let name_off = pin_off + 6;
    let src = b"\\_SB.GPI0\0";
    let vendor_off = name_off + u16::try_from(src.len()).unwrap();
    d.extend_from_slice(&pin_off.to_le_bytes());
    d.push(0);
    d.extend_from_slice(&name_off.to_le_bytes());
    d.extend_from_slice(&vendor_off.to_le_bytes());
    d.extend_from_slice(&0u16.to_le_bytes());
    for p in [5u16, 6, 7] {
        d.extend_from_slice(&p.to_le_bytes());
    }
    d.extend_from_slice(src);
    let len = u16::try_from(d.len() - 3).unwrap();
    d[1..3].copy_from_slice(&len.to_le_bytes());
    d
}

fn access(region: NodeId, conn: &[u8], pin: u64, count: u64) -> GpioAccess<'_> {
    GpioAccess {
        region,
        connection: conn,
        pin,
        count,
        handler: None,
    }
}

#[test]
fn restrictions_and_bounds() {
    let s = s();
    let r = Interpreter::new(aml::Config::default()).root();
    let model = Model::new();
    let handler = SpaceHandler::new(GpioSource::new(Arc::clone(&model) as Arc<dyn Gpio>));
    // Bits 0 and 1 of the value go to the second and third pins.
    let none = gpio_io(0);
    handler
        .gpio_write(&s, &access(r, &none, 1, 2), 0b10)
        .unwrap();
    assert_eq!(
        *model.log.lock().unwrap(),
        [("set", 6, false), ("set", 7, true)]
    );
    assert_eq!(handler.gpio_read(&s, &access(r, &none, 0, 3)), Ok(0b100));
    // Past the connection's table.
    assert_eq!(
        handler.gpio_read(&s, &access(r, &none, 2, 2)),
        Err(HostError::Failed)
    );
    // An input-only connection is not driven, an output-only one not read.
    let input = gpio_io(1);
    assert_eq!(
        handler.gpio_write(&s, &access(r, &input, 0, 1), 1),
        Err(HostError::Denied)
    );
    assert!(handler.gpio_read(&s, &access(r, &input, 0, 1)).is_ok());
    let output = gpio_io(2);
    assert_eq!(
        handler.gpio_read(&s, &access(r, &output, 0, 1)),
        Err(HostError::Denied)
    );
    assert!(handler.gpio_write(&s, &access(r, &output, 0, 1), 1).is_ok());
    // Not a GPIO connection.
    assert_eq!(
        handler.gpio_read(&s, &access(r, &[0x79, 0], 0, 1)),
        Err(HostError::Failed)
    );
}
