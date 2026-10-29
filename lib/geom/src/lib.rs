// SPDX-License-Identifier: BSD-2-Clause
//! GEOM's userland view: the mesh of classes, geoms, consumers, and
//! providers that `kern.geom.confxml`, `conftxt`, and `confdot` show
//! (libgeom reads the first; `gpart`, `geom`, and `glabel` print it), and
//! the names and numbers its classes' `dumpconf` methods derive from what
//! a disk holds.
//!
//! - [`Mesh`]: built in the order FreeBSD creates things (each list then
//!   comes out newest first, as GEOM's `LIST_INSERT_HEAD` lists do), with
//!   access counts passed up the way each class's `access` method passes
//!   them; formatted as `geom_dump.c` formats it.
//! - [`label`]: the LABEL class's names (`gpt/…`, `gptid/…`) and the
//!   escaping `dumpconf` applies.
//! - [`part`]: the PART class's GPT details: type names, `efimedia`, and
//!   the geometry `g_part` makes up for disks without one.
//!
//! Behavior reference: sys/geom/geom_dump.c, sys/geom/geom_subr.c,
//! sys/geom/geom_slice.c, sys/geom/part/g_part.c, g_part_gpt.c,
//! sys/geom/label/g_label.c, g_label_gpt.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod label;
pub mod mesh;
pub mod part;

pub use mesh::{Access, ClassRef, ConsumerRef, Field, GeomRef, Kind, Media, Mesh, ProviderRef};
