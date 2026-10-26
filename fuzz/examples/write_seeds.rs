//! Writes the generated seeds as `<dir>/<target>/<name>`, replacing what
//! is there (scripts/fuzz.sh runs it before each fuzz run).
//!
//! Usage: `write_seeds <dir>`

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(dir), None) = (args.next(), args.next()) else {
        eprintln!("usage: write_seeds <dir>");
        return ExitCode::from(2);
    };
    let dir = PathBuf::from(dir);
    for (target, seeds) in fuzz::seeds::all() {
        let target_dir = dir.join(target);
        let written = (|| {
            if target_dir.exists() {
                std::fs::remove_dir_all(&target_dir)?;
            }
            std::fs::create_dir_all(&target_dir)?;
            for (name, bytes) in &seeds {
                std::fs::write(target_dir.join(name), bytes)?;
            }
            std::io::Result::Ok(())
        })();
        if let Err(e) = written {
            eprintln!("write_seeds: {}: {e}", target_dir.display());
            return ExitCode::FAILURE;
        }
        println!("{} seeds in {}", seeds.len(), target_dir.display());
    }
    ExitCode::SUCCESS
}
