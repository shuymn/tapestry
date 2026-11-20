//! Small Win32 helpers the test device's commands share.

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// `s` as a nul-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The UTF-16 text of `buf` up to its first nul.
pub fn until_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(buf.get(..end).unwrap_or_default())
}

/// The strings of a nul-separated list that ends with an empty string.
pub fn multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The host's performance counter.
pub fn qpc() -> i64 {
    let mut t = 0;
    // SAFETY: one i64 written.
    let _ = unsafe { QueryPerformanceCounter(&mut t) };
    t
}

/// The performance counter's frequency in Hz.
pub fn qpc_freq() -> i64 {
    let mut f = 0;
    // SAFETY: one i64 written.
    let _ = unsafe { QueryPerformanceFrequency(&mut f) };
    f.max(1)
}

/// A handle closed on drop.
pub struct Owned(pub HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is owned by this value.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Owned {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_cross_utf16_both_ways() {
        let w = wide("COM5");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(until_nul(&w), "COM5");
        let list: Vec<u16> = "HID_DEVICE_UP:0001_U:0005\0HID_DEVICE\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(multi_sz(&list), ["HID_DEVICE_UP:0001_U:0005", "HID_DEVICE"]);
        assert!(qpc_freq() > 0);
    }
}
