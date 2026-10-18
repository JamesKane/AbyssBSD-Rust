// SPDX-License-Identifier: BSD-2-Clause
//! Term execution: statements, named objects, and expressions (ACPI 6.5
//! §20.2.5), interpreted directly from the AML byte stream.
//!
//! Behavior reference: ACPI 6.5 §19.6 (operator semantics);
//! `sys/contrib/dev/acpica/components/dispatcher/dsopcode.c` and
//! `executer/exoparg*.c` for observable results in corner cases.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::{Flow, Interpreter, LoadWarning};
use crate::Error;
use crate::host::{Host, RegionSpace};
use crate::name::{Anchor, NameSeg, NameString, starts_name_string};
use crate::namespace::{
    AccessType, BufferField, FieldKind, FieldUnit, Method, MutexObj, NodeId, Object, Region,
    UpdateRule,
};
use crate::stream::Cursor;
use crate::value::{Location, ObjectType, Reference, Value, string_from_bytes};

/// AML opcodes (ACPI 6.5 §20.3).
pub(crate) mod op {
    pub(crate) const ZERO: u8 = 0x00;
    pub(crate) const ONE: u8 = 0x01;
    pub(crate) const ALIAS: u8 = 0x06;
    pub(crate) const NAME: u8 = 0x08;
    pub(crate) const BYTE: u8 = 0x0a;
    pub(crate) const WORD: u8 = 0x0b;
    pub(crate) const DWORD: u8 = 0x0c;
    pub(crate) const STRING: u8 = 0x0d;
    pub(crate) const QWORD: u8 = 0x0e;
    pub(crate) const SCOPE: u8 = 0x10;
    pub(crate) const BUFFER: u8 = 0x11;
    pub(crate) const PACKAGE: u8 = 0x12;
    pub(crate) const VAR_PACKAGE: u8 = 0x13;
    pub(crate) const METHOD: u8 = 0x14;
    pub(crate) const EXTERNAL: u8 = 0x15;
    pub(crate) const EXT: u8 = 0x5b;
    pub(crate) const LOCAL0: u8 = 0x60;
    pub(crate) const LOCAL7: u8 = 0x67;
    pub(crate) const ARG0: u8 = 0x68;
    pub(crate) const ARG6: u8 = 0x6e;
    pub(crate) const STORE: u8 = 0x70;
    pub(crate) const REF_OF: u8 = 0x71;
    pub(crate) const ADD: u8 = 0x72;
    pub(crate) const CONCAT: u8 = 0x73;
    pub(crate) const SUBTRACT: u8 = 0x74;
    pub(crate) const INCREMENT: u8 = 0x75;
    pub(crate) const DECREMENT: u8 = 0x76;
    pub(crate) const MULTIPLY: u8 = 0x77;
    pub(crate) const DIVIDE: u8 = 0x78;
    pub(crate) const SHIFT_LEFT: u8 = 0x79;
    pub(crate) const SHIFT_RIGHT: u8 = 0x7a;
    pub(crate) const AND: u8 = 0x7b;
    pub(crate) const NAND: u8 = 0x7c;
    pub(crate) const OR: u8 = 0x7d;
    pub(crate) const NOR: u8 = 0x7e;
    pub(crate) const XOR: u8 = 0x7f;
    pub(crate) const NOT: u8 = 0x80;
    pub(crate) const FIND_SET_LEFT_BIT: u8 = 0x81;
    pub(crate) const FIND_SET_RIGHT_BIT: u8 = 0x82;
    pub(crate) const DEREF_OF: u8 = 0x83;
    pub(crate) const CONCAT_RES: u8 = 0x84;
    pub(crate) const MOD: u8 = 0x85;
    pub(crate) const NOTIFY: u8 = 0x86;
    pub(crate) const SIZE_OF: u8 = 0x87;
    pub(crate) const INDEX: u8 = 0x88;
    pub(crate) const MATCH: u8 = 0x89;
    pub(crate) const CREATE_DWORD_FIELD: u8 = 0x8a;
    pub(crate) const CREATE_WORD_FIELD: u8 = 0x8b;
    pub(crate) const CREATE_BYTE_FIELD: u8 = 0x8c;
    pub(crate) const CREATE_BIT_FIELD: u8 = 0x8d;
    pub(crate) const OBJECT_TYPE: u8 = 0x8e;
    pub(crate) const CREATE_QWORD_FIELD: u8 = 0x8f;
    pub(crate) const LAND: u8 = 0x90;
    pub(crate) const LOR: u8 = 0x91;
    pub(crate) const LNOT: u8 = 0x92;
    pub(crate) const LEQUAL: u8 = 0x93;
    pub(crate) const LGREATER: u8 = 0x94;
    pub(crate) const LLESS: u8 = 0x95;
    pub(crate) const TO_BUFFER: u8 = 0x96;
    pub(crate) const TO_DECIMAL_STRING: u8 = 0x97;
    pub(crate) const TO_HEX_STRING: u8 = 0x98;
    pub(crate) const TO_INTEGER: u8 = 0x99;
    pub(crate) const TO_STRING: u8 = 0x9c;
    pub(crate) const COPY_OBJECT: u8 = 0x9d;
    pub(crate) const MID: u8 = 0x9e;
    pub(crate) const CONTINUE: u8 = 0x9f;
    pub(crate) const IF: u8 = 0xa0;
    pub(crate) const ELSE: u8 = 0xa1;
    pub(crate) const WHILE: u8 = 0xa2;
    pub(crate) const NOOP: u8 = 0xa3;
    pub(crate) const RETURN: u8 = 0xa4;
    pub(crate) const BREAK: u8 = 0xa5;
    pub(crate) const BREAKPOINT: u8 = 0xcc;
    pub(crate) const ONES: u8 = 0xff;

    // Extended opcodes, after EXT.
    pub(crate) const MUTEX: u8 = 0x01;
    pub(crate) const EVENT: u8 = 0x02;
    pub(crate) const COND_REF_OF: u8 = 0x12;
    pub(crate) const CREATE_FIELD: u8 = 0x13;
    pub(crate) const LOAD_TABLE: u8 = 0x1f;
    pub(crate) const LOAD: u8 = 0x20;
    pub(crate) const STALL: u8 = 0x21;
    pub(crate) const SLEEP: u8 = 0x22;
    pub(crate) const ACQUIRE: u8 = 0x23;
    pub(crate) const SIGNAL: u8 = 0x24;
    pub(crate) const WAIT: u8 = 0x25;
    pub(crate) const RESET: u8 = 0x26;
    pub(crate) const RELEASE: u8 = 0x27;
    pub(crate) const FROM_BCD: u8 = 0x28;
    pub(crate) const TO_BCD: u8 = 0x29;
    pub(crate) const UNLOAD: u8 = 0x2a;
    pub(crate) const REVISION: u8 = 0x30;
    pub(crate) const DEBUG: u8 = 0x31;
    pub(crate) const FATAL: u8 = 0x32;
    pub(crate) const TIMER: u8 = 0x33;
    pub(crate) const OP_REGION: u8 = 0x80;
    pub(crate) const FIELD: u8 = 0x81;
    pub(crate) const DEVICE: u8 = 0x82;
    pub(crate) const PROCESSOR: u8 = 0x83;
    pub(crate) const POWER_RES: u8 = 0x84;
    pub(crate) const THERMAL_ZONE: u8 = 0x85;
    pub(crate) const INDEX_FIELD: u8 = 0x86;
    pub(crate) const BANK_FIELD: u8 = 0x87;
    pub(crate) const DATA_REGION: u8 = 0x88;
}

/// Where `Index` or `CreateXxxField` finds its source.
enum Source {
    /// A storage location that can be written through.
    Loc(Location),
    /// A computed value.
    Temp(Value),
}

impl Interpreter {
    /// Enters one nesting level: checks the depth and the stack actually
    /// used since the outermost level, so hostile nesting ends in an error
    /// rather than a stack overflow whatever the build's frame sizes.
    pub(crate) fn enter(&mut self) -> Result<(), Error> {
        let marker = 0u8;
        let here = core::ptr::addr_of!(marker).addr();
        if self.depth == 0 {
            self.stack_base = here;
        } else {
            // Stacks grow down on every supported architecture; the
            // distance is taken either way.
            let used = self.stack_base.abs_diff(here);
            self.peak_stack = self.peak_stack.max(used);
            if used > self.config.limits.stack_budget {
                return Err(Error::DepthLimit);
            }
        }
        if self.depth >= self.config.limits.max_depth {
            return Err(Error::DepthLimit);
        }
        self.depth += 1;
        self.peak_depth = self.peak_depth.max(self.depth);
        #[cfg(test)]
        {
            let d = usize::try_from(self.depth).unwrap_or(usize::MAX);
            if self.stack_probe.len() < d {
                self.stack_probe.resize(d, usize::MAX);
            }
            if let Some(slot) = self.stack_probe.get_mut(d - 1) {
                *slot = (*slot).min(here);
            }
        }
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn recovering(&self) -> bool {
        self.loading && self.frames.last().is_some_and(|f| !f.method)
    }

    pub(crate) fn frame_id(&self) -> Result<u32, Error> {
        self.frames.last().map(|f| f.id).ok_or(Error::Internal)
    }

    /// Executes a term list to its end or until control leaves it.
    pub(crate) fn exec_list(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Flow, Error> {
        self.enter()?;
        let r = self.exec_list_inner(h, cur, scope);
        self.leave();
        r
    }

    fn exec_list_inner(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Flow, Error> {
        while !cur.at_end() {
            let start = cur.pos;
            match self.exec_term(h, cur, scope) {
                Ok(Flow::Normal) => {}
                Ok(f) => return Ok(f),
                Err(e) if self.recovering() => {
                    self.warnings.push(LoadWarning {
                        offset: start,
                        error: e,
                    });
                    cur.pos = cur.end;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Flow::Normal)
    }

    /// Creates a named object in `scope`; objects a method creates are
    /// deleted when it returns.
    pub(crate) fn create(
        &mut self,
        scope: NodeId,
        name: &NameString,
        obj: Object,
    ) -> Result<NodeId, Error> {
        let (parent, seg) = self.ns.creation_target(scope, name)?;
        let id = self.ns.add(parent, seg, obj)?;
        if let Some(f) = self.frames.last_mut()
            && f.method
        {
            f.temps.try_reserve(1).map_err(|_| Error::NoMemory)?;
            f.temps.push(id);
        }
        Ok(id)
    }

    fn create_seg(&mut self, scope: NodeId, seg: NameSeg, obj: Object) -> Result<NodeId, Error> {
        let name = NameString {
            anchor: Anchor::Relative(0),
            segs: alloc::vec![seg],
        };
        self.create(scope, &name, obj)
    }

    fn resolve_or_err(&self, scope: NodeId, name: &NameString) -> Result<NodeId, Error> {
        self.ns
            .resolve(scope, name)
            .ok_or_else(|| Error::Undefined(alloc::format!("{name}")))
    }

    fn exec_term(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Flow, Error> {
        let opc = cur.u8()?;
        match opc {
            op::ALIAS => {
                let src = cur.name_string()?;
                let alias = cur.name_string()?;
                let target = self.resolve_or_err(scope, &src)?;
                self.create(scope, &alias, Object::Alias(target))?;
            }
            op::NAME => {
                let name = cur.name_string()?;
                let v = self.eval_data_ref(h, cur, scope)?;
                self.create(scope, &name, Object::Name(v))?;
            }
            op::SCOPE => {
                let end = cur.pkg_end()?;
                let name = cur.name_string()?;
                let mut body = cur.sub(cur.pos, end);
                cur.pos = end;
                let target = self.resolve_or_err(scope, &name)?;
                return self.exec_list(h, &mut body, target);
            }
            op::METHOD => {
                let end = cur.pkg_end()?;
                let name = cur.name_string()?;
                let flags = cur.u8()?;
                let m = Method {
                    data: cur.data().clone(),
                    start: cur.pos,
                    end,
                    args: flags & 7,
                    serialized: flags & 8 != 0,
                    sync_level: flags >> 4,
                };
                cur.pos = end;
                self.create(scope, &name, Object::Method(m))?;
            }
            op::EXTERNAL => {
                let name = cur.name_string()?;
                let ty = cur.u8()?;
                let args = cur.u8()?;
                self.declare_external(scope, &name, ty, args)?;
            }
            op::IF => return self.exec_if(h, cur, scope),
            op::ELSE => return Err(Error::InvalidOpcode(u16::from(op::ELSE))),
            op::WHILE => return self.exec_while(h, cur, scope),
            op::NOOP | op::BREAKPOINT => {}
            op::RETURN => {
                let v = self.eval_term_arg(h, cur, scope)?;
                return Ok(Flow::Return(v));
            }
            op::BREAK => return Ok(Flow::Break),
            op::CONTINUE => return Ok(Flow::Continue),
            op::NOTIFY => {
                let loc = self.parse_super_name(h, cur, scope, false)?;
                let v = self.eval_int(h, cur, scope)?;
                let node = self.location_node(&loc)?;
                h.notify(node, v);
            }
            op::CREATE_BIT_FIELD => self.exec_create_field(h, cur, scope, Some(1), false)?,
            op::CREATE_BYTE_FIELD => self.exec_create_field(h, cur, scope, Some(8), true)?,
            op::CREATE_WORD_FIELD => self.exec_create_field(h, cur, scope, Some(16), true)?,
            op::CREATE_DWORD_FIELD => self.exec_create_field(h, cur, scope, Some(32), true)?,
            op::CREATE_QWORD_FIELD => self.exec_create_field(h, cur, scope, Some(64), true)?,
            op::EXT => {
                let ext = cur.u8()?;
                if let Some(f) = self.exec_ext_term(h, cur, scope, ext)? {
                    return Ok(f);
                }
                cur.pos -= 2;
                self.eval_statement_expr(h, cur, scope)?;
            }
            _ => {
                cur.pos -= 1;
                self.eval_statement_expr(h, cur, scope)?;
            }
        }
        Ok(Flow::Normal)
    }

    fn eval_statement_expr(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<(), Error> {
        let v = self.eval_term_arg(h, cur, scope)?;
        if let Some(f) = self.frames.last_mut() {
            f.implicit = v;
        }
        Ok(())
    }

    /// Extended-opcode statements and named objects. `None` means the
    /// opcode is an expression.
    fn exec_ext_term(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        ext: u8,
    ) -> Result<Option<Flow>, Error> {
        match ext {
            op::MUTEX => {
                let name = cur.name_string()?;
                let flags = cur.u8()?;
                let m = MutexObj {
                    sync_level: flags & 0x0f,
                    depth: 0,
                    global: false,
                };
                self.create(scope, &name, Object::Mutex(m))?;
            }
            op::EVENT => {
                let name = cur.name_string()?;
                self.create(scope, &name, Object::Event(0))?;
            }
            op::CREATE_FIELD => self.exec_create_field(h, cur, scope, None, false)?,
            op::OP_REGION => {
                let name = cur.name_string()?;
                let space = RegionSpace::from_u8(cur.u8()?);
                let base = self.eval_int(h, cur, scope)?;
                let len = self.eval_int(h, cur, scope)?;
                let r = Region {
                    space,
                    base,
                    len,
                    pci: None,
                };
                self.create(scope, &name, Object::Region(r))?;
            }
            op::DATA_REGION => {
                let name = cur.name_string()?;
                let sig = self.eval_operand(h, cur, scope)?;
                let oem = self.eval_operand(h, cur, scope)?;
                let oem_table = self.eval_operand(h, cur, scope)?;
                let s = |v: &Value| v.as_str().map(|s| s.as_bytes().to_vec());
                let (Some(sig), Some(oem), Some(oem_table)) = (s(&sig), s(&oem), s(&oem_table))
                else {
                    return Err(Error::TypeMismatch);
                };
                let (base, len) = h
                    .data_table(&sig, &oem, &oem_table)
                    .ok_or(Error::Undefined(string_from_bytes(&sig)))?;
                let r = Region {
                    space: RegionSpace::SystemMemory,
                    base,
                    len,
                    pci: None,
                };
                self.create(scope, &name, Object::Region(r))?;
            }
            op::FIELD => {
                let end = cur.pkg_end()?;
                let region = cur.name_string()?;
                let region = self.resolve_or_err(scope, &region)?;
                if !matches!(self.ns.object(region), Some(Object::Region(_))) {
                    return Err(Error::TypeMismatch);
                }
                let flags = cur.u8()?;
                let mut list = cur.sub(cur.pos, end);
                cur.pos = end;
                self.parse_field_list(h, &mut list, scope, &FieldKind::Region(region), flags)?;
            }
            op::INDEX_FIELD => {
                let end = cur.pkg_end()?;
                let index = cur.name_string()?;
                let data = cur.name_string()?;
                let index = self.resolve_or_err(scope, &index)?;
                let data = self.resolve_or_err(scope, &data)?;
                for n in [index, data] {
                    if !matches!(self.ns.object(n), Some(Object::Field(_))) {
                        return Err(Error::TypeMismatch);
                    }
                }
                let flags = cur.u8()?;
                let mut list = cur.sub(cur.pos, end);
                cur.pos = end;
                let kind = FieldKind::Index { index, data };
                self.parse_field_list(h, &mut list, scope, &kind, flags)?;
            }
            op::BANK_FIELD => {
                let end = cur.pkg_end()?;
                let region = cur.name_string()?;
                let bank = cur.name_string()?;
                let region = self.resolve_or_err(scope, &region)?;
                let bank = self.resolve_or_err(scope, &bank)?;
                if !matches!(self.ns.object(region), Some(Object::Region(_)))
                    || !matches!(self.ns.object(bank), Some(Object::Field(_)))
                {
                    return Err(Error::TypeMismatch);
                }
                let mut list = cur.sub(cur.pos, end);
                cur.pos = end;
                let value = self.eval_int(h, &mut list, scope)?;
                let flags = list.u8()?;
                let kind = FieldKind::Bank {
                    region,
                    bank,
                    value,
                };
                self.parse_field_list(h, &mut list, scope, &kind, flags)?;
            }
            op::DEVICE | op::THERMAL_ZONE => {
                let end = cur.pkg_end()?;
                let name = cur.name_string()?;
                let mut body = cur.sub(cur.pos, end);
                cur.pos = end;
                let obj = if ext == op::DEVICE {
                    Object::Device
                } else {
                    Object::ThermalZone
                };
                let node = self.create(scope, &name, obj)?;
                return self.exec_list(h, &mut body, node).map(Some);
            }
            op::PROCESSOR => {
                let end = cur.pkg_end()?;
                let name = cur.name_string()?;
                let mut body = cur.sub(cur.pos, end);
                cur.pos = end;
                let id = body.u8()?;
                let pblk = body.u32()?;
                let pblk_len = body.u8()?;
                let obj = Object::Processor { id, pblk, pblk_len };
                let node = self.create(scope, &name, obj)?;
                return self.exec_list(h, &mut body, node).map(Some);
            }
            op::POWER_RES => {
                let end = cur.pkg_end()?;
                let name = cur.name_string()?;
                let mut body = cur.sub(cur.pos, end);
                cur.pos = end;
                let system_level = body.u8()?;
                let resource_order = body.u16()?;
                let obj = Object::PowerResource {
                    system_level,
                    resource_order,
                };
                let node = self.create(scope, &name, obj)?;
                return self.exec_list(h, &mut body, node).map(Some);
            }
            op::SLEEP => {
                let ms = self.eval_int(h, cur, scope)?;
                h.sleep(ms);
            }
            op::STALL => {
                let us = self.eval_int(h, cur, scope)?;
                h.stall(us);
            }
            op::SIGNAL | op::RESET => {
                let loc = self.parse_super_name(h, cur, scope, false)?;
                let node = self.location_node(&loc)?;
                let Some(Object::Event(count)) = self.ns.get_mut(node).map(|n| &mut n.object)
                else {
                    return Err(Error::TypeMismatch);
                };
                *count = if ext == op::SIGNAL {
                    count.saturating_add(1)
                } else {
                    0
                };
            }
            op::RELEASE => {
                let loc = self.parse_super_name(h, cur, scope, false)?;
                let node = self.location_node(&loc)?;
                self.release(h, node)?;
            }
            op::FATAL => {
                let kind = cur.u8()?;
                let code = cur.u32()?;
                let arg = self.eval_int(h, cur, scope)?;
                h.fatal(kind, code, arg);
            }
            op::LOAD | op::UNLOAD => return Err(Error::Unsupported("Load and Unload")),
            _ => return Ok(None),
        }
        Ok(Some(Flow::Normal))
    }

    fn declare_external(
        &mut self,
        scope: NodeId,
        name: &NameString,
        ty: u8,
        args: u8,
    ) -> Result<(), Error> {
        if name.segs.is_empty() {
            return Err(Error::InvalidName);
        }
        // Walk to the parent, creating placeholder scopes for missing
        // segments so that later `Scope` operators can reach the name.
        let mut at = match name.anchor {
            Anchor::Root => crate::namespace::Namespace::root(),
            Anchor::Relative(up) => {
                let mut at = scope;
                for _ in 0..up {
                    at = self.ns.parent(at).ok_or(Error::NotFound)?;
                }
                at
            }
        };
        let last = name.segs.len() - 1;
        for (i, seg) in name.segs.iter().enumerate() {
            match self.ns.child(at, *seg) {
                Some(c) => at = self.ns.follow(c).ok_or(Error::NotFound)?,
                None => {
                    let obj = if i == last {
                        Object::External(ty, args)
                    } else {
                        Object::External(ObjectType::Device as u8, 0)
                    };
                    at = self.ns.add(at, *seg, obj)?;
                }
            }
        }
        Ok(())
    }

    fn exec_if(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Flow, Error> {
        let end = cur.pkg_end()?;
        let mut body = cur.sub(cur.pos, end);
        cur.pos = end;
        let pred = self.eval_int(h, &mut body, scope)? != 0;
        let has_else = cur.peek() == Some(op::ELSE);
        if pred {
            if has_else {
                cur.u8()?;
                cur.pos = cur.pkg_end()?;
            }
            self.exec_list(h, &mut body, scope)
        } else if has_else {
            cur.u8()?;
            let end = cur.pkg_end()?;
            let mut body = cur.sub(cur.pos, end);
            cur.pos = end;
            self.exec_list(h, &mut body, scope)
        } else {
            Ok(Flow::Normal)
        }
    }

    fn exec_while(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Flow, Error> {
        let end = cur.pkg_end()?;
        let start = cur.pos;
        cur.pos = end;
        let t0 = h.timer();
        let mut iterations: u64 = 0;
        loop {
            let mut body = cur.sub(start, end);
            if self.eval_int(h, &mut body, scope)? == 0 {
                return Ok(Flow::Normal);
            }
            match self.exec_list(h, &mut body, scope)? {
                Flow::Normal | Flow::Continue => {}
                Flow::Break => return Ok(Flow::Normal),
                Flow::Return(v) => return Ok(Flow::Return(v)),
            }
            iterations += 1;
            if iterations >= self.config.limits.max_loop_iterations {
                return Err(Error::LoopLimit);
            }
            if t0 != 0 && h.timer().wrapping_sub(t0) > self.config.limits.loop_timeout {
                return Err(Error::LoopLimit);
            }
        }
    }

    fn parse_field_list(
        &mut self,
        h: &mut dyn Host,
        list: &mut Cursor,
        scope: NodeId,
        kind: &FieldKind,
        flags: u8,
    ) -> Result<(), Error> {
        let mut bit: u64 = 0;
        let mut access = access_type(flags & 0x0f)?;
        let lock = flags & 0x10 != 0;
        let update = match (flags >> 5) & 3 {
            0 => UpdateRule::Preserve,
            1 => UpdateRule::WriteAsOnes,
            2 => UpdateRule::WriteAsZeros,
            _ => return Err(Error::InvalidValue("field update rule")),
        };
        let mut attrib: u8 = 0;
        let mut access_len: u8 = 0;
        let mut connection: Option<alloc::sync::Arc<[u8]>> = None;
        while !list.at_end() {
            match list.peek().ok_or(Error::UnexpectedEnd)? {
                0x00 => {
                    list.u8()?;
                    let n = u64::try_from(list.pkg_length_value()?).map_err(|_| Error::Internal)?;
                    bit = bit
                        .checked_add(n)
                        .ok_or(Error::InvalidValue("field offset"))?;
                }
                0x01 => {
                    list.u8()?;
                    let t = list.u8()?;
                    attrib = list.u8()?;
                    access = access_type(t & 0x0f)?;
                    access_len = if t >> 6 == 0 { 0 } else { attrib };
                }
                0x03 => {
                    list.u8()?;
                    let t = list.u8()?;
                    attrib = list.u8()?;
                    access_len = list.u8()?;
                    access = access_type(t & 0x0f)?;
                }
                0x02 => {
                    list.u8()?;
                    let v = if list.peek() == Some(op::BUFFER) {
                        self.eval_term_arg(h, list, scope)?
                    } else {
                        let name = list.name_string()?;
                        let node = self.resolve_or_err(scope, &name)?;
                        self.read_node(h, node)?
                    };
                    let Value::Buffer(b) = v else {
                        return Err(Error::TypeMismatch);
                    };
                    connection = Some(alloc::sync::Arc::from(b));
                }
                c if crate::name::is_lead_char(c) => {
                    let seg = NameSeg::from_aml(list.array()?).ok_or(Error::InvalidName)?;
                    let n = u64::try_from(list.pkg_length_value()?).map_err(|_| Error::Internal)?;
                    let unit = FieldUnit {
                        kind: kind.clone(),
                        bit_offset: bit,
                        bit_len: n,
                        access,
                        attrib,
                        access_len,
                        lock,
                        update,
                        connection: connection.clone(),
                    };
                    self.create_seg(scope, seg, Object::Field(unit))?;
                    bit = bit
                        .checked_add(n)
                        .ok_or(Error::InvalidValue("field offset"))?;
                }
                c => return Err(Error::InvalidOpcode(u16::from(c))),
            }
        }
        Ok(())
    }

    fn exec_create_field(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        bits: Option<u64>,
        byte_index: bool,
    ) -> Result<(), Error> {
        let source = self.parse_source(h, cur, scope)?;
        let index = self.eval_int(h, cur, scope)?;
        let bit_len = match bits {
            Some(b) => b,
            None => self.eval_int(h, cur, scope)?,
        };
        let name = cur.name_string()?;
        let bit_offset = if byte_index {
            index.checked_mul(8).ok_or(Error::BufferLimit)?
        } else {
            index
        };
        let source = match source {
            Source::Loc(l) => l,
            Source::Temp(v) => Location::Temp(Box::new(v)),
        };
        let len = match self.storage(&source)? {
            Value::Buffer(b) => b.len(),
            _ => return Err(Error::TypeMismatch),
        };
        let end = bit_offset.checked_add(bit_len).ok_or(Error::BufferLimit)?;
        if bit_len == 0 || end > u64::try_from(len).unwrap_or(u64::MAX).saturating_mul(8) {
            return Err(Error::BufferLimit);
        }
        let f = BufferField {
            source,
            bit_offset,
            bit_len,
        };
        self.create(scope, &name, Object::BufferField(f))?;
        Ok(())
    }

    /// The source operand of `Index` or `CreateXxxField`: a location when
    /// the operand names storage, so writes through the result land there.
    fn parse_source(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Source, Error> {
        let c = cur.peek().ok_or(Error::UnexpectedEnd)?;
        let loc = match c {
            op::LOCAL0..=op::LOCAL7 => {
                cur.u8()?;
                Location::Local(self.frame_id()?, c - op::LOCAL0)
            }
            op::ARG0..=op::ARG6 => {
                cur.u8()?;
                Location::Arg(self.frame_id()?, c - op::ARG0)
            }
            c if starts_name_string(c) => {
                let start = cur.pos;
                let name = cur.name_string()?;
                let node = self.resolve_or_err(scope, &name)?;
                if matches!(self.ns.object(node), Some(Object::Name(_))) {
                    Location::Node(node)
                } else {
                    cur.pos = start;
                    return Ok(Source::Temp(self.eval_operand(h, cur, scope)?));
                }
            }
            op::DEREF_OF => {
                cur.u8()?;
                let v = self.eval_term_arg(h, cur, scope)?;
                match v {
                    Value::Reference(r) => r.0,
                    other => {
                        let node = self.value_node(scope, &other)?;
                        Location::Node(node)
                    }
                }
            }
            _ => return Ok(Source::Temp(self.eval_operand(h, cur, scope)?)),
        };
        if self.storage(&loc).is_ok() {
            Ok(Source::Loc(loc))
        } else {
            let v = self.read_location(h, &loc)?;
            let v = self.deref(h, v)?;
            Ok(Source::Temp(v))
        }
    }

    /// The node a string or package name value designates.
    pub(crate) fn value_node(&self, scope: NodeId, v: &Value) -> Result<NodeId, Error> {
        match v {
            Value::String(s) => {
                let name = NameString::parse(s)?;
                self.resolve_or_err(scope, &name)
            }
            Value::Name(sc, name) => self.resolve_or_err(*sc, name),
            Value::Reference(r) => r.node().ok_or(Error::TypeMismatch),
            _ => Err(Error::TypeMismatch),
        }
    }

    /// Evaluates a `DataRefObject` (the value of `Name`): a name there is a
    /// reference, kept unresolved like a package element.
    fn eval_data_ref(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        if cur.peek().is_some_and(starts_name_string) {
            let name = cur.name_string()?;
            return Ok(Value::Name(scope, name));
        }
        self.eval_term_arg(h, cur, scope)
    }

    /// Evaluates a `TermArg` and dereferences the result.
    pub(crate) fn eval_operand(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        match self.eval_term_arg(h, cur, scope)? {
            v @ (Value::Reference(_) | Value::Name(..)) => self.deref(h, v),
            v => Ok(v),
        }
    }

    /// Evaluates a `TermArg` as an integer.
    pub(crate) fn eval_int(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<u64, Error> {
        match self.eval_term_arg(h, cur, scope)? {
            Value::Integer(x) => Ok(self.width.mask(x)),
            v => self.int_slow(h, v),
        }
    }

    #[inline(never)]
    fn int_slow(&mut self, h: &mut dyn Host, v: Value) -> Result<u64, Error> {
        self.deref(h, v)?.to_integer(self.width)
    }

    /// Follows references and package names to the data they designate.
    #[inline(never)]
    pub(crate) fn deref(&mut self, h: &mut dyn Host, mut v: Value) -> Result<Value, Error> {
        for _ in 0..8 {
            v = match v {
                Value::Reference(r) => self.read_location(h, &r.0)?,
                Value::Name(scope, name) => {
                    let node = self.resolve_or_err(scope, &name)?;
                    self.read_node(h, node)?
                }
                v => return Ok(v),
            };
        }
        Err(Error::DepthLimit)
    }

    /// Evaluates a `TermArg`.
    pub(crate) fn eval_term_arg(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        self.enter()?;
        let r = self.eval_inner(h, cur, scope);
        self.leave();
        r
    }

    pub(crate) fn int(&self, v: u64) -> Value {
        Value::Integer(self.width.mask(v))
    }

    pub(crate) fn bool_value(&self, b: bool) -> Value {
        Value::Integer(if b { self.width.ones() } else { 0 })
    }

    /// Dispatches one `TermArg`. Operators live in separate functions
    /// that are never inlined, so each level of nesting costs only the
    /// stack frame of the operator actually running.
    fn eval_inner(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let opc = cur.u8()?;
        match opc {
            op::ZERO => Ok(Value::Integer(0)),
            op::ONE => Ok(Value::Integer(1)),
            op::ONES => Ok(Value::Integer(self.width.ones())),
            op::BYTE => Ok(Value::Integer(u64::from(cur.u8()?))),
            op::WORD => Ok(Value::Integer(u64::from(cur.u16()?))),
            op::DWORD => Ok(Value::Integer(u64::from(cur.u32()?))),
            op::QWORD => Ok(self.int(cur.u64()?)),
            op::STRING => Self::op_string(cur),
            op::BUFFER => self.parse_buffer(h, cur, scope),
            op::PACKAGE => self.parse_package(h, cur, scope, false),
            op::VAR_PACKAGE => self.parse_package(h, cur, scope, true),
            op::LOCAL0..=op::ARG6 => self.op_local_arg(opc),
            c if starts_name_string(c) => {
                cur.pos -= 1;
                self.eval_name(h, cur, scope)
            }
            op::STORE | op::COPY_OBJECT => self.op_store(h, cur, scope, opc),
            op::REF_OF => self.op_ref_of(h, cur, scope),
            op::DEREF_OF => self.op_deref_of(h, cur, scope),
            op::ADD
            | op::SUBTRACT
            | op::MULTIPLY
            | op::SHIFT_LEFT
            | op::SHIFT_RIGHT
            | op::AND
            | op::NAND
            | op::OR
            | op::NOR
            | op::XOR
            | op::MOD => self.op_binary(h, cur, scope, opc),
            op::DIVIDE => self.op_divide(h, cur, scope),
            op::NOT | op::FIND_SET_LEFT_BIT | op::FIND_SET_RIGHT_BIT => {
                self.op_unary(h, cur, scope, opc)
            }
            op::INCREMENT | op::DECREMENT => self.op_inc_dec(h, cur, scope, opc),
            op::LAND | op::LOR | op::LNOT => self.op_logical(h, cur, scope, opc),
            op::LEQUAL | op::LGREATER | op::LLESS => self.op_compare(h, cur, scope, opc),
            op::CONCAT | op::CONCAT_RES => self.op_concat(h, cur, scope, opc),
            op::TO_BUFFER | op::TO_DECIMAL_STRING | op::TO_HEX_STRING | op::TO_INTEGER => {
                self.op_convert(h, cur, scope, opc)
            }
            op::TO_STRING => self.op_to_string(h, cur, scope),
            op::MID => self.op_mid(h, cur, scope),
            op::SIZE_OF => self.op_size_of(h, cur, scope),
            op::INDEX => self.eval_index(h, cur, scope),
            op::MATCH => self.op_match(h, cur, scope),
            op::OBJECT_TYPE => self.op_object_type(h, cur, scope),
            op::EXT => {
                let ext = cur.u8()?;
                self.eval_ext(h, cur, scope, ext)
            }
            other => Err(Error::InvalidOpcode(u16::from(other))),
        }
    }

    #[inline(never)]
    fn eval_ext(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        ext: u8,
    ) -> Result<Value, Error> {
        Ok(match ext {
            op::REVISION => Value::Integer(self.width.mask(self.config.revision)),
            op::TIMER => self.int(h.timer()),
            op::COND_REF_OF => {
                let loc = if cur.peek().is_some_and(starts_name_string) {
                    let name = cur.name_string()?;
                    self.ns.resolve(scope, &name).map(Location::Node)
                } else {
                    Some(self.parse_super_name(h, cur, scope, false)?)
                };
                let t = self.parse_target(h, cur, scope)?;
                match loc {
                    Some(l) => {
                        self.store_target(h, Value::Reference(Reference(l)), t.as_ref())?;
                        Value::Integer(self.width.ones())
                    }
                    None => Value::Integer(0),
                }
            }
            op::ACQUIRE => {
                let loc = self.parse_super_name(h, cur, scope, false)?;
                let timeout = cur.u16()?;
                let node = self.location_node(&loc)?;
                let ok = self.acquire(h, node, timeout)?;
                self.bool_value(!ok)
            }
            op::WAIT => {
                let loc = self.parse_super_name(h, cur, scope, false)?;
                let timeout = self.eval_int(h, cur, scope)?;
                let node = self.location_node(&loc)?;
                let Some(Object::Event(count)) = self.ns.get_mut(node).map(|n| &mut n.object)
                else {
                    return Err(Error::TypeMismatch);
                };
                if *count > 0 {
                    *count -= 1;
                    Value::Integer(0)
                } else {
                    // Nothing else runs while AML executes, so no one can
                    // signal: wait out a finite timeout and report it.
                    if timeout != 0 && timeout < 0xffff {
                        h.sleep(timeout);
                    }
                    Value::Integer(self.width.ones())
                }
            }
            op::FROM_BCD | op::TO_BCD => {
                let a = self.eval_int(h, cur, scope)?;
                let t = self.parse_target(h, cur, scope)?;
                let r = if ext == op::FROM_BCD {
                    super::ops::from_bcd(a)
                } else {
                    super::ops::to_bcd(a)
                };
                let r = self.int(r);
                self.store_target(h, r.clone(), t.as_ref())?;
                r
            }
            op::LOAD_TABLE => return Err(Error::Unsupported("LoadTable")),
            op::DEBUG => return Err(Error::InvalidOpcode(0x5b31)),
            other => return Err(Error::InvalidOpcode(0x5b00 | u16::from(other))),
        })
    }

    #[inline(never)]
    fn op_string(cur: &mut Cursor) -> Result<Value, Error> {
        let s = cur.c_string()?;
        Ok(Value::String(string_from_bytes(&s)))
    }

    /// Reads `Local0`–`Local7` or `Arg0`–`Arg6` of the current frame.
    #[inline(never)]
    fn op_local_arg(&self, opc: u8) -> Result<Value, Error> {
        let f = self.frames.last().ok_or(Error::Internal)?;
        let v = if opc <= op::LOCAL7 {
            f.locals.get(usize::from(opc - op::LOCAL0))
        } else {
            f.args.get(usize::from(opc - op::ARG0))
        };
        self.uninit_check(v.ok_or(Error::Internal)?.clone())
    }

    fn uninit_check(&self, v: Value) -> Result<Value, Error> {
        match v {
            Value::Uninitialized if self.config.slack => Ok(Value::Integer(0)),
            Value::Uninitialized => Err(Error::Uninitialized),
            v => Ok(v),
        }
    }

    pub(crate) fn check_size(&self, v: &Value) -> Result<(), Error> {
        let n = match v {
            Value::String(s) => s.len(),
            Value::Buffer(b) => b.len(),
            Value::Package(p) => p.len(),
            _ => 0,
        };
        if n > self.config.limits.max_object_size {
            Err(Error::NoMemory)
        } else {
            Ok(())
        }
    }

    /// A name in a `TermArg`: a method invocation or an object's value.
    #[inline(never)]
    fn eval_name(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let name = cur.name_string()?;
        if name.segs.is_empty() {
            return Err(Error::InvalidName);
        }
        let node = self.resolve_or_err(scope, &name)?;
        let obj = self.ns.object(node).ok_or(Error::Internal)?;
        if let Some(n) = obj.method_args() {
            let args = self.parse_args(h, cur, scope, n)?;
            return self.invoke(h, node, args);
        }
        self.read_node(h, node)
    }

    fn parse_args(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        n: u8,
    ) -> Result<Vec<Value>, Error> {
        let mut args = Vec::new();
        args.try_reserve_exact(usize::from(n))
            .map_err(|_| Error::NoMemory)?;
        for _ in 0..n {
            args.push(self.eval_term_arg(h, cur, scope)?);
        }
        Ok(args)
    }

    /// Invokes a method (or a native method) with arguments.
    #[inline(never)]
    pub(crate) fn invoke(
        &mut self,
        h: &mut dyn Host,
        node: NodeId,
        args: Vec<Value>,
    ) -> Result<Value, Error> {
        let obj = self.ns.object(node).ok_or(Error::Internal)?;
        let m = match obj {
            Object::Method(m) => m.clone(),
            Object::Native(n, _) => {
                let n = *n;
                return self.invoke_native(n, &args);
            }
            Object::External(..) => {
                return Err(Error::Undefined(alloc::format!("{}", self.ns.path(node))));
            }
            _ => return Err(Error::NotAMethod),
        };
        if self.frames.len() > self.config.limits.max_call_depth {
            return Err(Error::CallDepth);
        }
        if m.serialized && m.sync_level < self.current_sync_level() {
            return Err(Error::MutexOrder);
        }
        self.enter()?;
        let id = self.alloc_frame_id();
        let mut frame = super::Frame::new(id, true, self.held.len());
        frame.sync_level = m.serialized.then_some(m.sync_level);
        for (slot, a) in frame
            .args
            .iter_mut()
            .zip(args.into_iter().take(usize::from(m.args)))
        {
            *slot = a;
        }
        self.frames.push(frame);
        let saved_loading = core::mem::replace(&mut self.loading, false);
        let mut cur = Cursor::new(m.data.clone(), m.start, m.end);
        let r = self.exec_list(h, &mut cur, node);
        self.loading = saved_loading;
        let frame = self.frames.pop();
        self.leave();
        let Some(frame) = frame else {
            return Err(Error::Internal);
        };
        for t in frame.temps.iter().rev() {
            self.ns.remove(*t);
        }
        self.release_held_from(h, frame.held_base);
        match r? {
            Flow::Return(v) => Ok(v),
            Flow::Normal if self.config.slack => Ok(frame.implicit),
            Flow::Normal => Ok(Value::Uninitialized),
            Flow::Break | Flow::Continue => Err(Error::InvalidOpcode(u16::from(op::BREAK))),
        }
    }

    fn invoke_native(
        &mut self,
        n: crate::namespace::Native,
        args: &[Value],
    ) -> Result<Value, Error> {
        match n {
            crate::namespace::Native::Osi => {
                let Some(Value::String(s)) = args.first() else {
                    return Err(Error::TypeMismatch);
                };
                let yes = self.config.osi.iter().any(|o| o == s);
                Ok(self.bool_value(yes))
            }
        }
    }

    pub(crate) fn acquire(
        &mut self,
        h: &mut dyn Host,
        node: NodeId,
        timeout: u16,
    ) -> Result<bool, Error> {
        let current = self.current_sync_level();
        let Some(Object::Mutex(m)) = self.ns.get_mut(node).map(|n| &mut n.object) else {
            return Err(Error::TypeMismatch);
        };
        if m.depth > 0 {
            m.depth += 1;
            return Ok(true);
        }
        if m.sync_level < current {
            return Err(Error::MutexOrder);
        }
        let global = m.global;
        if global && !h.acquire_global_lock(timeout) {
            return Ok(false);
        }
        if let Some(Object::Mutex(m)) = self.ns.get_mut(node).map(|n| &mut n.object) {
            m.depth = 1;
        }
        self.held.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.held.push(node);
        Ok(true)
    }

    /// The highest sync level among held mutexes and running serialized
    /// methods.
    fn current_sync_level(&self) -> u8 {
        let mutexes = self.held.iter().filter_map(|&n| match self.ns.object(n) {
            Some(Object::Mutex(m)) => Some(m.sync_level),
            _ => None,
        });
        let methods = self.frames.iter().filter_map(|f| f.sync_level);
        mutexes.chain(methods).max().unwrap_or(0)
    }

    pub(crate) fn release(&mut self, h: &mut dyn Host, node: NodeId) -> Result<(), Error> {
        let Some(Object::Mutex(m)) = self.ns.get_mut(node).map(|n| &mut n.object) else {
            return Err(Error::TypeMismatch);
        };
        if m.depth == 0 {
            return Err(Error::MutexNotOwned);
        }
        m.depth -= 1;
        if m.depth == 0 {
            let global = m.global;
            if let Some(pos) = self.held.iter().rposition(|&n| n == node) {
                self.held.remove(pos);
            }
            if global {
                h.release_global_lock();
            }
        }
        Ok(())
    }

    /// Releases mutexes acquired since `base` (a method that returns,
    /// normally or by error, gives up what it still holds).
    pub(crate) fn release_held_from(&mut self, h: &mut dyn Host, base: usize) {
        while self.held.len() > base {
            let Some(node) = self.held.pop() else {
                break;
            };
            if let Some(Object::Mutex(m)) = self.ns.get_mut(node).map(|n| &mut n.object) {
                m.depth = 0;
                if m.global {
                    h.release_global_lock();
                }
            }
        }
    }

    #[inline(never)]
    fn parse_buffer(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let end = cur.pkg_end()?;
        let mut body = cur.sub(cur.pos, end);
        cur.pos = end;
        let size = self.eval_int(h, &mut body, scope)?;
        let size = usize::try_from(size).map_err(|_| Error::NoMemory)?;
        if size > self.config.limits.max_object_size {
            return Err(Error::NoMemory);
        }
        let init = body.bytes(body.remaining())?;
        let n = size.max(init.len());
        let mut b = Vec::new();
        b.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        b.extend_from_slice(init);
        b.resize(n, 0);
        Ok(Value::Buffer(b))
    }

    #[inline(never)]
    fn parse_package(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        var: bool,
    ) -> Result<Value, Error> {
        let end = cur.pkg_end()?;
        let mut body = cur.sub(cur.pos, end);
        cur.pos = end;
        let count = if var {
            let n = self.eval_int(h, &mut body, scope)?;
            usize::try_from(n).map_err(|_| Error::NoMemory)?
        } else {
            usize::from(body.u8()?)
        };
        if count > self.config.limits.max_object_size {
            return Err(Error::NoMemory);
        }
        let mut elems = Vec::new();
        // Each element takes at least one byte, which bounds the
        // reservation by the table size as well.
        elems
            .try_reserve(count.min(body.remaining()))
            .map_err(|_| Error::NoMemory)?;
        while !body.at_end() {
            let c = body.peek().ok_or(Error::UnexpectedEnd)?;
            let v = if starts_name_string(c) {
                Value::Name(scope, body.name_string()?)
            } else {
                self.eval_term_arg(h, &mut body, scope)?
            };
            elems.try_reserve(1).map_err(|_| Error::NoMemory)?;
            elems.push(v);
        }
        if elems.len() < count {
            elems.resize(count, Value::Uninitialized);
        }
        let v = Value::Package(elems);
        super::store::check_nesting(&v)?;
        Ok(v)
    }

    #[inline(never)]
    pub(crate) fn eval_index(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let source = self.parse_source(h, cur, scope)?;
        let idx = self.eval_int(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let idx = usize::try_from(idx).map_err(|_| Error::IndexOutOfBounds)?;
        let loc = match source {
            Source::Loc(l) => {
                let len = container_len(self.storage(&l)?)?;
                if idx >= len {
                    return Err(Error::IndexOutOfBounds);
                }
                Location::Index(Box::new(l), idx)
            }
            Source::Temp(v) => {
                if idx >= container_len(&v)? {
                    return Err(Error::IndexOutOfBounds);
                }
                Location::TempIndex(Box::new(v), idx)
            }
        };
        let r = Value::Reference(Reference(loc));
        super::store::check_nesting(&r)?;
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    /// Parses a `Target`: `None` for the null name.
    #[inline(never)]
    pub(crate) fn parse_target(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Option<Location>, Error> {
        if cur.peek() == Some(0) {
            cur.u8()?;
            return Ok(None);
        }
        self.parse_super_name(h, cur, scope, true).map(Some)
    }

    /// Parses a `SuperName` to the location it designates. With `invoke`,
    /// a method name is invoked and must return a reference; otherwise it
    /// designates the method itself.
    #[inline(never)]
    pub(crate) fn parse_super_name(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        invoke: bool,
    ) -> Result<Location, Error> {
        self.enter()?;
        let r = self.parse_super_name_inner(h, cur, scope, invoke);
        self.leave();
        r
    }

    fn parse_super_name_inner(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        invoke: bool,
    ) -> Result<Location, Error> {
        let c = cur.peek().ok_or(Error::UnexpectedEnd)?;
        Ok(match c {
            op::LOCAL0..=op::LOCAL7 => {
                cur.u8()?;
                Location::Local(self.frame_id()?, c - op::LOCAL0)
            }
            op::ARG0..=op::ARG6 => {
                cur.u8()?;
                Location::Arg(self.frame_id()?, c - op::ARG0)
            }
            op::EXT if cur.peek_at(1) == Some(op::DEBUG) => {
                cur.u8()?;
                cur.u8()?;
                Location::Debug
            }
            c if starts_name_string(c) => {
                let name = cur.name_string()?;
                if name.segs.is_empty() {
                    return Err(Error::InvalidName);
                }
                let node = self.resolve_or_err(scope, &name)?;
                let args = self.ns.object(node).and_then(Object::method_args);
                match args {
                    Some(n) if invoke => {
                        let args = self.parse_args(h, cur, scope, n)?;
                        match self.invoke(h, node, args)? {
                            Value::Reference(r) => r.0,
                            _ => return Err(Error::InvalidTarget),
                        }
                    }
                    _ => Location::Node(node),
                }
            }
            op::REF_OF => {
                cur.u8()?;
                self.parse_super_name(h, cur, scope, false)?
            }
            op::DEREF_OF => {
                cur.u8()?;
                let v = self.eval_term_arg(h, cur, scope)?;
                match v {
                    Value::Reference(r) => r.0,
                    other => Location::Node(self.value_node(scope, &other)?),
                }
            }
            op::INDEX => {
                cur.u8()?;
                match self.eval_index(h, cur, scope)? {
                    Value::Reference(r) => r.0,
                    _ => return Err(Error::Internal),
                }
            }
            _ => return Err(Error::InvalidTarget),
        })
    }
}

fn access_type(v: u8) -> Result<AccessType, Error> {
    Ok(match v {
        0 => AccessType::Any,
        1 => AccessType::Byte,
        2 => AccessType::Word,
        3 => AccessType::DWord,
        4 => AccessType::QWord,
        5 => AccessType::Buffer,
        _ => return Err(Error::InvalidValue("field access type")),
    })
}

pub(crate) fn container_len(v: &Value) -> Result<usize, Error> {
    match v {
        Value::Package(p) => Ok(p.len()),
        Value::Buffer(b) => Ok(b.len()),
        Value::String(s) => Ok(s.len()),
        _ => Err(Error::TypeMismatch),
    }
}
