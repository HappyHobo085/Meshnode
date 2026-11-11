//! The machine's name, offered as this device's name on First run (spec §6.2).

/// The word used when the host name can't be read; matches the page's own fallback.
#[cfg(windows)]
const FALLBACK: &str = "Windows PC";
#[cfg(target_os = "macos")]
const FALLBACK: &str = "Mac";
#[cfg(not(any(windows, target_os = "macos")))]
const FALLBACK: &str = "Linux PC";

/// Longest name offered (the name field's own limit).
const MAX_CHARS: usize = 64;

/// This machine's host name, or the platform's word when it can't be read. Never empty.
pub fn default_device_name() -> String {
    clean(raw_hostname()).unwrap_or_else(|| FALLBACK.to_string())
}

/// A raw host name made presentable: trimmed, without a DNS domain ("desk.lan" → "desk"),
/// at most [`MAX_CHARS`] characters; `None` when nothing usable is left.
pub fn clean(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let name = raw.trim();
    let name = name.split('.').next().unwrap_or(name).trim();
    if name.is_empty() || name.eq_ignore_ascii_case("localhost") {
        return None;
    }
    Some(name.chars().take(MAX_CHARS).collect())
}

#[cfg(windows)]
fn raw_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[cfg(not(windows))]
fn raw_hostname() -> Option<String> {
    // GUI sessions often lack $HOSTNAME, so also ask the kernel's file and `hostname`.
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_device_name_is_never_empty() {
        let name = default_device_name();
        assert!(!name.trim().is_empty());
        assert!(name.chars().count() <= MAX_CHARS);
    }

    #[test]
    fn clean_cases() {
        assert_eq!(
            clean(Some("DESKTOP-AB12CD\r\n".into())).as_deref(),
            Some("DESKTOP-AB12CD")
        );
        assert_eq!(clean(Some("desk.lan\n".into())).as_deref(), Some("desk"));
        assert_eq!(clean(Some("  ".into())), None);
        assert_eq!(clean(Some("localhost".into())), None);
        assert_eq!(clean(None), None);
        assert_eq!(
            clean(Some("x".repeat(100))).map(|s| s.len()),
            Some(MAX_CHARS)
        );
    }
}
