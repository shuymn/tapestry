//! The reference rows' captures (docs/test-device.md "Run Guard",
//! captures): a screen copy of the attached window's client area, the
//! technique screen recorders use for a window, behind the click guard's
//! own checks read before and after the copy. The window must be in front,
//! and the root window at each corner of its client area and at its centre
//! must be it, on the run's monitor as it was, with its client area where
//! and as large as it was when the run attached; a check that fails before
//! or after the copy fails the run, so no capture of a covered, moved or
//! hidden window is written.
//!
//! The decision is pure ([`check`]); [`guarded`] runs it against a
//! [`Screen`]: the real desktop, or a stand-in in tests.

use super::click::{Win, contains};

/// What a guarded capture reads and does.
pub trait Screen {
    /// The run's monitor, enumerated again by name: [x, y, w, h].
    fn monitor(&mut self) -> Option<[i32; 4]>;
    /// The window's client area on the screen: [x, y, w, h].
    fn client(&mut self, w: Win) -> Option<[i32; 4]>;
    /// The root window under a screen pixel.
    fn root_at(&mut self, x: i32, y: i32) -> Win;
    fn foreground(&mut self) -> Win;
    /// Copies the screen rectangle [x, y, w, h]: its pixels, 32-bit BGRA,
    /// top row first.
    ///
    /// # Errors
    ///
    /// The copy failed.
    fn grab(&mut self, rect: [i32; 4]) -> Result<Vec<u8>, String>;
}

/// The points a capture checks the window at: the client area's four
/// corner pixels and its centre, on the screen.
#[must_use]
pub fn points(client: [i32; 4]) -> [(i32, i32); 5] {
    let [x, y, w, h] = client;
    let (r, b) = (x + w - 1, y + h - 1);
    [(x, y), (r, y), (x, b), (r, b), (x + w / 2, y + h / 2)]
}

/// What one look before or after a capture reads.
#[derive(Clone, Debug)]
pub struct Scene {
    pub monitor_then: [i32; 4],
    pub monitor_now: Option<[i32; 4]>,
    /// The client area when the run attached, and now.
    pub client_then: [i32; 4],
    pub client_now: Option<[i32; 4]>,
    pub window: Win,
    pub foreground: Win,
    /// The root window at each of [`points`]`(client_then)`.
    pub roots: Vec<Win>,
}

/// Whether the window may be captured now: the monitor as it was, the
/// client area where and as large as it was and inside the monitor, the
/// window in front, and the root window at each checked point the window.
///
/// # Errors
///
/// The first rule the scene breaks.
pub fn check(s: &Scene) -> Result<(), String> {
    let monitor = s.monitor_now.ok_or("the run's monitor has gone")?;
    if monitor != s.monitor_then {
        return Err(format!(
            "the run's monitor moved from {:?} to {monitor:?}",
            s.monitor_then
        ));
    }
    let client = s.client_now.ok_or("the run's window has gone")?;
    if client != s.client_then {
        return Err(format!(
            "the window's client area moved from {:?} to {client:?}",
            s.client_then
        ));
    }
    if !contains(monitor, client) {
        return Err(format!(
            "the window's client area {client:?} is not inside the monitor {monitor:?}"
        ));
    }
    if s.foreground != s.window {
        return Err(format!(
            "the foreground ({:#x}) is not the run's window",
            s.foreground
        ));
    }
    for (p, root) in points(s.client_then).iter().zip(&s.roots) {
        if *root != s.window {
            return Err(format!(
                "the window at {p:?} ({root:#x}) is not the run's: the client area is covered"
            ));
        }
    }
    Ok(())
}

/// One look at `screen` for [`check`].
fn look<S: Screen>(
    screen: &mut S,
    window: Win,
    monitor_then: [i32; 4],
    client_then: [i32; 4],
) -> Scene {
    Scene {
        monitor_then,
        monitor_now: screen.monitor(),
        client_then,
        client_now: screen.client(window),
        window,
        foreground: screen.foreground(),
        roots: points(client_then)
            .iter()
            .map(|&(x, y)| screen.root_at(x, y))
            .collect(),
    }
}

/// A capture of `window`'s client area `client_then` on the run's monitor
/// (`monitor_then` at the run's start), checked before and after the copy:
/// its BGRA pixels.
///
/// # Errors
///
/// A check before or after the copy, or the copy itself, failed.
pub fn guarded<S: Screen>(
    screen: &mut S,
    window: Win,
    monitor_then: [i32; 4],
    client_then: [i32; 4],
) -> Result<Vec<u8>, String> {
    check(&look(screen, window, monitor_then, client_then))
        .map_err(|e| format!("before the capture: {e}"))?;
    let pixels = screen.grab(client_then)?;
    check(&look(screen, window, monitor_then, client_then))
        .map_err(|e| format!("after the capture: {e}"))?;
    Ok(pixels)
}

/// RGBA pixels of BGRA ones, opaque.
#[must_use]
pub fn rgba(bgra: &[u8]) -> Vec<u8> {
    bgra.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[b, g, r, _]| [r, g, b, 255])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Win = 0x77;
    const MONITOR: [i32; 4] = [0, 0, 2560, 1440];
    const CLIENT: [i32; 4] = [8, 31, 1920, 1080];

    /// A desktop whose foreground and roots change after the copy when
    /// asked to.
    struct Fake {
        foreground: [Win; 2],
        covered_at: Option<(usize, (i32, i32))>,
        grabbed: usize,
    }

    impl Fake {
        fn phase(&self) -> usize {
            self.grabbed.min(1)
        }
    }

    impl Screen for Fake {
        fn monitor(&mut self) -> Option<[i32; 4]> {
            Some(MONITOR)
        }
        fn client(&mut self, _: Win) -> Option<[i32; 4]> {
            Some(CLIENT)
        }
        fn root_at(&mut self, x: i32, y: i32) -> Win {
            match self.covered_at {
                Some((phase, p)) if phase == self.phase() && p == (x, y) => 0x99,
                _ => WINDOW,
            }
        }
        fn foreground(&mut self) -> Win {
            self.foreground[self.phase()]
        }
        fn grab(&mut self, rect: [i32; 4]) -> Result<Vec<u8>, String> {
            assert_eq!(rect, CLIENT);
            self.grabbed += 1;
            Ok(vec![1, 2, 3, 0])
        }
    }

    fn fake() -> Fake {
        Fake {
            foreground: [WINDOW; 2],
            covered_at: None,
            grabbed: 0,
        }
    }

    /// A capture is taken only of the run's window in front and uncovered,
    /// as it was, both before the copy and after it; a window that left the
    /// front, or a corner covered, before or after fails the capture.
    #[test]
    fn a_capture_whose_window_left_the_front_is_refused() {
        assert_eq!(
            guarded(&mut fake(), WINDOW, MONITOR, CLIENT).unwrap(),
            [1, 2, 3, 0]
        );
        for phase in [0, 1] {
            let mut f = fake();
            f.foreground[phase] = 0x55;
            let e = guarded(&mut f, WINDOW, MONITOR, CLIENT).unwrap_err();
            assert!(e.contains("foreground"), "{phase}: {e}");
            assert_eq!(
                f.grabbed, phase,
                "{phase}: no copy before a failed first look"
            );
            let corner = points(CLIENT)[3];
            let mut f = fake();
            f.covered_at = Some((phase, corner));
            let e = guarded(&mut f, WINDOW, MONITOR, CLIENT).unwrap_err();
            assert!(e.contains("covered"), "{phase}: {e}");
            assert!(e.starts_with(["before", "after"][phase]), "{e}");
        }
    }

    /// Each rule of the check, alone.
    #[test]
    fn each_rule_of_the_capture_check_refuses() {
        let good = Scene {
            monitor_then: MONITOR,
            monitor_now: Some(MONITOR),
            client_then: CLIENT,
            client_now: Some(CLIENT),
            window: WINDOW,
            foreground: WINDOW,
            roots: vec![WINDOW; 5],
        };
        assert_eq!(check(&good), Ok(()));
        let broken: [(&str, Scene); 6] = [
            (
                "monitor has gone",
                Scene {
                    monitor_now: None,
                    ..good.clone()
                },
            ),
            (
                "monitor moved",
                Scene {
                    monitor_now: Some([1, 0, 2560, 1440]),
                    ..good.clone()
                },
            ),
            (
                "window has gone",
                Scene {
                    client_now: None,
                    ..good.clone()
                },
            ),
            (
                "client area moved",
                Scene {
                    client_now: Some([9, 31, 1920, 1080]),
                    ..good.clone()
                },
            ),
            (
                "foreground",
                Scene {
                    foreground: 1,
                    ..good.clone()
                },
            ),
            (
                "covered",
                Scene {
                    roots: vec![WINDOW, WINDOW, WINDOW, WINDOW, 1],
                    ..good.clone()
                },
            ),
        ];
        for (want, s) in broken {
            let e = check(&s).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
        }
        let off = Scene {
            client_then: [2000, 31, 1920, 1080],
            client_now: Some([2000, 31, 1920, 1080]),
            ..good
        };
        assert!(check(&off).unwrap_err().contains("not inside the monitor"));
    }

    #[test]
    fn the_checked_points_lie_inside_the_client_area() {
        for (x, y) in points(CLIENT) {
            assert!((CLIENT[0]..CLIENT[0] + CLIENT[2]).contains(&x));
            assert!((CLIENT[1]..CLIENT[1] + CLIENT[3]).contains(&y));
        }
        assert_eq!(rgba(&[1, 2, 3, 0]), [3, 2, 1, 255]);
    }
}
