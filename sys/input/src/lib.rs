// SPDX-License-Identifier: BSD-2-Clause
//! The input class (USB.md §6.3): keyboards, as FreeBSD's kbd layer
//! presents them.
//!
//! A keyboard driver (`hkbd`, `virtio-input`) registers a [`Keyboard`]
//! and reports key presses and releases by HID usage; it may sleep for
//! nothing, since reports arrive in completion context. The class queues
//! them, and its thread applies the modifiers, locks, and keymap
//! ([`keymap`]), repeats a held key (FreeBSD's default 500 ms, then every
//! 100 ms), and hands the bytes to the [`Consumer`]: the console terminal,
//! and later the active virtual terminal. Every keyboard feeds the one
//! consumer, as `kbdmux` merges FreeBSD's.
//!
//! Behavior reference: sys/dev/kbd/kbd.c, sys/dev/kbdmux/kbdmux.c,
//! sys/dev/hid/hkbd.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod keymap;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sched::{Sleepable, WaitQueue};
use sync::SpinLock;

use crate::keymap::{Action, Lock, Modifier, Mods};

/// Where typed bytes go.
pub trait Consumer: Send + Sync {
    /// Delivers bytes typed.
    fn input(&self, s: &Sleepable, bytes: &[u8]);
    /// Alt-F*n*: switch to virtual terminal `n`.
    fn switch(&self, s: &Sleepable, n: u8) {
        let _ = (s, n);
    }
}

/// The lock LEDs to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Leds {
    /// Num Lock.
    pub num: bool,
    /// Caps Lock.
    pub caps: bool,
    /// Scroll Lock.
    pub scroll: bool,
}

/// Repeat delay and period, FreeBSD's defaults until `kbdcontrol -r`.
const DELAY_MS: u32 = 500;
const RATE_MS: u32 = 100;
/// The thread's polling period while a key repeats.
const TICK_MS: u32 = 10;

/// A registered keyboard.
pub struct Keyboard {
    /// Its `kbdN` index.
    pub index: u32,
    name: String,
    state: SpinLock<State>,
    leds: Box<dyn Fn(Leds) + Send + Sync>,
    gone: AtomicBool,
}

impl core::fmt::Debug for Keyboard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Keyboard")
            .field("index", &self.index)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    shift: u8,
    ctrl: u8,
    alt: u8,
    caps: bool,
    num: bool,
    scroll: bool,
}

impl State {
    fn mods(&self) -> Mods {
        Mods {
            shift: self.shift > 0,
            ctrl: self.ctrl > 0,
            alt: self.alt > 0,
            caps: self.caps,
            num: self.num,
        }
    }

    fn leds(&self) -> Leds {
        Leds {
            num: self.num,
            caps: self.caps,
            scroll: self.scroll,
        }
    }
}

struct Class {
    events: SpinLock<VecDeque<(Arc<Keyboard>, u16, bool)>>,
    queue: WaitQueue,
    consumer: SpinLock<Option<Arc<dyn Consumer>>>,
    started: AtomicBool,
    /// `kbd0` is `kbdmux0`.
    next_index: AtomicU32,
}

static CLASS: Class = Class {
    events: SpinLock::new(VecDeque::new()),
    queue: WaitQueue::new(),
    consumer: SpinLock::new(None),
    started: AtomicBool::new(false),
    next_index: AtomicU32::new(1),
};

/// Sets where typed bytes go.
pub fn set_consumer(c: Arc<dyn Consumer>) {
    *CLASS.consumer.lock() = Some(c);
}

/// Registers a keyboard: `name` is its driver instance (`hkbd0`), `leds`
/// what to call when the lock LEDs change. Logs FreeBSD's `kbdN at
/// hkbd0`.
///
/// # Errors
///
/// If the class's thread cannot start.
pub fn attach(
    name: &str,
    leds: impl Fn(Leds) + Send + Sync + 'static,
) -> Result<Arc<Keyboard>, sched::SpawnError> {
    if !CLASS.started.swap(true, Ordering::AcqRel) {
        bus::manager().log(format_args!("kbd0 at kbdmux0"));
        let r = sched::Builder::new("kbd")
            .priority(sched::Priority::KERNEL)
            .spawn(thread);
        if let Err(e) = r {
            CLASS.started.store(false, Ordering::Release);
            return Err(e);
        }
    }
    let index = CLASS.next_index.fetch_add(1, Ordering::Relaxed);
    bus::manager().log(format_args!("kbd{index} at {name}"));
    Ok(Arc::new(Keyboard {
        index,
        name: String::from(name),
        state: SpinLock::new(State {
            num: false,
            ..State::default()
        }),
        leds: Box::new(leds),
        gone: AtomicBool::new(false),
    }))
}

impl Keyboard {
    /// Reports key `usage` (HID keyboard page) pressed or released.
    /// Callable from completion context: it only queues.
    pub fn key(self: &Arc<Self>, usage: u16, down: bool) {
        CLASS
            .events
            .lock()
            .push_back((Arc::clone(self), usage, down));
        CLASS.queue.wake_all();
    }

    /// The keyboard is going away: its held keys stop repeating.
    pub fn detach(&self) {
        self.gone.store(true, Ordering::Release);
    }

    /// Applies one event: the bytes it sends, if any, and whether it
    /// starts (`Some(true)`) or stops (`Some(false)`) a repeat.
    fn apply(&self, usage: u16, down: bool, out: &mut Vec<u8>) -> (Option<bool>, Option<u8>) {
        let mut st = self.state.lock();
        let mods = st.mods();
        match keymap::action(usage, mods) {
            Action::Modifier(m) => {
                let c = match m {
                    Modifier::Shift => &mut st.shift,
                    Modifier::Ctrl => &mut st.ctrl,
                    Modifier::Alt => &mut st.alt,
                    Modifier::Gui => return (None, None),
                };
                *c = if down {
                    c.saturating_add(1)
                } else {
                    c.saturating_sub(1)
                };
                (None, None)
            }
            Action::Lock(l) => {
                if down {
                    match l {
                        Lock::Caps => st.caps = !st.caps,
                        Lock::Num => st.num = !st.num,
                        Lock::Scroll => st.scroll = !st.scroll,
                    }
                    let leds = st.leds();
                    drop(st);
                    (self.leds)(leds);
                }
                (None, None)
            }
            Action::Switch(n) => (None, down.then_some(n)),
            Action::Byte(b) if down => {
                if mods.alt {
                    // Alt sends the byte with ESC before it (`kbdcontrol`'s
                    // default META handling in a terminal).
                    out.push(0x1b);
                }
                out.push(b);
                (Some(true), None)
            }
            Action::Seq(q) if down => {
                out.extend_from_slice(q);
                (Some(true), None)
            }
            Action::None if down => (None, None),
            _ => (Some(false), None),
        }
    }
}

/// The class's thread: keymap, repeat, delivery.
fn thread(s: &Sleepable) {
    let mut held: Option<(Arc<Keyboard>, u16)> = None;
    let mut wait_ms = 0u32;
    loop {
        if held.is_none() {
            CLASS
                .queue
                .wait_until(s, || !CLASS.events.lock().is_empty());
        } else {
            bus::manager().sleep_ms(s, u64::from(TICK_MS));
            wait_ms = wait_ms.saturating_sub(TICK_MS);
        }
        let mut out = Vec::new();
        let mut switch = None;
        loop {
            let e = CLASS.events.lock().pop_front();
            let Some((k, usage, down)) = e else { break };
            let (repeat, sw) = k.apply(usage, down, &mut out);
            switch = sw.or(switch);
            match repeat {
                Some(true) => {
                    held = Some((k, usage));
                    wait_ms = DELAY_MS;
                }
                Some(false)
                    if held
                        .as_ref()
                        .is_some_and(|(hk, hu)| Arc::ptr_eq(hk, &k) && *hu == usage) =>
                {
                    held = None;
                }
                _ => {}
            }
        }
        if let Some((k, usage)) = &held {
            if k.gone.load(Ordering::Acquire) {
                held = None;
            } else if wait_ms == 0 {
                let _ = k.apply(*usage, true, &mut out);
                wait_ms = RATE_MS;
            }
        }
        let consumer = CLASS.consumer.lock().clone();
        if let Some(c) = consumer {
            if let Some(n) = switch {
                c.switch(s, n);
            }
            if !out.is_empty() {
                c.input(s, &out);
            }
        }
    }
}
