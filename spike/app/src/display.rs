//! Exact refresh rate of the window's display (V3 needs the rational, not
//! winit's whole hertz). QueryDisplayConfig lists every active path with
//! its refresh rational and source GDI device name (`\\.\DISPLAYn`); the
//! window's monitor is matched by that name, so two monitors with the same
//! whole-hertz rate cannot be confused.

/// One active display path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayPath {
    /// Source GDI device name, e.g. `\\.\DISPLAY1`.
    pub gdi_name: String,
    pub num: u32,
    pub den: u32,
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

/// Every active display path (empty if the query fails).
pub fn active_paths() -> Vec<DisplayPath> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;
    let (mut np, mut nm) = (0u32, 0u32);
    // SAFETY: the buffers are sized from GetDisplayConfigBufferSizes, the
    // counts are updated to what QueryDisplayConfig wrote, and each source
    // name request is a correctly sized, typed packet.
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
        paths
            .iter()
            .map(|p| {
                let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                        adapterId: p.sourceInfo.adapterId,
                        id: p.sourceInfo.id,
                    },
                    ..Default::default()
                };
                let gdi_name = if DisplayConfigGetDeviceInfo(&mut name.header) == 0 {
                    let n = &name.viewGdiDeviceName;
                    let len = n.iter().position(|&c| c == 0).unwrap_or(n.len());
                    String::from_utf16_lossy(&n[..len])
                } else {
                    String::new()
                };
                DisplayPath {
                    gdi_name,
                    num: p.targetInfo.refreshRate.Numerator,
                    den: p.targetInfo.refreshRate.Denominator,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str, num: u32, den: u32) -> DisplayPath {
        DisplayPath {
            gdi_name: name.into(),
            num,
            den,
        }
    }

    #[test]
    fn same_whole_hertz_on_two_monitors_is_matched_by_name() {
        let paths = [
            path(r"\\.\DISPLAY1", 240_000, 1000),
            path(r"\\.\DISPLAY2", 240_085, 1000),
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
            pick_refresh(None, 59_000, &[path(r"\\.\DISPLAY1", 59_951, 1000)]),
            (59_951, 1000)
        );
    }
}
