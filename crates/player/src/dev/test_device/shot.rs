//! Shots of the run's window (docs/test-device.md "Run Guard"): the step
//! thread captures the client area with GDI `BitBlt` (milliseconds, so it
//! beats around it), and a writer thread of its own encodes the PNG,
//! writes it as a new file of the run's folder, closes it and only then
//! seals it, so no step waits on a file write.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use super::owner;

/// One shot handed to the writer: its file and its pixels (BGRA rows from
/// the top, as GDI reads them).
struct Job {
    path: PathBuf,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

/// The writer thread.
pub struct Shots {
    tx: Option<Sender<Job>>,
    thread: Option<JoinHandle<Vec<String>>>,
}

/// How long the writer may take to finish once the run ends.
const FINISH_WAIT: Duration = Duration::from_secs(30);

/// The PNG of BGRA pixels, opaque.
#[must_use]
pub fn png(width: u32, height: u32, bgra: &[u8]) -> Vec<u8> {
    let rgba: Vec<u8> = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[b, g, r, _]| [r, g, b, 255])
        .collect();
    crate::dev::png(width, height, &rgba)
}

impl Shots {
    /// Starts the writer for the run whose folder is `dir`: each written
    /// shot's seal (`run`, `role`) goes to `record`.
    pub fn start(
        dir: PathBuf,
        run: String,
        role: String,
        record: Box<dyn Fn(Value) + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let thread = std::thread::spawn(move || {
            let mut errors = Vec::new();
            for j in rx {
                let written = owner::write_new(&j.path, &png(j.width, j.height, &j.bgra))
                    .and_then(|()| owner::seal(&dir, &j.path, &run, &role));
                match written {
                    Ok(seal) => record(seal),
                    Err(e) => errors.push(format!("the shot {}: {e:#}", j.path.display())),
                }
            }
            errors
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    /// Hands a captured shot to the writer.
    ///
    /// # Errors
    ///
    /// The writer has ended.
    pub fn write(
        &self,
        path: PathBuf,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    ) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or("the shot writer has ended")?
            .send(Job {
                path,
                width,
                height,
                bgra,
            })
            .map_err(|_| "the shot writer has ended".to_owned())
    }

    /// Waits for every shot handed over to be written and sealed: the
    /// writer's errors.
    pub fn finish(&mut self) -> Vec<String> {
        drop(self.tx.take());
        let Some(t) = self.thread.take() else {
            return Vec::new();
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(t.join());
        });
        match rx.recv_timeout(FINISH_WAIT) {
            Ok(Ok(errors)) => errors,
            Ok(Err(_)) => vec!["the shot writer panicked".into()],
            Err(_) => vec![format!(
                "the shot writer did not finish within {FINISH_WAIT:?}"
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A shot is written as a PNG of its pixels, opaque, and sealed only
    /// once its file is closed: the seal's digest is the file's.
    #[test]
    fn a_shot_is_written_then_sealed() {
        let tmp = tempfile::tempdir().unwrap();
        let seals = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seals);
        let mut shots = Shots::start(
            tmp.path().to_owned(),
            "r".into(),
            "run".into(),
            Box::new(move |v| s2.lock().unwrap().push(v)),
        );
        // 2×1: blue, then red (BGRA).
        let bgra = vec![255, 0, 0, 0, 0, 0, 255, 0];
        let path = tmp.path().join("shot-a.png");
        shots.write(path.clone(), 2, 1, bgra).unwrap();
        assert!(shots.finish().is_empty());
        let img = image::open(&path).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 255]);
        let seals = seals.lock().unwrap();
        assert_eq!(seals.len(), 1);
        assert_eq!(seals[0]["path"], "shot-a.png");
        assert_eq!(seals[0]["sha256"], owner::digest(&path).unwrap());
        // A second shot of the same name is refused, not overwritten.
        let mut again = Shots::start(
            tmp.path().to_owned(),
            "r".into(),
            "run".into(),
            Box::new(|_| {}),
        );
        again.write(path, 1, 1, vec![0; 4]).unwrap();
        assert_eq!(again.finish().len(), 1);
    }
}
