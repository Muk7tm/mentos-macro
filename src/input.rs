//! Low-level input injection: X11 keymap resolution plus two backends.
//!
//! * **XTest** (default): synthetic events through the XTEST extension. No privileges needed.
//!   Every event is sent with a checked request, so the worker only continues once the X
//!   server has actually processed it; hold timings are measured from that point.
//! * **uinput** (optional): a virtual kernel input device. Events look like a physical
//!   keyboard/mouse to every consumer. Requires write access to `/dev/uinput`.

use crate::keys::{self, Keysym, MouseButton};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(pub String);

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

impl InputError {
    pub fn new(msg: impl Into<String>) -> Self {
        InputError(msg.into())
    }
}

pub type InputResult<T = ()> = Result<T, InputError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    #[default]
    Xtest,
    Uinput,
}

impl BackendKind {
    pub const ALL: [BackendKind; 2] = [BackendKind::Xtest, BackendKind::Uinput];
    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Xtest => "XTest",
            BackendKind::Uinput => "uinput",
        }
    }
    pub fn index(self) -> usize {
        BackendKind::ALL.iter().position(|b| *b == self).unwrap_or(0)
    }
    pub fn from_index(i: usize) -> Self {
        BackendKind::ALL.get(i).copied().unwrap_or_default()
    }
}

/// Something that can press and release keys (by X keycode) and mouse buttons.
pub trait InputBackend: Send {
    fn key(&mut self, keycode: u8, down: bool) -> InputResult;
    fn button(&mut self, button: MouseButton, down: bool) -> InputResult;
    fn kind(&self) -> BackendKind;
    /// Whether events are delivered yet. A freshly created uinput device is only usable once
    /// the X server has opened it; until then its events are silently dropped.
    fn is_ready(&mut self) -> InputResult<bool>;
}

/// Where a keysym lives on the current keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyLoc {
    pub keycode: u8,
    /// The keysym is on the shifted level, so Shift must be held to produce it.
    pub needs_shift: bool,
}

pub trait KeyResolver {
    fn resolve(&self, sym: Keysym) -> Option<KeyLoc>;
}

/// Snapshot of the X server's core keyboard mapping.
#[derive(Clone, Debug)]
pub struct Keymap {
    min_keycode: u8,
    per_keycode: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load<C: Connection>(conn: &C) -> Result<Self, String> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| format!("GetKeyboardMapping failed: {e}"))?
            .reply()
            .map_err(|e| format!("GetKeyboardMapping failed: {e}"))?;
        Ok(Keymap { min_keycode: min, per_keycode: usize::from(reply.keysyms_per_keycode).max(1), syms: reply.keysyms })
    }

    /// Build from raw data (used by tests).
    pub fn from_raw(min_keycode: u8, per_keycode: usize, syms: Vec<u32>) -> Self {
        Keymap { min_keycode, per_keycode: per_keycode.max(1), syms }
    }

    fn keycode_count(&self) -> usize {
        self.syms.len() / self.per_keycode
    }

    /// Keysym at `col` (0 = base level, 1 = shifted) of `keycode`.
    pub fn keysym(&self, keycode: u8, col: usize) -> Keysym {
        if keycode < self.min_keycode || col >= self.per_keycode {
            return 0;
        }
        let idx = usize::from(keycode - self.min_keycode) * self.per_keycode + col;
        self.syms.get(idx).copied().unwrap_or(0)
    }

    /// Keysym produced by a key press given the X modifier state (for key capture).
    pub fn keysym_for_event(&self, keycode: u8, numlock_on: bool) -> Keysym {
        let base = self.keysym(keycode, 0);
        if numlock_on && keys::is_keypad(base) {
            let shifted = self.keysym(keycode, 1);
            if keys::is_keypad(shifted) {
                return shifted;
            }
        }
        base
    }

    /// Keycodes whose base level is `sym` (falls back to the shifted level).
    pub fn keycodes_for(&self, sym: Keysym) -> Vec<u8> {
        for col in 0..2.min(self.per_keycode) {
            let found: Vec<u8> = (0..self.keycode_count())
                .filter(|i| self.syms[i * self.per_keycode + col] == sym)
                .filter_map(|i| u8::try_from(i + usize::from(self.min_keycode)).ok())
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }
}

impl KeyResolver for Keymap {
    fn resolve(&self, sym: Keysym) -> Option<KeyLoc> {
        for col in 0..2.min(self.per_keycode) {
            for i in 0..self.keycode_count() {
                if self.syms[i * self.per_keycode + col] == sym {
                    let keycode = u8::try_from(i + usize::from(self.min_keycode)).ok()?;
                    // Keypad keys are sent as the physical key; NumLock decides their meaning.
                    let needs_shift = col == 1 && !keys::is_keypad(sym);
                    return Some(KeyLoc { keycode, needs_shift });
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------
// XTest
// ---------------------------------------------------------------------------------------

pub struct XTestBackend {
    conn: RustConnection,
    root: Window,
}

impl XTestBackend {
    pub fn connect() -> InputResult<Self> {
        let (conn, screen) =
            x11rb::connect(None).map_err(|e| InputError::new(format!("Cannot connect to the X server: {e}")))?;
        let root = conn.setup().roots[screen].root;
        conn.extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| InputError::new(format!("X connection error: {e}")))?
            .ok_or_else(|| InputError::new("The X server does not provide the XTEST extension"))?;
        conn.xtest_get_version(2, 2)
            .map_err(|e| InputError::new(format!("XTEST query failed: {e}")))?
            .reply()
            .map_err(|e| InputError::new(format!("XTEST query failed: {e}")))?;
        Ok(XTestBackend { conn, root })
    }

    fn fake(&mut self, kind: u8, detail: u8) -> InputResult {
        self.conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| InputError::new(format!("XTEST request failed: {e}")))?
            .check()
            .map_err(|e| InputError::new(format!("X server rejected a synthetic event: {e}")))
    }
}

impl InputBackend for XTestBackend {
    fn key(&mut self, keycode: u8, down: bool) -> InputResult {
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, keycode)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, button.x11_button())
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Xtest
    }

    fn is_ready(&mut self) -> InputResult<bool> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------------------
// uinput
// ---------------------------------------------------------------------------------------

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const BUS_VIRTUAL: u16 = 0x06;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'U' as u64) << 8) | nr
}
const UI_SET_EVBIT: u64 = ioc(1, 100, 4);
const UI_SET_KEYBIT: u64 = ioc(1, 101, 4);
const UI_SET_RELBIT: u64 = ioc(1, 102, 4);
const UI_DEV_SETUP: u64 = ioc(1, 3, std::mem::size_of::<UinputSetup>() as u64);
const UI_DEV_CREATE: u64 = ioc(0, 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, 2, 0);

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct RawInputEvent {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

/// Extra time after the device shows up in the X device list before it is used.
const UINPUT_SETTLE: Duration = Duration::from_millis(100);

pub struct UinputBackend {
    file: File,
    name: String,
    /// Connection used to watch for the device in the X server's device list.
    watch: Option<RustConnection>,
    seen_at: Option<Instant>,
}

impl UinputBackend {
    pub fn create(base_name: &str) -> InputResult<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        // A unique name lets readiness detection find exactly this device.
        let device_name =
            format!("{base_name} {}-{}", std::process::id(), SERIAL.fetch_add(1, Ordering::Relaxed));
        let device_name = device_name.as_str();
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .map_err(|e| {
                InputError::new(format!(
                    "Cannot open /dev/uinput ({e}). Add your user to the 'input' group or install the udev rule from the README."
                ))
            })?;
        let fd = file.as_raw_fd();
        let ioctl_int = |req: u64, val: i32| -> InputResult {
            // SAFETY: valid fd; these uinput requests take an int argument by value.
            let rc = unsafe { libc::ioctl(fd, req as _, val as libc::c_int) };
            if rc < 0 {
                Err(InputError::new(format!("uinput ioctl failed: {}", std::io::Error::last_os_error())))
            } else {
                Ok(())
            }
        };
        ioctl_int(UI_SET_EVBIT, i32::from(EV_KEY))?;
        ioctl_int(UI_SET_EVBIT, i32::from(EV_SYN))?;
        // Ordinary keyboard keys (evdev codes 1..=248 cover every key an X keycode can name).
        for code in 1..=248 {
            ioctl_int(UI_SET_KEYBIT, code)?;
        }
        for b in MouseButton::ALL {
            ioctl_int(UI_SET_KEYBIT, i32::from(b.evdev_code()))?;
        }
        // Relative axes make libinput classify the device as a pointer, so buttons work.
        ioctl_int(UI_SET_EVBIT, i32::from(EV_REL))?;
        ioctl_int(UI_SET_RELBIT, i32::from(REL_X))?;
        ioctl_int(UI_SET_RELBIT, i32::from(REL_Y))?;

        let mut setup = UinputSetup {
            id: InputId { bustype: BUS_VIRTUAL, vendor: 0x1d6b, product: 0x4d4d, version: 1 },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let bytes = device_name.as_bytes();
        let n = bytes.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&bytes[..n]);
        // SAFETY: setup is a properly laid out uinput_setup that lives across the call.
        if unsafe { libc::ioctl(fd, UI_DEV_SETUP as _, &setup as *const UinputSetup) } < 0 {
            return Err(InputError::new(format!("UI_DEV_SETUP failed: {}", std::io::Error::last_os_error())));
        }
        // SAFETY: argument-less request on a valid fd.
        if unsafe { libc::ioctl(fd, UI_DEV_CREATE as _) } < 0 {
            return Err(InputError::new(format!("UI_DEV_CREATE failed: {}", std::io::Error::last_os_error())));
        }
        tracing::info!("created uinput device \"{device_name}\"");
        Ok(UinputBackend { file, name: device_name.to_string(), watch: None, seen_at: None })
    }

    /// Has the X server opened the device (it appears in the XInput device list)?
    fn listed_by_x(&mut self) -> InputResult<bool> {
        use x11rb::protocol::xinput::ConnectionExt as _;
        if self.watch.is_none() {
            let (conn, _) = x11rb::connect(None).map_err(|e| InputError::new(format!("Cannot connect to the X server: {e}")))?;
            self.watch = Some(conn);
        }
        let conn = self.watch.as_ref().expect("just set");
        let reply = conn
            .xinput_list_input_devices()
            .map_err(|e| InputError::new(format!("XInput query failed: {e}")))?
            .reply()
            .map_err(|e| InputError::new(format!("XInput query failed: {e}")))?;
        Ok(reply.names.iter().any(|n| n.name == self.name.as_bytes()))
    }

    fn emit(&mut self, kind: u16, code: u16, value: i32) -> InputResult {
        let ev = RawInputEvent { time: libc::timeval { tv_sec: 0, tv_usec: 0 }, kind, code, value };
        // SAFETY: RawInputEvent is a plain repr(C) struct; we view it as bytes for write().
        let bytes = unsafe {
            std::slice::from_raw_parts((&ev as *const RawInputEvent).cast::<u8>(), std::mem::size_of::<RawInputEvent>())
        };
        self.file.write_all(bytes).map_err(|e| InputError::new(format!("uinput write failed: {e}")))
    }

    fn key_code(&mut self, code: u16, down: bool) -> InputResult {
        self.emit(EV_KEY, code, i32::from(down))?;
        self.emit(EV_SYN, SYN_REPORT, 0)
    }
}

impl InputBackend for UinputBackend {
    fn key(&mut self, keycode: u8, down: bool) -> InputResult {
        // X keycodes are evdev codes offset by 8 on every evdev/libinput based server.
        let code = keycode
            .checked_sub(8)
            .filter(|c| *c > 0)
            .ok_or_else(|| InputError::new(format!("keycode {keycode} has no evdev equivalent")))?;
        self.key_code(u16::from(code), down)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> InputResult {
        self.key_code(button.evdev_code(), down)
    }

    fn kind(&self) -> BackendKind {
        BackendKind::Uinput
    }

    fn is_ready(&mut self) -> InputResult<bool> {
        if let Some(t) = self.seen_at {
            return Ok(t.elapsed() >= UINPUT_SETTLE);
        }
        if self.listed_by_x()? {
            self.seen_at = Some(Instant::now());
            self.watch = None;
        }
        Ok(false)
    }
}

impl Drop for UinputBackend {
    fn drop(&mut self) {
        // SAFETY: argument-less request on a valid fd; the fd is closed by File's Drop.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _);
        }
    }
}

/// Create the backend selected in the configuration.
pub fn create_backend(kind: BackendKind) -> InputResult<Box<dyn InputBackend>> {
    match kind {
        BackendKind::Xtest => Ok(Box::new(XTestBackend::connect()?)),
        BackendKind::Uinput => Ok(Box::new(UinputBackend::create("Mentos Macro virtual input")?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_keymap() -> Keymap {
        // keycode 10: '1' / '!', keycode 11: 'e' / 'E', keycode 12: KP_Home / KP_7, 13: Tab
        Keymap::from_raw(10, 2, vec![0x31, 0x21, 0x65, 0x45, 0xff95, 0xffb7, 0xff09, 0xfe20])
    }

    #[test]
    fn resolves_base_and_shifted_levels() {
        let km = sample_keymap();
        assert_eq!(km.resolve(0x65), Some(KeyLoc { keycode: 11, needs_shift: false }));
        assert_eq!(km.resolve(0x45), Some(KeyLoc { keycode: 11, needs_shift: true }));
        assert_eq!(km.resolve(0x21), Some(KeyLoc { keycode: 10, needs_shift: true }));
        assert_eq!(km.resolve(0xff09), Some(KeyLoc { keycode: 13, needs_shift: false }));
        assert_eq!(km.resolve(0x1234), None);
    }

    #[test]
    fn keypad_never_needs_shift() {
        let km = sample_keymap();
        assert_eq!(km.resolve(0xffb7), Some(KeyLoc { keycode: 12, needs_shift: false }));
        assert_eq!(km.keysym_for_event(12, true), 0xffb7);
        assert_eq!(km.keysym_for_event(12, false), 0xff95);
    }

    #[test]
    fn keycodes_for_prefers_base_level() {
        let km = sample_keymap();
        assert_eq!(km.keycodes_for(0x65), vec![11]);
        assert_eq!(km.keycodes_for(0x45), vec![11]);
        assert!(km.keycodes_for(0x99).is_empty());
    }

    #[test]
    fn uinput_ioctl_numbers_match_kernel_headers() {
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_RELBIT, 0x4004_5566);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(std::mem::size_of::<RawInputEvent>(), 24);
    }
}
