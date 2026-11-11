// SPDX-License-Identifier: BSD-2-Clause
//! Consumer Control collections (`hcons`) on `hidbus`: a keyboard's or
//! remote's volume, mute, media, brightness, browser, and application
//! keys, reported through evdev by `hidmap` ([`hidbus::hidmap`]) with
//! FreeBSD's usage-to-key table.
//!
//! A collection of usage Consumer Control (page 0x0c, usage 1) is
//! `hcons`'s if the table maps one of its input values. Keys, AC Pan
//! (`REL_HWHEEL`), and an absolute Volume (`ABS_VOLUME`) are the table's
//! events; a relative Volume is pressed and released as `KEY_VOLUMEUP` or
//! `KEY_VOLUMEDOWN` once per step of its value. Several usages share keys
//! (Sleep and Sleep Mode, both `KEY_SLEEP`), so where a device has two of
//! them a report's presses and releases of a key are merged.
//!
//! Behavior reference: sys/dev/hid/hcons.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::any::Any;

use abi::evdev as ev;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::ids::HidId;
use hid::hidmap::{Cb, MapItem, Stage};
use hid::usage::CONSUMER;
use hidbus::hidmap::Mapper;

/// Consumer: Consumer Control (`HUC_CONTROL`), the collection's usage.
pub const CONSUMER_CONTROL: u32 = 0x000c_0001;

/// What [`hidbus::hidmap::set_desc`] adds to the device's name.
const SUFFIX: &str = "Consumer Control";

/// `hcons_rel_volume_cb`: a relative Volume as Volume Up or Down pressed
/// and released once per step.
fn rel_volume(cb: &mut Cb<'_>) -> bool {
    match &mut cb.stage {
        Stage::Attaching(_, s) => {
            s.event(ev::EV_KEY);
            s.key(ev::KEY_VOLUMEUP);
            s.key(ev::KEY_VOLUMEDOWN);
            true
        }
        // Nothing to report (FreeBSD's ENOMSG).
        Stage::Running { data: 0, .. } => false,
        Stage::Running { data, events, .. } => {
            let code = if *data > 0 {
                ev::KEY_VOLUMEUP
            } else {
                ev::KEY_VOLUMEDOWN
            };
            for _ in 0..data.unsigned_abs() {
                events.push_key(code, true);
                events.push_key(code, false);
            }
            true
        }
        Stage::Probing(_) | Stage::Detaching => true,
    }
}

/// `hcons_map`, in FreeBSD's order (which decides a usage two entries
/// name, Volume's absolute and relative forms).
pub static MAP: [MapItem; 161] = [
    MapItem::key(CONSUMER, 0x030, ev::KEY_POWER),
    MapItem::key(CONSUMER, 0x031, ev::KEY_RESTART),
    MapItem::key(CONSUMER, 0x032, ev::KEY_SLEEP),
    MapItem::key(CONSUMER, 0x034, ev::KEY_SLEEP),
    MapItem::key(CONSUMER, 0x035, ev::KEY_KBDILLUMTOGGLE),
    MapItem::key(CONSUMER, 0x036, ev::BTN_MISC),
    MapItem::key(CONSUMER, 0x040, ev::KEY_MENU),   // Menu
    MapItem::key(CONSUMER, 0x041, ev::KEY_SELECT), // Menu Pick
    MapItem::key(CONSUMER, 0x042, ev::KEY_UP),     // Menu Up
    MapItem::key(CONSUMER, 0x043, ev::KEY_DOWN),   // Menu Down
    MapItem::key(CONSUMER, 0x044, ev::KEY_LEFT),   // Menu Left
    MapItem::key(CONSUMER, 0x045, ev::KEY_RIGHT),  // Menu Right
    MapItem::key(CONSUMER, 0x046, ev::KEY_ESC),    // Menu Escape
    MapItem::key(CONSUMER, 0x047, ev::KEY_KPPLUS), // Menu Value Increase
    MapItem::key(CONSUMER, 0x048, ev::KEY_KPMINUS), // Menu Value Decrease
    MapItem::key(CONSUMER, 0x060, ev::KEY_INFO),   // Data On Screen
    MapItem::key(CONSUMER, 0x061, ev::KEY_SUBTITLE), // Closed Caption
    MapItem::key(CONSUMER, 0x063, ev::KEY_VCR),    // VCR/TV
    MapItem::key(CONSUMER, 0x065, ev::KEY_CAMERA), // Snapshot
    MapItem::key(CONSUMER, 0x069, ev::KEY_RED),
    MapItem::key(CONSUMER, 0x06a, ev::KEY_GREEN),
    MapItem::key(CONSUMER, 0x06b, ev::KEY_BLUE),
    MapItem::key(CONSUMER, 0x06c, ev::KEY_YELLOW),
    MapItem::key(CONSUMER, 0x06d, ev::KEY_ASPECT_RATIO),
    MapItem::key(CONSUMER, 0x06f, ev::KEY_BRIGHTNESSUP),
    MapItem::key(CONSUMER, 0x070, ev::KEY_BRIGHTNESSDOWN),
    MapItem::key(CONSUMER, 0x072, ev::KEY_BRIGHTNESS_TOGGLE),
    MapItem::key(CONSUMER, 0x073, ev::KEY_BRIGHTNESS_MIN),
    MapItem::key(CONSUMER, 0x074, ev::KEY_BRIGHTNESS_MAX),
    MapItem::key(CONSUMER, 0x075, ev::KEY_BRIGHTNESS_AUTO),
    MapItem::key(CONSUMER, 0x079, ev::KEY_KBDILLUMUP),
    MapItem::key(CONSUMER, 0x07a, ev::KEY_KBDILLUMDOWN),
    MapItem::key(CONSUMER, 0x07c, ev::KEY_KBDILLUMTOGGLE),
    MapItem::key(CONSUMER, 0x082, ev::KEY_VIDEO_NEXT),
    MapItem::key(CONSUMER, 0x083, ev::KEY_LAST),
    MapItem::key(CONSUMER, 0x084, ev::KEY_ENTER),
    MapItem::key(CONSUMER, 0x088, ev::KEY_PC),
    MapItem::key(CONSUMER, 0x089, ev::KEY_TV),
    MapItem::key(CONSUMER, 0x08a, ev::KEY_WWW),
    MapItem::key(CONSUMER, 0x08b, ev::KEY_DVD),
    MapItem::key(CONSUMER, 0x08c, ev::KEY_PHONE),
    MapItem::key(CONSUMER, 0x08d, ev::KEY_PROGRAM),
    MapItem::key(CONSUMER, 0x08e, ev::KEY_VIDEOPHONE),
    MapItem::key(CONSUMER, 0x08f, ev::KEY_GAMES),
    MapItem::key(CONSUMER, 0x090, ev::KEY_MEMO),
    MapItem::key(CONSUMER, 0x091, ev::KEY_CD),
    MapItem::key(CONSUMER, 0x092, ev::KEY_VCR),
    MapItem::key(CONSUMER, 0x093, ev::KEY_TUNER),
    MapItem::key(CONSUMER, 0x094, ev::KEY_EXIT),
    MapItem::key(CONSUMER, 0x095, ev::KEY_HELP),
    MapItem::key(CONSUMER, 0x096, ev::KEY_TAPE),
    MapItem::key(CONSUMER, 0x097, ev::KEY_TV2),
    MapItem::key(CONSUMER, 0x098, ev::KEY_SAT),
    MapItem::key(CONSUMER, 0x09a, ev::KEY_PVR),
    MapItem::key(CONSUMER, 0x09c, ev::KEY_CHANNELUP),
    MapItem::key(CONSUMER, 0x09d, ev::KEY_CHANNELDOWN),
    MapItem::key(CONSUMER, 0x0a0, ev::KEY_VCR2),
    MapItem::key(CONSUMER, 0x0b0, ev::KEY_PLAY),
    MapItem::key(CONSUMER, 0x0b1, ev::KEY_PAUSE),
    MapItem::key(CONSUMER, 0x0b2, ev::KEY_RECORD),
    MapItem::key(CONSUMER, 0x0b3, ev::KEY_FASTFORWARD),
    MapItem::key(CONSUMER, 0x0b4, ev::KEY_REWIND),
    MapItem::key(CONSUMER, 0x0b5, ev::KEY_NEXTSONG),
    MapItem::key(CONSUMER, 0x0b6, ev::KEY_PREVIOUSSONG),
    MapItem::key(CONSUMER, 0x0b7, ev::KEY_STOPCD),
    MapItem::key(CONSUMER, 0x0b8, ev::KEY_EJECTCD),
    MapItem::key(CONSUMER, 0x0bc, ev::KEY_MEDIA_REPEAT),
    MapItem::key(CONSUMER, 0x0b9, ev::KEY_SHUFFLE),
    MapItem::key(CONSUMER, 0x0bf, ev::KEY_SLOW),
    MapItem::key(CONSUMER, 0x0cd, ev::KEY_PLAYPAUSE),
    MapItem::key(CONSUMER, 0x0cf, ev::KEY_VOICECOMMAND),
    MapItem::abs(CONSUMER, 0x0e0, ev::ABS_VOLUME),
    MapItem::rel_cb(CONSUMER, 0x0e0, rel_volume),
    MapItem::key(CONSUMER, 0x0e2, ev::KEY_MUTE),
    MapItem::key(CONSUMER, 0x0e5, ev::KEY_BASSBOOST),
    MapItem::key(CONSUMER, 0x0e9, ev::KEY_VOLUMEUP),
    MapItem::key(CONSUMER, 0x0ea, ev::KEY_VOLUMEDOWN),
    MapItem::key(CONSUMER, 0x0f5, ev::KEY_SLOW),
    MapItem::key(CONSUMER, 0x181, ev::KEY_BUTTONCONFIG),
    MapItem::key(CONSUMER, 0x182, ev::KEY_BOOKMARKS),
    MapItem::key(CONSUMER, 0x183, ev::KEY_CONFIG),
    MapItem::key(CONSUMER, 0x184, ev::KEY_WORDPROCESSOR),
    MapItem::key(CONSUMER, 0x185, ev::KEY_EDITOR),
    MapItem::key(CONSUMER, 0x186, ev::KEY_SPREADSHEET),
    MapItem::key(CONSUMER, 0x187, ev::KEY_GRAPHICSEDITOR),
    MapItem::key(CONSUMER, 0x188, ev::KEY_PRESENTATION),
    MapItem::key(CONSUMER, 0x189, ev::KEY_DATABASE),
    MapItem::key(CONSUMER, 0x18a, ev::KEY_MAIL),
    MapItem::key(CONSUMER, 0x18b, ev::KEY_NEWS),
    MapItem::key(CONSUMER, 0x18c, ev::KEY_VOICEMAIL),
    MapItem::key(CONSUMER, 0x18d, ev::KEY_ADDRESSBOOK),
    MapItem::key(CONSUMER, 0x18e, ev::KEY_CALENDAR),
    MapItem::key(CONSUMER, 0x18f, ev::KEY_TASKMANAGER),
    MapItem::key(CONSUMER, 0x190, ev::KEY_JOURNAL),
    MapItem::key(CONSUMER, 0x191, ev::KEY_FINANCE),
    MapItem::key(CONSUMER, 0x192, ev::KEY_CALC),
    MapItem::key(CONSUMER, 0x193, ev::KEY_PLAYER),
    MapItem::key(CONSUMER, 0x194, ev::KEY_FILE),
    MapItem::key(CONSUMER, 0x196, ev::KEY_WWW),
    MapItem::key(CONSUMER, 0x199, ev::KEY_CHAT),
    MapItem::key(CONSUMER, 0x19c, ev::KEY_LOGOFF),
    MapItem::key(CONSUMER, 0x19e, ev::KEY_COFFEE),
    MapItem::key(CONSUMER, 0x19f, ev::KEY_CONTROLPANEL),
    MapItem::key(CONSUMER, 0x1a2, ev::KEY_APPSELECT),
    MapItem::key(CONSUMER, 0x1a3, ev::KEY_NEXT),
    MapItem::key(CONSUMER, 0x1a4, ev::KEY_PREVIOUS),
    MapItem::key(CONSUMER, 0x1a6, ev::KEY_HELP),
    MapItem::key(CONSUMER, 0x1a7, ev::KEY_DOCUMENTS),
    MapItem::key(CONSUMER, 0x1ab, ev::KEY_SPELLCHECK),
    MapItem::key(CONSUMER, 0x1ae, ev::KEY_KEYBOARD),
    MapItem::key(CONSUMER, 0x1b1, ev::KEY_SCREENSAVER),
    MapItem::key(CONSUMER, 0x1b4, ev::KEY_FILE),
    MapItem::key(CONSUMER, 0x1b6, ev::KEY_IMAGES),
    MapItem::key(CONSUMER, 0x1b7, ev::KEY_AUDIO),
    MapItem::key(CONSUMER, 0x1b8, ev::KEY_VIDEO),
    MapItem::key(CONSUMER, 0x1bc, ev::KEY_MESSENGER),
    MapItem::key(CONSUMER, 0x1bd, ev::KEY_INFO),
    MapItem::key(CONSUMER, 0x1cb, ev::KEY_ASSISTANT),
    MapItem::key(CONSUMER, 0x201, ev::KEY_NEW),
    MapItem::key(CONSUMER, 0x202, ev::KEY_OPEN),
    MapItem::key(CONSUMER, 0x203, ev::KEY_CLOSE),
    MapItem::key(CONSUMER, 0x204, ev::KEY_EXIT),
    MapItem::key(CONSUMER, 0x207, ev::KEY_SAVE),
    MapItem::key(CONSUMER, 0x208, ev::KEY_PRINT),
    MapItem::key(CONSUMER, 0x209, ev::KEY_PROPS),
    MapItem::key(CONSUMER, 0x21a, ev::KEY_UNDO),
    MapItem::key(CONSUMER, 0x21b, ev::KEY_COPY),
    MapItem::key(CONSUMER, 0x21c, ev::KEY_CUT),
    MapItem::key(CONSUMER, 0x21d, ev::KEY_PASTE),
    MapItem::key(CONSUMER, 0x21f, ev::KEY_FIND),
    MapItem::key(CONSUMER, 0x221, ev::KEY_SEARCH),
    MapItem::key(CONSUMER, 0x222, ev::KEY_GOTO),
    MapItem::key(CONSUMER, 0x223, ev::KEY_HOMEPAGE),
    MapItem::key(CONSUMER, 0x224, ev::KEY_BACK),
    MapItem::key(CONSUMER, 0x225, ev::KEY_FORWARD),
    MapItem::key(CONSUMER, 0x226, ev::KEY_STOP),
    MapItem::key(CONSUMER, 0x227, ev::KEY_REFRESH),
    MapItem::key(CONSUMER, 0x22a, ev::KEY_BOOKMARKS),
    MapItem::key(CONSUMER, 0x22d, ev::KEY_ZOOMIN),
    MapItem::key(CONSUMER, 0x22e, ev::KEY_ZOOMOUT),
    MapItem::key(CONSUMER, 0x22f, ev::KEY_ZOOMRESET),
    MapItem::key(CONSUMER, 0x232, ev::KEY_FULL_SCREEN),
    MapItem::key(CONSUMER, 0x233, ev::KEY_SCROLLUP),
    MapItem::key(CONSUMER, 0x234, ev::KEY_SCROLLDOWN),
    MapItem::rel(CONSUMER, 0x238, ev::REL_HWHEEL), // AC Pan
    MapItem::key(CONSUMER, 0x23d, ev::KEY_EDIT),
    MapItem::key(CONSUMER, 0x25f, ev::KEY_CANCEL),
    MapItem::key(CONSUMER, 0x269, ev::KEY_INSERT),
    MapItem::key(CONSUMER, 0x26a, ev::KEY_DELETE),
    MapItem::key(CONSUMER, 0x279, ev::KEY_REDO),
    MapItem::key(CONSUMER, 0x289, ev::KEY_REPLY),
    MapItem::key(CONSUMER, 0x28b, ev::KEY_FORWARDMAIL),
    MapItem::key(CONSUMER, 0x28c, ev::KEY_SEND),
    MapItem::key(CONSUMER, 0x29d, ev::KEY_KBD_LAYOUT_NEXT),
    MapItem::key(CONSUMER, 0x2c7, ev::KEY_KBDINPUTASSIST_PREV),
    MapItem::key(CONSUMER, 0x2c8, ev::KEY_KBDINPUTASSIST_NEXT),
    MapItem::key(CONSUMER, 0x2c9, ev::KEY_KBDINPUTASSIST_PREVGROUP),
    MapItem::key(CONSUMER, 0x2ca, ev::KEY_KBDINPUTASSIST_NEXTGROUP),
    MapItem::key(CONSUMER, 0x2cb, ev::KEY_KBDINPUTASSIST_ACCEPT),
    MapItem::key(CONSUMER, 0x2cc, ev::KEY_KBDINPUTASSIST_CANCEL),
    MapItem::key(CONSUMER, 0x29f, ev::KEY_SCALE),
];

/// The `hcons` driver.
pub struct Hcons {
    map: Mapper,
    unit: u32,
    desc: String,
}

impl core::fmt::Debug for Hcons {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hcons").field("unit", &self.unit).finish()
    }
}

impl Driver for Hcons {
    const NAME: &'static str = "hcons";
    const IDS: Ids = Ids::Hid(&[HidId::usage(CONSUMER_CONTROL)]);

    fn describe(ident: &Ident) -> String {
        hidbus::hidmap::describe(ident, None, SUFFIX)
    }

    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        hidbus::hidmap::describe(ident, bus_data, SUFFIX)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let (map, _) = Mapper::probe(cx, &MAP)?;
        let child = map.child();
        let desc = Self::describe_with(cx.ident(), Some(&**child as &(dyn Any + Send + Sync)));
        Ok(Self {
            map,
            unit: cx.unit(),
            desc,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.map
            .attach(cx.sleepable(), Self::NAME, self.unit, &self.desc, |_| {})?;
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        self.map.detach(cx.sleepable());
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Hcons>();

#[cfg(test)]
mod tests;
