//! Linux capture: find keyboards, mice and touchpads, read them on one
//! thread each and turn raw evdev events into `InputEvent`s on one channel.

use crate::input::event::InputEvent;
use crate::platform::linux::{Device, RawEvent};
use crate::threads::LockAnyway;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const REL_HWHEEL: u16 = 6;
const REL_WHEEL: u16 = 8;
const REL_WHEEL_HI_RES: u16 = 11;
const REL_HWHEEL_HI_RES: u16 = 12;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const KEY_A: u16 = 30;
const KEY_Z: u16 = 44;
const BTN_LEFT: u16 = 0x110;
/// Exactly one finger is on the touchpad.
const BTN_TOOL_FINGER: u16 = 0x145;
/// Mouse buttons are 0x110 (left) to 0x117; everything below 0x100 is a key.
const MOUSE_BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
/// One wheel notch in hi-res units.
const NOTCH: i32 = 120;
/// Used when a touchpad does not report its resolution.
const FALLBACK_UNITS_PER_MM: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyboard,
    Mouse,
    Touchpad,
}

pub struct Found {
    pub device: Arc<Device>,
    pub name: String,
    pub kind: Kind,
    /// Its node under `/dev/input`, to tell it from one plugged in later.
    pub path: PathBuf,
    /// Cleared by its reader thread when the device fails (unplugged).
    pub alive: Arc<AtomicBool>,
    /// A text expander's own virtual keyboard (espanso's): what it types is
    /// not repeated on the typing mirror, or the expander would read itself.
    expander: bool,
    hi_res_wheel: bool,
    /// Touchpad units per millimetre on X and Y.
    resolution: [i32; 2],
}

/// How often `watch` looks for devices plugged in after Mickey started.
const WATCH_EVERY: Duration = Duration::from_secs(2);

/// When a text expander's keyboard last typed, in milliseconds since the
/// first call here; 0 means never.
static EXPANDER_TYPED: AtomicU64 = AtomicU64::new(0);

fn clock_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    // +1: 0 stays "never".
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Whether a text expander typed during the last `window`.
pub fn expander_typed_within(window: Duration) -> bool {
    match EXPANDER_TYPED.load(Ordering::Relaxed) {
        0 => false,
        at => clock_ms().saturating_sub(at) <= window.as_millis() as u64,
    }
}

/// Opens every keyboard, mouse and touchpad under `/dev/input` (the kernel's
/// fixed location for evdev nodes). Other devices, like the power button,
/// are skipped.
///
/// A device this user may not open is skipped, not fatal: one appearing
/// right now is root's until udev hands it to the `input` group, and
/// `mickey run` creates its own virtual device just as it starts. Only if
/// every device is refused is that the answer.
pub fn find_devices() -> io::Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut refused = None;
    for entry in std::fs::read_dir("/dev/input")? {
        let path = entry?.path();
        let is_event_node = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("event"));
        if !is_event_node {
            continue;
        }
        match classify(&path) {
            Ok(Some(device)) => found.push(device),
            Ok(None) => {}
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => refused = Some(err),
            Err(err) => return Err(err),
        }
    }
    match refused {
        Some(err) if found.is_empty() => Err(err),
        _ => Ok(found),
    }
}

fn classify(path: &Path) -> io::Result<Option<Found>> {
    let device = Device::open(path)?;
    // Our own uinput device (a target in the same `mickey run`) carries
    // input that came from another PC; capturing it would send it back.
    // The typing mirror repeats what was already captured.
    let name = device.name()?;
    if name == crate::platform::uinput::DEVICE_NAME || name == crate::platform::uinput::MIRROR_NAME
    {
        return Ok(None);
    }
    let kind = if device.supports(EV_KEY, KEY_A)? && device.supports(EV_KEY, KEY_Z)? {
        Kind::Keyboard
    } else if device.supports(EV_REL, REL_X)?
        && device.supports(EV_REL, REL_Y)?
        && device.supports(EV_KEY, BTN_LEFT)?
    {
        Kind::Mouse
    } else if device.supports(EV_ABS, ABS_X)?
        && device.supports(EV_ABS, ABS_Y)?
        && device.supports(EV_KEY, BTN_TOOL_FINGER)?
    {
        Kind::Touchpad
    } else {
        return Ok(None);
    };
    let mut resolution = [0; 2];
    if kind == Kind::Touchpad {
        for (axis, value) in [ABS_X, ABS_Y].into_iter().zip(&mut resolution) {
            *value = match device.abs_resolution(axis)? {
                0 => FALLBACK_UNITS_PER_MM,
                units => units,
            };
        }
    }
    Ok(Some(Found {
        expander: name.to_lowercase().contains("espanso"),
        name,
        path: path.to_path_buf(),
        alive: Arc::new(AtomicBool::new(true)),
        hi_res_wheel: device.supports(EV_REL, REL_WHEEL_HI_RES)?,
        kind,
        resolution,
        device: Arc::new(device),
    }))
}

/// Starts one reader thread per device. A thread ends when its device fails
/// (e.g. it was unplugged) or the receiver is gone.
/// `touchpad_speed` is in mouse counts per millimetre (`config.toml`).
/// Events go into `events` as any type that can be made from an
/// `InputEvent`, so the source can share one channel with other wake-ups.
pub fn spawn_readers<T: From<InputEvent> + Send + 'static>(
    devices: &[Found],
    events: Sender<T>,
    touchpad_speed: i32,
) {
    for found in devices {
        spawn_reader(found, events.clone(), touchpad_speed);
    }
}

fn spawn_reader<T: From<InputEvent> + Send + 'static>(
    found: &Found,
    events: Sender<T>,
    touchpad_speed: i32,
) {
    let device = Arc::clone(&found.device);
    let alive = Arc::clone(&found.alive);
    let expander = found.expander;
    let touchpad =
        (found.kind == Kind::Touchpad).then(|| Touchpad::new(found.resolution, touchpad_speed));
    let mut translator = Translator::new(found.hi_res_wheel, touchpad);
    crate::threads::spawn("capture", move || {
        let mut batch = Vec::with_capacity(64);
        while device.read_events(&mut batch).is_ok() {
            if expander {
                EXPANDER_TYPED.store(clock_ms(), Ordering::Relaxed);
            }
            let mut closed = false;
            for &raw in &batch {
                translator.feed(raw, |event| closed |= events.send(T::from(event)).is_err());
            }
            if closed {
                break;
            }
        }
        alive.store(false, Ordering::Relaxed);
    });
}

/// Keeps `devices` true to what is plugged in: every `WATCH_EVERY`, a
/// device whose reader ended (unplugged) leaves the list, and a keyboard,
/// mouse or touchpad that appeared under `/dev/input` joins it with a
/// reader of its own. Without this, a keyboard that drops off a USB hub and
/// comes back, or a text expander that starts again with a new virtual
/// keyboard, would be lost until Mickey restarts.
///
/// `grabbed` says whether the devices are taken from the desktop right
/// now: one that joins meanwhile is taken too. It is read under the list's
/// lock, which `Input::grab` holds while it grabs, so no device slips
/// between the two.
pub fn watch<T: From<InputEvent> + Send + 'static>(
    devices: Arc<Mutex<Vec<Found>>>,
    grabbed: Arc<AtomicBool>,
    events: Sender<T>,
    touchpad_speed: i32,
) {
    crate::threads::spawn("device watch", move || {
        // Nodes that are neither keyboard, mouse nor touchpad: not asked again.
        let mut other: HashSet<PathBuf> = HashSet::new();
        loop {
            std::thread::sleep(WATCH_EVERY);
            let Ok(entries) = std::fs::read_dir("/dev/input") else {
                continue;
            };
            let present: HashSet<PathBuf> = entries
                .filter_map(|entry| Some(entry.ok()?.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("event"))
                })
                .collect();
            other.retain(|path| present.contains(path));
            let mut list = devices.lock_anyway();
            list.retain(|found| {
                let stays = found.alive.load(Ordering::Relaxed) && present.contains(&found.path);
                if !stays {
                    println!("mickey: {} is gone", found.name);
                }
                stays
            });
            for path in &present {
                if other.contains(path) || list.iter().any(|found| &found.path == path) {
                    continue;
                }
                match classify(path) {
                    Ok(Some(found)) => {
                        if grabbed.load(Ordering::Relaxed)
                            && let Err(err) = found.device.set_grab(true)
                        {
                            eprintln!("mickey: cannot grab {}: {err}", found.name);
                        }
                        println!("mickey: found {} (plugged in)", found.name);
                        spawn_reader(&found, events.clone(), touchpad_speed);
                        list.push(found);
                    }
                    Ok(None) => {
                        other.insert(path.clone());
                    }
                    // Not ours to open yet (udev is still handing it to the
                    // input group), or already gone: asked again next time.
                    Err(_) => {}
                }
            }
        }
    });
}

/// Raw evdev events to `InputEvent`s, for one device. Motion and wheel
/// arrive as separate axis events and are complete only at `SYN_REPORT`,
/// so they are summed until then.
struct Translator {
    hi_res_wheel: bool,
    touchpad: Option<Touchpad>,
    dx: i32,
    dy: i32,
    vertical: i32,
    horizontal: i32,
}

impl Translator {
    fn new(hi_res_wheel: bool, touchpad: Option<Touchpad>) -> Translator {
        Translator {
            hi_res_wheel,
            touchpad,
            dx: 0,
            dy: 0,
            vertical: 0,
            horizontal: 0,
        }
    }

    fn feed(&mut self, raw: RawEvent, mut out: impl FnMut(InputEvent)) {
        if let Some(touchpad) = &mut self.touchpad {
            if touchpad.feed(raw) {
                return;
            }
            if (raw.kind, raw.code) == (EV_SYN, SYN_REPORT) {
                let (dx, dy) = touchpad.motion();
                self.dx = self.dx.saturating_add(dx);
                self.dy = self.dy.saturating_add(dy);
            }
        }
        // value: 0 = released, 1 = pressed, 2 = auto-repeat (sent as a press,
        // because Windows does not repeat injected keys by itself).
        let pressed = raw.value != 0;
        let value = raw.value;
        match (raw.kind, raw.code) {
            (EV_KEY, code) if code < 0x100 => out(InputEvent::Key { code, pressed }),
            (EV_KEY, code) if MOUSE_BUTTONS.contains(&code) => {
                out(InputEvent::Button { code, pressed })
            }
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(value),
            // A hi-res wheel sends both kinds; count only one of them.
            (EV_REL, REL_WHEEL_HI_RES) => self.vertical = self.vertical.saturating_add(value),
            (EV_REL, REL_HWHEEL_HI_RES) => self.horizontal = self.horizontal.saturating_add(value),
            (EV_REL, REL_WHEEL) if !self.hi_res_wheel => {
                self.vertical = self.vertical.saturating_add(value.saturating_mul(NOTCH))
            }
            (EV_REL, REL_HWHEEL) if !self.hi_res_wheel => {
                self.horizontal = self.horizontal.saturating_add(value.saturating_mul(NOTCH))
            }
            (EV_SYN, SYN_REPORT) => self.flush(out),
            _ => {}
        }
    }

    fn flush(&mut self, mut out: impl FnMut(InputEvent)) {
        if self.dx != 0 || self.dy != 0 {
            out(InputEvent::Motion {
                dx: self.dx,
                dy: self.dy,
            });
        }
        if self.vertical != 0 || self.horizontal != 0 {
            out(InputEvent::Scroll {
                vertical: self.vertical,
                horizontal: self.horizontal,
            });
        }
        (self.dx, self.dy, self.vertical, self.horizontal) = (0, 0, 0, 0);
    }
}

/// One finger moving on a touchpad becomes relative motion. A touchpad
/// reports where the finger is, not how far it moved, so motion is the
/// difference between two reports, scaled from millimetres to mouse units.
struct Touchpad {
    /// Device units per millimetre on X and Y.
    resolution: [i32; 2],
    /// Mouse counts per millimetre of finger travel.
    speed: i32,
    one_finger: bool,
    position: [i32; 2],
    /// Position at the previous report; `None` right after a finger lands,
    /// so putting it down elsewhere never makes the cursor jump.
    last: Option<[i32; 2]>,
    /// What the integer division left over, so slow motion is not lost.
    remainder: [i32; 2],
}

impl Touchpad {
    fn new(resolution: [i32; 2], speed: i32) -> Touchpad {
        Touchpad {
            resolution,
            speed,
            one_finger: false,
            position: [0; 2],
            last: None,
            remainder: [0; 2],
        }
    }

    /// Takes the events only a touchpad sends; `false` for anything else.
    fn feed(&mut self, raw: RawEvent) -> bool {
        match (raw.kind, raw.code) {
            (EV_ABS, ABS_X) => self.position[0] = raw.value,
            (EV_ABS, ABS_Y) => self.position[1] = raw.value,
            (EV_KEY, BTN_TOOL_FINGER) => self.one_finger = raw.value != 0,
            _ => return false,
        }
        true
    }

    /// At `SYN_REPORT`: the motion since the previous report.
    fn motion(&mut self) -> (i32, i32) {
        if !self.one_finger {
            self.last = None;
            self.remainder = [0; 2];
            return (0, 0);
        }
        let Some(last) = self.last.replace(self.position) else {
            return (0, 0);
        };
        let mut moved = [0; 2];
        for axis in 0..2 {
            let scaled = (self.position[axis] - last[axis]) * self.speed + self.remainder[axis];
            moved[axis] = scaled / self.resolution[axis];
            self.remainder[axis] = scaled % self.resolution[axis];
        }
        (moved[0], moved[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(hi_res_wheel: bool, raw: &[(u16, u16, i32)]) -> Vec<InputEvent> {
        let mut translator = Translator::new(hi_res_wheel, None);
        let mut out = Vec::new();
        for &(kind, code, value) in raw {
            translator.feed(RawEvent { kind, code, value }, |event| out.push(event));
        }
        out
    }

    const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

    #[test]
    fn keys_pass_through_and_repeats_are_presses() {
        let out = run(
            false,
            &[(EV_KEY, KEY_A, 1), (EV_KEY, KEY_A, 2), (EV_KEY, KEY_A, 0)],
        );
        assert_eq!(
            out,
            [true, true, false].map(|pressed| InputEvent::Key {
                code: KEY_A,
                pressed
            })
        );
    }

    #[test]
    fn motion_is_summed_until_syn_report() {
        let out = run(
            false,
            &[
                (EV_REL, REL_X, 3),
                (EV_REL, REL_Y, -2),
                (EV_REL, REL_X, 1),
                SYN,
                SYN,
            ],
        );
        assert_eq!(out, [InputEvent::Motion { dx: 4, dy: -2 }]);
    }

    #[test]
    fn hi_res_wheels_are_not_counted_twice() {
        let hi_res = run(
            true,
            &[(EV_REL, REL_WHEEL, 1), (EV_REL, REL_WHEEL_HI_RES, 120), SYN],
        );
        assert_eq!(
            hi_res,
            [InputEvent::Scroll {
                vertical: 120,
                horizontal: 0
            }]
        );
        let classic = run(false, &[(EV_REL, REL_HWHEEL, -1), SYN]);
        assert_eq!(
            classic,
            [InputEvent::Scroll {
                vertical: 0,
                horizontal: -120
            }]
        );
    }

    #[test]
    fn buttons_are_kept_and_other_codes_dropped() {
        let out = run(
            false,
            &[(EV_KEY, BTN_LEFT, 1), (EV_KEY, 0x14a, 1), (4, 4, 30)],
        );
        assert_eq!(
            out,
            [InputEvent::Button {
                code: BTN_LEFT,
                pressed: true
            }]
        );
    }

    const SPEED: i32 = 16;

    /// A touchpad with 24 units per millimetre, like the owner's.
    fn touchpad(raw: &[(u16, u16, i32)]) -> Vec<InputEvent> {
        let mut translator = Translator::new(false, Some(Touchpad::new([24, 24], SPEED)));
        let mut out = Vec::new();
        for &(kind, code, value) in raw {
            translator.feed(RawEvent { kind, code, value }, |event| out.push(event));
        }
        out
    }

    const FINGER_DOWN: (u16, u16, i32) = (EV_KEY, BTN_TOOL_FINGER, 1);
    const FINGER_UP: (u16, u16, i32) = (EV_KEY, BTN_TOOL_FINGER, 0);

    #[test]
    fn one_finger_moves_the_cursor_without_a_first_jump() {
        let out = touchpad(&[
            FINGER_DOWN,
            (EV_ABS, ABS_X, 1000),
            (EV_ABS, ABS_Y, 500),
            SYN,                   // landing: no motion yet
            (EV_ABS, ABS_X, 1024), // 1 mm right
            SYN,
        ]);
        assert_eq!(out, [InputEvent::Motion { dx: SPEED, dy: 0 }]);
    }

    #[test]
    fn lifting_and_landing_elsewhere_does_not_jump() {
        let out = touchpad(&[
            FINGER_DOWN,
            (EV_ABS, ABS_X, 100),
            SYN,
            FINGER_UP,
            SYN,
            FINGER_DOWN,
            (EV_ABS, ABS_X, 2000),
            SYN,
        ]);
        assert_eq!(out, []);
    }

    #[test]
    fn slow_motion_adds_up_through_the_remainder() {
        // 1 unit per report is 16/24 of a count: 0, then 1, then 1.
        let mut raw = vec![FINGER_DOWN, (EV_ABS, ABS_X, 0), SYN];
        for x in 1..=3 {
            raw.extend([(EV_ABS, ABS_X, x), SYN]);
        }
        let total: i32 = touchpad(&raw)
            .iter()
            .map(|event| match event {
                InputEvent::Motion { dx, .. } => *dx,
                _ => 0,
            })
            .sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn two_fingers_do_not_move_and_the_click_is_a_button() {
        let out = touchpad(&[
            FINGER_DOWN,
            SYN,
            FINGER_UP, // a second finger landed (BTN_TOOL_DOUBLETAP)
            (EV_ABS, ABS_X, 900),
            SYN,
            (EV_KEY, BTN_LEFT, 1),
            SYN,
        ]);
        assert_eq!(
            out,
            [InputEvent::Button {
                code: BTN_LEFT,
                pressed: true
            }]
        );
    }
}
