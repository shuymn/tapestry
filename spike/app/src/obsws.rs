//! Unattended OBS shutdown (design §4.3) through OBS 33's built-in
//! obs-websocket server (protocol v5) on 127.0.0.1: authenticate, stop the
//! stream, wait until no output is active, then close OBS's main window so
//! OBS exits normally (its final log line confirms teardown). OBS asks for
//! confirmation only while an output is active, so the close goes through.
//! Nothing is ever killed: if the server cannot be reached the caller
//! leaves OBS to the owner. One absolute deadline bounds the whole exchange,
//! and a guard runs before every write (`measure` checks that the OBS it
//! started is still the only OBS), so nothing reaches another OBS.
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

/// The server password in OBS's current configuration, for the export scan
/// only. It is deliberately not copied into the run (no file may hold it),
/// so the scan covers the password configured when `report` runs, not one
/// changed or disabled since the run.
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

/// Checked before anything is written to the server: `Err` stops the
/// exchange with nothing more sent (`measure`: the OBS it started is still
/// running and is the only OBS).
pub type Guard<'a> = &'a mut dyn FnMut() -> Result<(), String>;

/// One connection under one absolute deadline: connecting, the upgrade,
/// every read and every write get only the time left, so neither pings nor
/// trickled bytes extend it.
struct Ws<'a> {
    s: TcpStream,
    next_id: u64,
    deadline: Instant,
    guard: Guard<'a>,
}

fn left(deadline: Instant) -> Result<Duration, String> {
    let d = deadline.saturating_duration_since(Instant::now());
    if d.is_zero() {
        Err("obs-websocket: out of time".into())
    } else {
        Ok(d)
    }
}

/// A timed-out or interrupted call: try again with the time left.
fn retry(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
    matches!(e.kind(), WouldBlock | TimedOut | Interrupted)
}

impl<'a> Ws<'a> {
    fn connect(port: u16, deadline: Instant, guard: Guard<'a>) -> Result<Self, String> {
        guard()?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let s = TcpStream::connect_timeout(&addr, left(deadline)?)
            .map_err(|e| format!("obs-websocket on 127.0.0.1:{port}: {e}"))?;
        let mut ws = Self {
            s,
            next_id: 0,
            deadline,
            guard,
        };
        // The key only has to be 16 base64-encoded bytes.
        let key = base64(&crate::timebase::qpc().to_le_bytes().repeat(2));
        let req = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: obswebsocket.json\r\n\r\n"
        );
        ws.write(req.as_bytes())?;
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            ws.read(&mut b)?;
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

    /// Fills `buf` before the deadline.
    fn read(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let mut n = 0;
        while n < buf.len() {
            self.s
                .set_read_timeout(Some(left(self.deadline)?))
                .map_err(|e| e.to_string())?;
            match self.s.read(&mut buf[n..]) {
                Ok(0) => return Err("obs-websocket closed the connection".into()),
                Ok(k) => n += k,
                Err(e) if retry(&e) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }

    /// Writes `buf` before the deadline, after the guard allows it.
    fn write(&mut self, buf: &[u8]) -> Result<(), String> {
        (self.guard)()?;
        let mut n = 0;
        while n < buf.len() {
            self.s
                .set_write_timeout(Some(left(self.deadline)?))
                .map_err(|e| e.to_string())?;
            match self.s.write(&buf[n..]) {
                Ok(0) => return Err("obs-websocket closed the connection".into()),
                Ok(k) => n += k,
                Err(e) if retry(&e) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
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
        self.write(&f)
    }

    /// The next text message (control frames are skipped; a ping gets its
    /// pong).
    fn recv(&mut self) -> Result<serde_json::Value, String> {
        loop {
            let mut h = [0u8; 2];
            self.read(&mut h)?;
            let op = h[0] & 0x0f;
            let mut len = u64::from(h[1] & 0x7f);
            if len == 126 {
                let mut b = [0u8; 2];
                self.read(&mut b)?;
                len = u64::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.read(&mut b)?;
                len = u64::from_be_bytes(b);
            }
            if len > 1 << 24 {
                return Err("obs-websocket: oversized frame".into());
            }
            let mut p = vec![0u8; len as usize];
            self.read(&mut p)?;
            match op {
                0x1 => return serde_json::from_slice(&p).map_err(|e| e.to_string()),
                0x8 => return Err("obs-websocket closed the connection".into()),
                0x9 if p.len() < 126 => {
                    let mut f = vec![0x8a, 0x80 | p.len() as u8, 0, 0, 0, 0];
                    f.extend_from_slice(&p);
                    self.write(&f)?;
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

/// Stops OBS's stream through the server on `port` and waits until no
/// output is active, all before `deadline`; `guard` is checked before every
/// write.
pub fn stop_stream(
    port: u16,
    password: Option<&str>,
    deadline: Instant,
    guard: Guard<'_>,
) -> Result<(), String> {
    let mut ws = Ws::connect(port, deadline, guard)?;
    ws.identify(password)?;
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
    while active(&mut ws)? {
        std::thread::sleep(left(deadline)?.min(Duration::from_millis(200)));
    }
    // No other output may stay active either (a recording would keep OBS
    // asking on close).
    let rec = ws.request("GetRecordStatus")?;
    if rec["responseData"]["outputActive"] == true {
        return Err("a recording output is active in OBS".into());
    }
    Ok(())
}

/// Closes the unowned top-level windows of process `pid` whose title
/// starts with "OBS" (OBS's main window, hidden or not: unattended runs
/// keep OBS in the tray) with WM_CLOSE, the
/// same as the window's close button.
pub fn close_main_window(pid: u32) -> bool {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetWindow, GetWindowTextW, GetWindowThreadProcessId, PostMessageW,
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
            let mut title = [0u16; 64];
            let n = GetWindowTextW(h, &mut title).max(0) as usize;
            if pid == f.pid
                && String::from_utf16_lossy(&title[..n]).starts_with("OBS")
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

    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A fake server on a free port; `serve` gets the accepted connection.
    fn fake(serve: impl FnOnce(TcpStream) + Send + 'static) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((s, _)) = l.accept() {
                serve(s);
            }
        });
        port
    }

    fn handshake(s: &mut TcpStream) {
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        // With the upgrade headers: a bare 101 can make a loopback HTTP
        // filter drop the frames that follow.
        s.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .unwrap();
    }

    fn text(s: &mut TcpStream, v: serde_json::Value) {
        let p = v.to_string().into_bytes();
        let mut f = vec![0x81, p.len() as u8];
        f.extend_from_slice(&p);
        s.write_all(&f).unwrap();
    }

    /// The next client frame's payload (unmasked), `None` once closed.
    fn frame(s: &mut TcpStream) -> Option<Vec<u8>> {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).ok()?;
        let len = usize::from(h[1] & 0x7f);
        assert!(len < 126);
        let mut m = [0u8; 4];
        s.read_exact(&mut m).ok()?;
        let mut p = vec![0u8; len];
        s.read_exact(&mut p).ok()?;
        Some(p.iter().enumerate().map(|(i, b)| b ^ m[i % 4]).collect())
    }

    #[test]
    fn the_deadline_bounds_trickled_bytes_and_pings() {
        // A handshake trickled one byte at a time without end.
        let trickle = fake(|mut s| {
            for _ in 0..200 {
                if s.write_all(b"H").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        // A server that answers the handshake and Identify, then only pings.
        let pings = fake(|mut s| {
            handshake(&mut s);
            text(&mut s, serde_json::json!({"op": 0, "d": {"rpcVersion": 1}}));
            let _ = frame(&mut s);
            text(&mut s, serde_json::json!({"op": 2, "d": {}}));
            for _ in 0..200 {
                if s.write_all(&[0x89, 0]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        for port in [trickle, pings] {
            let t0 = Instant::now();
            let deadline = t0 + Duration::from_secs(1);
            let e = stop_stream(port, None, deadline, &mut || Ok(())).unwrap_err();
            assert!(
                t0.elapsed() < Duration::from_millis(2500),
                "{e} after {:?}",
                t0.elapsed()
            );
        }
    }

    /// A server whose stream is active: records every request it receives.
    fn active_stream(seen: mpsc::Sender<String>) -> u16 {
        fake(move |mut s| {
            handshake(&mut s);
            text(&mut s, serde_json::json!({"op": 0, "d": {"rpcVersion": 1}}));
            let _ = frame(&mut s);
            text(&mut s, serde_json::json!({"op": 2, "d": {}}));
            while let Some(p) = frame(&mut s) {
                let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
                let kind = v["d"]["requestType"].as_str().unwrap_or("").to_owned();
                let _ = seen.send(kind.clone());
                text(
                    &mut s,
                    serde_json::json!({"op": 7, "d": {"requestId": v["d"]["requestId"],
                        "requestStatus": {"result": true},
                        "responseData": {"outputActive": kind == "GetStreamStatus"}}}),
                );
            }
        })
    }

    #[test]
    fn a_failing_guard_sends_nothing_more() {
        // Before the connection: no byte reaches the server.
        let (tx, rx) = mpsc::channel();
        let port = fake(move |mut s| {
            s.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let mut b = [0u8; 1];
            let _ = tx.send(matches!(s.read(&mut b), Ok(n) if n > 0));
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut never = || Err("another OBS is running".to_string());
        let e = stop_stream(port, None, deadline, &mut never).unwrap_err();
        assert!(e.contains("another OBS"), "{e}");
        // The fake server never got a connection, so it reports nothing.
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err());
        // Failing at the fifth check (connect, handshake, Identify, the
        // first GetStreamStatus, then StopStream): StopStream is never sent.
        let (tx, rx) = mpsc::channel();
        let port = active_stream(tx);
        let mut writes = 0;
        let mut guard = || {
            writes += 1;
            if writes >= 5 {
                Err("the OBS this run started has exited".to_string())
            } else {
                Ok(())
            }
        };
        let e = stop_stream(port, None, deadline, &mut guard).unwrap_err();
        assert!(e.contains("exited"), "{e}");
        let seen: Vec<String> = rx.try_iter().collect();
        assert_eq!(seen, ["GetStreamStatus"]);
        // With the guard passing, the stream is stopped.
        let (tx, rx) = mpsc::channel();
        let port = active_stream(tx);
        let deadline = Instant::now() + Duration::from_millis(1500);
        let e = stop_stream(port, None, deadline, &mut || Ok(())).unwrap_err();
        // The fake reports the stream active forever: out of time.
        assert!(e.contains("out of time"), "{e}");
        assert!(rx.try_iter().any(|k| k == "StopStream"));
    }
}
