//! The node-wide memory budget's default: half of the machine's physical RAM, capped.

/// The budget when the machine's RAM cannot be read, and the cap otherwise.
const MAX_DEFAULT_MB: u32 = 4096;
/// The smallest default budget: one service at the default `memory_mb`.
const MIN_DEFAULT_MB: u32 = 64;

/// The default for `RuntimeConfig::max_memory_mb`: half of physical RAM in MiB, at most
/// 4096 and at least 64; 4096 when the RAM cannot be read.
pub fn default_memory_budget_mb() -> u32 {
    budget_from(total_ram_bytes())
}

fn budget_from(total: Option<u64>) -> u32 {
    match total {
        Some(bytes) => {
            let half_mb = bytes / 2 / (1024 * 1024);
            u32::try_from(half_mb)
                .unwrap_or(u32::MAX)
                .clamp(MIN_DEFAULT_MB, MAX_DEFAULT_MB)
        }
        None => MAX_DEFAULT_MB,
    }
}

#[cfg(windows)]
fn total_ram_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: `MEMORYSTATUSEX` is plain data; zeroed with `dwLength` set is the documented
    // way to call `GlobalMemoryStatusEx`, which only writes into the struct it is given.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut status) != 0).then_some(status.ullTotalPhys)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn total_ram_bytes() -> Option<u64> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// `MemTotal` from `/proc/meminfo` (given in kB), in bytes.
#[cfg(any(test, target_os = "linux", target_os = "android"))]
fn parse_meminfo(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let mut fields = line["MemTotal:".len()..].split_whitespace();
    let kb: u64 = fields.next()?.parse().ok()?;
    match fields.next() {
        Some("kB") | None => kb.checked_mul(1024),
        Some(_) => None,
    }
}

#[cfg(target_os = "macos")]
fn total_ram_bytes() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut len = size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer; `bytes` and `len` describe a buffer of
    // exactly that size, and no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == size_of::<u64>()).then_some(bytes)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos"
)))]
fn total_ram_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn budget_is_half_of_ram_capped() {
        assert_eq!(budget_from(Some(16 * GIB)), 4096);
        assert_eq!(budget_from(Some(6 * GIB)), 3072);
        assert_eq!(budget_from(Some(64 * GIB)), 4096);
        assert_eq!(budget_from(Some(100 << 20)), 64);
        assert_eq!(budget_from(None), 4096);
    }

    #[test]
    fn this_machine_has_ram() {
        assert!(total_ram_bytes().is_some_and(|b| b >= 64 << 20));
    }

    #[test]
    fn meminfo_mem_total() {
        let info = "MemTotal:        7864320 kB\nMemFree:          123456 kB\n";
        assert_eq!(parse_meminfo(info), Some(7_864_320 * 1024));
        assert_eq!(parse_meminfo("MemFree: 1 kB\n"), None);
        assert_eq!(parse_meminfo("MemTotal: x kB\n"), None);
    }
}
