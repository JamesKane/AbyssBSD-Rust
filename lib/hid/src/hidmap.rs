// SPDX-License-Identifier: BSD-2-Clause
//! HID input usages mapped one to one to evdev events, as FreeBSD's
//! `hidmap` does for its drivers on `hidbus` (`hms`, `hcons`, `hsctrl`):
//! a driver gives tables of [`MapItem`]s, each a usage (or a run of them)
//! and the event it becomes, or a [`Callback`] that reports it itself.
//!
//! [`probe`] says how many of a collection's input values a table maps
//! and which of its entries matched ([`Caps`]); a driver declines the
//! collection when none does. [`Hidmap::attach`] then makes an item for
//! each mapped value, telling the evdev device being built what it will
//! report ([`Support`]), and [`Hidmap::intr`] turns each input report into
//! events ([`Events`]): a variable value reports when it changes (a
//! relative one when it is not 0), an array slot releases the key it held
//! and presses the one its value names. Where two usages map to one key
//! code, a report's presses and releases of it are merged first, so a key
//! held in one slot and released in another stays down.
//!
//! The quirks a FreeBSD userland sees are kept: values past the report's
//! end read as 0, a usage array over a range reports each key by its map
//! entry's first code, and an array list holds at most eight usages.
//!
//! Behavior reference: sys/dev/hid/hidmap.c, sys/dev/hid/hidmap.h, and
//! `hid_get_item`'s and `hid_get_data`'s rules (sys/dev/hid/hid.c).

use alloc::vec;
use alloc::vec::Vec;

use abi::evdev::{EV_ABS, EV_KEY, EV_REL, EV_SW, KEY_CNT, KEY_RESERVED};

use crate::{Descriptor, Field, Kind, Usages, flags};

/// The most tables one device takes (`HIDMAP_MAX_MAPS`).
pub const MAX_MAPS: usize = 4;

/// The event code that discards an array slot's value (`HIDMAP_KEY_NULL`).
pub const KEY_NULL: u16 = 0xff;

/// The most usages an array field's slot lists (`HID_ITEM_MAXUSAGE`).
const ITEM_MAX_USAGES: usize = 8;

/// The bytes of a key bitmap.
const KEY_BYTES: usize = KEY_CNT as usize / 8;

/// Whether an entry takes relative values, absolute ones, or either
/// (`enum hidmap_relabs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelAbs {
    /// Either.
    Any,
    /// Relative only.
    Relative,
    /// Absolute only.
    Absolute,
}

/// The value an item's callback is called about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemInfo {
    /// The item's usage (extended).
    pub usage: u32,
    /// Its field's flags.
    pub flags: u32,
    /// Its logical range.
    pub logical_min: i32,
    /// Its logical range.
    pub logical_max: i32,
}

/// When a callback is called, and with what (`enum hidmap_cb_state` and
/// `union hidmap_cb_ctx`).
pub enum Stage<'a> {
    /// The collection is being probed: the callback says whether it maps
    /// `item` (`None` for a finalizing callback). Nothing is set up yet.
    Probing(Option<&'a ItemInfo>),
    /// The device is being built: the callback says again whether it maps
    /// `item`, as it did when probing, and what it will report.
    Attaching(Option<&'a ItemInfo>, &'a mut dyn Support),
    /// A report came: `data` is the item's value (0 for a finalizing
    /// callback), `id` the report's ID.
    Running {
        /// The value.
        data: i32,
        /// The report ID.
        id: u8,
        /// Where its events go.
        events: &'a mut dyn Events,
    },
    /// The device is going away.
    Detaching,
}

/// A callback's context: its stage, and a word of its own kept with the
/// item (`HIDMAP_CB_UDATA64`).
pub struct Cb<'a> {
    /// The stage.
    pub stage: Stage<'a>,
    /// The item's word, 0 until the callback sets it at attach.
    pub udata: &'a mut u64,
}

impl core::fmt::Debug for Stage<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Probing(i) => f.debug_tuple("Probing").field(i).finish(),
            Self::Attaching(i, _) => f.debug_tuple("Attaching").field(i).finish(),
            Self::Running { data, id, .. } => f
                .debug_struct("Running")
                .field("data", data)
                .field("id", id)
                .finish_non_exhaustive(),
            Self::Detaching => f.write_str("Detaching"),
        }
    }
}

impl core::fmt::Debug for Cb<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cb")
            .field("stage", &self.stage)
            .field("udata", &self.udata)
            .finish()
    }
}

/// A callback (`hidmap_cb_t`): `true` where FreeBSD's returns 0 (it maps
/// the item, or it reported something), `false` otherwise.
pub type Callback = fn(&mut Cb<'_>) -> bool;

/// What an entry does with the values it maps.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    /// Reports them as events of `kind` from `code` on (one code per
    /// usage of the entry's run).
    Event {
        /// The event type.
        kind: u16,
        /// The first code.
        code: u16,
        /// An absolute axis's fuzz.
        fuzz: u16,
        /// An absolute axis's flat.
        flat: u16,
    },
    /// Hands them to a callback.
    Callback(Callback),
    /// Runs a callback for every report, not tied to a usage
    /// (`HIDMAP_FINAL_CB`).
    Final(Callback),
}

/// One entry of a driver's table (`struct hidmap_item`).
#[derive(Clone, Copy, Debug)]
pub struct MapItem {
    /// The first usage (extended).
    pub usage: u32,
    /// How many usages from it the entry covers.
    pub nusages: u16,
    /// What it does.
    pub action: Action,
    /// Which values it takes.
    pub relabs: RelAbs,
    /// A collection without it is not the driver's.
    pub required: bool,
    /// A collection with it is not the driver's.
    pub forbidden: bool,
    /// Values are reported negated (relative) or mirrored in their range
    /// (absolute).
    pub invert: bool,
}

const fn extended(page: u16, usage: u16) -> u32 {
    ((page as u32) << 16) | usage as u32
}

impl MapItem {
    const fn event(page: u16, usage: u16, n: u16, kind: u16, code: u16, relabs: RelAbs) -> Self {
        Self {
            usage: extended(page, usage),
            nusages: n,
            action: Action::Event {
                kind,
                code,
                fuzz: 0,
                flat: 0,
            },
            relabs,
            required: false,
            forbidden: false,
            invert: false,
        }
    }

    /// Usage `usage` of `page` as key `code` (`HIDMAP_KEY`).
    #[must_use]
    pub const fn key(page: u16, usage: u16, code: u16) -> Self {
        Self::event(page, usage, 1, EV_KEY, code, RelAbs::Any)
    }

    /// Usages `from..=to` as keys from `code` on (`HIDMAP_KEY_RANGE`).
    #[must_use]
    pub const fn key_range(page: u16, from: u16, to: u16, code: u16) -> Self {
        Self::event(page, from, to - from + 1, EV_KEY, code, RelAbs::Any)
    }

    /// A relative usage as relative axis `code` (`HIDMAP_REL`).
    #[must_use]
    pub const fn rel(page: u16, usage: u16, code: u16) -> Self {
        Self::event(page, usage, 1, EV_REL, code, RelAbs::Relative)
    }

    /// An absolute usage as absolute axis `code` (`HIDMAP_ABS`).
    #[must_use]
    pub const fn abs(page: u16, usage: u16, code: u16) -> Self {
        Self::event(page, usage, 1, EV_ABS, code, RelAbs::Absolute)
    }

    /// A usage as switch `code` (`HIDMAP_SW`).
    #[must_use]
    pub const fn sw(page: u16, usage: u16, code: u16) -> Self {
        Self::event(page, usage, 1, EV_SW, code, RelAbs::Any)
    }

    const fn callback(page: u16, usage: u16, cb: Callback, relabs: RelAbs) -> Self {
        Self {
            usage: extended(page, usage),
            nusages: 1,
            action: Action::Callback(cb),
            relabs,
            required: false,
            forbidden: false,
            invert: false,
        }
    }

    /// A relative usage reported by `cb` (`HIDMAP_REL_CB`).
    #[must_use]
    pub const fn rel_cb(page: u16, usage: u16, cb: Callback) -> Self {
        Self::callback(page, usage, cb, RelAbs::Relative)
    }

    /// An absolute usage reported by `cb` (`HIDMAP_ABS_CB`).
    #[must_use]
    pub const fn abs_cb(page: u16, usage: u16, cb: Callback) -> Self {
        Self::callback(page, usage, cb, RelAbs::Absolute)
    }

    /// A callback run for every report (`HIDMAP_FINAL_CB`).
    #[must_use]
    pub const fn final_cb(cb: Callback) -> Self {
        Self {
            usage: 0,
            nusages: 1,
            action: Action::Final(cb),
            relabs: RelAbs::Any,
            required: false,
            forbidden: false,
            invert: false,
        }
    }

    /// The entry must match for the collection to be the driver's.
    #[must_use]
    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The entry must not match for the collection to be the driver's.
    #[must_use]
    pub const fn forbidden(mut self) -> Self {
        self.forbidden = true;
        self
    }

    /// Values are reported inverted (`invert_value`).
    #[must_use]
    pub const fn inverted(mut self) -> Self {
        self.invert = true;
        self
    }

    /// The usage covered at offset `off`, if the entry's run is that long.
    fn usage_at(&self, off: u16) -> u32 {
        self.usage.wrapping_add(u32::from(off))
    }

    /// The offsets an entry is tried at: at least one, as
    /// `hidmap_get_next_map_index` walks a table.
    fn offsets(&self) -> core::ops::Range<u16> {
        0..self.nusages.max(1)
    }

    /// Its per-usage callback, if it has one.
    fn item_cb(&self) -> Option<Callback> {
        match self.action {
            Action::Callback(cb) => Some(cb),
            _ => None,
        }
    }

    /// Its event type and first code, if it reports events itself.
    fn event_code(&self) -> Option<(u16, u16)> {
        match self.action {
            Action::Event { kind, code, .. } => Some((kind, code)),
            _ => None,
        }
    }

    /// Whether `relative` values suit it.
    fn takes(&self, relative: bool) -> bool {
        match self.relabs {
            RelAbs::Any => true,
            RelAbs::Relative => relative,
            RelAbs::Absolute => !relative,
        }
    }

    /// `can_map_callback`.
    fn maps_callback(&self, v: &Value<'_>, off: u16) -> bool {
        self.item_cb().is_some() && v.usage == self.usage_at(off) && self.takes(v.relative())
    }

    /// `can_map_variable`.
    fn maps_variable(&self, v: &Value<'_>, off: u16) -> bool {
        v.variable()
            && self.event_code().is_some()
            && v.usage == self.usage_at(off)
            && self.takes(v.relative())
    }

    /// Whether it is a key that an array can press.
    fn array_key(&self) -> bool {
        matches!(self.event_code(), Some((EV_KEY, code)) if code != KEY_RESERVED && code != KEY_NULL)
    }

    /// `can_map_arr_range`.
    fn maps_range(&self, v: &Value<'_>, off: u16) -> bool {
        let u = self.usage_at(off);
        !v.variable() && v.umin <= u && v.umax >= u && self.array_key()
    }

    /// `can_map_arr_list`.
    fn maps_listed(&self, v: &Value<'_>, usage: u32, off: u16) -> bool {
        !v.variable() && usage == self.usage_at(off) && self.array_key()
    }
}

/// Which entries of a table matched (`hidmap_caps_t`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps(Vec<bool>);

impl Caps {
    /// Whether entry `i` matched (`hidmap_test_cap`).
    #[must_use]
    pub fn test(&self, i: usize) -> bool {
        self.0.get(i).copied().unwrap_or(false)
    }

    fn set(&mut self, i: usize) {
        if let Some(b) = self.0.get_mut(i) {
            *b = true;
        }
    }
}

/// What an evdev device being built is told it reports
/// (`evdev_support_*`).
pub trait Support {
    /// Events of type `kind`.
    fn event(&mut self, kind: u16);
    /// Key `code`.
    fn key(&mut self, code: u16);
    /// Relative axis `code`.
    fn rel(&mut self, code: u16);
    /// Absolute axis `code`.
    fn abs(&mut self, code: u16, min: i32, max: i32, fuzz: i32, flat: i32, resolution: i32);
    /// Switch `code`.
    fn sw(&mut self, code: u16);
}

/// Where a device's events go (`evdev_push_*`).
pub trait Events {
    /// One event.
    fn push(&mut self, kind: u16, code: u16, value: i32);
    /// Key `code` down or up (`evdev_push_key`).
    fn push_key(&mut self, code: u16, down: bool) {
        self.push(EV_KEY, code, i32::from(down));
    }
    /// The end of a report (`evdev_sync`).
    fn sync(&mut self);
}

/// One value of an input field, as `hid_get_item` gives it.
struct Value<'a> {
    field: &'a Field,
    /// Its first bit.
    pos: u32,
    /// Its usage (an array's first).
    usage: u32,
    /// An array's usage range (`usage_minimum`, `usage_maximum`), or 0.
    umin: u32,
    umax: u32,
    /// An array's listed usages.
    list: &'a [u32],
}

impl<'a> Value<'a> {
    fn variable(&self) -> bool {
        self.field.is_variable()
    }

    fn relative(&self) -> bool {
        self.field.is_relative()
    }

    fn info(&self) -> ItemInfo {
        ItemInfo {
            usage: self.usage,
            flags: self.field.flags,
            logical_min: self.field.logical_min,
            logical_max: self.field.logical_max,
        }
    }

    /// Value `i` of `f`.
    fn of(f: &'a Field, i: u16) -> Self {
        let pos = f.bit + u32::from(i) * u32::from(f.size);
        if f.is_variable() {
            // Each value has its usage; past the last, the last repeats.
            let usage = match &f.usages {
                Usages::List(l) => l
                    .get(usize::from(i))
                    .or_else(|| l.last())
                    .copied()
                    .unwrap_or(0),
                Usages::Range(lo, hi) => lo.saturating_add(u32::from(i)).min(*hi),
            };
            return Self {
                field: f,
                pos,
                usage,
                umin: 0,
                umax: 0,
                list: &[],
            };
        }
        match &f.usages {
            Usages::Range(lo, hi) => Self {
                field: f,
                pos,
                usage: *lo,
                umin: *lo,
                umax: *hi,
                list: &[],
            },
            Usages::List(l) => Self {
                field: f,
                pos,
                usage: l.first().copied().unwrap_or(0),
                umin: 0,
                umax: 0,
                list: &l[..l.len().min(ITEM_MAX_USAGES)],
            },
        }
    }
}

/// The input values of collection `tlc`, in descriptor order
/// (`HIDBUS_FOREACH_ITEM` over the inputs, constants skipped).
fn values(desc: &Descriptor, tlc: usize) -> impl Iterator<Item = Value<'_>> {
    desc.fields_of(tlc)
        .filter(|f| f.kind == Kind::Input && !f.is_constant())
        .flat_map(|f| (0..f.count).map(move |i| Value::of(f, i)))
}

/// `hidmap_probe_hid_item`: whether `map` maps `v`, noting the entries
/// that do.
fn probe_value(v: &Value<'_>, map: &[MapItem], caps: &mut Caps) -> bool {
    let info = v.info();
    'callbacks: for (i, mi) in map.iter().enumerate() {
        for off in mi.offsets() {
            if mi.maps_callback(v, off) {
                let Some(cb) = mi.item_cb() else { continue };
                let mut udata = 0;
                if !cb(&mut Cb {
                    stage: Stage::Probing(Some(&info)),
                    udata: &mut udata,
                }) {
                    break 'callbacks;
                }
                caps.set(i);
                return true;
            }
        }
    }
    if v.variable() {
        for (i, mi) in map.iter().enumerate() {
            if mi.offsets().any(|off| mi.maps_variable(v, off)) {
                caps.set(i);
                return true;
            }
        }
        return false;
    }
    let mut found = false;
    if v.umin != 0 || v.umax != 0 {
        for (i, mi) in map.iter().enumerate() {
            if mi.offsets().any(|off| mi.maps_range(v, off)) {
                caps.set(i);
                found = true;
            }
        }
        return found;
    }
    for &u in v.list {
        for (i, mi) in map.iter().enumerate() {
            if mi.offsets().any(|off| mi.maps_listed(v, u, off)) {
                caps.set(i);
                found = true;
            }
        }
    }
    found
}

/// `hidmap_probe_hid_descr`: how many values of collection `tlc` `map`
/// maps (finalizing callbacks that accept counted too), and which of its
/// entries matched; 0 if a required entry did not or a forbidden one did.
#[must_use]
pub fn probe(desc: &Descriptor, tlc: usize, map: &[MapItem]) -> (u32, Caps) {
    let mut caps = Caps(vec![false; map.len()]);
    let mut items: u32 = 0;
    for v in values(desc, tlc) {
        if probe_value(&v, map, &mut caps) {
            items = items.saturating_add(1);
        }
    }
    for (i, mi) in map.iter().enumerate() {
        if let Action::Final(cb) = mi.action {
            let mut udata = 0;
            if cb(&mut Cb {
                stage: Stage::Probing(None),
                udata: &mut udata,
            }) {
                caps.set(i);
                items = items.saturating_add(1);
            }
        }
    }
    if items != 0
        && map
            .iter()
            .enumerate()
            .any(|(i, mi)| (mi.required && !caps.test(i)) || (mi.forbidden && caps.test(i)))
    {
        items = 0;
    }
    (items, caps)
}

/// What a parsed item is (`enum hidmap_type`, with its per-type data).
#[derive(Clone, Debug)]
pub enum ItemKind {
    /// A finalizing callback, run for every report.
    Final(Callback),
    /// A value reported by a callback.
    Callback(Callback),
    /// A variable value reported as event `kind`, `code`; with a null
    /// state, out-of-range values change nothing.
    Variable {
        /// The event type.
        kind: u16,
        /// The code.
        code: u16,
        /// Whether the field has a null state.
        null_state: bool,
    },
    /// An array slot whose value indexes these key codes.
    List(Vec<u16>),
    /// An array slot whose value indexes a usage range from this one.
    Range(u32),
}

/// One mapped value (`struct hidmap_hid_item`).
#[derive(Clone, Debug)]
pub struct Item {
    /// What it is.
    pub kind: ItemKind,
    /// Its report's ID.
    pub id: u8,
    pos: u32,
    size: u8,
    /// Its logical range.
    pub logical_min: i32,
    /// Its logical range.
    pub logical_max: i32,
    invert: bool,
    /// The last value reported (variables).
    last_val: i32,
    /// The last key pressed (arrays).
    last_key: u16,
    /// A callback's word.
    udata: u64,
}

impl Item {
    fn new(kind: ItemKind, v: Option<&Value<'_>>) -> Self {
        Self {
            kind,
            id: v.map_or(0, |v| v.field.report_id),
            pos: v.map_or(0, |v| v.pos),
            size: v.map_or(0, |v| v.field.size),
            logical_min: v.map_or(0, |v| v.field.logical_min),
            logical_max: v.map_or(0, |v| v.field.logical_max),
            invert: false,
            last_val: 0,
            last_key: KEY_RESERVED,
            udata: 0,
        }
    }

    /// Its value in `report`: `hid_get_data` (signed) where either end of
    /// the range is negative, `hid_get_udata` otherwise (HID 1.11 §5.8).
    fn data(&self, report: &[u8]) -> i32 {
        get_data(
            report,
            self.pos,
            self.size,
            self.logical_min < 0 || self.logical_max < 0,
        )
    }
}

/// `hid_get_data_sub`: `size` bits at bit `pos`, from the whole bytes the
/// value's size spans starting at its first byte; bytes past the report's
/// end read as 0.
#[must_use]
pub fn get_data(report: &[u8], pos: u32, size: u8, signed: bool) -> i32 {
    let size = u32::from(size.min(32));
    if size == 0 {
        return 0;
    }
    let first = pos / 8;
    let n = size.div_ceil(8);
    let mut data: u32 = 0;
    for k in 0..n {
        let b = usize::try_from(first + k)
            .ok()
            .and_then(|at| report.get(at))
            .copied()
            .unwrap_or(0);
        data |= u32::from(b) << (8 * k);
    }
    data >>= pos % 8;
    let shift = 32 - size;
    if signed {
        // Sign extension: the value's top bit moved to bit 31 and back.
        (data << shift).cast_signed() >> shift
    } else {
        ((data << shift) >> shift).cast_signed()
    }
}

/// Every entry of `maps`, with its usage offsets.
fn entries_of<'m>(
    maps: &'m [&'static [MapItem]],
) -> impl Iterator<Item = (&'static MapItem, u16)> + 'm {
    maps.iter()
        .flat_map(|m| m.iter())
        .flat_map(|mi| mi.offsets().map(move |off| (mi, off)))
}

/// Merged key presses and releases (`key_press`, `key_rel`).
struct Keys {
    /// While attaching, the keys supported; then the keys a report
    /// presses.
    press: Vec<u8>,
    /// The keys a report releases, once two usages share a key.
    rel: Option<Vec<u8>>,
    min: u16,
    max: u16,
}

fn bit(b: &[u8], k: u16) -> bool {
    b.get(usize::from(k / 8))
        .is_some_and(|x| x & (1 << (k % 8)) != 0)
}

fn set_bit(b: &mut [u8], k: u16) {
    if let Some(x) = b.get_mut(usize::from(k / 8)) {
        *x |= 1 << (k % 8);
    }
}

/// A device's tables and mapped values (`struct hidmap`).
#[derive(Default)]
pub struct Hidmap {
    maps: Vec<&'static [MapItem]>,
    /// The values the tables map, as probing counted them.
    expected: u32,
    items: Vec<Item>,
    keys: Option<Keys>,
}

impl core::fmt::Debug for Hidmap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hidmap")
            .field("maps", &self.maps.len())
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

/// Why a table was not added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// It maps nothing in the collection (`ENXIO`).
    NoMatch,
    /// The device has [`MAX_MAPS`] tables already.
    TooMany,
}

impl Hidmap {
    /// No tables yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `hidmap_add_map`: adds `map` if it maps something in collection
    /// `tlc` of `desc`, and says which of its entries matched. A table
    /// added already is not added again.
    ///
    /// # Errors
    ///
    /// [`AddError::NoMatch`] if it maps nothing; [`AddError::TooMany`].
    pub fn add_map(
        &mut self,
        desc: &Descriptor,
        tlc: usize,
        map: &'static [MapItem],
    ) -> Result<Caps, AddError> {
        let (items, caps) = probe(desc, tlc, map);
        if self.maps.iter().any(|m| core::ptr::eq(*m, map)) {
            return Ok(caps);
        }
        if items == 0 {
            return Err(AddError::NoMatch);
        }
        if self.maps.len() >= MAX_MAPS {
            return Err(AddError::TooMany);
        }
        self.expected = self.expected.saturating_add(items);
        self.maps.push(map);
        Ok(caps)
    }

    /// Every entry of every table, with its usage offsets
    /// (`HIDMAP_FOREACH_ITEM`).
    fn entries(&self) -> impl Iterator<Item = (&'static MapItem, u16)> + '_ {
        entries_of(&self.maps)
    }

    /// `hidmap_support_key`: the device reports key `code`; a second
    /// usage on it turns merging on.
    fn support_key(&mut self, s: &mut dyn Support, code: u16) {
        let keys = self.keys.get_or_insert_with(|| {
            s.event(EV_KEY);
            Keys {
                press: vec![0; KEY_BYTES],
                rel: None,
                min: code,
                max: code,
            }
        });
        keys.min = keys.min.min(code);
        keys.max = keys.max.max(code);
        if bit(&keys.press, code) {
            if keys.rel.is_none() {
                keys.rel = Some(vec![0; KEY_BYTES]);
            }
        } else {
            set_bit(&mut keys.press, code);
            s.key(code);
        }
    }

    /// `hidmap_parse_hid_item`: the item for value `v`, if a table maps it.
    fn parse_value(&mut self, v: &Value<'_>, s: &mut dyn Support) -> Option<Item> {
        let info = v.info();
        let mut callback = None;
        for (mi, off) in self.entries() {
            if mi.maps_callback(v, off) {
                callback = mi.item_cb();
                break;
            }
        }
        if let Some(cb) = callback {
            let mut udata = 0;
            if cb(&mut Cb {
                stage: Stage::Attaching(Some(&info), s),
                udata: &mut udata,
            }) {
                let mut item = Item::new(ItemKind::Callback(cb), Some(v));
                item.udata = udata;
                return Some(item);
            }
        }
        if v.variable() {
            let found = self.entries().find(|(mi, off)| mi.maps_variable(v, *off));
            let (mi, off) = found?;
            let Action::Event {
                kind,
                code,
                fuzz,
                flat,
            } = mi.action
            else {
                return None;
            };
            let code = code.wrapping_add(off);
            match kind {
                EV_KEY => self.support_key(s, code),
                EV_REL => {
                    s.event(EV_REL);
                    s.rel(code);
                }
                EV_ABS => {
                    s.event(EV_ABS);
                    s.abs(
                        code,
                        v.field.logical_min,
                        v.field.logical_max,
                        i32::from(fuzz),
                        i32::from(flat),
                        v.field.resolution(),
                    );
                }
                EV_SW => {
                    s.event(EV_SW);
                    s.sw(code);
                }
                _ => return None,
            }
            let mut item = Item::new(
                ItemKind::Variable {
                    kind,
                    code,
                    null_state: v.field.flags & flags::NULL_STATE != 0,
                },
                Some(v),
            );
            item.invert = mi.invert;
            return Some(item);
        }
        if v.umin != 0 || v.umax != 0 {
            let codes: Vec<u16> = self
                .entries()
                .filter(|(mi, off)| mi.maps_range(v, *off))
                .filter_map(|(mi, off)| mi.event_code().map(|(_, c)| c.wrapping_add(off)))
                .collect();
            if codes.is_empty() {
                return None;
            }
            for c in codes {
                self.support_key(s, c);
            }
            return Some(Item::new(ItemKind::Range(v.umin), Some(v)));
        }
        let mut codes = vec![KEY_RESERVED; v.list.len()];
        let mut found = false;
        for (i, &u) in v.list.iter().enumerate() {
            let code = self
                .entries()
                .find(|(mi, off)| mi.maps_listed(v, u, *off))
                .and_then(|(mi, off)| mi.event_code().map(|(_, c)| c.wrapping_add(off)));
            if let Some(c) = code {
                self.support_key(s, c);
                codes[i] = c;
                found = true;
            }
        }
        found.then(|| Item::new(ItemKind::List(codes), Some(v)))
    }

    /// `hidmap_parse_hid_descr`: makes the items for collection `tlc`,
    /// telling `s` what the device reports. Finalizing callbacks that
    /// accept come last.
    pub fn attach(&mut self, desc: &Descriptor, tlc: usize, s: &mut dyn Support) {
        let mut items = Vec::new();
        for v in values(desc, tlc) {
            if let Some(item) = self.parse_value(&v, s) {
                items.push(item);
            }
        }
        let finals: Vec<Callback> = self
            .maps
            .iter()
            .flat_map(|m| m.iter())
            .filter_map(|mi| match mi.action {
                Action::Final(cb) => Some(cb),
                _ => None,
            })
            .collect();
        for cb in finals {
            let mut udata = 0;
            if cb(&mut Cb {
                stage: Stage::Attaching(None, s),
                udata: &mut udata,
            }) {
                let mut item = Item::new(ItemKind::Final(cb), None);
                item.udata = udata;
                items.push(item);
            }
        }
        self.items = items;
        // The supported set has served; from here it holds a report's
        // presses.
        if let Some(k) = self.keys.as_mut()
            && k.rel.is_some()
        {
            k.press.fill(0);
        }
    }

    /// The items, in report order (`hid_items`).
    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The values probing counted (`nhid_items` before attach).
    #[must_use]
    pub fn expected(&self) -> u32 {
        self.expected
    }

    /// Whether key events are merged per report.
    #[must_use]
    pub fn merges_keys(&self) -> bool {
        self.keys.as_ref().is_some_and(|k| k.rel.is_some())
    }

    /// `hidmap_push_key`.
    fn push_key(keys: &mut Option<Keys>, ev: &mut dyn Events, code: u16, value: i32) {
        match keys {
            Some(Keys {
                press,
                rel: Some(rel),
                ..
            }) => set_bit(if value != 0 { press } else { rel }, code),
            _ => ev.push_key(code, value != 0),
        }
    }

    /// `hidmap_sync_keys`: each key a report pressed or released, but not
    /// both, reported.
    fn sync_keys(keys: &mut Keys, ev: &mut dyn Events) {
        let Some(rel) = keys.rel.as_mut() else {
            return;
        };
        for j in keys.min / 8..=keys.max / 8 {
            let at = usize::from(j);
            if keys.press.get(at) != rel.get(at) {
                for k in j * 8..j * 8 + 8 {
                    let (p, r) = (bit(&keys.press, k), bit(rel, k));
                    if p != r {
                        ev.push_key(k, p);
                    }
                }
            }
        }
        keys.press.fill(0);
        rel.fill(0);
    }

    /// `hidmap_intr`: report `id` (its ID byte stripped) as events.
    pub fn intr(&mut self, id: u8, report: &[u8], ev: &mut dyn Events) {
        let mut sync = false;
        let maps = &self.maps;
        let keys = &mut self.keys;
        for item in &mut self.items {
            if let ItemKind::Final(cb) = item.kind {
                if cb(&mut Cb {
                    stage: Stage::Running {
                        data: 0,
                        id,
                        events: &mut *ev,
                    },
                    udata: &mut item.udata,
                }) {
                    sync = true;
                }
                continue;
            }
            if item.id != id {
                continue;
            }
            let mut data = item.data(report);
            if item.invert
                && let ItemKind::Variable { kind, .. } = item.kind
            {
                data = if kind == EV_REL {
                    data.wrapping_neg()
                } else {
                    item.logical_min
                        .wrapping_add(item.logical_max)
                        .wrapping_sub(data)
                };
            }
            let key = match &item.kind {
                ItemKind::Final(_) => continue,
                ItemKind::Callback(cb) => {
                    if !cb(&mut Cb {
                        stage: Stage::Running {
                            data,
                            id,
                            events: &mut *ev,
                        },
                        udata: &mut item.udata,
                    }) {
                        continue;
                    }
                    sync = true;
                    continue;
                }
                &ItemKind::Variable {
                    kind,
                    code,
                    null_state,
                } => {
                    // HID 1.11 §5.10: an out-of-range value leaves a
                    // control with a null state as it was.
                    if null_state && (data < item.logical_min || data > item.logical_max) {
                        continue;
                    }
                    // Unchanged absolute values and zero relative ones
                    // report nothing (evdev would drop them anyway).
                    if data == if kind == EV_REL { 0 } else { item.last_val } {
                        continue;
                    }
                    if kind == EV_KEY {
                        Self::push_key(keys, ev, code, data);
                    } else {
                        ev.push(kind, code, data);
                    }
                    item.last_val = data;
                    sync = true;
                    continue;
                }
                // §6.2.2.5: an out-of-range array value asserts no
                // control; an in-range one indexes the slot's usages.
                ItemKind::List(codes) => {
                    if data < item.logical_min || data > item.logical_max {
                        KEY_RESERVED
                    } else {
                        usize::try_from(i64::from(data) - i64::from(item.logical_min))
                            .ok()
                            .and_then(|i| codes.get(i))
                            .copied()
                            .unwrap_or(KEY_RESERVED)
                    }
                }
                &ItemKind::Range(umin) => {
                    if data < item.logical_min || data > item.logical_max {
                        KEY_RESERVED
                    } else {
                        let usage = data
                            .wrapping_sub(item.logical_min)
                            .cast_unsigned()
                            .wrapping_add(umin);
                        // The matching entry's code, not offset by the
                        // usage's place in its run: FreeBSD's lookup.
                        entries_of(maps)
                            .find(|(mi, off)| {
                                usage == mi.usage_at(*off)
                                    && matches!(mi.event_code(), Some((EV_KEY, _)))
                            })
                            .and_then(|(mi, _)| mi.event_code())
                            .map_or(KEY_RESERVED, |(_, c)| c)
                    }
                }
            };
            if key == KEY_NULL || key == item.last_key {
                continue;
            }
            if item.last_key != KEY_RESERVED {
                Self::push_key(keys, ev, item.last_key, 0);
            }
            if key != KEY_RESERVED {
                Self::push_key(keys, ev, key, 1);
            }
            item.last_key = key;
            sync = true;
        }
        if sync {
            if let Some(k) = keys.as_mut() {
                Self::sync_keys(k, ev);
            }
            ev.sync();
        }
    }

    /// `hidmap_detach`: callbacks told the device is going.
    pub fn detach(&mut self) {
        for item in &mut self.items {
            if let ItemKind::Final(cb) | ItemKind::Callback(cb) = item.kind {
                cb(&mut Cb {
                    stage: Stage::Detaching,
                    udata: &mut item.udata,
                });
            }
        }
    }
}

/// A device that records what it is told and the events it is given, for
/// tests of drivers' tables.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorder {
    /// Event types supported, in order.
    pub types: Vec<u16>,
    /// Keys supported, in order.
    pub keys: Vec<u16>,
    /// Relative axes supported.
    pub rels: Vec<u16>,
    /// Absolute axes supported, with their ranges.
    pub abs: Vec<(u16, i32, i32)>,
    /// Switches supported.
    pub sws: Vec<u16>,
    /// Events pushed: type, code, value; `sync` records `(0, 0, 1)`
    /// (`SYN_REPORT`).
    pub events: Vec<(u16, u16, i32)>,
}

impl Support for Recorder {
    fn event(&mut self, kind: u16) {
        self.types.push(kind);
    }

    fn key(&mut self, code: u16) {
        self.keys.push(code);
    }

    fn rel(&mut self, code: u16) {
        self.rels.push(code);
    }

    fn abs(&mut self, code: u16, min: i32, max: i32, _: i32, _: i32, _: i32) {
        self.abs.push((code, min, max));
    }

    fn sw(&mut self, code: u16) {
        self.sws.push(code);
    }
}

impl Events for Recorder {
    fn push(&mut self, kind: u16, code: u16, value: i32) {
        self.events.push((kind, code, value));
    }

    fn sync(&mut self) {
        self.events.push((0, 0, 1));
    }
}

impl Recorder {
    /// The events recorded since the last call.
    pub fn take(&mut self) -> Vec<(u16, u16, i32)> {
        core::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BOOT_MOUSE;

    #[test]
    fn data_reads_as_hid_get_data() {
        // Signed and unsigned, within a byte and across two.
        assert_eq!(get_data(&[0xf0], 4, 4, true), -1);
        assert_eq!(get_data(&[0xf0], 4, 4, false), 15);
        assert_eq!(get_data(&[0x34, 0x12], 0, 16, false), 0x1234);
        // Past the end: zeros.
        assert_eq!(get_data(&[0x34], 0, 16, false), 0x34);
        assert_eq!(get_data(&[], 0, 8, true), 0);
        // Only the bytes the size spans from the first are read, as
        // FreeBSD reads them: 8 bits at bit 4 lose the second byte's.
        assert_eq!(get_data(&[0x10, 0x02], 4, 8, false), 0x01);
        assert_eq!(get_data(&[1], 0, 0, false), 0);
    }

    static BUTTONS: [MapItem; 1] = [MapItem::key_range(0x09, 1, 3, 0x110)];

    /// A mouse whose buttons are an array (1 to 3 by value).
    const BUTTON_ARRAY: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x01, 0x25,
        0x03, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
    ];

    #[test]
    fn range_arrays_report_the_entrys_first_code() {
        let d = Descriptor::parse(BUTTON_ARRAY).unwrap();
        let mut m = Hidmap::new();
        m.add_map(&d, 0, &BUTTONS).unwrap();
        let mut r = Recorder::default();
        m.attach(&d, 0, &mut r);
        // Each button is supported...
        assert_eq!(r.keys, [0x110, 0x111, 0x112]);
        // ...but a press of the third reports the run's first code, as
        // `hidmap_intr`'s lookup does.
        m.intr(0, &[3], &mut r);
        assert_eq!(r.take(), [(EV_KEY, 0x110, 1), (0, 0, 1)]);
    }

    #[test]
    fn variable_buttons_take_their_offsets() {
        let d = Descriptor::parse(BOOT_MOUSE).unwrap();
        let (items, caps) = probe(&d, 0, &BUTTONS);
        assert_eq!(items, 3);
        assert!(caps.test(0));
        let mut m = Hidmap::new();
        m.add_map(&d, 0, &BUTTONS).unwrap();
        let mut r = Recorder::default();
        m.attach(&d, 0, &mut r);
        m.intr(0, &[0b100, 0, 0], &mut r);
        assert_eq!(r.take(), [(EV_KEY, 0x112, 1), (0, 0, 1)]);
        // A table added twice is kept once.
        m.add_map(&d, 0, &BUTTONS).unwrap();
        assert_eq!(m.expected(), 3);
    }

    static REQUIRED: [MapItem; 2] = [
        MapItem::key_range(0x09, 1, 3, 0x110),
        MapItem::rel(0x01, 0x38, 0x08).required(),
    ];

    #[test]
    fn a_missing_required_entry_declines() {
        let d = Descriptor::parse(BOOT_MOUSE).unwrap();
        assert_eq!(probe(&d, 0, &REQUIRED).0, 0);
        assert_eq!(
            Hidmap::new().add_map(&d, 0, &REQUIRED).unwrap_err(),
            AddError::NoMatch
        );
    }
}
