// SPDX-License-Identifier: BSD-2-Clause
//! Expression operators (ACPI 6.5 §19.6): each family is its own function,
//! never inlined into the dispatcher, so that deeply nested expressions
//! cost one small frame per level.
//!
//! Behavior reference: ACPI 6.5 §19.6 (operator semantics); integer
//! results are truncated to the integer width, and logical results are
//! `Ones` for true.

use super::Interpreter;
use super::exec::op;
use super::ops;
use crate::Error;
use crate::host::Host;
use crate::namespace::NodeId;
use crate::stream::Cursor;
use crate::value::{Reference, Value, string_from_bytes};

impl Interpreter {
    /// `Store` and `CopyObject`.
    #[inline(never)]
    pub(crate) fn op_store(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let v = self.eval_term_arg(h, cur, scope)?;
        let t = self.parse_super_name(h, cur, scope, true)?;
        if opc == op::STORE {
            self.store(h, v, &t)
        } else {
            self.copy_object(h, v.clone(), &t)?;
            Ok(v)
        }
    }

    #[inline(never)]
    pub(crate) fn op_ref_of(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let loc = self.parse_super_name(h, cur, scope, false)?;
        Ok(Value::Reference(Reference(loc)))
    }

    /// `DerefOf`: a reference, or a string naming an object.
    #[inline(never)]
    pub(crate) fn op_deref_of(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        match self.eval_term_arg(h, cur, scope)? {
            Value::Reference(r) => self.read_location(h, &r.0),
            other => {
                let node = self.value_node(scope, &other)?;
                self.read_node(h, node)
            }
        }
    }

    #[inline(never)]
    pub(crate) fn op_binary(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_int(h, cur, scope)?;
        let b = self.eval_int(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let bits = u64::from(self.width.bits());
        let r = match opc {
            op::ADD => a.wrapping_add(b),
            op::SUBTRACT => a.wrapping_sub(b),
            op::MULTIPLY => a.wrapping_mul(b),
            op::SHIFT_LEFT if b >= bits => 0,
            op::SHIFT_LEFT => a << b,
            op::SHIFT_RIGHT if b >= bits => 0,
            op::SHIFT_RIGHT => a >> b,
            op::AND => a & b,
            op::NAND => !(a & b),
            op::OR => a | b,
            op::NOR => !(a | b),
            op::XOR => a ^ b,
            _ => a.checked_rem(b).ok_or(Error::DivideByZero)?,
        };
        let r = self.int(r);
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    /// `Divide (dividend, divisor, remainder, quotient)`.
    #[inline(never)]
    pub(crate) fn op_divide(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let a = self.eval_int(h, cur, scope)?;
        let b = self.eval_int(h, cur, scope)?;
        let rem_t = self.parse_target(h, cur, scope)?;
        let quot_t = self.parse_target(h, cur, scope)?;
        let q = a.checked_div(b).ok_or(Error::DivideByZero)?;
        let r = a.checked_rem(b).ok_or(Error::DivideByZero)?;
        self.store_target(h, self.int(r), rem_t.as_ref())?;
        let q = self.int(q);
        self.store_target(h, q.clone(), quot_t.as_ref())?;
        Ok(q)
    }

    /// `Not`, `FindSetLeftBit`, `FindSetRightBit`.
    #[inline(never)]
    pub(crate) fn op_unary(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_int(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let r = match opc {
            op::NOT => !a,
            _ if a == 0 => 0,
            op::FIND_SET_LEFT_BIT => u64::from(64 - a.leading_zeros()),
            _ => u64::from(a.trailing_zeros() + 1),
        };
        let r = self.int(r);
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    #[inline(never)]
    pub(crate) fn op_inc_dec(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let loc = self.parse_super_name(h, cur, scope, true)?;
        let v = self.read_location(h, &loc)?;
        let v = self.deref(h, v)?.to_integer(self.width)?;
        let r = if opc == op::INCREMENT {
            v.wrapping_add(1)
        } else {
            v.wrapping_sub(1)
        };
        let r = self.int(r);
        self.store(h, r.clone(), &loc)?;
        Ok(r)
    }

    /// `LAnd`, `LOr` (both operands are always evaluated), `LNot`.
    #[inline(never)]
    pub(crate) fn op_logical(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_int(h, cur, scope)?;
        if opc == op::LNOT {
            return Ok(self.bool_value(a == 0));
        }
        let b = self.eval_int(h, cur, scope)?;
        let r = if opc == op::LAND {
            a != 0 && b != 0
        } else {
            a != 0 || b != 0
        };
        Ok(self.bool_value(r))
    }

    /// `LEqual`, `LGreater`, `LLess`, comparing with the first operand's
    /// type.
    #[inline(never)]
    pub(crate) fn op_compare(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_operand(h, cur, scope)?;
        let b = self.eval_operand(h, cur, scope)?;
        let ord = ops::compare(&a, &b, self.width)?;
        let r = match opc {
            op::LEQUAL => ord.is_eq(),
            op::LGREATER => ord.is_gt(),
            _ => ord.is_lt(),
        };
        Ok(self.bool_value(r))
    }

    /// `Concatenate` and `ConcatenateResTemplate`.
    #[inline(never)]
    pub(crate) fn op_concat(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_operand(h, cur, scope)?;
        let b = self.eval_operand(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let r = if opc == op::CONCAT {
            ops::concat(&a, &b, self.width)?
        } else {
            let (Value::Buffer(a), Value::Buffer(b)) = (a, b) else {
                return Err(Error::TypeMismatch);
            };
            Value::Buffer(ops::concat_res(&a, &b)?)
        };
        self.check_size(&r)?;
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    /// `ToBuffer`, `ToDecimalString`, `ToHexString`, `ToInteger`.
    #[inline(never)]
    pub(crate) fn op_convert(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
        opc: u8,
    ) -> Result<Value, Error> {
        let a = self.eval_operand(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let r = match opc {
            op::TO_BUFFER => Value::Buffer(a.to_buffer(self.width)?),
            op::TO_DECIMAL_STRING => ops::to_decimal_string(&a, self.width)?,
            op::TO_HEX_STRING => ops::to_hex_string(&a, self.width)?,
            _ => self.int(ops::to_integer_explicit(&a, self.width)?),
        };
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    /// `ToString (source, length)`: bytes up to a NUL or the length.
    #[inline(never)]
    pub(crate) fn op_to_string(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let a = self.eval_operand(h, cur, scope)?;
        let len = self.eval_int(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let b = a.to_buffer(self.width)?;
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let n = usize::try_from(len).map_or(n, |l| n.min(l));
        let r = Value::String(string_from_bytes(&b[..n]));
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    #[inline(never)]
    pub(crate) fn op_mid(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let a = self.eval_operand(h, cur, scope)?;
        let idx = self.eval_int(h, cur, scope)?;
        let len = self.eval_int(h, cur, scope)?;
        let t = self.parse_target(h, cur, scope)?;
        let r = ops::mid(&a, idx, len, self.width)?;
        self.store_target(h, r.clone(), t.as_ref())?;
        Ok(r)
    }

    #[inline(never)]
    pub(crate) fn op_size_of(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let loc = self.parse_super_name(h, cur, scope, false)?;
        let v = self.read_location(h, &loc)?;
        let n = match self.deref(h, v)? {
            Value::String(s) => s.len(),
            Value::Buffer(b) => b.len(),
            Value::Package(p) => p.len(),
            _ => return Err(Error::TypeMismatch),
        };
        Ok(Value::Integer(u64::try_from(n).unwrap_or(u64::MAX)))
    }

    /// `Match (package, op1, value1, op2, value2, start)`.
    #[inline(never)]
    pub(crate) fn op_match(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let pkg = self.eval_operand(h, cur, scope)?;
        let op1 = cur.u8()?;
        let v1 = self.eval_operand(h, cur, scope)?;
        let op2 = cur.u8()?;
        let v2 = self.eval_operand(h, cur, scope)?;
        let start = self.eval_int(h, cur, scope)?;
        let Value::Package(elems) = pkg else {
            return Err(Error::TypeMismatch);
        };
        let start = usize::try_from(start).map_err(|_| Error::IndexOutOfBounds)?;
        if start >= elems.len() {
            return Err(Error::IndexOutOfBounds);
        }
        for (i, e) in elems.into_iter().enumerate().skip(start) {
            let e = match e {
                Value::Name(..) | Value::Reference(_) => self.deref(h, e)?,
                e => e,
            };
            if ops::match_test(op1, &e, &v1, self.width)?
                && ops::match_test(op2, &e, &v2, self.width)?
            {
                return Ok(Value::Integer(u64::try_from(i).unwrap_or(u64::MAX)));
            }
        }
        Ok(Value::Integer(self.width.ones()))
    }

    #[inline(never)]
    pub(crate) fn op_object_type(
        &mut self,
        h: &mut dyn Host,
        cur: &mut Cursor,
        scope: NodeId,
    ) -> Result<Value, Error> {
        let loc = self.parse_super_name(h, cur, scope, false)?;
        let t = self.location_type(&loc, 0)?;
        Ok(Value::Integer(u64::from(t as u8)))
    }
}
