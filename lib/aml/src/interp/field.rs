// SPDX-License-Identifier: BSD-2-Clause
//! Field units and buffer fields (ACPI 6.5 §19.6.46–48, §19.6.64,
//! §19.6.15–20, §5.5.2.4).
//!
//! A field unit is read and written in access units of its access width,
//! aligned to that width within the region. A unit the field covers only
//! partly is completed according to the update rule: `Preserve` reads the
//! unit first, `WriteAsOnes` and `WriteAsZeros` fill the other bits. For an
//! `IndexField`, each unit's byte offset is written to the index field and
//! the unit is then moved through the data field; a `BankField` writes its
//! bank value to the bank field before each unit. A field with the `Lock`
//! rule holds the global lock (`\_GL`) for the whole access.
//!
//! `AnyAcc` picks the narrowest width whose aligned unit holds the whole
//! field inside the region, else bytes. SMBus, IPMI, and
//! `GenericSerialBus` fields move whole buffers through [`Host::transfer`];
//! `GeneralPurposeIo` fields go to the GPIO hooks.
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/executer/exfldio.c`
//! (observable access sequences; the slack-mode region limit),
//! `sys/contrib/dev/acpica/components/hardware/hwpci.c` (deriving a PCI
//! address through bridges).

use alloc::vec::Vec;

use super::Interpreter;
use crate::Error;
use crate::host::{
    AccessWidth, BufferTransfer, GpioAccess, Host, HostError, PciAddress, RegionAccess, RegionSpace,
};
use crate::namespace::{AccessType, FieldKind, FieldUnit, NodeId, Object, Region, UpdateRule};
use crate::value::{Location, Value};

fn get_bit(b: &[u8], i: u64) -> bool {
    usize::try_from(i / 8)
        .ok()
        .and_then(|k| b.get(k))
        .is_some_and(|&x| x & (1 << (i % 8)) != 0)
}

fn set_bit(b: &mut [u8], i: u64, v: bool) {
    if let Some(x) = usize::try_from(i / 8).ok().and_then(|k| b.get_mut(k)) {
        let m = 1u8 << (i % 8);
        if v {
            *x |= m;
        } else {
            *x &= !m;
        }
    }
}

fn get_bits_u64(b: &[u8], pos: u64, n: u64) -> u64 {
    let mut v = 0u64;
    for i in 0..n.min(64) {
        if get_bit(b, pos + i) {
            v |= 1 << i;
        }
    }
    v
}

fn put_bits_u64(b: &mut [u8], pos: u64, n: u64, v: u64) {
    for i in 0..n.min(64) {
        set_bit(b, pos + i, v & (1 << i) != 0);
    }
}

const fn low_mask(n: u64) -> u64 {
    if n >= 64 { u64::MAX } else { (1 << n) - 1 }
}

impl Interpreter {
    fn bytes_for_bits(&self, bits: u64) -> Result<usize, Error> {
        let n = usize::try_from(bits.div_ceil(8)).map_err(|_| Error::NoMemory)?;
        if n > self.config.limits.max_object_size {
            return Err(Error::NoMemory);
        }
        Ok(n)
    }

    /// A field's bits as a value: an integer if they fit the integer width.
    fn bits_value(&self, bytes: Vec<u8>, bit_len: u64) -> Value {
        if bit_len <= u64::from(self.width.bits()) {
            Value::Integer(crate::value::buffer_to_int(&bytes, self.width))
        } else {
            Value::Buffer(bytes)
        }
    }

    /// The bytes a value supplies to a field `bit_len` bits wide.
    fn field_source(&self, v: &Value, bit_len: u64) -> Result<Vec<u8>, Error> {
        let n = self.bytes_for_bits(bit_len)?;
        let src: Vec<u8> = match v {
            Value::Integer(x) => x.to_le_bytes().to_vec(),
            Value::Buffer(b) => b.clone(),
            Value::String(s) => s.as_bytes().to_vec(),
            _ => return Err(Error::TypeMismatch),
        };
        let mut out = alloc::vec![0u8; n];
        let k = n.min(src.len());
        out[..k].copy_from_slice(&src[..k]);
        Ok(out)
    }

    fn region_of(&self, f: &FieldUnit) -> Option<(NodeId, Region)> {
        let r = match f.kind {
            FieldKind::Region(r) | FieldKind::Bank { region: r, .. } => r,
            FieldKind::Index { .. } => return None,
        };
        match self.ns.object(r) {
            Some(Object::Region(reg)) => Some((r, reg.clone())),
            _ => None,
        }
    }

    fn unit_width(&self, f: &FieldUnit, region_len: Option<u64>) -> AccessWidth {
        match f.access {
            AccessType::Byte | AccessType::Buffer => AccessWidth::U8,
            AccessType::Word => AccessWidth::U16,
            AccessType::DWord => AccessWidth::U32,
            AccessType::QWord => AccessWidth::U64,
            AccessType::Any => {
                let last = f.bit_offset + f.bit_len.max(1) - 1;
                for w in [
                    AccessWidth::U8,
                    AccessWidth::U16,
                    AccessWidth::U32,
                    AccessWidth::U64,
                ] {
                    let ub = w.bits();
                    let unit = f.bit_offset / ub;
                    let fits_region = region_len.is_none_or(|len| (unit + 1) * w.bytes() <= len);
                    if unit == last / ub && fits_region {
                        return w;
                    }
                }
                AccessWidth::U8
            }
        }
    }

    fn global_lock(&mut self, h: &mut dyn Host, take: bool) -> Result<(), Error> {
        let Some(gl) = self.child(self.root(), "_GL") else {
            return Ok(());
        };
        if take {
            if !self.acquire(h, gl, 0xffff)? {
                return Err(Error::Host(HostError::Failed));
            }
            Ok(())
        } else {
            self.release(h, gl)
        }
    }

    /// Reads a field unit.
    #[inline(never)]
    pub(crate) fn read_field(&mut self, h: &mut dyn Host, node: NodeId) -> Result<Value, Error> {
        self.enter()?;
        let f = match self.ns.object(node) {
            Some(Object::Field(f)) => Ok(f.clone()),
            _ => Err(Error::Internal),
        };
        let r = match f {
            Ok(f) if f.lock => match self.global_lock(h, true) {
                Ok(()) => {
                    let r = self.read_field_unlocked(h, node, &f);
                    let u = self.global_lock(h, false);
                    r.and_then(|v| u.map(|()| v))
                }
                Err(e) => Err(e),
            },
            Ok(f) => self.read_field_unlocked(h, node, &f),
            Err(e) => Err(e),
        };
        self.leave();
        r
    }

    fn read_field_unlocked(
        &mut self,
        h: &mut dyn Host,
        _node: NodeId,
        f: &FieldUnit,
    ) -> Result<Value, Error> {
        let region = self.region_of(f);
        if let Some((rnode, reg)) = &region {
            if reg.space.is_buffer_protocol() {
                let req = BufferTransfer {
                    space: reg.space,
                    region: *rnode,
                    command: f.bit_offset / 8,
                    attrib: f.attrib,
                    access_len: f.access_len,
                    connection: f.connection.as_deref(),
                    write: None,
                };
                let b = h.transfer(&req).map_err(Error::Host)?;
                return Ok(Value::Buffer(b));
            }
            if reg.space == RegionSpace::GeneralPurposeIo {
                let conn = f.connection.as_deref().ok_or(Error::NoConnection)?;
                let a = GpioAccess {
                    region: *rnode,
                    connection: conn,
                    pin: f.bit_offset,
                    count: f.bit_len,
                };
                let v = h.gpio_read(&a).map_err(Error::Host)?;
                return Ok(Value::Integer(self.width.mask(v & low_mask(f.bit_len))));
            }
        }
        let n = self.bytes_for_bits(f.bit_len)?;
        let mut out = alloc::vec![0u8; n];
        if f.bit_len > 0 {
            let w = self.unit_width(f, region.as_ref().map(|(_, r)| r.len));
            let ub = w.bits();
            let first = f.bit_offset / ub;
            let last = (f.bit_offset + f.bit_len - 1) / ub;
            for u in first..=last {
                let raw = self.unit_read(h, f, u * w.bytes(), w)?;
                let lo = f.bit_offset.max(u * ub);
                let hi = (f.bit_offset + f.bit_len).min((u + 1) * ub);
                let bits = (raw >> (lo - u * ub)) & low_mask(hi - lo);
                put_bits_u64(&mut out, lo - f.bit_offset, hi - lo, bits);
            }
        }
        Ok(self.bits_value(out, f.bit_len))
    }

    /// Writes a field unit. Returns the result buffer of a buffer-protocol
    /// transfer.
    #[inline(never)]
    pub(crate) fn write_field(
        &mut self,
        h: &mut dyn Host,
        node: NodeId,
        v: &Value,
    ) -> Result<Option<Value>, Error> {
        self.enter()?;
        let f = match self.ns.object(node) {
            Some(Object::Field(f)) => Ok(f.clone()),
            _ => Err(Error::Internal),
        };
        let r = match f {
            Ok(f) if f.lock => match self.global_lock(h, true) {
                Ok(()) => {
                    let r = self.write_field_unlocked(h, &f, v);
                    let u = self.global_lock(h, false);
                    r.and_then(|v| u.map(|()| v))
                }
                Err(e) => Err(e),
            },
            Ok(f) => self.write_field_unlocked(h, &f, v),
            Err(e) => Err(e),
        };
        self.leave();
        r
    }

    fn write_field_unlocked(
        &mut self,
        h: &mut dyn Host,
        f: &FieldUnit,
        v: &Value,
    ) -> Result<Option<Value>, Error> {
        let region = self.region_of(f);
        if let Some((rnode, reg)) = &region {
            if reg.space.is_buffer_protocol() {
                let data = v.to_buffer(self.width)?;
                let req = BufferTransfer {
                    space: reg.space,
                    region: *rnode,
                    command: f.bit_offset / 8,
                    attrib: f.attrib,
                    access_len: f.access_len,
                    connection: f.connection.as_deref(),
                    write: Some(&data),
                };
                let b = h.transfer(&req).map_err(Error::Host)?;
                return Ok(Some(Value::Buffer(b)));
            }
            if reg.space == RegionSpace::GeneralPurposeIo {
                let conn = f.connection.as_deref().ok_or(Error::NoConnection)?;
                let a = GpioAccess {
                    region: *rnode,
                    connection: conn,
                    pin: f.bit_offset,
                    count: f.bit_len,
                };
                let x = v.to_integer(self.width)? & low_mask(f.bit_len);
                h.gpio_write(&a, x).map_err(Error::Host)?;
                return Ok(None);
            }
        }
        if f.bit_len == 0 {
            return Ok(None);
        }
        let src = self.field_source(v, f.bit_len)?;
        let w = self.unit_width(f, region.as_ref().map(|(_, r)| r.len));
        let ub = w.bits();
        let first = f.bit_offset / ub;
        let last = (f.bit_offset + f.bit_len - 1) / ub;
        for u in first..=last {
            let lo = f.bit_offset.max(u * ub);
            let hi = (f.bit_offset + f.bit_len).min((u + 1) * ub);
            let n = hi - lo;
            let shift = lo - u * ub;
            let bits = get_bits_u64(&src, lo - f.bit_offset, n);
            let value = if n == ub {
                bits
            } else {
                let base = match f.update {
                    UpdateRule::Preserve => self.unit_read(h, f, u * w.bytes(), w)?,
                    UpdateRule::WriteAsOnes => low_mask(ub),
                    UpdateRule::WriteAsZeros => 0,
                };
                let m = low_mask(n) << shift;
                (base & !m) | (bits << shift)
            };
            self.unit_write(h, f, u * w.bytes(), w, value & low_mask(ub))?;
        }
        Ok(None)
    }

    fn unit_read(
        &mut self,
        h: &mut dyn Host,
        f: &FieldUnit,
        offset: u64,
        w: AccessWidth,
    ) -> Result<u64, Error> {
        match f.kind {
            FieldKind::Region(r) => self.region_read(h, r, offset, w),
            FieldKind::Bank {
                region,
                bank,
                value,
            } => {
                self.write_field(h, bank, &Value::Integer(value))?;
                self.region_read(h, region, offset, w)
            }
            FieldKind::Index { index, data } => {
                self.write_field(h, index, &Value::Integer(offset))?;
                let v = self.read_field(h, data)?;
                Ok(v.to_integer(self.width)? & low_mask(w.bits()))
            }
        }
    }

    fn unit_write(
        &mut self,
        h: &mut dyn Host,
        f: &FieldUnit,
        offset: u64,
        w: AccessWidth,
        value: u64,
    ) -> Result<(), Error> {
        match f.kind {
            FieldKind::Region(r) => self.region_write(h, r, offset, w, value),
            FieldKind::Bank {
                region,
                bank,
                value: bv,
            } => {
                self.write_field(h, bank, &Value::Integer(bv))?;
                self.region_write(h, region, offset, w, value)
            }
            FieldKind::Index { index, data } => {
                self.write_field(h, index, &Value::Integer(offset))?;
                self.write_field(h, data, &Value::Integer(value))?;
                Ok(())
            }
        }
    }

    fn region_access(
        &mut self,
        h: &mut dyn Host,
        region: NodeId,
        offset: u64,
        w: AccessWidth,
    ) -> Result<RegionAccess, Error> {
        let Some(Object::Region(r)) = self.ns.object(region) else {
            return Err(Error::TypeMismatch);
        };
        let r = r.clone();
        let end = offset.checked_add(w.bytes()).ok_or(Error::RegionLimit)?;
        if end > r.len {
            let rounded = r.len.div_ceil(w.bytes()).saturating_mul(w.bytes());
            if !self.config.slack || end > rounded {
                return Err(Error::RegionLimit);
            }
        }
        let pci = if matches!(r.space, RegionSpace::PciConfig | RegionSpace::PciBarTarget) {
            Some(self.region_pci(h, region)?)
        } else {
            None
        };
        Ok(RegionAccess {
            space: r.space,
            region,
            base: r.base,
            offset,
            width: w,
            pci,
        })
    }

    fn region_read(
        &mut self,
        h: &mut dyn Host,
        region: NodeId,
        offset: u64,
        w: AccessWidth,
    ) -> Result<u64, Error> {
        let a = self.region_access(h, region, offset, w)?;
        Ok(h.read(&a).map_err(Error::Host)? & low_mask(w.bits()))
    }

    fn region_write(
        &mut self,
        h: &mut dyn Host,
        region: NodeId,
        offset: u64,
        w: AccessWidth,
        value: u64,
    ) -> Result<(), Error> {
        let a = self.region_access(h, region, offset, w)?;
        h.write(&a, value & low_mask(w.bits())).map_err(Error::Host)
    }

    fn eval_child_int(&mut self, h: &mut dyn Host, node: NodeId, name: &str) -> Option<u64> {
        let v = self.evaluate_child(node, name, Vec::new(), h).ok()??;
        v.to_integer(self.width).ok()
    }

    /// The PCI function a `PCI_Config` region belongs to: `_ADR` of its
    /// device, on the bus reached from the host bridge's `_BBN` through the
    /// secondary bus numbers of the bridges in between, in `_SEG`.
    fn region_pci(&mut self, h: &mut dyn Host, region: NodeId) -> Result<PciAddress, Error> {
        if let Some(Object::Region(Region { pci: Some(p), .. })) = self.ns.object(region) {
            return Ok(*p);
        }
        let dev = self.ns.parent(region).ok_or(Error::Internal)?;
        let adr = self.eval_child_int(h, dev, "_ADR").unwrap_or(0);
        let mut chain = Vec::new();
        let mut root = None;
        let mut at = Some(dev);
        while let Some(n) = at {
            if self.is_pci_root(h, n) {
                root = Some(n);
                break;
            }
            chain.push(n);
            at = self.ns.parent(n);
        }
        let (segment, mut bus) = match root {
            Some(r) => (
                self.eval_child_int(h, r, "_SEG").unwrap_or(0),
                self.eval_child_int(h, r, "_BBN").unwrap_or(0),
            ),
            None => (0, 0),
        };
        let lo8 = |v: u64| v.to_le_bytes()[0];
        // Bridges between the host bridge and the device, outermost first.
        if root.is_some() {
            for &b in chain.iter().skip(1).rev() {
                let Some(badr) = self.eval_child_int(h, b, "_ADR") else {
                    continue;
                };
                let addr = PciAddress {
                    segment: u16::try_from(segment & 0xffff).unwrap_or(0),
                    bus: lo8(bus),
                    device: lo8(badr >> 16) & 0x1f,
                    function: lo8(badr) & 7,
                };
                let mut cfg = |off: u64| {
                    let a = RegionAccess {
                        space: RegionSpace::PciConfig,
                        region,
                        base: 0,
                        offset: off,
                        width: AccessWidth::U8,
                        pci: Some(addr),
                    };
                    h.read(&a).ok()
                };
                if let Some(ht) = cfg(0x0e)
                    && matches!(ht & 0x7f, 1 | 2)
                    && let Some(sec) = cfg(0x19)
                {
                    bus = sec & 0xff;
                }
            }
        }
        let p = PciAddress {
            segment: u16::try_from(segment & 0xffff).unwrap_or(0),
            bus: lo8(bus),
            device: lo8(adr >> 16),
            function: lo8(adr),
        };
        if let Some(Object::Region(r)) = self.ns.get_mut(region).map(|n| &mut n.object) {
            r.pci = Some(p);
        }
        Ok(p)
    }

    fn is_pci_root(&mut self, h: &mut dyn Host, node: NodeId) -> bool {
        const ROOTS: [&str; 3] = ["PNP0A03", "PNP0A08", "ACPI0016"];
        self.ids(h, node)
            .iter()
            .any(|id| ROOTS.contains(&id.as_str()))
    }

    /// Reads a buffer field.
    pub(crate) fn read_buffer_field(&mut self, node: NodeId) -> Result<Value, Error> {
        let Some(Object::BufferField(bf)) = self.ns.object(node) else {
            return Err(Error::Internal);
        };
        let bf = bf.clone();
        let Value::Buffer(buf) = self.storage(&bf.source)? else {
            return Err(Error::TypeMismatch);
        };
        let end = bf
            .bit_offset
            .checked_add(bf.bit_len)
            .ok_or(Error::BufferLimit)?;
        if end
            > u64::try_from(buf.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(8)
        {
            return Err(Error::BufferLimit);
        }
        let n = self.bytes_for_bits(bf.bit_len)?;
        let mut out = alloc::vec![0u8; n];
        if bf.bit_offset % 8 == 0 && bf.bit_len % 8 == 0 {
            let s = usize::try_from(bf.bit_offset / 8).map_err(|_| Error::BufferLimit)?;
            out.copy_from_slice(&buf[s..s + n]);
        } else {
            for i in 0..bf.bit_len {
                set_bit(&mut out, i, get_bit(buf, bf.bit_offset + i));
            }
        }
        Ok(self.bits_value(out, bf.bit_len))
    }

    /// Writes a buffer field.
    pub(crate) fn write_buffer_field(&mut self, node: NodeId, v: &Value) -> Result<(), Error> {
        let Some(Object::BufferField(bf)) = self.ns.object(node) else {
            return Err(Error::Internal);
        };
        let (source, bit_offset, bit_len) = (bf.source.clone(), bf.bit_offset, bf.bit_len);
        let src = self.field_source(v, bit_len)?;
        let buf: &mut Value = if matches!(source, Location::Temp(_)) {
            match self.ns.get_mut(node).map(|n| &mut n.object) {
                Some(Object::BufferField(crate::namespace::BufferField {
                    source: Location::Temp(b),
                    ..
                })) => b,
                _ => return Err(Error::Internal),
            }
        } else {
            self.storage_mut_for_field(&source)?
        };
        let Value::Buffer(buf) = buf else {
            return Err(Error::TypeMismatch);
        };
        let end = bit_offset.checked_add(bit_len).ok_or(Error::BufferLimit)?;
        if end
            > u64::try_from(buf.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(8)
        {
            return Err(Error::BufferLimit);
        }
        for i in 0..bit_len {
            set_bit(buf, bit_offset + i, get_bit(&src, i));
        }
        Ok(())
    }
}
