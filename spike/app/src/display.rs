//! Active displays: exact refresh rational (V3 needs it, not winit's whole
//! hertz), resolution, source GDI device name (`\\.\DISPLAYn`, which winit
//! reports as the monitor name) and the monitor's friendly name, from
//! QueryDisplayConfig. The window's monitor is matched by GDI name, so two
//! monitors with the same whole-hertz rate cannot be confused.

use serde::Serialize;

/// One active display path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DisplayPath {
    /// Source GDI device name, e.g. `\\.\DISPLAY1`.
    pub gdi_name: String,
    /// Monitor friendly name (console output only; not written to files).
    #[serde(skip)]
    pub friendly: String,
    pub num: u32,
    pub den: u32,
    pub width: u32,
    pub height: u32,
}

impl DisplayPath {
    pub fn hz(&self) -> f64 {
        f64::from(self.num) / f64::from(self.den.max(1))
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}x{} @ {}/{} Hz ({:.5} Hz)",
            self.gdi_name,
            self.width,
            self.height,
            self.num,
            self.den,
            self.hz()
        )
    }
}

/// The refresh rational for the monitor named `monitor` (winit's monitor
/// name is its GDI device name). Without a name match, a unique path whose
/// rate truncates to winit's whole hertz; otherwise winit's value.
pub fn pick_refresh(monitor: Option<&str>, winit_mhz: u32, paths: &[DisplayPath]) -> (u32, u32) {
    let valid = paths.iter().filter(|p| p.den > 0);
    if let Some(name) = monitor
        && let Some(p) = valid
            .clone()
            .find(|p| p.gdi_name.eq_ignore_ascii_case(name))
    {
        return (p.num, p.den);
    }
    let whole: Vec<&DisplayPath> = valid
        .filter(|p| p.num / p.den == winit_mhz / 1000)
        .collect();
    match whole.as_slice() {
        [p] => (p.num, p.den),
        _ => (winit_mhz, 1000),
    }
}

/// The one active display whose GDI or friendly name contains `part`
/// (case-insensitive); an error lists the displays when none or several
/// match.
pub fn find_display<'a>(part: &str, paths: &'a [DisplayPath]) -> Result<&'a DisplayPath, String> {
    let part = part.to_lowercase();
    let hits: Vec<&DisplayPath> = paths
        .iter()
        .filter(|p| {
            p.gdi_name.to_lowercase().contains(&part) || p.friendly.to_lowercase().contains(&part)
        })
        .collect();
    match hits.as_slice() {
        [p] => Ok(p),
        _ => Err(format!(
            "--monitor {part:?} matches {} displays; active displays:\n{}",
            hits.len(),
            paths
                .iter()
                .map(|p| format!("  {} \"{}\"", p.describe(), p.friendly))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

fn wide(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// Every active display path (empty if the query fails).
pub fn active_paths() -> Vec<DisplayPath> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: the buffers are sized from GetDisplayConfigBufferSizes, the
    // counts are updated to what QueryDisplayConfig wrote, each device-info
    // request is a correctly sized, typed packet, and the mode index is
    // bounds-checked before the source-mode union is read.
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return vec![];
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut np,
            paths.as_mut_ptr(),
            &mut nm,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return vec![];
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        paths
            .iter()
            .map(|p| {
                let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: p.sourceInfo.adapterId,
                        id: p.sourceInfo.id,
                    },
                    ..Default::default()
                };
                let gdi_name = if DisplayConfigGetDeviceInfo(&mut source.header) == 0 {
                    wide(&source.viewGdiDeviceName)
                } else {
                    String::new()
                };
                let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                        adapterId: p.targetInfo.adapterId,
                        id: p.targetInfo.id,
                    },
                    ..Default::default()
                };
                let friendly = if DisplayConfigGetDeviceInfo(&mut target.header) == 0 {
                    wide(&target.monitorFriendlyDeviceName)
                } else {
                    String::new()
                };
                let (width, height) = modes
                    .get(p.sourceInfo.Anonymous.modeInfoIdx as usize)
                    .map_or((0, 0), |m| {
                        let s = m.Anonymous.sourceMode;
                        (s.width, s.height)
                    });
                DisplayPath {
                    gdi_name,
                    friendly,
                    num: p.targetInfo.refreshRate.Numerator,
                    den: p.targetInfo.refreshRate.Denominator,
                    width,
                    height,
                }
            })
            .collect()
    }
}

/// `displays`: every active display with its exact mode.
pub fn print_displays() {
    let paths = active_paths();
    if paths.is_empty() {
        println!("displays: none found (QueryDisplayConfig failed)");
    }
    for p in &paths {
        let exact = if p.den > 0 && p.num % p.den == 0 {
            "exact"
        } else {
            "not whole"
        };
        println!("{} \"{}\" [{exact}]", p.describe(), p.friendly);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str, friendly: &str, num: u32) -> DisplayPath {
        DisplayPath {
            gdi_name: name.into(),
            friendly: friendly.into(),
            num,
            den: 1000,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn same_whole_hertz_on_two_monitors_is_matched_by_name() {
        let paths = [
            path(r"\\.\DISPLAY1", "Panel A", 240_000),
            path(r"\\.\DISPLAY2", "Panel B", 240_085),
        ];
        assert_eq!(
            pick_refresh(Some(r"\\.\DISPLAY2"), 240_000, &paths),
            (240_085, 1000)
        );
        assert_eq!(
            pick_refresh(Some(r"\\.\display1"), 240_000, &paths),
            (240_000, 1000)
        );
        // No name match and two candidates: winit's value, not a guess.
        assert_eq!(pick_refresh(None, 240_000, &paths), (240_000, 1000));
        // A unique whole-hertz match is still used without a name.
        assert_eq!(
            pick_refresh(None, 59_000, &[path(r"\\.\DISPLAY1", "", 59_951)]),
            (59_951, 1000)
        );
    }

    #[test]
    fn monitor_is_found_by_gdi_or_friendly_name() {
        let paths = [
            path(r"\\.\DISPLAY1", "Panel A", 240_085),
            path(r"\\.\DISPLAY2", "Panel B", 60_000),
        ];
        assert_eq!(find_display("panel b", &paths).unwrap().num, 60_000);
        assert_eq!(find_display("DISPLAY1", &paths).unwrap().num, 240_085);
        let e = find_display("panel", &paths).unwrap_err();
        assert!(
            e.contains("matches 2 displays") && e.contains("Panel B"),
            "{e}"
        );
        assert!(find_display("nothing", &paths).is_err());
    }
}
