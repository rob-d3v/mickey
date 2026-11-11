//! The source's keyboard and mouse, per platform (PRD 0008): evdev devices
//! on Linux, low-level hooks on Windows. `mickey source` only uses this
//! interface, so the focus, hotkey and switcher work the same on both.

#[cfg(target_os = "linux")]
pub mod capture;
pub mod emit;
pub mod event;
pub mod held;
pub mod hotkey;
#[cfg_attr(not(windows), allow(dead_code))]
pub mod keymap;
pub mod record;

use crate::input::event::InputEvent;
#[cfg(windows)]
use crate::threads::LockAnyway;
use std::sync::mpsc::Sender;

#[cfg(target_os = "linux")]
pub use linux::Input;
#[cfg(target_os = "linux")]
pub use linux::typing_mirror;
#[cfg(target_os = "linux")]
pub use linux::cursor_now;
#[cfg(windows)]
pub use windows::Input;
#[cfg(windows)]
pub use windows::cursor_now;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::input::capture;
    use crate::platform::uinput::{EV_KEY, MIRROR_NAME, VirtualDevice};
    use crate::threads::LockAnyway;
    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub struct Input {
        /// Shared with `capture::watch`, which adds the devices plugged in
        /// later and drops the ones unplugged.
        devices: Arc<Mutex<Vec<capture::Found>>>,
        grabbed: Arc<AtomicBool>,
    }

    /// While this lives, the devices are taken from the desktop. Dropping
    /// it gives them back, including during a panic.
    pub struct Grabbed<'a> {
        devices: Arc<Mutex<Vec<capture::Found>>>,
        grabbed: Arc<AtomicBool>,
        _input: PhantomData<&'a Input>,
    }

    impl Drop for Grabbed<'_> {
        fn drop(&mut self) {
            let list = self.devices.lock_anyway();
            self.grabbed.store(false, Ordering::Relaxed);
            for found in list.iter() {
                // One unplugged meanwhile has no grab left to release.
                if let Err(err) = found.device.set_grab(false)
                    && found.alive.load(Ordering::Relaxed)
                {
                    eprintln!("mickey: failed to release the grab: {err}");
                }
            }
        }
    }

    /// How long after a text expander typed its keys are still taken for
    /// its own: its presses in that time are not repeated on the mirror.
    const EXPANDER_QUIET: Duration = Duration::from_millis(60);

    /// What is typed for another PC, repeated on a keyboard of this PC that
    /// only text expanders read (PRD: none yet; omakvm's espanso support).
    ///
    /// A text expander such as espanso reads the keyboards itself. While
    /// the focus is on another PC they are grabbed, so it hears nothing and
    /// its triggers stop working. With the mirror it hears them again; what
    /// it then types comes out of its own virtual keyboard, which Mickey
    /// captures like any other and sends to the focused PC. The desktop
    /// must not read the mirror, or the keys would also be typed here: a
    /// udev rule gives it `LIBINPUT_IGNORE_DEVICE`.
    pub struct TypingMirror {
        device: VirtualDevice,
    }

    /// The mirror, when `MICKEY_TYPING_MIRROR=1` is in the environment.
    pub fn typing_mirror() -> Option<TypingMirror> {
        if std::env::var_os("MICKEY_TYPING_MIRROR").is_none_or(|value| value != "1") {
            return None;
        }
        match VirtualDevice::create_keyboard(MIRROR_NAME) {
            Ok(device) => {
                println!("mickey: typing mirror on, for text expanders");
                Some(TypingMirror { device })
            }
            Err(err) => {
                eprintln!("mickey: no typing mirror: {err}");
                None
            }
        }
    }

    impl TypingMirror {
        /// Repeats a key that was just sent to the focused PC. Releases
        /// always pass, so no key stays down on the mirror; presses that
        /// are the expander's own typing do not.
        pub fn typed(&mut self, event: InputEvent) {
            let InputEvent::Key { code, pressed } = event else {
                return;
            };
            if pressed && capture::expander_typed_within(EXPANDER_QUIET) {
                return;
            }
            self.device.emit(EV_KEY, code, i32::from(pressed));
            // A failed write only costs the expander a key.
            let _ = self.device.sync();
        }
    }

    /// Where the cursor is: a question to Hyprland or the X server,
    /// which may take a few milliseconds (#187).
    pub fn cursor_now() -> Option<(i32, i32)> {
        match crate::desktop::session() {
            crate::desktop::Session::X11 => crate::desktop::x11::cursor().ok(),
            _ => crate::desktop::hyprland::cursor().ok(),
        }
    }

    impl Input {
        /// Every keyboard, mouse and touchpad, with a clear message when we
        /// may not read them.
        pub fn open() -> Result<Input, String> {
            let devices = capture::find_devices().map_err(|err| match err.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    "cannot read /dev/input: permission denied; run \
                     `sudo usermod -aG input $USER`, then log in again (or use `newgrp input`)"
                        .to_string()
                }
                _ => format!("cannot list input devices: {err}"),
            })?;
            if devices.is_empty() {
                return Err("no keyboard or mouse found under /dev/input".into());
            }
            for found in &devices {
                let kind = match found.kind {
                    capture::Kind::Keyboard => "keyboard",
                    capture::Kind::Mouse => "mouse",
                    capture::Kind::Touchpad => "touchpad",
                };
                println!("mickey: found {kind}: {}", found.name);
            }
            Ok(Input {
                devices: Arc::new(Mutex::new(devices)),
                grabbed: Arc::new(AtomicBool::new(false)),
            })
        }

        pub fn start<T: From<InputEvent> + Send + 'static>(
            &self,
            events: Sender<T>,
            touchpad_speed: i32,
        ) -> Result<(), String> {
            capture::spawn_readers(&self.devices.lock_anyway(), events.clone(), touchpad_speed);
            capture::watch(
                Arc::clone(&self.devices),
                Arc::clone(&self.grabbed),
                events,
                touchpad_speed,
            );
            Ok(())
        }

        /// A device that cannot answer counts as down, unless it is gone.
        pub fn any_key_down(&self) -> bool {
            self.devices.lock_anyway().iter().any(|found| {
                found
                    .device
                    .any_key_down()
                    .unwrap_or_else(|_| found.alive.load(Ordering::Relaxed))
            })
        }

        pub fn button_down(&self) -> bool {
            self.devices.lock_anyway().iter().any(|found| {
                found
                    .device
                    .button_down()
                    .unwrap_or_else(|_| found.alive.load(Ordering::Relaxed))
            })
        }

        /// Takes every device from the desktop until the guard is dropped.
        pub fn grab(&self) -> Result<Grabbed<'_>, String> {
            // Held until the flag is set: see `capture::watch`.
            let list = self.devices.lock_anyway();
            for (index, found) in list.iter().enumerate() {
                // One just unplugged, whose reader has not noticed yet, is
                // no reason to refuse the switch.
                if let Err(err) = found.device.set_grab(true)
                    && err.raw_os_error() != Some(19)
                {
                    for taken in &list[..index] {
                        let _ = taken.device.set_grab(false);
                    }
                    return Err(format!("cannot grab the input devices: {err}"));
                }
            }
            self.grabbed.store(true, Ordering::Relaxed);
            Ok(Grabbed {
                devices: Arc::clone(&self.devices),
                grabbed: Arc::clone(&self.grabbed),
                _input: PhantomData,
            })
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::input::keymap;
    use crate::platform::hooks::{self, HookEvent, Point};
    use std::marker::PhantomData;
    use std::sync::{Arc, Mutex};

    /// evdev codes for left, right, middle, back and forward.
    const BUTTONS: [u16; 5] = [0x110, 0x111, 0x112, 0x113, 0x114];

    pub struct Input {
        /// Where the cursor was pinned when the focus left; `None` while
        /// local. Motion is measured from it.
        anchor: Arc<Mutex<Option<Point>>>,
    }

    /// While this lives, the hooks swallow every event.
    pub struct Grabbed<'a> {
        anchor: Arc<Mutex<Option<Point>>>,
        _input: PhantomData<&'a Input>,
    }

    impl Drop for Grabbed<'_> {
        fn drop(&mut self) {
            hooks::set_swallow(false);
            *self.anchor.lock_anyway() = None;
        }
    }

    /// Where the cursor is: a local call, cheap.
    pub fn cursor_now() -> Option<(i32, i32)> {
        let point = hooks::cursor_pos();
        Some((point.x, point.y))
    }

    impl Input {
        pub fn open() -> Result<Input, String> {
            println!("mickey: capturing the keyboard and mouse with low-level hooks");
            Ok(Input {
                anchor: Arc::new(Mutex::new(None)),
            })
        }

        /// Installs the hooks; a thread turns what they see into
        /// `InputEvent`s with evdev codes.
        pub fn start<T: From<InputEvent> + Send + 'static>(
            &self,
            events: Sender<T>,
            _touchpad_speed: i32,
        ) -> Result<(), String> {
            let (raw_sender, raw) = std::sync::mpsc::channel();
            hooks::start(raw_sender).map_err(|err| format!("cannot install the hooks: {err}"))?;
            let anchor = Arc::clone(&self.anchor);
            crate::threads::spawn("input translate", move || {
                for event in raw {
                    let anchor = *anchor.lock_anyway();
                    if let Some(event) = translate(event, anchor)
                        && events.send(T::from(event)).is_err()
                    {
                        return;
                    }
                }
            });
            Ok(())
        }

        pub fn any_key_down(&self) -> bool {
            hooks::any_key_down()
        }

        pub fn button_down(&self) -> bool {
            hooks::button_down()
        }

        /// Pins the cursor where it is and starts swallowing.
        pub fn grab(&self) -> Result<Grabbed<'_>, String> {
            *self.anchor.lock_anyway() = Some(hooks::cursor_pos());
            hooks::set_swallow(true);
            Ok(Grabbed {
                anchor: Arc::clone(&self.anchor),
                _input: PhantomData,
            })
        }
    }

    /// Hook events to Mickey's events. Motion only counts while the cursor
    /// is pinned: swallowed moves leave it at `anchor`, so each reported
    /// position minus the anchor is exactly one movement.
    fn translate(event: HookEvent, anchor: Option<Point>) -> Option<InputEvent> {
        match event {
            HookEvent::Key { scan, pressed } => {
                keymap::evdev_code(scan).map(|code| InputEvent::Key { code, pressed })
            }
            HookEvent::Button { button, pressed } => BUTTONS
                .get(usize::from(button))
                .map(|&code| InputEvent::Button { code, pressed }),
            HookEvent::Delta { dx, dy } => Some(InputEvent::Motion { dx, dy }),
            // With raw input, positions are not motion (a game may pin the
            // cursor); without it, motion is measured from the anchor.
            HookEvent::Move { .. } if hooks::raw_mouse_active() => None,
            HookEvent::Move { x, y } => {
                let anchor = anchor?;
                let (dx, dy) = (x - anchor.x, y - anchor.y);
                (dx != 0 || dy != 0).then_some(InputEvent::Motion { dx, dy })
            }
            HookEvent::Wheel {
                vertical,
                horizontal,
            } => Some(InputEvent::Scroll {
                vertical,
                horizontal,
            }),
        }
    }
}
