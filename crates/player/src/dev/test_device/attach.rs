//! The reference rows' attachment (docs/test-device.md "Host Commands",
//! `--attach`): the run's window is the main window of a process the
//! runner did not start, as UI test drivers attach to an application by its
//! process and top-level window. The window must be that process's, on the
//! run's monitor, with the client size the targets are for; the runner
//! records the process and never ends it.

use super::click::{Win, contains};

/// Whether `window` (`None`: the process shows none) may be the run's
/// window for process `pid`: the window's own process (`owner`) is `pid`,
/// its client area (`client`, on the screen) is `size` and lies inside the
/// run's `monitor`.
///
/// # Errors
///
/// Why not.
pub fn check(
    pid: u32,
    window: Option<Win>,
    owner: u32,
    client: Option<[i32; 4]>,
    size: [i32; 2],
    monitor: [i32; 4],
) -> Result<Win, String> {
    let w = window.ok_or_else(|| format!("process {pid} shows no window"))?;
    if owner != pid {
        return Err(format!(
            "window {w:#x} is process {owner}'s, not the attached process {pid}'s"
        ));
    }
    let c = client.ok_or_else(|| format!("window {w:#x} has no client area"))?;
    if [c[2], c[3]] != size {
        return Err(format!(
            "window {w:#x}'s client area is {}x{}, not {}x{}",
            c[2], c[3], size[0], size[1]
        ));
    }
    if !contains(monitor, c) {
        return Err(format!(
            "window {w:#x}'s client area {c:?} is not inside the run's monitor {monitor:?}"
        ));
    }
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: [i32; 4] = [0, 0, 2560, 1440];
    const SIZE: [i32; 2] = [1920, 1080];

    /// A process without a window of its own on the run's monitor at the
    /// targets' client size is refused: no window, another process's
    /// window, another size, or off the monitor. The control: its own
    /// window, as it must be.
    #[test]
    fn an_attached_pid_without_its_window_is_refused() {
        let client = Some([8, 31, 1920, 1080]);
        assert_eq!(check(40, Some(0x10), 40, client, SIZE, MONITOR), Ok(0x10));
        let cases = [
            (
                "shows no window",
                check(40, None, 40, client, SIZE, MONITOR),
            ),
            (
                "process 41's",
                check(40, Some(0x10), 41, client, SIZE, MONITOR),
            ),
            (
                "no client area",
                check(40, Some(0x10), 40, None, SIZE, MONITOR),
            ),
            (
                "1280x720",
                check(40, Some(0x10), 40, Some([8, 31, 1280, 720]), SIZE, MONITOR),
            ),
            (
                "not inside",
                check(
                    40,
                    Some(0x10),
                    40,
                    Some([2000, 31, 1920, 1080]),
                    SIZE,
                    MONITOR,
                ),
            ),
        ];
        for (want, got) in cases {
            let e = got.unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
        }
    }
}
