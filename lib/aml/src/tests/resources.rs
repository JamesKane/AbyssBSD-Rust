// SPDX-License-Identifier: BSD-2-Clause
//! Resource template decoding, from hand-assembled descriptors.

extern crate std;

use alloc::vec::Vec;

use crate::resource::*;
use crate::{Error, eisa_id, eisa_id_to_string};

fn with_end(parts: &[&[u8]]) -> Vec<u8> {
    let mut v: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
    v.extend_from_slice(&[0x79, 0]);
    v
}

fn large(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut v = std::vec![tag];
    v.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn one(t: &[u8]) -> Resource {
    let r = parse(t).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(r.len(), 1, "{r:?}");
    r.into_iter().next().unwrap()
}

#[test]
fn small_descriptors() {
    // IRQ (Level, ActiveLow, Shared) {3, 4}; IRQNoFlags () {9}.
    let r = parse(&with_end(&[&[0x23, 0x18, 0x00, 0x18], &[0x22, 0x00, 0x02]])).unwrap();
    assert_eq!(
        r,
        [
            Resource::Irq(Irq {
                mask: 0x18,
                trigger: Trigger::Level,
                polarity: Polarity::ActiveLow,
                shared: true,
                wake: false
            }),
            Resource::Irq(Irq {
                mask: 0x200,
                trigger: Trigger::Edge,
                polarity: Polarity::ActiveHigh,
                shared: false,
                wake: false
            })
        ]
    );
    // DMA (Compatibility, BusMaster, Transfer8_16) {2}.
    assert_eq!(
        one(&with_end(&[&[0x2a, 0x04, 0x05]])),
        Resource::Dma(Dma {
            channels: 4,
            transfer: 1,
            bus_master: true,
            speed: 0
        })
    );
    // StartDependentFn (1, 2) {...} EndDependentFn ().
    let r = parse(&with_end(&[
        &[0x31, 0x09],
        &[0x47, 1, 0x80, 2, 0x80, 2, 8, 8],
        &[0x38],
    ]))
    .unwrap();
    assert_eq!(
        r[0],
        Resource::StartDependent {
            compatibility: 1,
            performance: 2
        }
    );
    assert_eq!(r[2], Resource::EndDependent);
    // StartDependentFnNoPri: acceptable/acceptable.
    assert!(matches!(
        parse(&with_end(&[&[0x30], &[0x38]])).unwrap()[0],
        Resource::StartDependent {
            compatibility: 1,
            performance: 1
        }
    ));
    // IO (Decode16, 0x3F8, 0x3F8, 1, 8).
    assert_eq!(
        one(&with_end(&[&[0x47, 1, 0xf8, 3, 0xf8, 3, 1, 8]])),
        Resource::Io(Io {
            decode16: true,
            min: 0x3f8,
            max: 0x3f8,
            alignment: 1,
            length: 8
        })
    );
    // FixedIO (0x60, 1): ten address bits.
    assert_eq!(
        one(&with_end(&[&[0x4b, 0x60, 0xfc, 1]])),
        Resource::FixedIo {
            base: 0x060,
            length: 1
        }
    );
    // FixedDMA (5, 6, Width32bit).
    assert_eq!(
        one(&with_end(&[&[0x55, 5, 0, 6, 0, 2]])),
        Resource::FixedDma {
            request: 5,
            channel: 6,
            width: 2
        }
    );
    assert_eq!(
        one(&with_end(&[&[0x73, 1, 2, 3]])),
        Resource::VendorShort(std::vec![1, 2, 3])
    );
}

#[test]
fn memory_and_register_descriptors() {
    // Memory24 (ReadWrite, 0x0A0000, 0x0BFFFF, 0, 0x20000): units of 256.
    let m24 = large(0x81, &[1, 0x00, 0x0a, 0xff, 0x0b, 0, 0, 0x00, 0x02]);
    assert_eq!(
        one(&with_end(&[&m24])),
        Resource::Memory24(Memory {
            writeable: true,
            min: 0x0a_0000,
            max: 0x0b_ff00,
            alignment: 0,
            length: 0x2_0000
        })
    );
    let mut d = std::vec![0];
    for v in [0x1000u32, 0x2000, 0x10, 0x100] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(
        one(&with_end(&[&large(0x85, &d)])),
        Resource::Memory32(Memory {
            writeable: false,
            min: 0x1000,
            max: 0x2000,
            alignment: 0x10,
            length: 0x100
        })
    );
    let mut d = std::vec![1];
    d.extend_from_slice(&0xfed0_0000u32.to_le_bytes());
    d.extend_from_slice(&0x400u32.to_le_bytes());
    assert_eq!(
        one(&with_end(&[&large(0x86, &d)])),
        Resource::FixedMemory32(Memory {
            writeable: true,
            min: 0xfed0_0000,
            max: 0xfed0_0000,
            alignment: 0,
            length: 0x400
        })
    );
    // Register (SystemIO, 8, 0, 0xB2, 1).
    let mut d = std::vec![1, 8, 0, 1];
    d.extend_from_slice(&0xb2u64.to_le_bytes());
    assert_eq!(
        one(&with_end(&[&large(0x82, &d)])),
        Resource::GenericRegister(GenericRegister {
            space: 1,
            bit_width: 8,
            bit_offset: 0,
            access_size: 1,
            address: 0xb2
        })
    );
    assert_eq!(
        one(&with_end(&[&large(0x84, &[0x10, 1, 2])])),
        Resource::VendorLong(std::vec![0x10, 1, 2])
    );
}

#[test]
fn address_descriptors() {
    // DWordMemory (ResourceConsumer, PosDecode, MinFixed, MaxFixed,
    // Prefetchable, ReadWrite, 0, 0x8000_0000, 0x8FFF_FFFF, 0, 0x1000_0000,
    // 2, "\_SB.RES")
    let mut d = std::vec![0, 0x0d, 0x07];
    for v in [0u32, 0x8000_0000, 0x8fff_ffff, 0, 0x1000_0000] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    d.push(2);
    d.extend_from_slice(b"\\_SB.RES\0");
    assert_eq!(
        one(&with_end(&[&large(0x87, &d)])),
        Resource::Address(Address {
            width: AddressWidth::DWord,
            kind: AddressKind::Memory {
                writeable: true,
                caching: Caching::Prefetchable,
                memory_type: 0,
                translation: false
            },
            consumer: true,
            subtractive: false,
            min_fixed: true,
            max_fixed: true,
            granularity: 0,
            min: 0x8000_0000,
            max: 0x8fff_ffff,
            translation: 0,
            length: 0x1000_0000,
            attributes: None,
            source: Some(ResourceSource {
                index: 2,
                name: "\\_SB.RES".into()
            })
        })
    );
    // QWordIO with sparse translation; WordBusNumber, subtractive.
    let mut d = std::vec![1, 0x0c, 0x33];
    for v in [0u64, 0, 0xffff, 0x3eff_0000, 0x1_0000] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    let Resource::Address(a) = one(&with_end(&[&large(0x8a, &d)])) else {
        panic!()
    };
    assert_eq!(
        a.kind,
        AddressKind::Io {
            range: 3,
            translation: true,
            sparse: true
        }
    );
    assert_eq!((a.translation, a.width), (0x3eff_0000, AddressWidth::QWord));
    let mut d = std::vec![2, 0x02, 0];
    for v in [0u16, 0, 0x7f, 0, 0x80] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    let Resource::Address(a) = one(&with_end(&[&large(0x88, &d)])) else {
        panic!()
    };
    assert_eq!(
        (a.kind, a.subtractive, a.max),
        (AddressKind::BusNumber, true, 0x7f)
    );
    // ExtendedMemory with type-specific attributes.
    let mut d = std::vec![0, 0x0c, 0x01, 1, 0];
    for v in [0u64, 0x1_0000_0000, 0x1_ffff_ffff, 0, 0x1_0000_0000, 0x8000] {
        d.extend_from_slice(&v.to_le_bytes());
    }
    let Resource::Address(a) = one(&with_end(&[&large(0x8b, &d)])) else {
        panic!()
    };
    assert_eq!(
        (a.width, a.attributes, a.min),
        (AddressWidth::Extended, Some(0x8000), 0x1_0000_0000)
    );
    // A vendor-defined resource type keeps its raw flags.
    let mut d = std::vec![0xc0, 0, 0x5a];
    d.extend_from_slice(&[0; 10]);
    let Resource::Address(a) = one(&with_end(&[&large(0x88, &d)])) else {
        panic!()
    };
    assert_eq!(
        a.kind,
        AddressKind::Other {
            kind: 0xc0,
            flags: 0x5a
        }
    );
}

#[test]
fn extended_interrupts() {
    // Interrupt (ResourceConsumer, Edge, ActiveLow, Shared, "\_SB.GIC")
    // {33, 34}
    let mut d = std::vec![0x0f, 2];
    d.extend_from_slice(&33u32.to_le_bytes());
    d.extend_from_slice(&34u32.to_le_bytes());
    d.push(0);
    d.extend_from_slice(b"\\_SB.GIC\0");
    assert_eq!(
        one(&with_end(&[&large(0x89, &d)])),
        Resource::ExtendedIrq(ExtendedIrq {
            consumer: true,
            trigger: Trigger::Edge,
            polarity: Polarity::ActiveLow,
            shared: true,
            wake: false,
            interrupts: std::vec![33, 34],
            source: Some(ResourceSource {
                index: 0,
                name: "\\_SB.GIC".into()
            })
        })
    );
    // A count larger than the descriptor holds, and a zero count.
    assert!(parse(&with_end(&[&large(0x89, &[0x01, 3, 1, 0, 0, 0])])).is_err());
    assert!(parse(&with_end(&[&large(0x89, &[0x01, 0])])).is_err());
}

/// A GPIO descriptor with the given pins, source, and vendor data.
fn gpio(ty: u8, flags: u16, pins: &[u16], source: &[u8], vendor: &[u8]) -> Vec<u8> {
    let pin_off = 23u16;
    let name_off = pin_off + 2 * u16::try_from(pins.len()).unwrap();
    let vendor_off = name_off + u16::try_from(source.len()).unwrap();
    let mut d = std::vec![0x8c, 0, 0, 1, ty, 1, 0];
    d.extend_from_slice(&flags.to_le_bytes());
    d.push(2); // pull-down
    d.extend_from_slice(&100u16.to_le_bytes());
    d.extend_from_slice(&500u16.to_le_bytes());
    d.extend_from_slice(&pin_off.to_le_bytes());
    d.push(0);
    d.extend_from_slice(&name_off.to_le_bytes());
    d.extend_from_slice(&vendor_off.to_le_bytes());
    d.extend_from_slice(&u16::try_from(vendor.len()).unwrap().to_le_bytes());
    for p in pins {
        d.extend_from_slice(&p.to_le_bytes());
    }
    d.extend_from_slice(source);
    d.extend_from_slice(vendor);
    let len = u16::try_from(d.len() - 3).unwrap();
    d[1..3].copy_from_slice(&len.to_le_bytes());
    d
}

#[test]
fn gpio_descriptors() {
    // GpioInt (Edge, ActiveBoth, ExclusiveAndWake, PullDown, 500,
    // "\_SB.GIO0") {124}
    let t = with_end(&[&gpio(0, 0x15, &[124], b"\\_SB.GIO0\0", &[])]);
    assert_eq!(
        one(&t),
        Resource::Gpio(Gpio {
            revision: 1,
            kind: GpioKind::Interrupt {
                trigger: Trigger::Edge,
                polarity: Polarity::ActiveBoth,
                shared: false,
                wake: true
            },
            consumer: true,
            pin_config: 2,
            drive_strength: 100,
            debounce: 500,
            pins: std::vec![124],
            source: ResourceSource {
                index: 0,
                name: "\\_SB.GIO0".into()
            },
            vendor: Vec::new()
        })
    );
    // GpioIo (Shared, ..., IoRestrictionOutputOnly) {1, 2} with vendor data.
    let Resource::Gpio(g) = one(&with_end(&[&gpio(1, 0x0a, &[1, 2], b"\\GP\0", &[9, 8])])) else {
        panic!()
    };
    assert_eq!(
        g.kind,
        GpioKind::Io {
            restriction: 2,
            shared: true
        }
    );
    assert_eq!((g.pins, g.vendor), (std::vec![1, 2], std::vec![9, 8]));
    // Offsets outside the descriptor, and an odd pin table.
    let mut bad = gpio(0, 0, &[1], b"\\X\0", &[]);
    bad[17] = 0xff;
    assert!(parse(&with_end(&[&bad])).is_err());
    let mut odd = gpio(0, 0, &[1], b"\\X\0", &[]);
    odd[14] = 24;
    assert!(parse(&with_end(&[&odd])).is_err());
}

fn serial(ty: u8, flags: u16, data: &[u8], source: &[u8]) -> Vec<u8> {
    let mut d = std::vec![1, 0, ty, 0x02];
    d.extend_from_slice(&flags.to_le_bytes());
    d.push(1);
    d.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
    d.extend_from_slice(data);
    d.extend_from_slice(source);
    large(0x8e, &d)
}

#[test]
fn serial_bus_descriptors() {
    // I2cSerialBusV2 (0x1C, ControllerInitiated, 400000, AddressingMode7Bit,
    // "\_SB.I2C1")
    let mut data = 400_000u32.to_le_bytes().to_vec();
    data.extend_from_slice(&0x1cu16.to_le_bytes());
    let Resource::SerialBus(s) = one(&with_end(&[&serial(1, 0, &data, b"\\_SB.I2C1\0")])) else {
        panic!()
    };
    assert_eq!(
        s.bus,
        SerialBusKind::I2c {
            ten_bit: false,
            speed: 400_000,
            address: 0x1c
        }
    );
    assert!(s.consumer && !s.slave_mode);
    assert_eq!(s.source.name, "\\_SB.I2C1");
    // SpiSerialBusV2 (1, PolarityHigh, FourWireMode, 8, ControllerInitiated,
    // 1000000, ClockPolarityHigh, ClockPhaseSecond, "\_SB.SPI0")
    let mut data = 1_000_000u32.to_le_bytes().to_vec();
    data.extend_from_slice(&[8, 1, 1, 1, 0]);
    let Resource::SerialBus(s) = one(&with_end(&[&serial(2, 2, &data, b"\\_SB.SPI0\0")])) else {
        panic!()
    };
    assert_eq!(
        s.bus,
        SerialBusKind::Spi {
            three_wire: false,
            cs_active_high: true,
            speed: 1_000_000,
            data_bits: 8,
            phase: 1,
            polarity: 1,
            device_select: 1
        }
    );
    // UartSerialBusV2 (115200, DataBitsEight, StopBitsOne, 0xC0,
    // LittleEndian, ParityTypeNone, FlowControlHardware, 32, 32, "\_SB.UAR3")
    // with two bytes of vendor data.
    let mut data = 115_200u32.to_le_bytes().to_vec();
    data.extend_from_slice(&[32, 0, 32, 0, 0, 0xc0, 0xaa, 0xbb]);
    let Resource::SerialBus(s) = one(&with_end(&[&serial(3, 0x35, &data, b"\\_SB.UAR3\0")])) else {
        panic!()
    };
    assert_eq!(
        s.bus,
        SerialBusKind::Uart {
            flags: 0x35,
            baud: 115_200,
            rx_fifo: 32,
            tx_fifo: 32,
            parity: 0,
            lines: 0xc0
        }
    );
    assert_eq!(s.vendor, [0xaa, 0xbb]);
    // Bus-specific data too short for its type.
    assert!(parse(&with_end(&[&serial(1, 0, &[0; 4], b"\\X\0")])).is_err());
    // A type data length past the descriptor.
    let mut bad = serial(1, 0, &data[..6], b"\\X\0");
    bad[11] = 0x40;
    assert!(parse(&with_end(&[&bad])).is_err());
}

#[test]
fn pin_and_unknown_descriptors_are_kept() {
    // PinFunction (0x8D) and a reserved large type.
    let r = parse(&with_end(&[&large(0x8d, &[1, 2, 3]), &large(0xa0, &[4])])).unwrap();
    assert_eq!(
        r,
        [
            Resource::Unknown {
                tag: 0x8d,
                data: std::vec![1, 2, 3]
            },
            Resource::Unknown {
                tag: 0xa0,
                data: std::vec![4]
            }
        ]
    );
}

#[test]
fn end_tag_and_malformed_templates() {
    // A non-zero checksum must make the template sum to zero.
    let mut t = std::vec![0x22, 0x10, 0x00, 0x79, 0];
    let sum = t.iter().fold(0u8, |a, &b| a.wrapping_add(b));
    t[4] = 0u8.wrapping_sub(sum);
    assert_eq!(parse(&t).unwrap().len(), 1);
    t[4] = t[4].wrapping_add(1);
    assert_eq!(parse(&t), Err(Error::BadResource("checksum")));
    // Empty templates, missing end tags, truncation, bad lengths.
    assert_eq!(parse(&[0x79, 0]).unwrap(), []);
    assert_eq!(parse(&[]), Err(Error::BadResource("no end tag")));
    assert_eq!(
        parse(&[0x22, 0x10, 0x00]),
        Err(Error::BadResource("no end tag"))
    );
    assert!(parse(&[0x47, 1, 2]).is_err());
    assert!(parse(&[0x86, 0xff, 0xff, 0, 0x79, 0]).is_err());
    assert!(parse(&with_end(&[&[0x21, 0]])).is_err());
    assert!(parse(&with_end(&[&large(0x86, &[0; 8])])).is_err());
    assert!(parse(&with_end(&[&large(0x87, &[0; 10])])).is_err());
    assert!(parse(&with_end(&[&large(0x8c, &[0; 10])])).is_err());
    // Bytes after the end tag are ignored, as ACPICA does.
    assert_eq!(parse(&[0x79, 0, 0xff, 0xff]).unwrap(), []);
    assert_eq!(end_tag_offset(&[0x22, 1, 0, 0x79, 0]), Ok(3));
}

#[test]
fn eisa_ids() {
    assert_eq!(eisa_id("PNP0A08"), Some(0x080a_d041));
    assert_eq!(eisa_id_to_string(0x080a_d041), "PNP0A08");
    for s in ["PNP0C0F", "QEM0002", "ABC1234", "PNP0501"] {
        assert_eq!(eisa_id_to_string(eisa_id(s).unwrap()), s);
    }
    assert_eq!(eisa_id("ACPI0007"), None);
    assert_eq!(eisa_id("pnp0a08"), None);
    assert_eq!(eisa_id("PNP0A0G"), None);
}
