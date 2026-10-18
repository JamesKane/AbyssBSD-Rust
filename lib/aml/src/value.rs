// SPDX-License-Identifier: BSD-2-Clause
//! Data values (ACPI 6.5 §19.3.5): integers, strings, buffers, packages,
//! and references, with the implicit and explicit conversions between
//! them (§19.3.5.7).
//!
//! Values have copy semantics: storing a package copies it. References to
//! storage (`RefOf`, `Index`) name a location rather than sharing an
//! object, which keeps every value plain owned data.
//!
//! Behavior reference: `sys/contrib/dev/acpica/components/executer/exconvrt.c`
//! (observable conversion results: string forms of integers and buffers).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::Error;
use crate::name::NameString;
use crate::namespace::NodeId;

/// Integer width, set by the DSDT's revision (ACPI 6.5 §19.6.29): 32 bits
/// for revisions below 2, otherwise 64.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntWidth {
    /// 32-bit integers.
    W32,
    /// 64-bit integers.
    W64,
}

impl IntWidth {
    /// All ones at this width (the `Ones` constant).
    #[must_use]
    pub const fn ones(self) -> u64 {
        match self {
            Self::W32 => 0xffff_ffff,
            Self::W64 => u64::MAX,
        }
    }

    /// Truncates `v` to this width.
    #[must_use]
    pub const fn mask(self, v: u64) -> u64 {
        v & self.ones()
    }

    /// Bytes in an integer.
    #[must_use]
    pub const fn bytes(self) -> usize {
        match self {
            Self::W32 => 4,
            Self::W64 => 8,
        }
    }

    /// Bits in an integer.
    #[must_use]
    pub const fn bits(self) -> u32 {
        match self {
            Self::W32 => 32,
            Self::W64 => 64,
        }
    }
}

/// A storage location: what a reference points at.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Location {
    /// A namespace object.
    Node(NodeId),
    /// A method local of the frame with this id.
    Local(u32, u8),
    /// A method argument of the frame with this id.
    Arg(u32, u8),
    /// An element of the package, buffer, or string at a location.
    Index(alloc::boxed::Box<Location>, usize),
    /// An element of a temporary value; writes to it are discarded with
    /// the temporary.
    TempIndex(alloc::boxed::Box<Value>, usize),
    /// A temporary value (the source of a buffer field over an expression).
    Temp(alloc::boxed::Box<Value>),
    /// The Debug object.
    Debug,
}

/// A reference to a storage location, made by `RefOf`, `CondRefOf`, or
/// `Index`.
#[derive(Clone, Debug, PartialEq)]
pub struct Reference(pub(crate) Location);

impl Reference {
    /// The namespace object referred to, if this is a reference to one.
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        match self.0 {
            Location::Node(n) => Some(n),
            _ => None,
        }
    }
}

/// An AML data value.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    /// No value yet (a fresh local, an absent return value).
    #[default]
    Uninitialized,
    /// An integer, already truncated to the integer width.
    Integer(u64),
    /// An ASCII string.
    String(String),
    /// A byte buffer.
    Buffer(Vec<u8>),
    /// A package of values.
    Package(Vec<Value>),
    /// A reference to a location.
    Reference(Reference),
    /// A name inside a package, resolved when it is used: the scope the
    /// package was created in and the name as written.
    Name(NodeId, NameString),
}

/// Object type codes returned by `ObjectType` (ACPI 6.5 §19.6.97).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectType {
    /// Uninitialized.
    Uninitialized = 0,
    /// Integer.
    Integer = 1,
    /// String.
    String = 2,
    /// Buffer.
    Buffer = 3,
    /// Package.
    Package = 4,
    /// Field unit.
    FieldUnit = 5,
    /// Device.
    Device = 6,
    /// Event.
    Event = 7,
    /// Control method.
    Method = 8,
    /// Mutex.
    Mutex = 9,
    /// Operation region.
    Region = 10,
    /// Power resource.
    PowerResource = 11,
    /// Processor (deprecated).
    Processor = 12,
    /// Thermal zone.
    ThermalZone = 13,
    /// Buffer field.
    BufferField = 14,
    /// DDB handle.
    DdbHandle = 15,
    /// The Debug object.
    Debug = 16,
}

impl ObjectType {
    /// Decodes an `External` object type byte.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Uninitialized,
            1 => Self::Integer,
            2 => Self::String,
            3 => Self::Buffer,
            4 => Self::Package,
            5 => Self::FieldUnit,
            6 => Self::Device,
            7 => Self::Event,
            8 => Self::Method,
            9 => Self::Mutex,
            10 => Self::Region,
            11 => Self::PowerResource,
            12 => Self::Processor,
            13 => Self::ThermalZone,
            14 => Self::BufferField,
            15 => Self::DdbHandle,
            16 => Self::Debug,
            _ => return None,
        })
    }
}

impl Value {
    /// The `ObjectType` of this value (references report as uninitialized
    /// here; the interpreter follows them first).
    #[must_use]
    pub fn object_type(&self) -> ObjectType {
        match self {
            Self::Uninitialized | Self::Reference(_) | Self::Name(..) => ObjectType::Uninitialized,
            Self::Integer(_) => ObjectType::Integer,
            Self::String(_) => ObjectType::String,
            Self::Buffer(_) => ObjectType::Buffer,
            Self::Package(_) => ObjectType::Package,
        }
    }

    /// The integer, if this is one.
    #[must_use]
    pub fn as_integer(&self) -> Option<u64> {
        match self {
            Self::Integer(v) => Some(*v),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The buffer bytes, if this is a buffer.
    #[must_use]
    pub fn as_buffer(&self) -> Option<&[u8]> {
        match self {
            Self::Buffer(b) => Some(b),
            _ => None,
        }
    }

    /// The elements, if this is a package.
    #[must_use]
    pub fn as_package(&self) -> Option<&[Value]> {
        match self {
            Self::Package(p) => Some(p),
            _ => None,
        }
    }

    /// Implicit conversion to an integer (§19.3.5.7): strings are read as
    /// hexadecimal up to the first non-hex character; buffers supply their
    /// first integer-width bytes, little endian.
    ///
    /// # Errors
    ///
    /// [`Error::TypeMismatch`] for packages, references, and uninitialized
    /// values.
    pub fn to_integer(&self, w: IntWidth) -> Result<u64, Error> {
        match self {
            Self::Integer(v) => Ok(w.mask(*v)),
            Self::String(s) => Ok(w.mask(parse_hex_prefix(s.trim_start().as_bytes()))),
            Self::Buffer(b) => Ok(buffer_to_int(b, w)),
            _ => Err(Error::TypeMismatch),
        }
    }

    /// Implicit conversion to a buffer: an integer's bytes (little endian,
    /// integer width), a string's bytes plus its NUL terminator.
    ///
    /// # Errors
    ///
    /// [`Error::TypeMismatch`] for packages, references, and uninitialized
    /// values.
    pub fn to_buffer(&self, w: IntWidth) -> Result<Vec<u8>, Error> {
        match self {
            Self::Integer(v) => Ok(v.to_le_bytes()[..w.bytes()].to_vec()),
            Self::String(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                Ok(b)
            }
            Self::Buffer(b) => Ok(b.clone()),
            _ => Err(Error::TypeMismatch),
        }
    }

    /// Implicit conversion to a string: an integer as fixed-width upper-case
    /// hexadecimal, a buffer as space-separated `0xNN` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::TypeMismatch`] for packages, references, and uninitialized
    /// values.
    pub fn to_aml_string(&self, w: IntWidth) -> Result<String, Error> {
        match self {
            Self::Integer(v) => {
                let mut s = String::new();
                let digits = w.bytes() * 2;
                let _ = write!(s, "{:0digits$X}", w.mask(*v));
                Ok(s)
            }
            Self::String(s) => Ok(s.clone()),
            Self::Buffer(b) => Ok(join_bytes(b, " ", true)),
            _ => Err(Error::TypeMismatch),
        }
    }
}

/// Reads hexadecimal digits up to the first character that is not one.
pub(crate) fn parse_hex_prefix(b: &[u8]) -> u64 {
    let b = b
        .strip_prefix(b"0x")
        .or_else(|| b.strip_prefix(b"0X"))
        .unwrap_or(b);
    let mut v: u64 = 0;
    for &c in b {
        let Some(d) = char::from(c).to_digit(16) else {
            break;
        };
        v = v.wrapping_shl(4) | u64::from(d);
    }
    v
}

/// Reads decimal digits up to the first character that is not one.
pub(crate) fn parse_dec_prefix(b: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for &c in b {
        let Some(d) = char::from(c).to_digit(10) else {
            break;
        };
        v = v.wrapping_mul(10).wrapping_add(u64::from(d));
    }
    v
}

pub(crate) fn buffer_to_int(b: &[u8], w: IntWidth) -> u64 {
    let mut bytes = [0u8; 8];
    let n = b.len().min(w.bytes());
    bytes[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(bytes)
}

/// Formats bytes as `0xNN` (hex) or decimal, joined by `sep`.
pub(crate) fn join_bytes(b: &[u8], sep: &str, hex: bool) -> String {
    let mut s = String::new();
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push_str(sep);
        }
        if hex {
            let _ = write!(s, "0x{x:02X}");
        } else {
            let _ = write!(s, "{x}");
        }
    }
    s
}

/// Makes an AML string from bytes, replacing anything outside ASCII.
pub(crate) fn string_from_bytes(b: &[u8]) -> String {
    b.iter()
        .map(|&c| if c.is_ascii() { char::from(c) } else { '?' })
        .collect()
}
