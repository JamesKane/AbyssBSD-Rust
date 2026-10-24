// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/sysmouse`, as `vt` has it: the system's one mouse, which is
//! whatever `moused` reports to the console (`CONS_MOUSECTL` on
//! `/dev/consolectl`, which calls [`process_event`]), served to programs in the MouseSystems
//! protocol (level 0, five-byte packets) or sysmouse's own (level 1,
//! eight bytes, with the wheel and buttons 4 to 10). Its evdev device,
//! "System mouse", reports the same events when `kern.evdev.rcpt_mask`
//! has `SYSMOUSE`, and while a client has grabbed it they go only there.
//!
//! The device's packet buffer exists while it is open: the first open
//! makes it and the last close drops it, with the level back to 0.
//! Opens share it, so each packet goes to one reader.
//!
//! Not yet: `poll` (devfs has no poll for devices), `SIGIO` for
//! `FIOASYNC`, and the `vt` mouse cursor, which is cut and paste's.
//!
//! Behavior reference: sys/dev/vt/vt_sysmouse.c, vt_consolectl.c.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use abi::consio::{
    CONS_GETVERS, CONS_MOUSECTL, MOUSE_ACTION, MOUSE_BUTTON_EVENT, MOUSE_MOTION_EVENT, MouseInfo,
};
use abi::errno::Errno;
use abi::evdev::{
    BTN_MOUSE, BUS_VIRTUAL, EV_KEY, EV_REL, INPUT_PROP_POINTER, InputId, REL_HWHEEL, REL_WHEEL,
    REL_X, REL_Y,
};
use abi::ioctl::{FIOGETOWN, FIOSETOWN};
use abi::mouse::*;
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use vfs::{CharDevice, Uio};

/// `SYSMOUSE_MAXFRAMES`.
const FRAMES: usize = 250;

type Packet = [u8; MOUSE_SYS_PACKETSIZE];

struct State {
    level: i32,
    status: MouseStatus,
    /// The packets not yet read, while the device is open.
    buf: Option<VecDeque<Packet>>,
    opens: usize,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    level: 0,
    status: MouseStatus {
        flags: 0,
        button: 0,
        obutton: 0,
        dx: 0,
        dy: 0,
        dz: 0,
    },
    buf: None,
    opens: 0,
});
static WAIT: WaitQueue = WaitQueue::new();
static EVDEV: Once<Option<Arc<evdev::Device>>> = Once::new();

/// Makes `/dev/sysmouse` and its evdev device (`sysmouse_drvinit`), and
/// `/dev/consolectl` (`consolectl_drvinit`).
pub fn init(s: &Sleepable) {
    let mut b = evdev::Builder::new("System mouse");
    b.phys("sysmouse").id(InputId {
        bustype: BUS_VIRTUAL,
        ..InputId::default()
    });
    b.prop(INPUT_PROP_POINTER).event(EV_REL).event(EV_KEY);
    b.rel(REL_X).rel(REL_Y).rel(REL_WHEEL).rel(REL_HWHEEL);
    for i in 0..8 {
        b.key(BTN_MOUSE + i);
    }
    let e = b.register(s).ok();
    let _ = EVDEV.set(e);
    let _ = vfs::devices::make_dev(b"sysmouse", 0o600, 0, 0, Arc::new(Node));
    let _ = vfs::devices::make_dev(b"consolectl", 0o600, 0, 0, Arc::new(ConsoleCtl));
}

fn evdev() -> Option<&'static Arc<evdev::Device>> {
    EVDEV.get().and_then(Option::as_ref)
}

/// `evdev_mouse_button_codes`: the buttons by bit.
const BUTTONS: [u16; 8] = [0x110, 0x112, 0x111, 0x113, 0x114, 0x115, 0x116, 0x117];

/// `sysmouse_evdev_store`, with `kern.evdev.sysmouse_t_axis` 0 (the wheel
/// only).
fn evdev_store(x: i32, y: i32, z: i32, buttons: i32) {
    let Some(e) = evdev().filter(|_| evdev::receives(evdev::RCPT_SYSMOUSE)) else {
        return;
    };
    let _ = e.push(EV_REL, REL_X, x);
    let _ = e.push(EV_REL, REL_Y, y);
    let _ = e.push(EV_REL, REL_WHEEL, z.wrapping_neg());
    for (i, &code) in BUTTONS.iter().enumerate() {
        e.push_key(code, buttons & (1 << i) != 0);
    }
    e.sync();
}

/// The low byte of `v`, as C's `unsigned char` takes it.
fn byte(v: i32) -> u8 {
    v.to_le_bytes()[0]
}

/// The packet for motion `x`, `y`, `z` with `buttons` down.
fn packet(x: i32, y: i32, z: i32, buttons: i32) -> Packet {
    // `MOUSE_BUTTON?DOWN` to `MOUSE_MSC_BUTTON?UP`.
    const BUTTONMAP: [u8; 8] = [
        MOUSE_MSC_BUTTON1UP | MOUSE_MSC_BUTTON2UP | MOUSE_MSC_BUTTON3UP,
        MOUSE_MSC_BUTTON2UP | MOUSE_MSC_BUTTON3UP,
        MOUSE_MSC_BUTTON1UP | MOUSE_MSC_BUTTON3UP,
        MOUSE_MSC_BUTTON3UP,
        MOUSE_MSC_BUTTON1UP | MOUSE_MSC_BUTTON2UP,
        MOUSE_MSC_BUTTON2UP,
        MOUSE_MSC_BUTTON1UP,
        0,
    ];
    let mut b = [0u8; MOUSE_SYS_PACKETSIZE];
    // The first five bytes are MouseSystems': each axis as two halves,
    // y upward.
    let std = usize::try_from(buttons & MOUSE_STDBUTTONS).unwrap_or(0);
    b[0] = MOUSE_MSC_SYNC | BUTTONMAP[std];
    let x = x.clamp(-256, 255);
    b[1] = byte(x >> 1);
    b[3] = byte(x - (x >> 1));
    let iy = -y.clamp(-256, 255);
    b[2] = byte(iy >> 1);
    b[4] = byte(iy - (iy >> 1));
    // The wheel, then buttons 4 to 10, active low.
    let z = z.clamp(-128, 127);
    b[5] = byte(z >> 1) & 0x7f;
    b[6] = byte(z - (z >> 1)) & 0x7f;
    b[7] = byte(!buttons >> 3) & 0x7f;
    b
}

/// What `moused` reports through `CONS_MOUSECTL`
/// (`sysmouse_process_event`): `MOUSE_ACTION` (motion and every
/// button), `MOUSE_MOTION_EVENT` (motion), or `MOUSE_BUTTON_EVENT` (one
/// button).
pub fn process_event(mi: &MouseInfo) {
    let mut st = STATE.lock();
    let (x, y, z) = match mi.operation {
        MOUSE_ACTION | MOUSE_MOTION_EVENT => {
            let d = mi.data();
            if mi.operation == MOUSE_ACTION {
                st.status.button = d.buttons;
            }
            (d.x, d.y, d.z)
        }
        MOUSE_BUTTON_EVENT => {
            let (id, value) = mi.event();
            if value > 0 {
                st.status.button |= id;
            } else {
                st.status.button &= !id;
            }
            (0, 0, 0)
        }
        _ => return,
    };
    let s = &mut st.status;
    s.dx = s.dx.wrapping_add(x);
    s.dy = s.dy.wrapping_add(y);
    s.dz = s.dz.wrapping_add(z);
    let moved = if x != 0 || y != 0 || z != 0 {
        MOUSE_POSCHANGED
    } else {
        0
    };
    s.flags |= moved | (s.obutton ^ s.button);
    if s.flags == 0 {
        return;
    }
    let buttons = s.button;
    evdev_store(x, y, z, buttons);
    if evdev().is_some_and(|e| e.is_grabbed()) {
        return;
    }
    if let Some(q) = st.buf.as_mut()
        && q.len() < FRAMES
    {
        q.push_back(packet(x, y, z, buttons));
        drop(st);
        WAIT.wake_all();
    }
}

/// `/dev/sysmouse`.
struct Node;

impl CharDevice for Node {
    fn instance(&self, _: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let mut st = STATE.lock();
        if st.opens == 0 {
            st.buf = Some(VecDeque::with_capacity(FRAMES));
            st.level = 0;
        }
        st.opens += 1;
        Ok(Some(Arc::new(Open)))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }
}

/// An open of `/dev/sysmouse`.
struct Open;

impl Drop for Open {
    fn drop(&mut self) {
        let mut st = STATE.lock();
        st.opens -= 1;
        if st.opens == 0 {
            st.buf = None;
            st.level = 0;
        }
    }
}

impl CharDevice for Open {
    /// Whole packets of the level's size, as many as fit; waits for the
    /// first unless `nonblock`.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let mut done = 0;
        loop {
            let (p, len) = {
                let mut st = STATE.lock();
                let len = if st.level >= 1 {
                    MOUSE_SYS_PACKETSIZE
                } else {
                    MOUSE_MSC_PACKETSIZE
                };
                if uio.resid() < len {
                    return Ok(done);
                }
                (st.buf.as_mut().ok_or(Errno::ENXIO)?.pop_front(), len)
            };
            if let Some(p) = p {
                done += uio.copy_out(&p[..len])?;
                continue;
            }
            if done > 0 {
                return Ok(done);
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            WAIT.wait_until_interruptible(s, || {
                STATE.lock().buf.as_ref().is_none_or(|q| !q.is_empty())
            })
            .map_err(|_| Errno::EINTR)?;
        }
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        ioctl(req, arg)
    }
}

/// `/dev/consolectl`: the console's controls that belong to no terminal
/// (`consolectl_ioctl`).
struct ConsoleCtl;

impl CharDevice for ConsoleCtl {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            CONS_GETVERS => put(arg, &0x200i32.to_ne_bytes()),
            CONS_MOUSECTL => process_event(&MouseInfo::from_bytes(arg).ok_or(Errno::EINVAL)?),
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

fn put(arg: &mut [u8], b: &[u8]) {
    let n = arg.len().min(b.len());
    arg[..n].copy_from_slice(&b[..n]);
}

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

/// `sysmouse_ioctl`. `FIONBIO` and `FIOASYNC` are the file's.
fn ioctl(req: u32, arg: &mut [u8]) -> Result<(), Errno> {
    match req {
        FIOSETOWN => {}
        FIOGETOWN => put(arg, &0i32.to_ne_bytes()),
        MOUSE_GETHWINFO => {
            let hw = MouseHw {
                buttons: 10,
                iftype: MOUSE_IF_SYSMOUSE,
                r#type: MOUSE_MOUSE,
                model: MOUSE_MODEL_GENERIC,
                hwid: 0,
            };
            put(arg, &hw.to_bytes());
        }
        MOUSE_GETLEVEL => put(arg, &STATE.lock().level.to_ne_bytes()),
        MOUSE_GETMODE => {
            let level = STATE.lock().level;
            let (protocol, packetsize, syncmask) = if level == 0 {
                (
                    MOUSE_PROTO_MSC,
                    MOUSE_MSC_PACKETSIZE,
                    [MOUSE_MSC_SYNCMASK, MOUSE_MSC_SYNC],
                )
            } else {
                (
                    MOUSE_PROTO_SYSMOUSE,
                    MOUSE_SYS_PACKETSIZE,
                    [MOUSE_SYS_SYNCMASK, MOUSE_SYS_SYNC],
                )
            };
            let mode = MouseMode {
                protocol,
                rate: -1,
                resolution: -1,
                accelfactor: 0,
                level,
                packetsize: i32::try_from(packetsize).unwrap_or(0),
                syncmask,
            };
            put(arg, &mode.to_bytes());
        }
        MOUSE_GETSTATUS => {
            let mut st = STATE.lock();
            put(arg, &st.status.to_bytes());
            let s = &mut st.status;
            s.flags = 0;
            s.obutton = s.button;
            s.dx = 0;
            s.dy = 0;
            s.dz = 0;
        }
        MOUSE_SETLEVEL => match int(arg) {
            level @ (0 | 1) => STATE.lock().level = level,
            _ => return Err(Errno::EINVAL),
        },
        MOUSE_SETMODE => {
            let mode = MouseMode::from_bytes(arg).ok_or(Errno::EINVAL)?;
            match mode.level {
                -1 => {}
                level @ (0 | 1) => STATE.lock().level = level,
                _ => return Err(Errno::EINVAL),
            }
        }
        _ => return Err(Errno::ENOTTY),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_split_each_axis_in_halves() {
        // Left down, right 3, up 5 (y is negative upward to moused), the
        // wheel 1 toward the user.
        let p = packet(3, -5, 1, MOUSE_BUTTON1DOWN);
        assert_eq!(p[0], 0x80 | MOUSE_MSC_BUTTON2UP | MOUSE_MSC_BUTTON3UP);
        assert_eq!((p[1], p[3]), (1, 2));
        assert_eq!((p[2], p[4]), (2, 3));
        assert_eq!((p[5], p[6]), (0, 1));
        assert_eq!(p[7], 0x7f, "buttons 4-10 up");
        // Negative motion, clamped to MouseSystems' range.
        let p = packet(-3, 1000, -1, 1 << 3);
        assert_eq!(p[0], 0x80 | 0x07);
        assert_eq!((p[1].cast_signed(), p[3].cast_signed()), (-2, -1));
        assert_eq!((p[2].cast_signed(), p[4].cast_signed()), (-128, -127));
        assert_eq!((p[5], p[6]), (0x7f, 0));
        assert_eq!(p[7], 0x7e, "button 4 down");
    }

    #[test]
    fn levels_and_status() {
        let info = |operation, u| MouseInfo { operation, u };
        let mut arg = [0u8; 4];
        arg.copy_from_slice(&1i32.to_ne_bytes());
        ioctl(MOUSE_SETLEVEL, &mut arg).unwrap();
        let mut m = [0u8; MouseMode::SIZE];
        ioctl(MOUSE_GETMODE, &mut m).unwrap();
        let m = MouseMode::from_bytes(&m).unwrap();
        assert_eq!((m.protocol, m.packetsize), (MOUSE_PROTO_SYSMOUSE, 8));
        arg.copy_from_slice(&2i32.to_ne_bytes());
        assert_eq!(ioctl(MOUSE_SETLEVEL, &mut arg), Err(Errno::EINVAL));

        // Motion and buttons accumulate until `MOUSE_GETSTATUS`.
        process_event(&info(MOUSE_ACTION, [4, -2, 0, MOUSE_BUTTON3DOWN]));
        process_event(&info(MOUSE_MOTION_EVENT, [1, 1, 0, 0]));
        process_event(&info(MOUSE_BUTTON_EVENT, [MOUSE_BUTTON1DOWN, 1, 0, 0]));
        let mut s = [0u8; MouseStatus::SIZE];
        ioctl(MOUSE_GETSTATUS, &mut s).unwrap();
        let ints: [i32; 6] = core::array::from_fn(|i| int(&s[i * 4..]));
        let both = MOUSE_BUTTON1DOWN | MOUSE_BUTTON3DOWN;
        assert_eq!(ints, [MOUSE_POSCHANGED | both, both, 0, 5, -1, 0]);
        ioctl(MOUSE_GETSTATUS, &mut s).unwrap();
        let ints: [i32; 6] = core::array::from_fn(|i| int(&s[i * 4..]));
        assert_eq!(ints, [0, both, both, 0, 0, 0]);
    }
}
