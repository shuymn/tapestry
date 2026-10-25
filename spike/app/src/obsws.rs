//! Unattended OBS shutdown (design §4.3) through OBS 33's built-in
//! obs-websocket server (protocol v5) on 127.0.0.1: authenticate, stop the
//! stream, wait until no output is active, then close OBS's main window so
//! OBS exits normally (its final log line confirms teardown). OBS asks for
//! confirmation only while an output is active, so the close goes through.
//! Nothing is ever killed: if the server cannot be reached the caller
//! leaves OBS to the owner.
//!
//! The server password is read from OBS's own configuration at run time;
//! it is never printed, logged or written anywhere, and `report` adds it to
//! the export scan's forbidden names.
//!
//! The client is the minimum the four requests need: one TCP connection,
//! the HTTP upgrade, masked text frames out, unfragmented text frames in.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// obs-websocket's settings (`server_port`, `server_password`, ...).
pub fn config_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join("obs-studio")
            .join("plugin_config")
            .join("obs-websocket")
            .join("config.json"),
    )
}

/// (port, password if authentication is on) when the server is enabled.
pub fn server() -> Option<(u16, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(config_path()?).ok()?).ok()?;
    if v["server_enabled"].as_bool() != Some(true) {
        return None;
    }
    let port = v["server_port"].as_u64().unwrap_or(4455) as u16;
    let password = (v["auth_required"].as_bool() == Some(true))
        .then(|| v["server_password"].as_str().map(str::to_owned))
        .flatten();
    Some((port, password))
}

/// The server password, for the export scan only.
pub fn password() -> Option<String> {
    server().and_then(|s| s.1)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= c.len() {
                out.push(B64[(n >> shift) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// obs-websocket v5 authentication string for `password`, `salt` and
/// `challenge`: base64(sha256(base64(sha256(password + salt)) + challenge)).
pub fn auth(password: &str, salt: &str, challenge: &str) -> String {
    let secret = base64(&Sha256::digest(format!("{password}{salt}")));
    base64(&Sha256::digest(format!("{secret}{challenge}")))
}

struct Ws {
    s: TcpStream,
    next_id: u64,
}

impl Ws {
    fn connect(port: u16) -> Result<Self, String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
            .map_err(|e| format!("obs-websocket on 127.0.0.1:{port}: {e}"))?;
        s.set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut ws = Self { s, next_id: 0 };
        // The key only has to be 16 base64-encoded bytes.
        let key = base64(&crate::timebase::qpc().to_le_bytes().repeat(2));
        let req = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: obswebsocket.json\r\n\r\n"
        );
        ws.s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            ws.s.read_exact(&mut b).map_err(|e| e.to_string())?;
            head.push(b[0]);
            if head.len() > 8192 {
                return Err("obs-websocket: oversized handshake".into());
            }
        }
        if !head.starts_with(b"HTTP/1.1 101") {
            return Err("obs-websocket refused the upgrade".into());
        }
        Ok(ws)
    }

    fn send(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let payload = v.to_string().into_bytes();
        let mut f = vec![0x81u8];
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65_536 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = (crate::timebase::qpc() as u32).to_le_bytes();
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&f).map_err(|e| e.to_string())
    }

    /// The next text message (control frames are skipped; a ping gets its
    /// pong).
    fn recv(&mut self) -> Result<serde_json::Value, String> {
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h).map_err(|e| e.to_string())?;
            let op = h[0] & 0x0f;
            let mut len = u64::from(h[1] & 0x7f);
            if len == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u64::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.s.read_exact(&mut b).map_err(|e| e.to_string())?;
                len = u64::from_be_bytes(b);
            }
            if len > 1 << 24 {
                return Err("obs-websocket: oversized frame".into());
            }
            let mut p = vec![0u8; len as usize];
            self.s.read_exact(&mut p).map_err(|e| e.to_string())?;
            match op {
                0x1 => return serde_json::from_slice(&p).map_err(|e| e.to_string()),
                0x8 => return Err("obs-websocket closed the connection".into()),
                0x9 => {
                    let mut f = vec![0x8a, 0x80 | p.len() as u8, 0, 0, 0, 0];
                    f.extend_from_slice(&p);
                    self.s.write_all(&f).map_err(|e| e.to_string())?;
                }
                _ => {}
            }
        }
    }

    /// Hello → Identify (with authentication) → Identified.
    fn identify(&mut self, password: Option<&str>) -> Result<(), String> {
        let hello = self.recv()?;
        if hello["op"] != 0 {
            return Err("obs-websocket: no Hello".into());
        }
        let mut d = serde_json::json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
        if let Some(a) = hello["d"].get("authentication") {
            let password = password.ok_or("obs-websocket wants a password; none configured")?;
            let (Some(salt), Some(challenge)) = (a["salt"].as_str(), a["challenge"].as_str())
            else {
                return Err("obs-websocket: malformed authentication".into());
            };
            d["authentication"] = auth(password, salt, challenge).into();
        }
        self.send(&serde_json::json!({ "op": 1, "d": d }))?;
        let ack = self.recv()?;
        if ack["op"] != 2 {
            return Err("obs-websocket: authentication failed".into());
        }
        Ok(())
    }

    fn request(&mut self, kind: &str) -> Result<serde_json::Value, String> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        self.send(&serde_json::json!({
            "op": 6, "d": { "requestType": kind, "requestId": id }
        }))?;
        loop {
            let m = self.recv()?;
            if m["op"] == 7 && m["d"]["requestId"] == id.as_str() {
                return Ok(m["d"].clone());
            }
        }
    }
}

/// Stops OBS's stream and waits until no output is active.
pub fn stop_stream(limit: Duration) -> Result<(), String> {
    let (port, password) = server().ok_or("obs-websocket is not enabled in OBS's settings")?;
    let mut ws = Ws::connect(port)?;
    ws.identify(password.as_deref())?;
    let active = |ws: &mut Ws| -> Result<bool, String> {
        let r = ws.request("GetStreamStatus")?;
        Ok(r["responseData"]["outputActive"].as_bool().unwrap_or(false))
    };
    if active(&mut ws)? {
        let r = ws.request("StopStream")?;
        if r["requestStatus"]["result"] != true {
            return Err(format!(
                "StopStream failed (code {})",
                r["requestStatus"]["code"]
            ));
        }
    }
    let deadline = Instant::now() + limit;
    while active(&mut ws)? {
        if Instant::now() > deadline {
            return Err("the stream did not stop".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // No other output may stay active either (a recording would keep OBS
    // asking on close).
    let rec = ws.request("GetRecordStatus")?;
    if rec["responseData"]["outputActive"] == true {
        return Err("a recording output is active in OBS".into());
    }
    Ok(())
}

/// Closes the visible top-level windows of process `pid` (OBS's main
/// window) with WM_CLOSE, the same as the window's close button.
pub fn close_main_window(pid: u32) -> bool {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
        WM_CLOSE,
    };
    struct Find {
        pid: u32,
        found: Vec<HWND>,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> windows::core::BOOL {
        // SAFETY: `l` is the `Find` passed below, alive for the enumeration.
        let f = unsafe { &mut *(l.0 as *mut Find) };
        let mut pid = 0u32;
        // SAFETY: plain window queries.
        unsafe {
            GetWindowThreadProcessId(h, Some(&mut pid));
            if pid == f.pid
                && IsWindowVisible(h).as_bool()
                && GetWindow(h, GW_OWNER).map_or(true, |o| o.is_invalid())
            {
                f.found.push(h);
            }
        }
        true.into()
    }
    let mut f = Find { pid, found: vec![] };
    // SAFETY: the callback only touches `f`.
    let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize)) };
    for h in &f.found {
        // SAFETY: posting a message to a window handle.
        let _ = unsafe { PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
    !f.found.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn authentication_follows_the_v5_recipe() {
        // Fixed inputs; the expected value is the recipe computed step by
        // step with the same primitives.
        let secret = base64(&Sha256::digest(b"supersecretpasswordsalt"));
        let expect = base64(&Sha256::digest(format!("{secret}challenge")));
        assert_eq!(auth("supersecretpassword", "salt", "challenge"), expect);
        assert_eq!(expect.len(), 44);
    }
}
