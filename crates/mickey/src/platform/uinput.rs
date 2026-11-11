//! Hand-written uinput bindings: a virtual keyboard and mouse. The kernel
//! hands their events to the compositor exactly like a real device's, so
//! this works on any Linux desktop (PRD 0008).

use super::linux::{EVENT_SIZE, EVENT_TIME, Request, ioctl};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

const PATH: &str = "/dev/uinput";
/// The name Mickey gives its virtual device, so capture can skip it.
pub const DEVICE_NAME: &str = "Mickey virtual input";
/// The keyboard that repeats on this PC what is typed for another one, for
/// text expanders to read (`input::TypingMirror`). Capture skips it too, and
/// a udev rule keeps the desktop from reading it.
pub const MIRROR_NAME: &str = "Mickey typing mirror";

// ioctl numbers from <linux/uinput.h>, checked against the C headers.
const UI_SET_EVBIT: Request = 0x4004_5564;
const UI_SET_KEYBIT: Request = 0x4004_5565;
const UI_SET_RELBIT: Request = 0x4004_5566;
const UI_DEV_SETUP: Request = 0x405c_5503;
const UI_DEV_CREATE: Request = 0x5501;
const UI_DEV_DESTROY: Request = 0x5502;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const BUS_VIRTUAL: u16 = 6;

/// Every keyboard key (1-255) and the mouse buttons Mickey sends.
const KEYS: std::ops::RangeInclusive<u16> = 1..=255;
const BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
/// REL_X, REL_Y, REL_HWHEEL, REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES.
const AXES: [u16; 6] = [0, 1, 6, 8, 11, 12];

/// `struct uinput_setup`: an 8-byte `input_id`, an 80-byte name and a
/// `u32`, 92 bytes in all.
const SETUP_LEN: usize = 92;
const NAME_LEN: usize = 80;

pub struct VirtualDevice {
    file: File,
    /// Events queued since the last `sync`, as `struct input_event`s.
    batch: Vec<u8>,
}

impl VirtualDevice {
    /// Creates the device. Fails with `PermissionDenied` unless the user
    /// may write to `/dev/uinput` (see the README's udev rule).
    pub fn create(name: &str) -> io::Result<VirtualDevice> {
        VirtualDevice::create_with(name, true)
    }

    /// A keyboard only: no buttons, no motion.
    pub fn create_keyboard(name: &str) -> io::Result<VirtualDevice> {
        VirtualDevice::create_with(name, false)
    }

    fn create_with(name: &str, mouse: bool) -> io::Result<VirtualDevice> {
        let file = OpenOptions::new().write(true).open(PATH)?;
        let device = VirtualDevice {
            file,
            batch: Vec::with_capacity(4 * EVENT_SIZE),
        };
        device.set(UI_SET_EVBIT, EV_SYN)?;
        device.set(UI_SET_EVBIT, EV_KEY)?;
        for code in KEYS {
            device.set(UI_SET_KEYBIT, code)?;
        }
        if mouse {
            device.set(UI_SET_EVBIT, EV_REL)?;
            for code in BUTTONS {
                device.set(UI_SET_KEYBIT, code)?;
            }
            for axis in AXES {
                device.set(UI_SET_RELBIT, axis)?;
            }
        }

        let mut setup = [0u8; SETUP_LEN];
        setup[0..2].copy_from_slice(&BUS_VIRTUAL.to_ne_bytes());
        let name = name.as_bytes();
        // Leave room for the terminating zero.
        let len = name.len().min(NAME_LEN - 1);
        setup[8..8 + len].copy_from_slice(&name[..len]);
        // SAFETY: UI_DEV_SETUP reads `sizeof(struct uinput_setup)` (92)
        // bytes from the pointer, and `setup` is exactly that long and lives
        // for the whole call. The fd is open for as long as `device`.
        let result = unsafe { ioctl(device.fd(), UI_DEV_SETUP, setup.as_ptr()) };
        check(result)?;
        // SAFETY: UI_DEV_CREATE takes no argument; the fd is open.
        check(unsafe { ioctl(device.fd(), UI_DEV_CREATE) })?;
        Ok(device)
    }

    /// Queues one event; it reaches the kernel with the rest of its batch,
    /// at `sync`.
    pub fn emit(&mut self, kind: u16, code: u16, value: i32) {
        // `struct input_event`: a timeval (zero: the kernel stamps it),
        // then type, code, value. See `linux::EVENT_TIME` for its size.
        const T: usize = EVENT_TIME;
        let mut event = [0u8; EVENT_SIZE];
        event[T..T + 2].copy_from_slice(&kind.to_ne_bytes());
        event[T + 2..T + 4].copy_from_slice(&code.to_ne_bytes());
        event[T + 4..T + 8].copy_from_slice(&value.to_ne_bytes());
        self.batch.extend_from_slice(&event);
    }

    /// Ends a batch with `SYN_REPORT` and hands it to the kernel in one
    /// write: one system call per key or motion instead of one per event
    /// (#189). uinput takes any whole number of events per write.
    pub fn sync(&mut self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0);
        let written = self.file.write_all(&self.batch);
        self.batch.clear();
        written
    }

    fn set(&self, request: Request, code: u16) -> io::Result<()> {
        // SAFETY: the UI_SET_* requests take an int by value, not a
        // pointer; the fd is open for as long as `self`.
        check(unsafe { ioctl(self.fd(), request, i32::from(code)) })
    }

    fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

/// The kernel would also remove the device when the file closes; destroying
/// it explicitly makes it vanish at once.
impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // SAFETY: UI_DEV_DESTROY takes no argument; the fd is still open
        // because `self.file` is dropped only after this returns.
        let _ = unsafe { ioctl(self.fd(), UI_DEV_DESTROY) };
    }
}

fn check(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    /// Measurement for #189, run by hand:
    /// `cargo test -- --ignored uinput_motions --nocapture`. Wiggles the
    /// cursor one unit right and back, 2000 times (it ends where it
    /// started), and counts the write calls (`syscw` in /proc/self/io).
    #[test]
    #[ignore = "creates a real input device; run by hand"]
    fn uinput_motions() {
        let mut device = VirtualDevice::create("Mickey measure").expect("needs /dev/uinput");
        sleep(Duration::from_secs(1));
        let writes = || {
            let io = std::fs::read_to_string("/proc/self/io").unwrap();
            let line = io.lines().find(|l| l.starts_with("syscw:")).unwrap();
            line[6..].trim().parse::<u64>().unwrap()
        };
        let before = writes();
        let start = std::time::Instant::now();
        for n in 0..2000 {
            let dx = if n % 2 == 0 { 1 } else { -1 };
            device.emit(EV_REL, 0, dx); // REL_X
            device.emit(EV_REL, 1, 0); // REL_Y, unchanged
            device.sync().unwrap();
        }
        let elapsed = start.elapsed();
        eprintln!("2000 motions: {} writes, {elapsed:?}", writes() - before);
    }

    /// Spike #69, run by hand: `cargo test -- --ignored uinput_spike`.
    /// It types `a` into the focused window and moves the cursor 200 units
    /// right, so focus a harmless window first.
    #[test]
    #[ignore = "creates a real input device; run by hand"]
    fn uinput_spike_types_and_moves() {
        let mut device = VirtualDevice::create("Mickey spike").expect("needs /dev/uinput");
        // Give the compositor time to notice the new device.
        sleep(Duration::from_secs(1));
        for value in [1, 0] {
            device.emit(EV_KEY, 30, value); // KEY_A
            device.sync().unwrap();
        }
        for _ in 0..20 {
            device.emit(EV_REL, 0, 10); // REL_X
            device.sync().unwrap();
            sleep(Duration::from_millis(10));
        }
        sleep(Duration::from_millis(200));
    }
}
