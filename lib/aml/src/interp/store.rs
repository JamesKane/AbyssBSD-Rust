// SPDX-License-Identifier: BSD-2-Clause
//! Locations, reading objects, and `Store`/`CopyObject` with the implicit
//! conversion rules (ACPI 6.5 §19.3.5.8, §19.6.132, §19.6.29).
//!
//! - A local is overwritten by `Store`.
//! - An argument holding a reference is stored through; otherwise it is
//!   overwritten.
//! - A named integer, string, or buffer converts the value to its own type;
//!   a buffer keeps its length (zero-filled or truncated) unless it is
//!   empty. Other named data is replaced.
//! - Field units and buffer fields are written.
//! - A package element is replaced; a buffer or string element takes the
//!   low byte of the value.
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/executer/exstore.c`,
//! `exstorob.c` (observable results of buffer and string stores).

use super::Interpreter;
use crate::Error;
use crate::host::Host;
use crate::namespace::{NodeId, Object};
use crate::value::{Location, ObjectType, Value};

const MAX_REF_CHAIN: u32 = 8;

/// Deepest package nesting a store into a package element may create.
/// Values are dropped and cloned recursively, so their nesting must stay
/// bounded; firmware packages nest two or three levels.
pub(crate) const MAX_VALUE_NESTING: usize = 64;

/// How deeply boxed values nest in `v`: packages, and references whose
/// locations hold further locations or temporary values. Counted without
/// recursion and capped just past `cap`.
pub(crate) fn value_nesting(v: &Value, cap: usize) -> usize {
    enum Item<'a> {
        V(&'a Value),
        L(&'a Location),
    }
    let mut deepest = 0;
    let mut stack = alloc::vec![(Item::V(v), 0usize)];
    while let Some((item, d)) = stack.pop() {
        if d > cap {
            deepest = deepest.max(d);
            continue;
        }
        match item {
            Item::V(Value::Package(p)) => {
                deepest = deepest.max(d + 1);
                stack.extend(p.iter().map(|e| (Item::V(e), d + 1)));
            }
            Item::V(Value::Reference(r)) => stack.push((Item::L(&r.0), d)),
            Item::V(_) => {}
            Item::L(Location::Index(base, _)) => {
                deepest = deepest.max(d + 1);
                stack.push((Item::L(base), d + 1));
            }
            Item::L(Location::TempIndex(v, _) | Location::Temp(v)) => {
                deepest = deepest.max(d + 1);
                stack.push((Item::V(v), d + 1));
            }
            Item::L(_) => {}
        }
    }
    deepest
}

/// Rejects a value nested deeper than [`MAX_VALUE_NESTING`].
pub(crate) fn check_nesting(v: &Value) -> Result<(), Error> {
    if value_nesting(v, MAX_VALUE_NESTING) > MAX_VALUE_NESTING {
        Err(Error::DepthLimit)
    } else {
        Ok(())
    }
}

impl Interpreter {
    fn frame_index(&self, id: u32) -> Result<usize, Error> {
        self.frames
            .iter()
            .rposition(|f| f.id == id)
            .ok_or(Error::DanglingReference)
    }

    fn slot(&self, loc: &Location) -> Result<&Value, Error> {
        match *loc {
            Location::Local(f, n) => {
                let i = self.frame_index(f)?;
                self.frames[i]
                    .locals
                    .get(usize::from(n))
                    .ok_or(Error::Internal)
            }
            Location::Arg(f, n) => {
                let i = self.frame_index(f)?;
                self.frames[i]
                    .args
                    .get(usize::from(n))
                    .ok_or(Error::Internal)
            }
            _ => Err(Error::Internal),
        }
    }

    fn slot_mut(&mut self, loc: &Location) -> Result<&mut Value, Error> {
        match *loc {
            Location::Local(f, n) => {
                let i = self.frame_index(f)?;
                self.frames[i]
                    .locals
                    .get_mut(usize::from(n))
                    .ok_or(Error::Internal)
            }
            Location::Arg(f, n) => {
                let i = self.frame_index(f)?;
                self.frames[i]
                    .args
                    .get_mut(usize::from(n))
                    .ok_or(Error::Internal)
            }
            _ => Err(Error::Internal),
        }
    }

    /// The stored value a location designates, following references held
    /// in locals and arguments. Fails for locations that are not plain
    /// storage (field units, buffer bytes).
    pub(crate) fn storage<'a>(&'a self, loc: &'a Location) -> Result<&'a Value, Error> {
        self.storage_at(loc, 0)
    }

    fn storage_at<'a>(&'a self, loc: &'a Location, depth: u32) -> Result<&'a Value, Error> {
        if depth > MAX_REF_CHAIN {
            return Err(Error::DepthLimit);
        }
        match loc {
            Location::Node(n) => {
                let n = self.ns.follow(*n).ok_or(Error::DanglingReference)?;
                match self.ns.object(n) {
                    Some(Object::Name(v)) => Ok(v),
                    Some(Object::BufferField(_) | Object::Field(_)) => Err(Error::InvalidTarget),
                    Some(_) => Err(Error::TypeMismatch),
                    None => Err(Error::DanglingReference),
                }
            }
            Location::Local(..) | Location::Arg(..) => {
                let v = self.slot(loc)?;
                if let Value::Reference(r) = v {
                    return self.storage_at(&r.0, depth + 1);
                }
                Ok(v)
            }
            Location::Index(base, i) => match self.storage_at(base, depth + 1)? {
                Value::Package(p) => p.get(*i).ok_or(Error::IndexOutOfBounds),
                _ => Err(Error::InvalidTarget),
            },
            Location::TempIndex(v, i) => match &**v {
                Value::Package(p) => p.get(*i).ok_or(Error::IndexOutOfBounds),
                _ => Err(Error::InvalidTarget),
            },
            Location::Temp(v) => Ok(v),
            Location::Debug => Err(Error::InvalidTarget),
        }
    }

    /// Mutable access to plain storage, for writing a buffer field.
    pub(crate) fn storage_mut_for_field(&mut self, loc: &Location) -> Result<&mut Value, Error> {
        self.storage_mut_at(loc, 0)
    }

    fn storage_mut_at(&mut self, loc: &Location, depth: u32) -> Result<&mut Value, Error> {
        if depth > MAX_REF_CHAIN {
            return Err(Error::DepthLimit);
        }
        match loc {
            Location::Node(n) => {
                let n = self.ns.follow(*n).ok_or(Error::DanglingReference)?;
                match self.ns.get_mut(n).map(|n| &mut n.object) {
                    Some(Object::Name(v)) => Ok(v),
                    Some(_) => Err(Error::InvalidTarget),
                    None => Err(Error::DanglingReference),
                }
            }
            Location::Local(..) | Location::Arg(..) => {
                if let Value::Reference(r) = self.slot(loc)? {
                    let inner = r.0.clone();
                    return self.storage_mut_at(&inner, depth + 1);
                }
                self.slot_mut(loc)
            }
            Location::Index(base, i) => match self.storage_mut_at(base, depth + 1)? {
                Value::Package(p) => p.get_mut(*i).ok_or(Error::IndexOutOfBounds),
                _ => Err(Error::InvalidTarget),
            },
            Location::TempIndex(..) | Location::Temp(_) | Location::Debug => {
                Err(Error::InvalidTarget)
            }
        }
    }

    /// Reads a named object: data, a field's contents, or (for devices,
    /// mutexes, and other non-data objects) a reference to the object.
    #[inline(never)]
    pub(crate) fn read_node(&mut self, h: &mut dyn Host, node: NodeId) -> Result<Value, Error> {
        let node = self.ns.follow(node).ok_or(Error::DanglingReference)?;
        match self.ns.object(node).ok_or(Error::DanglingReference)? {
            Object::Name(v) => Ok(v.clone()),
            Object::Field(_) => self.read_field(h, node),
            Object::BufferField(_) => self.read_buffer_field(node),
            Object::External(..) => Err(Error::Undefined(self.ns.path(node).to_text())),
            _ => Ok(Value::Reference(crate::value::Reference(Location::Node(
                node,
            )))),
        }
    }

    /// Reads the value at a location (one level: a reference stored there
    /// is returned as is).
    #[inline(never)]
    pub(crate) fn read_location(
        &mut self,
        h: &mut dyn Host,
        loc: &Location,
    ) -> Result<Value, Error> {
        self.enter()?;
        let r = self.read_location_inner(h, loc);
        self.leave();
        r
    }

    fn read_location_inner(&mut self, h: &mut dyn Host, loc: &Location) -> Result<Value, Error> {
        match loc {
            Location::Node(n) => self.read_node(h, *n),
            Location::Local(..) | Location::Arg(..) => {
                let v = self.slot(loc)?.clone();
                match v {
                    Value::Uninitialized if self.config.slack => Ok(Value::Integer(0)),
                    v => Ok(v),
                }
            }
            Location::Index(base, i) => {
                let elem = match self.storage(base) {
                    Ok(container) => element(container, *i)?,
                    Err(_) => {
                        let v = self.read_location(h, base)?;
                        let v = self.deref(h, v)?;
                        element(&v, *i)?
                    }
                };
                Ok(elem)
            }
            Location::TempIndex(v, i) => element(v, *i),
            Location::Temp(v) => Ok((**v).clone()),
            Location::Debug => Err(Error::InvalidTarget),
        }
    }

    /// The namespace node a location designates (following references in
    /// locals and arguments), for operators such as `Notify` and `Acquire`.
    #[inline(never)]
    pub(crate) fn location_node(&self, loc: &Location) -> Result<NodeId, Error> {
        let mut loc = loc.clone();
        for _ in 0..MAX_REF_CHAIN {
            match loc {
                Location::Node(n) => return self.ns.follow(n).ok_or(Error::DanglingReference),
                Location::Local(..) | Location::Arg(..) => match self.slot(&loc)? {
                    Value::Reference(r) => loc = r.0.clone(),
                    Value::Name(scope, name) => {
                        return self.ns.resolve(*scope, name).ok_or(Error::NotFound);
                    }
                    _ => return Err(Error::TypeMismatch),
                },
                Location::Index(..) | Location::TempIndex(..) => match self.storage(&loc)? {
                    Value::Reference(r) => loc = r.0.clone(),
                    Value::Name(scope, name) => {
                        return self.ns.resolve(*scope, name).ok_or(Error::NotFound);
                    }
                    _ => return Err(Error::TypeMismatch),
                },
                _ => return Err(Error::TypeMismatch),
            }
        }
        Err(Error::DepthLimit)
    }

    /// `ObjectType` of a location, following references.
    #[inline(never)]
    pub(crate) fn location_type(&self, loc: &Location, depth: u32) -> Result<ObjectType, Error> {
        if depth > MAX_REF_CHAIN {
            return Err(Error::DepthLimit);
        }
        let value_type = |me: &Self, v: &Value| -> Result<ObjectType, Error> {
            match v {
                Value::Reference(r) => me.location_type(&r.0, depth + 1),
                Value::Name(scope, name) => {
                    let n = me.ns.resolve(*scope, name).ok_or(Error::NotFound)?;
                    me.location_type(&Location::Node(n), depth + 1)
                }
                v => Ok(v.object_type()),
            }
        };
        match loc {
            Location::Node(n) => {
                let n = self.ns.follow(*n).ok_or(Error::DanglingReference)?;
                Ok(self
                    .ns
                    .object(n)
                    .ok_or(Error::DanglingReference)?
                    .object_type())
            }
            Location::Local(..) | Location::Arg(..) => value_type(self, self.slot(loc)?),
            Location::Index(base, _) => {
                if let Ok(v) = self.storage(loc) {
                    return value_type(self, v);
                }
                // A buffer or string element is an integer (a field read
                // through Index is a buffer too).
                match self.storage(base) {
                    Ok(Value::Buffer(_) | Value::String(_)) | Err(_) => Ok(ObjectType::Integer),
                    Ok(_) => Err(Error::TypeMismatch),
                }
            }
            Location::TempIndex(v, i) => match &**v {
                Value::Package(p) => value_type(self, p.get(*i).ok_or(Error::IndexOutOfBounds)?),
                _ => Ok(ObjectType::Integer),
            },
            Location::Temp(v) => value_type(self, v),
            Location::Debug => Ok(ObjectType::Debug),
        }
    }

    /// Stores to an optional target (operator results).
    #[inline(never)]
    pub(crate) fn store_target(
        &mut self,
        h: &mut dyn Host,
        v: Value,
        target: Option<&Location>,
    ) -> Result<(), Error> {
        if let Some(t) = target {
            self.store(h, v, t)?;
        }
        Ok(())
    }

    /// `Store`: returns the value stored (or, for a buffer-protocol field,
    /// the buffer the transfer returned).
    #[inline(never)]
    pub(crate) fn store(
        &mut self,
        h: &mut dyn Host,
        v: Value,
        target: &Location,
    ) -> Result<Value, Error> {
        self.enter()?;
        let r = self.store_inner(h, v, target);
        self.leave();
        r
    }

    fn store_inner(
        &mut self,
        h: &mut dyn Host,
        v: Value,
        target: &Location,
    ) -> Result<Value, Error> {
        match target {
            Location::Debug => {
                h.debug(&v);
                Ok(v)
            }
            Location::Local(..) => {
                *self.slot_mut(target)? = v.clone();
                Ok(v)
            }
            Location::Arg(..) => {
                if let Value::Reference(r) = self.slot(target)? {
                    let inner = r.0.clone();
                    return self.store(h, v, &inner);
                }
                *self.slot_mut(target)? = v.clone();
                Ok(v)
            }
            Location::Node(n) => self.store_to_node(h, *n, v),
            Location::Index(base, i) => {
                let v = self.deref(h, v)?;
                self.store_element(base, *i, v.clone())?;
                Ok(v)
            }
            // Element of a temporary, or a temporary: the write vanishes
            // with it, but the conversion must still be valid.
            Location::TempIndex(..) | Location::Temp(_) => self.deref(h, v),
        }
    }

    /// Package levels between a location's root object and the element it
    /// designates, following references held in locals and arguments.
    fn location_hops(&self, loc: &Location) -> usize {
        let mut hops = 0;
        let mut loc = loc;
        for _ in 0..MAX_REF_CHAIN * 4 {
            match loc {
                Location::Index(base, _) => {
                    hops += 1;
                    loc = base;
                }
                Location::Local(..) | Location::Arg(..) => match self.slot(loc) {
                    Ok(Value::Reference(r)) => loc = &r.0,
                    _ => return hops,
                },
                _ => return hops,
            }
        }
        usize::MAX
    }

    fn store_element(&mut self, base: &Location, i: usize, v: Value) -> Result<(), Error> {
        let w = self.width;
        if matches!(v, Value::Package(_) | Value::Reference(_)) {
            let hops = self.location_hops(base);
            if hops.saturating_add(1 + value_nesting(&v, MAX_VALUE_NESTING)) > MAX_VALUE_NESTING {
                return Err(Error::DepthLimit);
            }
        }
        let byte = match &v {
            Value::Integer(_) | Value::String(_) | Value::Buffer(_) => {
                v.to_integer(w).ok().map(|x| x.to_le_bytes()[0])
            }
            _ => None,
        };
        let container = self.storage_mut_at(base, 0)?;
        match container {
            Value::Package(p) => {
                *p.get_mut(i).ok_or(Error::IndexOutOfBounds)? = v;
            }
            Value::Buffer(b) => {
                *b.get_mut(i).ok_or(Error::IndexOutOfBounds)? = byte.ok_or(Error::TypeMismatch)?;
            }
            Value::String(s) => {
                let c = byte.ok_or(Error::TypeMismatch)?;
                if i >= s.len() {
                    return Err(Error::IndexOutOfBounds);
                }
                let mut bytes = core::mem::take(s).into_bytes();
                bytes[i] = if c.is_ascii() && c != 0 { c } else { b'?' };
                *s = crate::value::string_from_bytes(&bytes);
            }
            _ => return Err(Error::TypeMismatch),
        }
        Ok(())
    }

    fn store_to_node(&mut self, h: &mut dyn Host, node: NodeId, v: Value) -> Result<Value, Error> {
        let node = self.ns.follow(node).ok_or(Error::DanglingReference)?;
        let w = self.width;
        match self.ns.object(node).ok_or(Error::DanglingReference)? {
            Object::Field(_) => {
                let v = self.deref(h, v)?;
                let r = self.write_field(h, node, &v)?;
                Ok(r.unwrap_or(v))
            }
            Object::BufferField(_) => {
                let v = self.deref(h, v)?;
                self.write_buffer_field(node, &v)?;
                Ok(v)
            }
            Object::Name(existing) => {
                // What the existing value converts to: 0 integer, 1 string,
                // 2 buffer (with its length), 3 replaced.
                let kind = match existing {
                    Value::Integer(_) => (0, 0),
                    Value::String(_) => (1, 0),
                    Value::Buffer(old) => (2, old.len()),
                    _ => (3, 0),
                };
                let new = match kind {
                    (0, _) => Value::Integer(self.deref(h, v.clone())?.to_integer(w)?),
                    (1, _) => Value::String(self.deref(h, v.clone())?.to_aml_string(w)?),
                    (2, old_len) => {
                        let src = self.deref(h, v.clone())?.to_buffer(w)?;
                        if old_len == 0 {
                            Value::Buffer(src)
                        } else {
                            let mut b = alloc::vec![0u8; old_len];
                            let n = src.len().min(old_len);
                            b[..n].copy_from_slice(&src[..n]);
                            Value::Buffer(b)
                        }
                    }
                    _ => v.clone(),
                };
                if let Some(n) = self.ns.get_mut(node) {
                    n.object = Object::Name(new);
                }
                Ok(v)
            }
            Object::External(..) => Err(Error::Undefined(self.ns.path(node).to_text())),
            _ => Err(Error::InvalidTarget),
        }
    }

    /// `CopyObject`: replaces the target without conversion (fields are
    /// written, as with `Store`).
    #[inline(never)]
    pub(crate) fn copy_object(
        &mut self,
        h: &mut dyn Host,
        v: Value,
        target: &Location,
    ) -> Result<(), Error> {
        match target {
            Location::Local(..) | Location::Arg(..) => {
                *self.slot_mut(target)? = v;
                Ok(())
            }
            Location::Node(n) => {
                let n = self.ns.follow(*n).ok_or(Error::DanglingReference)?;
                match self.ns.object(n) {
                    Some(Object::Field(_) | Object::BufferField(_)) => {
                        self.store_to_node(h, n, v).map(|_| ())
                    }
                    Some(_) => {
                        let v = self.deref(h, v)?;
                        if let Some(node) = self.ns.get_mut(n) {
                            node.object = Object::Name(v);
                        }
                        Ok(())
                    }
                    None => Err(Error::DanglingReference),
                }
            }
            _ => self.store(h, v, target).map(|_| ()),
        }
    }
}

/// Element `i` of a package (a copy), buffer, or string (an integer).
fn element(v: &Value, i: usize) -> Result<Value, Error> {
    match v {
        Value::Package(p) => p.get(i).cloned().ok_or(Error::IndexOutOfBounds),
        Value::Buffer(b) => b
            .get(i)
            .map(|&x| Value::Integer(u64::from(x)))
            .ok_or(Error::IndexOutOfBounds),
        Value::String(s) => s
            .as_bytes()
            .get(i)
            .map(|&x| Value::Integer(u64::from(x)))
            .ok_or(Error::IndexOutOfBounds),
        _ => Err(Error::TypeMismatch),
    }
}
