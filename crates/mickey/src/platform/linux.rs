//! Hand-written evdev bindings: read input events and grab a device.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;

/// `ioctl`'s request is C's `unsigned long`: 64 bits on 64-bit Linux, 32 on
/// 32-bit (a Raspberry Pi with a 32-bit system). Declared any wider there,
/// the arguments after it would land in the wrong registers.
pub(super) type Request = std::ffi::c_ulong;

// ioctl numbers from <linux/input.h>, expanded from the _IOW/_IOR macros.
// `_IOR` puts the buffer size in bits 16..30, hence the `<< 16`.
const EVIOCGRAB: Request = 0x4004_4590;
const BITS_BYTES: usize = 96; // KEY_MAX (0x2ff) + 1 bits, the largest bitmap
const EVIOCGKEY: Request = 0x8000_4518 | ((BITS_BYTES as Request) << 16);
const NAME_BYTES: usize = 256;
const EVIOCGNAME: Request = 0x8000_4506 | ((NAME_BYTES as Request) << 16);
/// `EVIOCGABS(axis)`: one axis' `struct input_absinfo`, six `i32`s.
const ABSINFO_BYTES: usize = 24;
const fn eviocgabs(axis: u16) -> Request {
    (0x8000_4540 + axis as Request) | ((ABSINFO_BYTES as Request) << 16)
}
/// `EVIOCGBIT(ev_type)`: which codes of one event type the device has.
const fn eviocgbit(ev_type: u16) -> Request {
    (0x8000_4520 + ev_type as Request) | ((BITS_BYTES as Request) << 16)
}

/// `struct input_event` starts with a timeval of two C `long`s: 16 bytes on
/// 64-bit Linux, 8 on 32-bit (also for a 32-bit program on a 64-bit kernel,
/// which the kernel serves the 32-bit layout).
pub(super) const EVENT_TIME: usize = 2 * std::mem::size_of::<std::ffi::c_long>();
/// Then type (2 bytes), code (2) and value (4).
pub(super) const EVENT_SIZE: usize = EVENT_TIME + 8;

unsafe extern "C" {
    /// Shared with `uinput.rs`.
    pub(super) fn ioctl(fd: i32, request: Request, ...) -> i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawEvent {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

pub struct Device {
    file: File,
}

impl Device {
    pub fn open(path: &Path) -> io::Result<Device> {
        Ok(Device {
            file: File::open(path)?,
        })
    }

    /// Blocks until the device has events, then puts all it has (up to
    /// 64) in `into`: one system call for a whole batch, such as a move's
    /// X, Y and SYN_REPORT, instead of one per event (#189).
    pub fn read_events(&self, into: &mut Vec<RawEvent>) -> io::Result<()> {
        let mut buf = [0u8; EVENT_SIZE * 64];
        // The kernel only hands out whole events, never part of one.
        let read = (&self.file).read(&mut buf)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        into.clear();
        let (events, _) = buf[..read].as_chunks::<EVENT_SIZE>();
        const T: usize = EVENT_TIME;
        into.extend(events.iter().map(|e| RawEvent {
            kind: u16::from_ne_bytes([e[T], e[T + 1]]),
            code: u16::from_ne_bytes([e[T + 2], e[T + 3]]),
            value: i32::from_ne_bytes([e[T + 4], e[T + 5], e[T + 6], e[T + 7]]),
        }));
        Ok(())
    }

    pub fn any_key_down(&self) -> io::Result<bool> {
        let state = self.bitmap(EVIOCGKEY)?;
        Ok(state.iter().any(|&byte| byte != 0))
    }

    /// Whether a mouse button (`BTN_LEFT` to `BTN_TASK`) is down. Unlike
    /// `any_key_down`, a finger resting on a touchpad does not count.
    pub fn button_down(&self) -> io::Result<bool> {
        let state = self.bitmap(EVIOCGKEY)?;
        // BTN_LEFT is 0x110: bits 0 to 7 of byte 0x22.
        Ok(state.get(0x110 / 8).is_some_and(|&byte| byte != 0))
    }

    /// Whether the device can send `code` of `ev_type`, e.g. `EV_KEY`, `KEY_A`.
    pub fn supports(&self, ev_type: u16, code: u16) -> io::Result<bool> {
        let bits = self.bitmap(eviocgbit(ev_type))?;
        let code = usize::from(code);
        Ok(bits
            .get(code / 8)
            .is_some_and(|byte| byte & (1 << (code % 8)) != 0))
    }

    /// Device units per millimetre on an absolute axis; 0 when unknown.
    pub fn abs_resolution(&self, axis: u16) -> io::Result<i32> {
        let mut info = [0u8; ABSINFO_BYTES];
        self.ioctl_read(eviocgabs(axis), &mut info)?;
        // Fields: value, minimum, maximum, fuzz, flat, resolution.
        Ok(i32::from_ne_bytes([info[20], info[21], info[22], info[23]]))
    }

    pub fn name(&self) -> io::Result<String> {
        let mut name = [0u8; NAME_BYTES];
        let len = self.ioctl_read(EVIOCGNAME, &mut name)?;
        let name = name[..len].split(|&byte| byte == 0).next().unwrap_or(&[]);
        Ok(String::from_utf8_lossy(name).into_owned())
    }

    fn bitmap(&self, request: Request) -> io::Result<[u8; BITS_BYTES]> {
        let mut bits = [0u8; BITS_BYTES];
        self.ioctl_read(request, &mut bits)?;
        Ok(bits)
    }

    /// Runs a read ioctl whose size field equals `buf.len()`; returns the
    /// number of bytes the kernel wrote.
    fn ioctl_read(&self, request: Request, buf: &mut [u8]) -> io::Result<usize> {
        debug_assert_eq!((request >> 16) & 0x3fff, buf.len() as Request);
        // SAFETY: every caller passes a request whose size field is
        // `buf.len()`, so the kernel writes at most that many bytes into
        // `buf`, which lives for the whole call. The fd is open as long as
        // `self`.
        let written = unsafe { ioctl(self.file.as_raw_fd(), request, buf.as_mut_ptr()) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    /// Takes the device away from the desktop (`on`), or gives it back.
    /// `input::Grabbed` gives every device back when it is dropped,
    /// including during a panic. If the process is killed, the kernel
    /// releases the grab when the file is closed.
    pub fn set_grab(&self, on: bool) -> io::Result<()> {
        set_grab(&self.file, on)
    }
}

fn set_grab(file: &File, on: bool) -> io::Result<()> {
    // SAFETY: EVIOCGRAB takes an int by value, not a pointer, and the fd is
    // open because `file` is borrowed for the whole call.
    let result = unsafe { ioctl(file.as_raw_fd(), EVIOCGRAB, i32::from(on)) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
