//! `tapestry test-device <command>` (dev-tools; docs/test-device.md "Host
//! Commands"): the host side of the test input device, Windows-only and in
//! `player` for its Win32 access. `devices` finds it and checks its shape,
//! `flash` loads its firmware and `bootsel` puts it into its boot ROM's
//! drive, both on the USB location they are given, `trace` holds its
//! link passively, and `check` applies the verdict contract to a run's
//! files.

mod check;
mod click;
mod desktop;
mod devices;
mod flash;
mod guard;
mod job;
mod link;
mod listen;
mod owner;
mod release;
mod trace;
mod win;

use std::path::PathBuf;

use lexopt::Arg;

use crate::args::{Command, UsageError};

/// The commit this build of the runner was made from (the crate's `build.rs`).
pub const COMMIT: &str = env!("TAPESTRY_COMMIT");

const USAGE: &str = "usage: tapestry test-device devices [--out <json>] \
| flash <uf2> --build <id> --port <location> \
| bootsel --port <location> \
| trace --out <jsonl> --build <id> [--port <location>] [--ping-ms <ms>] [--no-trace] [--framing-probe] \
| listen --out <jsonl> --port <location> [--for <ms>] \
| check <row> <dir>";

/// A `test-device` command.
#[derive(Debug, PartialEq, Eq)]
pub enum TestDevice {
    Devices {
        out: Option<PathBuf>,
    },
    Flash {
        uf2: PathBuf,
        build: String,
        /// The device's USB location path.
        port: String,
    },
    Bootsel {
        port: String,
    },
    Trace(trace::Args),
    Listen(listen::Args),
    /// The contract for the run, set or outer run in `dir`.
    Check {
        row: String,
        dir: PathBuf,
    },
}

fn usage() -> UsageError {
    UsageError::Usage(USAGE.to_owned())
}

fn text(p: &mut lexopt::Parser) -> Result<String, UsageError> {
    Ok(p.value()?.to_string_lossy().into_owned())
}

/// The arguments after `test-device`.
///
/// # Errors
///
/// The usage error: an unknown command or argument, or a missing value.
pub fn args(p: &mut lexopt::Parser) -> Result<Command, UsageError> {
    let Some(Arg::Value(cmd)) = p.next()? else {
        return Err(usage());
    };
    let cmd = cmd.to_string_lossy().into_owned();
    let mut values: Vec<PathBuf> = Vec::new();
    let mut out = None;
    let mut build = None;
    let mut port = None;
    let mut ping_ms = 100;
    let mut trace_on = true;
    let mut framing_probe = false;
    let mut for_ms = None;
    while let Some(arg) = p.next()? {
        match (cmd.as_str(), arg) {
            ("flash" | "check", Arg::Value(v)) => values.push(PathBuf::from(v)),
            ("devices" | "trace" | "listen", Arg::Long("out")) => {
                out = Some(PathBuf::from(p.value()?));
            }
            ("flash" | "trace", Arg::Long("build")) => build = Some(text(p)?),
            ("flash" | "bootsel" | "trace" | "listen", Arg::Long("port")) => port = Some(text(p)?),
            ("listen", Arg::Long("for")) => {
                for_ms = Some(
                    text(p)?
                        .parse()
                        .map_err(|_| UsageError::Usage("--for takes milliseconds".to_owned()))?,
                );
            }
            ("trace", Arg::Long("ping-ms")) => {
                ping_ms = text(p)?
                    .parse()
                    .map_err(|_| UsageError::Usage("--ping-ms takes milliseconds".to_owned()))?;
            }
            ("trace", Arg::Long("no-trace")) => trace_on = false,
            ("trace", Arg::Long("framing-probe")) => framing_probe = true,
            (_, other) => return Err(other.unexpected().into()),
        }
    }
    let mut values = values.into_iter();
    let t = match cmd.as_str() {
        "devices" => TestDevice::Devices { out },
        "flash" => TestDevice::Flash {
            uf2: values.next().ok_or_else(usage)?,
            build: build.ok_or_else(usage)?,
            port: port.ok_or_else(usage)?,
        },
        "bootsel" => TestDevice::Bootsel {
            port: port.ok_or_else(usage)?,
        },
        "check" => TestDevice::Check {
            row: values
                .next()
                .ok_or_else(usage)?
                .to_string_lossy()
                .into_owned(),
            dir: values.next().ok_or_else(usage)?,
        },
        "trace" => TestDevice::Trace(trace::Args {
            out: out.ok_or_else(usage)?,
            build: build.ok_or_else(usage)?,
            port,
            ping_ms,
            trace: trace_on,
            framing_probe,
        }),
        "listen" => TestDevice::Listen(listen::Args {
            out: out.ok_or_else(usage)?,
            port: port.ok_or_else(usage)?,
            for_ms,
        }),
        _ => return Err(usage()),
    };
    if let Some(extra) = values.next() {
        return Err(UsageError::Unknown(extra.to_string_lossy().into_owned()));
    }
    Ok(Command::TestDevice(t))
}

/// Runs a `test-device` command.
///
/// # Errors
///
/// The command's failure.
pub fn run(t: TestDevice) -> anyhow::Result<()> {
    match t {
        TestDevice::Devices { out } => devices::devices(out.as_deref()),
        TestDevice::Flash { uf2, build, port } => flash::flash(&uf2, &build, &port),
        TestDevice::Bootsel { port } => flash::bootsel(&port),
        TestDevice::Trace(a) => trace::trace(&a),
        TestDevice::Listen(a) => listen::listen(&a),
        TestDevice::Check { row, dir } => check::check(&row, &dir),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn parsed(args: &[&str]) -> Result<TestDevice, UsageError> {
        let mut all = vec![OsString::from("test-device")];
        all.extend(args.iter().map(OsString::from));
        match crate::args::parse(all)? {
            Command::TestDevice(t) => Ok(t),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_command_takes_its_own_arguments() {
        assert_eq!(
            parsed(&["devices", "--out", "d.json"]).unwrap(),
            TestDevice::Devices {
                out: Some(PathBuf::from("d.json"))
            }
        );
        assert_eq!(
            parsed(&["trace", "--out", "t.jsonl", "--build", "abc"]).unwrap(),
            TestDevice::Trace(trace::Args {
                out: PathBuf::from("t.jsonl"),
                build: "abc".to_owned(),
                port: None,
                ping_ms: 100,
                trace: true,
                framing_probe: false,
            })
        );
        assert_eq!(
            parsed(&[
                "trace",
                "--out",
                "t.jsonl",
                "--build",
                "abc",
                "--port",
                "L",
                "--ping-ms",
                "50",
                "--no-trace",
                "--framing-probe",
            ])
            .unwrap(),
            TestDevice::Trace(trace::Args {
                out: PathBuf::from("t.jsonl"),
                build: "abc".to_owned(),
                port: Some("L".to_owned()),
                ping_ms: 50,
                trace: false,
                framing_probe: true,
            })
        );
        assert_eq!(
            parsed(&["listen", "--out", "l.jsonl", "--port", "L", "--for", "500"]).unwrap(),
            TestDevice::Listen(listen::Args {
                out: PathBuf::from("l.jsonl"),
                port: "L".to_owned(),
                for_ms: Some(500),
            })
        );
        // The recorder binds by a location: it needs one.
        assert_eq!(
            parsed(&["listen", "--out", "l.jsonl"]).unwrap_err(),
            usage()
        );
        // The trace needs its build; an option of another command, a
        // value it does not take, an unknown command.
        assert_eq!(parsed(&["trace", "--out", "t.jsonl"]).unwrap_err(), usage());
        assert_eq!(
            parsed(&["devices", "--port", "L"]).unwrap_err(),
            UsageError::Unknown("--port".to_owned())
        );
        assert_eq!(
            parsed(&["trace", "x", "--out", "t", "--build", "b"]).unwrap_err(),
            UsageError::Unknown("x".to_owned())
        );
        assert_eq!(
            parsed(&["flash", "a.uf2", "b.uf2", "--build", "b", "--port", "L"]).unwrap_err(),
            UsageError::Unknown("b.uf2".to_owned())
        );
        assert_eq!(
            parsed(&["check", "sink", "runs/a"]).unwrap(),
            TestDevice::Check {
                row: "sink".to_owned(),
                dir: PathBuf::from("runs/a"),
            }
        );
        assert_eq!(parsed(&["check", "sink"]).unwrap_err(), usage());
        assert_eq!(
            parsed(&["check", "sink", "a", "b"]).unwrap_err(),
            UsageError::Unknown("b".to_owned())
        );
        assert_eq!(parsed(&["jump"]).unwrap_err(), usage());
        assert_eq!(parsed(&[]).unwrap_err(), usage());
    }
}
