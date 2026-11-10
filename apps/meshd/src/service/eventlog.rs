//! The Windows Event Log source `Meshnode` (spec §6.5): registered by `service install`,
//! written to by the service for its start, stop and a fatal startup error.
//!
//! The source has no message table of its own. It names `EventCreate.exe` as its message
//! file, whose table maps every event id from 1 to 1000 to the message `%1` — the
//! approach `eventcreate.exe` itself uses for the sources it creates — so each entry shows
//! its single text string as written, without a "description cannot be found" preamble.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE, WIN32_ERROR};
use windows_sys::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, REPORT_EVENT_TYPE,
    RegisterEventSourceW, ReportEventW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW,
};

use super::windows::wide;

/// The event source's name.
pub const SOURCE: &str = "Meshnode";
/// The source's registry key under `HKEY_LOCAL_MACHINE`.
pub const REGISTRY_KEY: &str = r"SYSTEM\CurrentControlSet\Services\EventLog\Application\Meshnode";
/// The message file: event ids 1–1000, each formatted as its first string.
pub const MESSAGE_FILE: &str = r"%SystemRoot%\System32\EventCreate.exe";
/// Error, warning and information entries.
const TYPES_SUPPORTED: u32 = 0x7;

/// What an entry reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Started,
    Stopped,
    StartFailed,
    /// The running service ended on an internal error (a panic).
    Crashed,
}

impl Event {
    /// The entry's event id, within `EventCreate.exe`'s 1–1000.
    pub fn id(self) -> u32 {
        match self {
            Event::Started => 1,
            Event::Stopped => 2,
            Event::StartFailed => 3,
            Event::Crashed => 4,
        }
    }

    fn kind(self) -> REPORT_EVENT_TYPE {
        match self {
            Event::Started | Event::Stopped => EVENTLOG_INFORMATION_TYPE,
            Event::StartFailed | Event::Crashed => EVENTLOG_ERROR_TYPE,
        }
    }
}

fn check(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Creates (or updates) the source's registry key. Needs Administrator.
pub fn register() -> io::Result<()> {
    let key = wide(REGISTRY_KEY);
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: `key` is NUL-terminated; `hkey` is a valid out pointer; the other pointers
    // are optional and null.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    })?;
    let file = wide(MESSAGE_FILE);
    let result =
        set_value(hkey, "EventMessageFile", REG_EXPAND_SZ, as_bytes(&file)).and_then(|()| {
            set_value(
                hkey,
                "TypesSupported",
                REG_DWORD,
                &TYPES_SUPPORTED.to_le_bytes(),
            )
        });
    // SAFETY: opened above, closed once.
    unsafe { RegCloseKey(hkey) };
    result
}

fn set_value(hkey: HKEY, name: &str, kind: u32, data: &[u8]) -> io::Result<()> {
    let name = wide(name);
    // SAFETY: `hkey` is open for setting values; `name` is NUL-terminated; `data` is valid
    // for its length.
    check(unsafe {
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            kind,
            data.as_ptr(),
            data.len() as u32,
        )
    })
}

fn as_bytes(w: &[u16]) -> &[u8] {
    // SAFETY: any u16 slice is a valid byte slice of twice the length.
    unsafe { std::slice::from_raw_parts(w.as_ptr().cast(), std::mem::size_of_val(w)) }
}

/// Deletes the source's registry key; a missing key is fine. Needs Administrator.
pub fn unregister() -> io::Result<()> {
    let key = wide(REGISTRY_KEY);
    // SAFETY: `key` is NUL-terminated.
    match unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, key.as_ptr()) } {
        ERROR_FILE_NOT_FOUND => Ok(()),
        code => check(code),
    }
}

/// A handle for writing entries under the source.
pub struct EventLog(HANDLE);

// SAFETY: an event source handle may be used from any thread.
unsafe impl Send for EventLog {}
// SAFETY: as above; ReportEventW is thread-safe.
unsafe impl Sync for EventLog {}

impl EventLog {
    /// Opens the source; `None` when the Event Log refuses (entries are then skipped).
    pub fn open() -> Option<EventLog> {
        let source = wide(SOURCE);
        // SAFETY: `source` is NUL-terminated; a null server name is the local computer.
        let handle = unsafe { RegisterEventSourceW(std::ptr::null(), source.as_ptr()) };
        (!handle.is_null()).then_some(EventLog(handle))
    }

    /// Writes one entry with `message` as its text; failures are ignored (the same text
    /// also goes to the log file).
    pub fn report(&self, event: Event, message: &str) {
        let text = wide(message);
        let strings = [text.as_ptr()];
        // SAFETY: the handle is open; `strings` holds one NUL-terminated string that
        // outlives the call; no SID and no raw data.
        unsafe {
            ReportEventW(
                self.0,
                event.kind(),
                0,
                event.id(),
                std::ptr::null_mut(),
                1,
                0,
                strings.as_ptr(),
                std::ptr::null(),
            )
        };
    }
}

impl Drop for EventLog {
    fn drop(&mut self) {
        // SAFETY: opened by RegisterEventSourceW, closed once.
        unsafe { DeregisterEventSource(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_fit_the_message_file() {
        for e in [
            Event::Started,
            Event::Stopped,
            Event::StartFailed,
            Event::Crashed,
        ] {
            assert!((1..=1000).contains(&e.id()), "{e:?}");
        }
        assert_eq!(Event::StartFailed.kind(), EVENTLOG_ERROR_TYPE);
        assert_eq!(Event::Started.kind(), EVENTLOG_INFORMATION_TYPE);
    }

    #[test]
    fn registry_key_names_the_source() {
        assert!(REGISTRY_KEY.ends_with(&format!(r"\Application\{SOURCE}")));
        assert_eq!(&wide("ab")[..], &[b'a' as u16, b'b' as u16, 0]);
        assert_eq!(as_bytes(&wide("a")), &[b'a', 0, 0, 0]);
    }
}
