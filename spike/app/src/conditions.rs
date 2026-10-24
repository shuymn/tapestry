//! Measurement conditions (design §3.5): `conditions.json` is collected on
//! the measurement PC and stays there; only the whitelisted public fields
//! (`spike_core::export::CONDITION_KEYS`) leave it, through the export.
//! Names that must never leave (user, computer, monitor and audio endpoint
//! names) are kept in `private_names`, which the export scan searches for.
//! Owner-reported fields come from `owner-notes.toml`.

use crate::audio;
use crate::display::active_paths;
use serde_json::{Map, Value, json};
use std::path::Path;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
    IDXGIDevice, IDXGIFactory1, IDXGIFactory5,
};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerGetActiveScheme, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::core::{HSTRING, Interface};

pub const CONDITIONS: &str = "conditions.json";
/// Fields the owner fills (`owner-notes.toml`); the public ones are in
/// `CONDITION_KEYS`.
pub const OWNER_FIELDS: &[&str] = &[
    "pc_form",
    "vrr",
    "display_connection",
    "gpu_display_attached",
    "audio_enhancements",
    "audio_device_class",
    "game_visible",
    "audio_present",
];

fn reg_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its byte length are valid.
    let r = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    r.is_ok().then(|| {
        let n = (len as usize / 2).saturating_sub(1);
        String::from_utf16_lossy(&buf[..n]).trim().to_owned()
    })
}

fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let mut v = 0u32;
    let mut len = 4u32;
    // SAFETY: a DWORD out value and its size.
    let r = unsafe {
        RegGetValueW(
            root,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut v as *mut u32).cast()),
            Some(&mut len),
        )
    };
    r.is_ok().then_some(v)
}

fn power_scheme() -> String {
    let mut guid = std::ptr::null_mut();
    // SAFETY: the returned GUID is freed with LocalFree.
    let id = unsafe {
        if PowerGetActiveScheme(None, &mut guid).is_err() || guid.is_null() {
            return "unknown".into();
        }
        let g = *guid;
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            guid.cast(),
        )));
        format!("{g:?}").to_lowercase()
    };
    // Built-in schemes by name; anything else is reported as custom (a GUID
    // is not needed off the PC).
    match id.as_str() {
        "381b4222-f694-41f0-9685-ff5bb260df2e" => "balanced",
        "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c" => "high-performance",
        "a1841308-3541-4fab-bc81-f71556f20b4a" => "power-saver",
        "e9a42b02-d5df-448d-aa00-03f14749eb61" => "ultimate-performance",
        _ => "custom",
    }
    .into()
}

type Adapter = (String, u64, Option<String>);

/// Hardware DXGI adapters: (name, dedicated VRAM MB, user-mode driver
/// version), and tearing support.
fn dxgi() -> (Vec<Adapter>, Option<bool>) {
    // SAFETY: DXGI factory queries.
    unsafe {
        let Ok(f) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return (vec![], None);
        };
        let mut out = vec![];
        let mut i = 0;
        while let Ok(a) = f.EnumAdapters1(i) {
            i += 1;
            if let Ok(d) = a.GetDesc1()
                && d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
            {
                let n = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
                let driver = a.CheckInterfaceSupport(&IDXGIDevice::IID).ok().map(|v| {
                    let v = v as u64;
                    format!(
                        "{}.{}.{}.{}",
                        v >> 48,
                        (v >> 32) & 0xffff,
                        (v >> 16) & 0xffff,
                        v & 0xffff
                    )
                });
                out.push((
                    String::from_utf16_lossy(&d.Description[..n]),
                    d.DedicatedVideoMemory as u64 / (1024 * 1024),
                    driver,
                ));
            }
        }
        let tearing = f.cast::<IDXGIFactory5>().ok().map(|f5| {
            let mut allow = 0i32;
            f5.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&mut allow as *mut i32).cast(),
                4,
            )
            .is_ok()
                && allow != 0
        });
        (out, tearing)
    }
}

/// Owner fields from `owner-notes.toml` (`key = value` lines; values are
/// `true`/`false`, numbers or quoted strings; `#` starts a comment).
pub fn owner_notes(path: &Path) -> Map<String, Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Map::new();
    };
    text.lines()
        .filter_map(|l| {
            let l = l.split('#').next()?.trim();
            let (k, v) = l.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            if !OWNER_FIELDS.contains(&k) || v.is_empty() {
                return None;
            }
            let value = match v {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 => {
                    Value::String(v[1..v.len() - 1].to_owned())
                }
                _ => v.parse::<i64>().map_or(Value::Null, Value::from),
            };
            (!value.is_null()).then(|| (k.to_owned(), value))
        })
        .collect()
}

/// Collects the conditions. `audio_device` is the endpoint the run uses.
pub fn collect(audio_device: Option<&str>, bga: Option<&Path>) -> Map<String, Value> {
    let mut c = Map::new();
    let mut private: Vec<String> = vec![];
    for var in ["USERNAME", "COMPUTERNAME", "USERDOMAIN"] {
        if let Ok(v) = std::env::var(var) {
            private.push(v);
        }
    }
    c.insert("commit".into(), json!(env!("SPIKE_COMMIT")));
    c.insert("dirty".into(), json!(env!("SPIKE_TREE") != "clean"));
    c.insert("build_profile".into(), json!(env!("SPIKE_PROFILE")));
    let chart = spike_core::chart::Chart::generate(spike_core::chart::DEFAULT_SEED);
    c.insert("chart_hash".into(), json!(chart.content_hash()));
    if let Some(b) = bga {
        let sha = std::fs::read_to_string(b.with_extension("mp4.sha256"))
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_owned));
        c.insert("bga_sha256".into(), json!(sha));
        c.insert("bga_codec".into(), json!("h264"));
    }
    c.insert("wgpu_version".into(), json!("30"));
    c.insert(
        "ffmpeg_version".into(),
        json!(crate::ffmpeg_version_string()),
    );
    let cpu = reg_string(
        HKEY_LOCAL_MACHINE,
        r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        "ProcessorNameString",
    );
    c.insert("cpu_brand".into(), json!(cpu));
    c.insert(
        "cpu_cores".into(),
        json!(std::thread::available_parallelism().map_or(0, |n| n.get())),
    );
    let mut mem = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a sized MEMORYSTATUSEX.
    if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
        c.insert(
            "ram_gb".into(),
            json!((mem.ullTotalPhys as f64 / (1u64 << 30) as f64).round() as u64),
        );
    }
    let (adapters, tearing) = dxgi();
    if let Some((name, vram, driver)) = adapters.first() {
        c.insert("gpu_adapter".into(), json!(name));
        c.insert("gpu_vram_mb".into(), json!(vram));
        c.insert("gpu_driver".into(), json!(driver));
    }
    c.insert("tearing_support".into(), json!(tearing));
    let nt = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    c.insert(
        "windows_build".into(),
        json!(reg_string(HKEY_LOCAL_MACHINE, nt, "CurrentBuildNumber")),
    );
    c.insert(
        "windows_ubr".into(),
        json!(reg_dword(HKEY_LOCAL_MACHINE, nt, "UBR")),
    );
    let mut ps = SYSTEM_POWER_STATUS::default();
    // SAFETY: a valid out struct.
    if unsafe { GetSystemPowerStatus(&mut ps) }.is_ok() {
        let source = match ps.ACLineStatus {
            1 => "ac",
            0 => "battery",
            _ => "unknown",
        };
        c.insert("power_source".into(), json!(source));
    }
    c.insert("power_scheme".into(), json!(power_scheme()));
    // The effective power mode needs a notification callback; not read.
    c.insert("power_mode".into(), json!("unknown"));
    let game_mode = reg_dword(
        HKEY_CURRENT_USER,
        r"Software\Microsoft\GameBar",
        "AutoGameModeEnabled",
    )
    .map_or("default-on".to_owned(), |v| {
        if v == 0 { "off" } else { "on" }.to_owned()
    });
    c.insert("game_mode".into(), json!(game_mode));
    let hags = reg_dword(
        HKEY_LOCAL_MACHINE,
        r"SYSTEM\CurrentControlSet\Control\GraphicsDrivers",
        "HwSchMode",
    )
    .map(|v| if v == 2 { "on" } else { "off" });
    c.insert("hags".into(), json!(hags));
    let paths = active_paths();
    c.insert("monitor_count".into(), json!(paths.len()));
    // Every active display's exact mode, by GDI name (`DISPLAY1`, …);
    // friendly names are private.
    let displays: Vec<Value> = paths
        .iter()
        .map(|p| {
            private.push(p.friendly.clone());
            json!({
                "gdi_name": p.gdi_name.trim_start_matches(r"\\.\"),
                "width": p.width,
                "height": p.height,
                "refresh_num": p.num,
                "refresh_den": p.den,
            })
        })
        .collect();
    c.insert("displays".into(), Value::Array(displays));
    match audio::endpoint_info(audio_device) {
        Ok(e) => {
            private.push(e.name.clone());
            c.insert(
                "audio_mix_format".into(),
                json!(format!("float32 {} Hz {} ch", e.rate_hz, e.channels)),
            );
            c.insert("audio_share_mode".into(), json!("shared"));
            if let Some(p) = e.periods {
                c.insert("audio_period_min".into(), json!(p.min));
                c.insert("audio_period_default".into(), json!(p.default));
            }
        }
        Err(e) => {
            c.insert("audio_error".into(), json!(e));
        }
    }
    c.insert("input".into(), json!("synthetic only"));
    private.retain(|n| !n.trim().is_empty());
    private.sort();
    private.dedup();
    c.insert("private_names".into(), json!(private));
    c
}

/// `conditions [--out DIR] [--audio-device PART] [--bga FILE]`: writes
/// `conditions.json` and prints the public fields.
pub fn run(out: &Path, audio_device: Option<&str>, bga: Option<&Path>) -> Result<(), String> {
    let mut c = collect(audio_device, bga);
    let notes = owner_notes(Path::new("owner-notes.toml"));
    for (k, v) in notes {
        c.insert(k, v);
    }
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let path = out.join(CONDITIONS);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&c).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let public = spike_core::export::sanitize_conditions(&c);
    println!(
        "{}",
        serde_json::to_string_pretty(&public).map_err(|e| e.to_string())?
    );
    println!(
        "conditions: written to {} (private; only the fields above leave the PC)",
        path.display()
    );
    Ok(())
}
