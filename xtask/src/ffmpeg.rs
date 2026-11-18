//! The FFmpeg build the media worker links (design.md "Libraries", the
//! FFmpeg row; "Threat Model" T02), from the manifest `ffmpeg.toml` at the
//! repository root (docs/setup.md "FFmpeg").
//!
//! - `cargo xtask ffmpeg build [--expect <file>]`,
//!   in the flake's `ffmpeg` shell on Linux: fetches the pinned FFmpeg source
//!   and checks its hashes, configures it with the recorded flags at the
//!   manifest's fixed path with a cleared environment (only the shell's
//!   build tools on `PATH`), checks that configure registered exactly the
//!   manifest's components ([`registered`], [`compare_components`]), builds
//!   it, checks each DLL's imports against
//!   [`ALLOWED_IMPORTS`], hashes every DLL and `.def` file and compares them
//!   with the pinned ones ([`expectation`]) or records them, and packs the
//!   two release assets under `target/ffmpeg-build/`.
//! - `cargo xtask ffmpeg fetch` ([`install_release`]): keeps the release
//!   the manifest's `[release]` pins in `target/ffmpeg/` while every file
//!   there has its pinned sha256; otherwise downloads its binary asset,
//!   refuses it unless its sha256 is the pinned one ([`unpack_checked`]),
//!   unpacks it, refuses it unless every DLL and `.def` file in it has its
//!   pinned sha256, and puts it in place; either way it makes the import
//!   libraries there from the `.def` files.
//! - `cargo xtask ffmpeg check-exes <dir>` ([`worker_disallowed_imports`],
//!   [`game_ffmpeg_imports`]): in a profile directory, the media worker's
//!   executable imports only the system's core API sets, its C runtime and
//!   the build's DLLs, and the game's executable none of the build's DLLs.
//!
//! xtask links nothing of FFmpeg: it runs the build's tools and reads the
//! DLLs' import tables as bytes ([`pe_imports`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context as _, bail, ensure};
use sha2::{Digest as _, Sha256};

/// The manifest format this xtask reads; any other is refused.
const MANIFEST_VERSION: i64 = 1;

/// The DLLs a built FFmpeg DLL may import besides the build's own five
/// (D4 "Win32k": no Windows graphics component, no toolchain runtime), by
/// name without case; `api-ms-win-crt-` is the Universal C runtime's API set
/// prefix.
pub const ALLOWED_IMPORTS: [&str; 3] = ["kernel32.dll", "bcrypt.dll", "ws2_32.dll"];
const CRT_API_SET: &str = "api-ms-win-crt-";

/// The kinds of component the build checks, each with the list configure
/// generates for it (in the build directory) and the suffix its entries
/// carry there (`&ff_<name><suffix>,`).
pub const COMPONENT_LISTS: [(&str, &str, &str); 5] = [
    ("decoders", "libavcodec/codec_list.c", "_decoder"),
    ("parsers", "libavcodec/parser_list.c", "_parser"),
    ("bsfs", "libavcodec/bsf_list.c", "_bsf"),
    ("demuxers", "libavformat/demuxer_list.c", "_demuxer"),
    ("protocols", "libavformat/protocol_list.c", "_protocol"),
];

/// Component names by kind (a key of [`COMPONENT_LISTS`]).
pub type Components = BTreeMap<String, BTreeSet<String>>;

/// The manifest: the source, how it is built and, once the owner has
/// published one, the release it was built as.
#[derive(Debug)]
pub struct Manifest {
    pub source: Source,
    pub build: Build,
    pub zlib: Zlib,
    pub release: Option<Release>,
}

#[derive(Debug)]
pub struct Source {
    pub commit: String,
    /// `git describe` of the commit, written as the source's `VERSION`.
    pub describe: String,
    pub archive: String,
    pub sha256: String,
    /// `nix hash path` of the unpacked tree.
    pub tree: String,
    /// The commit's time, the build's `SOURCE_DATE_EPOCH`.
    pub date: i64,
}

#[derive(Debug)]
pub struct Build {
    /// Counts changes of flags, libraries or toolchain; the release tag's
    /// last part and `--extra-version`.
    pub revision: i64,
    /// The fixed path the build runs at (the DLLs embed it).
    pub dir: String,
    pub dlls: Vec<String>,
    pub configure: Vec<String>,
    /// What configure must register, every kind of [`COMPONENT_LISTS`].
    pub components: Components,
}

#[derive(Debug)]
pub struct Zlib {
    pub version: String,
    /// The release tarball's sha256.
    pub sha256: String,
}

#[derive(Debug)]
pub struct Release {
    pub revision: i64,
    pub repository: String,
    pub tag: String,
    pub binary: Asset,
    pub source: Asset,
    /// Each DLL's and `.def` file's sha256 by its path in the binary asset
    /// (`bin/<dll>.dll`, `lib/<dll>.def`).
    pub files: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
}

impl Manifest {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let t: toml::Table = toml::from_str(text).context("ffmpeg.toml is not TOML")?;
        let version = t.get("version").and_then(toml::Value::as_integer);
        ensure!(
            version == Some(MANIFEST_VERSION),
            "ffmpeg.toml has version {version:?}; this xtask reads version {MANIFEST_VERSION}"
        );
        let source = table(&t, "source")?;
        let build = table(&t, "build")?;
        let zlib = table(&t, "zlib")?;
        let release = match t.get("release") {
            None => None,
            Some(r) => {
                let r = r.as_table().context("[release] is not a table")?;
                let files = table(r, "files")?
                    .iter()
                    .map(|(k, v)| {
                        let v = v.as_str().with_context(|| format!("release.files.{k}"))?;
                        Ok((k.clone(), v.to_owned()))
                    })
                    .collect::<anyhow::Result<_>>()?;
                Some(Release {
                    revision: int(r, "revision")?,
                    repository: string(r, "repository")?,
                    tag: string(r, "tag")?,
                    binary: asset(table(r, "binary")?)?,
                    source: asset(table(r, "source")?)?,
                    files,
                })
            }
        };
        Ok(Self {
            source: Source {
                commit: string(source, "commit")?,
                describe: string(source, "describe")?,
                archive: string(source, "archive")?,
                sha256: string(source, "sha256")?,
                tree: string(source, "tree")?,
                date: int(source, "date")?,
            },
            build: Build {
                revision: int(build, "revision")?,
                dir: string(build, "dir")?,
                dlls: strings(build, "dlls")?,
                configure: strings(build, "configure")?,
                components: components(table(build, "components")?)?,
            },
            zlib: Zlib {
                version: string(zlib, "version")?,
                sha256: string(zlib, "sha256")?,
            },
            release,
        })
    }

    /// The version string FFmpeg reports: the source's `describe` and the
    /// build revision (`--extra-version`).
    pub fn version(&self) -> String {
        format!("{}-{}", self.source.describe, self.build.revision)
    }

    /// The release tag and the two assets' names for the build revision.
    pub fn names(&self) -> (String, String, String) {
        let base = format!("{}-{}", self.source.describe, self.build.revision);
        (
            format!("ffmpeg-{base}"),
            format!("tapestry-ffmpeg-{base}-win64-lgpl-shared.zip"),
            format!("tapestry-ffmpeg-{base}-source.tar.xz"),
        )
    }
}

fn table<'a>(t: &'a toml::Table, key: &str) -> anyhow::Result<&'a toml::Table> {
    t.get(key)
        .and_then(toml::Value::as_table)
        .with_context(|| format!("ffmpeg.toml has no [{key}] table"))
}

fn string(t: &toml::Table, key: &str) -> anyhow::Result<String> {
    t.get(key)
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("ffmpeg.toml: `{key}` is not a string"))
}

fn int(t: &toml::Table, key: &str) -> anyhow::Result<i64> {
    t.get(key)
        .and_then(toml::Value::as_integer)
        .with_context(|| format!("ffmpeg.toml: `{key}` is not an integer"))
}

fn strings(t: &toml::Table, key: &str) -> anyhow::Result<Vec<String>> {
    t.get(key)
        .and_then(toml::Value::as_array)
        .with_context(|| format!("ffmpeg.toml: `{key}` is not an array"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .with_context(|| format!("ffmpeg.toml: `{key}` holds a non-string"))
        })
        .collect()
}

/// `[build.components]`: a list for every kind of [`COMPONENT_LISTS`] and
/// no other key.
fn components(t: &toml::Table) -> anyhow::Result<Components> {
    for key in t.keys() {
        ensure!(
            COMPONENT_LISTS.iter().any(|&(kind, _, _)| kind == key),
            "ffmpeg.toml: [build.components] has `{key}`, which is no kind the build checks"
        );
    }
    COMPONENT_LISTS
        .iter()
        .map(|&(kind, _, _)| Ok((kind.to_owned(), strings(t, kind)?.into_iter().collect())))
        .collect()
}

fn asset(t: &toml::Table) -> anyhow::Result<Asset> {
    Ok(Asset {
        name: string(t, "name")?,
        sha256: string(t, "sha256")?,
    })
}

/// What a build does with its hashes: compare them with pinned ones, or
/// record them for the manifest's PR.
#[derive(Debug, PartialEq, Eq)]
pub enum Expect<'a> {
    Compare(&'a BTreeMap<String, String>),
    Record,
}

/// The build's expectation: `expected`, the files `--expect` pins
/// ([`expected_files`]), or the manifest's release files when its release pins the
/// revision being built; otherwise the hashes are recorded and nothing is
/// compared.
pub fn expectation<'a>(
    manifest: &'a Manifest,
    expected: Option<&'a BTreeMap<String, String>>,
) -> Expect<'a> {
    if let Some(files) = expected {
        return Expect::Compare(files);
    }
    match &manifest.release {
        Some(r) if r.revision == manifest.build.revision => Expect::Compare(&r.files),
        _ => Expect::Record,
    }
}

/// Compares the built files' hashes with the pinned ones: one line for each
/// file whose hash differs, that is pinned and not built, or built and not
/// pinned, naming the file.
pub fn compare_hashes(
    pinned: &BTreeMap<String, String>,
    built: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (file, want) in pinned {
        match built.get(file) {
            Some(got) if got == want => {}
            Some(got) => out.push(format!("{file}: built {got}, pinned {want}")),
            None => out.push(format!("{file}: pinned {want}, not built")),
        }
    }
    for (file, got) in built {
        if !pinned.contains_key(file) {
            out.push(format!("{file}: built {got}, not pinned"));
        }
    }
    out
}

/// The names one of configure's generated lists registers for a kind: the
/// `<name>` of each `&ff_<name><suffix>,` entry. `codec_list.c` also lists
/// the encoders, whose entries end in `_encoder` and are skipped. Any other
/// line than the array's opening, an entry and its closing `NULL };`, or a
/// list without that closing, is refused, so a list in another form cannot
/// read as empty.
pub fn registered(list: &str, suffix: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let mut closed = false;
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if closed {
            return Err(format!("`{line}` after the closing `NULL }};`"));
        }
        if line == "NULL };" {
            closed = true;
            continue;
        }
        if line.ends_with("[] = {") {
            continue;
        }
        let entry = line
            .strip_prefix("&ff_")
            .and_then(|l| l.strip_suffix(','))
            .ok_or_else(|| format!("unexpected line `{line}`"))?;
        if let Some(name) = entry.strip_suffix(suffix) {
            names.insert(name.to_owned());
        } else if !entry.ends_with("_encoder") {
            return Err(format!("`{line}` is no {suffix} entry"));
        }
    }
    if closed {
        Ok(names)
    } else {
        Err("the list has no closing `NULL };`".to_owned())
    }
}

/// Compares what configure registered with what the manifest lists, both
/// ways: one line for each listed component not registered and each
/// registered one not listed, naming its kind.
pub fn compare_components(listed: &Components, built: &Components) -> Vec<String> {
    let none = BTreeSet::new();
    let mut out = Vec::new();
    for (kind, _, _) in COMPONENT_LISTS {
        let want = listed.get(kind).unwrap_or(&none);
        let got = built.get(kind).unwrap_or(&none);
        out.extend(
            want.difference(got)
                .map(|name| format!("{kind}: {name} is listed and not registered")),
        );
        out.extend(
            got.difference(want)
                .map(|name| format!("{kind}: {name} is registered and not listed")),
        );
    }
    out
}

/// The imported DLL names of a PE image (its import and delay-import
/// directories), in table order.
pub fn pe_imports(bytes: &[u8]) -> Result<Vec<String>, String> {
    let u16_at = |at: usize| -> Result<u16, String> {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(|| format!("truncated at {at:#x}"))
    };
    let u32_at = |at: usize| -> Result<u32, String> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| format!("truncated at {at:#x}"))
    };
    if bytes.get(..2) != Some(b"MZ") {
        return Err("no MZ header".into());
    }
    let pe = u32_at(0x3c)? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("no PE signature".into());
    }
    let coff = pe + 4;
    let sections = usize::from(u16_at(coff + 2)?);
    let optional = coff + 20;
    let optional_size = usize::from(u16_at(coff + 16)?);
    if u16_at(optional)? != 0x20b {
        return Err("not a PE32+ image".into());
    }
    let directories = u32_at(optional + 108)? as usize;
    let directory = |index: usize| -> Result<(u32, u32), String> {
        if index >= directories {
            return Ok((0, 0));
        }
        let at = optional + 112 + 8 * index;
        Ok((u32_at(at)?, u32_at(at + 4)?))
    };
    let table = optional + optional_size;
    let mut spans = Vec::with_capacity(sections);
    for i in 0..sections {
        let at = table + 40 * i;
        let (virtual_size, address, raw_size, raw) = (
            u32_at(at + 8)?,
            u32_at(at + 12)?,
            u32_at(at + 16)?,
            u32_at(at + 20)?,
        );
        spans.push((address, virtual_size.max(raw_size), raw));
    }
    let offset = |rva: u32| -> Result<usize, String> {
        spans
            .iter()
            .find(|&&(address, size, _)| rva >= address && rva - address < size)
            .map(|&(address, _, raw)| (rva - address + raw) as usize)
            .ok_or_else(|| format!("RVA {rva:#x} is in no section"))
    };
    let name_at = |rva: u32| -> Result<String, String> {
        let at = offset(rva)?;
        let rest = bytes.get(at..).ok_or("name past the end")?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("unterminated name")?;
        String::from_utf8(rest[..end].to_vec()).map_err(|_| "a name is not UTF-8".to_owned())
    };
    let mut names = Vec::new();
    // Import descriptors (20 bytes, the name's RVA at 12) and delay-load
    // descriptors (32 bytes, at 4), each table ending with a zero name.
    for (index, size, name) in [(1, 20, 12), (13, 32, 4)] {
        let (rva, _) = directory(index)?;
        if rva == 0 {
            continue;
        }
        let mut at = offset(rva)?;
        loop {
            let name_rva = u32_at(at + name)?;
            if name_rva == 0 {
                break;
            }
            names.push(name_at(name_rva)?);
            at += size;
        }
    }
    Ok(names)
}

/// The imported DLL names of one of the build's DLLs that are neither in
/// [`ALLOWED_IMPORTS`], a C runtime API set nor another of `dlls` (the
/// build's own DLLs, without `.dll`).
pub fn disallowed_imports<'a>(dlls: &[String], imports: &'a [String]) -> Vec<&'a str> {
    imports
        .iter()
        .map(String::as_str)
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            !(ALLOWED_IMPORTS.contains(&lower.as_str())
                || lower.starts_with(CRT_API_SET)
                || dlls
                    .iter()
                    .any(|d| lower == format!("{}.dll", d.to_ascii_lowercase())))
        })
        .collect()
}

/// The DLLs the media worker's executable may import besides the build's
/// own and the system's API sets (design.md "Threat Model" T02: no Windows
/// graphics component), by name without case.
pub const WORKER_IMPORTS: [&str; 3] = ["kernel32.dll", "ntdll.dll", "vcruntime140.dll"];
/// The prefix of the system's core API sets.
const CORE_API_SET: &str = "api-ms-win-core-";

/// Whether `name` is one of `dlls` (the build's own DLLs, without `.dll`).
fn is_build_dll(dlls: &[String], name: &str) -> bool {
    dlls.iter()
        .any(|d| name.eq_ignore_ascii_case(&format!("{d}.dll")))
}

/// The worker executable's imported DLL names that are neither in
/// [`WORKER_IMPORTS`], a core or C runtime API set nor one of `dlls`.
pub fn worker_disallowed_imports<'a>(dlls: &[String], imports: &'a [String]) -> Vec<&'a str> {
    imports
        .iter()
        .map(String::as_str)
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            !(WORKER_IMPORTS.contains(&lower.as_str())
                || lower.starts_with(CORE_API_SET)
                || lower.starts_with(CRT_API_SET)
                || is_build_dll(dlls, name))
        })
        .collect()
}

/// The game executable's imported DLL names that are one of `dlls`: the
/// game process loads no FFmpeg library (design.md "Threat Model" T02).
pub fn game_ffmpeg_imports<'a>(dlls: &[String], imports: &'a [String]) -> Vec<&'a str> {
    imports
        .iter()
        .map(String::as_str)
        .filter(|name| is_build_dll(dlls, name))
        .collect()
}

/// `cargo xtask ffmpeg check-exes <dir>`: the worker's and the game's
/// executables in `dir`, a profile directory, import what design.md "Threat
/// Model" T02 allows them.
fn check_exes(root: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let [dir] = args else {
        bail!("check-exes takes the profile directory");
    };
    let m = read_manifest(&root.join("ffmpeg.toml"))?;
    type Check = for<'a> fn(&[String], &'a [String]) -> Vec<&'a str>;
    let checks: [(&str, Check); 2] = [
        ("tapestry-media.exe", worker_disallowed_imports),
        ("tapestry.exe", game_ffmpeg_imports),
    ];
    let mut bad = Vec::new();
    for (exe, check) in checks {
        let path = Path::new(dir).join(exe);
        let bytes = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let imports = pe_imports(&bytes).map_err(|e| anyhow::anyhow!("{exe}: {e}"))?;
        println!("{exe}: {}", imports.join(" "));
        bad.extend(
            check(&m.build.dlls, &imports)
                .into_iter()
                .map(|name| format!("{exe} imports {name}")),
        );
    }
    ensure!(bad.is_empty(), "{}", bad.join("; "));
    Ok(())
}

/// The sha256 of `bytes`, in lowercase hex.
pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Checks the asset at `path` against its pinned sha256 before anything is
/// done with it: only an asset whose hash matches is given to `unpack`
/// (with the directory to fill); one that differs is refused and `into` is
/// not touched.
pub fn unpack_checked(
    path: &Path,
    pinned: &str,
    into: &Path,
    unpack: impl FnOnce(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let got = sha256(&bytes);
    ensure!(
        got == pinned,
        "{}: sha256 {got}, but ffmpeg.toml pins {pinned}: refused, nothing unpacked",
        path.display()
    );
    unpack(path, into)
}

/// The hashes record a build writes beside its assets and into the binary
/// asset: the version, the build's inputs as built, and each file's sha256.
fn record(
    manifest: &Manifest,
    inputs: &[(&str, String)],
    files: &BTreeMap<String, String>,
) -> String {
    let mut s = String::from(
        "# Written by `cargo xtask ffmpeg build`: copy into ffmpeg.toml's [release]\n\
         # once the owner has published the release (docs/setup.md \"FFmpeg\").\n",
    );
    let _ = writeln!(s, "revision = {}", manifest.build.revision);
    let _ = writeln!(s, "version = \"{}\"", manifest.version());
    for (key, value) in inputs {
        let _ = writeln!(s, "{key} = \"{value}\"");
    }
    s.push_str("\n[files]\n");
    for (file, hash) in files {
        let _ = writeln!(s, "\"{file}\" = \"{hash}\"");
    }
    s
}

/// The pinned files of `--expect`'s file: a build's hashes record (its
/// `revision` and `[files]`) or a manifest with a `[release]` (its
/// `revision` and `[release.files]`), refused unless it pins `revision`, the
/// one being built.
fn expected_files(text: &str, revision: i64) -> anyhow::Result<BTreeMap<String, String>> {
    let t: toml::Table = toml::from_str(text).context("not TOML")?;
    let pins = match t.get("release") {
        Some(r) => r.as_table().context("[release] is not a table")?,
        None => &t,
    };
    let pinned = int(pins, "revision")?;
    ensure!(
        pinned == revision,
        "it pins revision {pinned}, and ffmpeg.toml builds revision {revision}"
    );
    table(pins, "files")?
        .iter()
        .map(|(k, v)| {
            Ok((
                k.clone(),
                v.as_str().with_context(|| format!("files.{k}"))?.to_owned(),
            ))
        })
        .collect()
}

pub fn run(root: &Path, args: &[OsString]) -> ExitCode {
    let result = match args.first().and_then(|a| a.to_str()) {
        Some("build") => build_command(root, &args[1..]),
        Some("fetch") if args.len() == 1 => fetch(root),
        Some("check-exes") => check_exes(root, &args[1..]),
        _ => Err(anyhow::anyhow!(
            "usage: cargo xtask ffmpeg (build [--expect <file>] | fetch | check-exes <dir>)"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ffmpeg: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn read_manifest(path: &Path) -> anyhow::Result<Manifest> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Manifest::parse(&text).with_context(|| path.display().to_string())
}

fn build_command(root: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let expect = match args {
        [] => None,
        [flag, path] if flag == "--expect" => Some(PathBuf::from(path)),
        _ => bail!("build takes only --expect <file>"),
    };
    let m = read_manifest(&root.join("ffmpeg.toml"))?;
    let expected = match expect {
        Some(p) => Some(
            fs::read_to_string(&p)
                .with_context(|| format!("cannot read {}", p.display()))
                .and_then(|text| expected_files(&text, m.build.revision))
                .with_context(|| format!("--expect {}", p.display()))?,
        ),
        None => None,
    };
    if cfg!(windows) {
        bail!(
            "the build runs in the flake's `ffmpeg` shell on Linux; from Windows: \
             TAPESTRY_NIX_SHELL=ffmpeg scripts/wsl.sh cargo xtask ffmpeg build"
        );
    }
    build(root, &m, expectation(&m, expected.as_ref()))
}

/// A variable the `ffmpeg` shell sets.
fn shell_var(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| {
        format!("{name} is unset: run in the flake's `ffmpeg` shell (docs/setup.md \"FFmpeg\")")
    })
}

fn run_logged(cmd: &mut Command, log: &Path, what: &str) -> anyhow::Result<()> {
    let file = fs::File::create(log).with_context(|| format!("cannot create {}", log.display()))?;
    let status = cmd
        .stdout(file.try_clone()?)
        .stderr(file)
        .status()
        .with_context(|| format!("cannot start {what}"))?;
    ensure!(
        status.success(),
        "{what} failed ({status}); see {}",
        log.display()
    );
    Ok(())
}

fn run_plain(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("cannot start {what}"))?;
    ensure!(status.success(), "{what} failed ({status})");
    Ok(())
}

/// The marker file that makes a directory the build's own.
const MARKER: &str = ".tapestry-ffmpeg-build";

/// Claims the build directory `dir` for one build: takes an exclusive lock on
/// `<dir>.lock` beside it, refusing while another build holds it, then
/// removes `dir`, which must hold [`MARKER`] if it exists (a directory
/// without it is not the build's and is left alone), and creates it empty
/// with the marker. The lock holds while the returned file is open, so two
/// builds never share the directory and none inherits another's files.
fn claim_build_dir(dir: &Path) -> anyhow::Result<fs::File> {
    let mut lock_path = dir.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => bail!(
            "another build holds {}: one build at a time works at {}",
            lock_path.display(),
            dir.display()
        ),
        Err(fs::TryLockError::Error(e)) => {
            return Err(e).with_context(|| format!("cannot lock {}", lock_path.display()));
        }
    }
    if dir.exists() {
        ensure!(
            dir.join(MARKER).is_file(),
            "{} exists without {MARKER}: not the build's, left alone",
            dir.display()
        );
        fs::remove_dir_all(dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    }
    fs::create_dir_all(dir)?;
    fs::write(dir.join(MARKER), "")?;
    Ok(lock)
}

/// `program` on `path` (a `PATH` value).
fn on_path(path: &str, program: &str) -> anyhow::Result<PathBuf> {
    std::env::split_paths(path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .with_context(|| format!("{program} is not on TAPESTRY_FFMPEG_PATH"))
}

fn symlink(target: &Path, link: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("cannot link {} to {}", link.display(), target.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, link);
        bail!("the build runs on Linux")
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one build, step by step, in the order the evidence lists them"
)]
fn build(root: &Path, m: &Manifest, expect: Expect<'_>) -> anyhow::Result<()> {
    let tools = shell_var("TAPESTRY_FFMPEG_PATH")?;
    let mcf = PathBuf::from(shell_var("TAPESTRY_FFMPEG_MCFGTHREAD")?);
    let zlib = PathBuf::from(shell_var("TAPESTRY_FFMPEG_ZLIB")?);
    let zlib_dev = PathBuf::from(shell_var("TAPESTRY_FFMPEG_ZLIB_DEV")?);
    let zlib_src = PathBuf::from(shell_var("TAPESTRY_FFMPEG_ZLIB_SRC")?);
    let zlib_version = shell_var("TAPESTRY_FFMPEG_ZLIB_VERSION")?;
    let nixpkgs = shell_var("TAPESTRY_NIXPKGS_REV")?;
    ensure!(
        zlib_version == m.zlib.version,
        "the shell's zlib is {zlib_version}, ffmpeg.toml's {}",
        m.zlib.version
    );
    let zlib_tarball = sha256(&fs::read(&zlib_src)?);
    ensure!(
        zlib_tarball == m.zlib.sha256,
        "zlib's source {}: sha256 {zlib_tarball}, ffmpeg.toml pins {}",
        zlib_src.display(),
        m.zlib.sha256
    );

    let out = root.join("target").join("ffmpeg-build");
    fs::create_dir_all(&out)?;
    let work = PathBuf::from(&m.build.dir);
    let _lock = claim_build_dir(&work)?;
    for d in [
        "src",
        "build",
        "dest",
        "pc-empty",
        "home",
        "tmp",
        "deps/mcf",
        "deps/zlib/include",
        "deps/zlib/lib",
    ] {
        fs::create_dir_all(work.join(d))?;
    }

    // The source, checked by the archive's sha256 and the unpacked tree's
    // nix hash (which still holds if the archive is ever re-compressed). The
    // download is kept in the workspace's target directory, outside the
    // build directory, and checked again on each run.
    let dl = root.join("target").join("ffmpeg-dl");
    fs::create_dir_all(&dl)?;
    let tarball = dl.join(format!("ffmpeg-{}.tar.gz", m.source.commit));
    if !tarball.exists() {
        let part = tarball.with_extension("part");
        run_plain(
            Command::new("curl")
                .args(["-fsSL", "--retry", "3", "-o"])
                .arg(&part)
                .arg(&m.source.archive),
            "curl",
        )?;
        fs::rename(&part, &tarball)?;
    }
    let got = sha256(&fs::read(&tarball)?);
    ensure!(
        got == m.source.sha256,
        "{}: sha256 {got}, ffmpeg.toml pins {}",
        tarball.display(),
        m.source.sha256
    );
    run_plain(
        Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(work.join("src")),
        "tar",
    )?;
    let src = work.join("src").join(format!("FFmpeg-{}", m.source.commit));
    let tree = String::from_utf8(
        Command::new("nix")
            .args(["hash", "path"])
            .arg(&src)
            .output()
            .context("cannot run nix hash path")?
            .stdout,
    )?;
    ensure!(
        tree.trim() == m.source.tree,
        "the source tree hashes {}, ffmpeg.toml pins {}",
        tree.trim(),
        m.source.tree
    );
    // What version.sh takes from `git describe` in a checkout.
    fs::write(src.join("VERSION"), format!("{}\n", m.source.describe))?;

    // Directories holding only the static archives, so -lz and the
    // toolchain's -lmcfgthread cannot resolve to an import library.
    let deps = work.join("deps");
    symlink(&mcf, &deps.join("mcf").join("libmcfgthread.a"))?;
    symlink(
        &zlib.join("lib").join("libz.a"),
        &deps.join("zlib/lib/libz.a"),
    )?;
    for h in ["zlib.h", "zconf.h"] {
        symlink(
            &zlib_dev.join("include").join(h),
            &deps.join("zlib/include").join(h),
        )?;
    }
    let libz = sha256(&fs::read(deps.join("zlib/lib/libz.a"))?);

    let mut flags = m.build.configure.clone();
    flags.push(format!("--extra-version={}", m.build.revision));
    flags.push(format!("--extra-ldflags=-L{}", deps.join("mcf").display()));
    flags.push(format!(
        "--extra-cflags=-I{}",
        deps.join("zlib/include").display()
    ));
    flags.push(format!(
        "--extra-ldflags=-L{}",
        deps.join("zlib/lib").display()
    ));

    // configure and make run under the shell's own bash with only its tools
    // on PATH: nothing of the host's environment reaches them.
    let bash = on_path(&tools, "bash")?;
    let clean = |cmd: &mut Command| {
        cmd.env_clear()
            .env("PATH", &tools)
            .env("HOME", work.join("home"))
            .env("TMPDIR", work.join("tmp"))
            .env("LC_ALL", "C")
            .env("SOURCE_DATE_EPOCH", m.source.date.to_string())
            .env("PKG_CONFIG_LIBDIR", work.join("pc-empty"))
            .current_dir(work.join("build"))
            .stdin(Stdio::null());
    };
    let configure_out = out.join("configure.log");
    let mut configure = Command::new(&bash);
    clean(&mut configure);
    configure.arg(src.join("configure")).args(&flags);
    eprintln!("ffmpeg: configure at {}", work.display());
    run_logged(&mut configure, &configure_out, "configure")?;
    let summary = fs::read_to_string(&configure_out)?;
    ensure!(
        summary
            .lines()
            .any(|l| l == "License: LGPL version 2.1 or later"),
        "configure did not report \"License: LGPL version 2.1 or later\" ({})",
        configure_out.display()
    );
    let config_h = fs::read_to_string(work.join("build/config.h"))?;
    ensure!(
        config_h.lines().any(|l| l == "#define CONFIG_ZLIB 1"),
        "configure left zlib off (config.h)"
    );
    // What configure registered, read from the lists it generated, which the
    // libraries' lookups walk: exactly ffmpeg.toml's components, checked
    // before anything is compiled.
    let mut built = Components::new();
    let mut components_txt = String::new();
    for (kind, list, suffix) in COMPONENT_LISTS {
        let path = work.join("build").join(list);
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let names =
            registered(&text, suffix).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        let _ = write!(components_txt, "{kind}:");
        for name in &names {
            let _ = write!(components_txt, " {name}");
        }
        components_txt.push('\n');
        built.insert(kind.to_owned(), names);
    }
    fs::write(out.join("components.txt"), &components_txt)?;
    let diff = compare_components(&m.build.components, &built);
    ensure!(
        diff.is_empty(),
        "configure registered other components than ffmpeg.toml's [build.components] \
         (registered: {}):\n  {}",
        out.join("components.txt").display(),
        diff.join("\n  ")
    );
    let jobs = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let make_log = out.join("make.log");
    let mut make = Command::new(&bash);
    clean(&mut make);
    make.arg("-c").arg(format!(
        "make -j{jobs} SHELL={b} && make install DESTDIR={d} SHELL={b}",
        b = bash.display(),
        d = work.join("dest").display()
    ));
    eprintln!("ffmpeg: make -j{jobs}");
    run_logged(&mut make, &make_log, "make")?;

    // Imports, then hashes.
    let inst = work.join("dest/usr/local");
    let mut files = BTreeMap::new();
    let mut bad = Vec::new();
    let mut imports_txt = String::new();
    for dll in &m.build.dlls {
        let bin = fs::read(inst.join("bin").join(format!("{dll}.dll")))?;
        let imports = pe_imports(&bin).map_err(|e| anyhow::anyhow!("{dll}.dll: {e}"))?;
        let _ = writeln!(imports_txt, "{dll}.dll: {}", imports.join(" "));
        for name in disallowed_imports(&m.build.dlls, &imports) {
            bad.push(format!("{dll}.dll imports {name}"));
        }
        files.insert(format!("bin/{dll}.dll"), sha256(&bin));
        let def = fs::read(inst.join("lib").join(format!("{dll}.def")))?;
        files.insert(format!("lib/{dll}.def"), sha256(&def));
    }
    fs::write(out.join("imports.txt"), &imports_txt)?;
    ensure!(
        bad.is_empty(),
        "imports outside the allow-list: {}",
        bad.join("; ")
    );

    let inputs = [
        ("source", format!("{} {}", m.source.commit, m.source.sha256)),
        ("tree", m.source.tree.clone()),
        ("nixpkgs", nixpkgs),
        ("zlib", format!("{} {zlib_tarball}", m.zlib.version)),
        ("libz", libz),
        ("dir", m.build.dir.clone()),
    ];
    let hashes = record(m, &inputs, &files);
    fs::write(out.join("hashes.toml"), &hashes)?;
    for (file, hash) in &files {
        println!("{hash}  {file}");
    }
    match expect {
        Expect::Compare(pinned) => {
            let diff = compare_hashes(pinned, &files);
            ensure!(
                diff.is_empty(),
                "the build differs from the pinned hashes:\n  {}",
                diff.join("\n  ")
            );
            println!("ffmpeg: every DLL and .def file matches the pinned hashes");
        }
        Expect::Record => println!(
            "ffmpeg: no release pins revision {}: hashes recorded in {}",
            m.build.revision,
            out.join("hashes.toml").display()
        ),
    }

    pack(root, m, &work, &src, &zlib_src, &flags, &hashes, &out)
}

/// The notice the binary asset carries beside the licence texts.
fn notice(m: &Manifest, source_asset: &str) -> String {
    format!(
        "tapestry's FFmpeg build {version}\n\n\
         FFmpeg {describe} (commit {commit}), built by tapestry as a shared library\n\
         without GPL or nonfree components and licensed under the GNU Lesser General\n\
         Public License version 2.1 or later (COPYING.LGPLv2.1, LICENSE.md). Its exact\n\
         source, tapestry's changes (changes.diff), the source of zlib, and the build\n\
         inputs are in {source_asset}. The DLLs may be replaced by another build of\n\
         the same FFmpeg interface.\n\n\
         zlib {zlib} is linked in statically, under the zlib licence (zlib-LICENSE).\n\n\
         This software is based in part on the work of the Independent JPEG Group.\n",
        version = m.version(),
        describe = m.source.describe,
        commit = m.source.commit,
        zlib = m.zlib.version,
    )
}

/// Packs the two release assets into `out`.
#[expect(clippy::too_many_arguments, reason = "the build's pieces, each named")]
fn pack(
    root: &Path,
    m: &Manifest,
    work: &Path,
    src: &Path,
    zlib_src: &Path,
    flags: &[String],
    hashes: &str,
    out: &Path,
) -> anyhow::Result<()> {
    let (tag, binary, source) = m.names();
    let base = binary
        .trim_end_matches(".zip")
        .replace("-win64-lgpl-shared", "");
    let stage = work.join("stage");
    let bin_dir = stage.join(&base);
    let src_dir = stage.join(format!("{base}-source"));
    let inst = work.join("dest/usr/local");
    for d in [
        bin_dir.join("bin"),
        bin_dir.join("lib"),
        bin_dir.join("licenses"),
        src_dir.join("build"),
    ] {
        fs::create_dir_all(d)?;
    }
    let configure_line = flags.join("\n") + "\n";
    // zlib's licence from its own tarball.
    run_plain(
        Command::new("tar")
            .arg("-xzf")
            .arg(zlib_src)
            .arg("-C")
            .arg(&stage)
            .arg(format!("zlib-{}/LICENSE", m.zlib.version)),
        "tar (zlib's LICENSE)",
    )?;
    let zlib_licence = stage.join(format!("zlib-{}/LICENSE", m.zlib.version));

    // The binary asset.
    for dll in &m.build.dlls {
        fs::copy(
            inst.join("bin").join(format!("{dll}.dll")),
            bin_dir.join("bin").join(format!("{dll}.dll")),
        )?;
        fs::copy(
            inst.join("lib").join(format!("{dll}.def")),
            bin_dir.join("lib").join(format!("{dll}.def")),
        )?;
    }
    run_plain(
        Command::new("cp")
            .arg("-r")
            .arg(inst.join("include"))
            .arg(&bin_dir),
        "cp (headers)",
    )?;
    fs::write(bin_dir.join("VERSION"), format!("{}\n", m.version()))?;
    fs::write(bin_dir.join("CONFIGURE"), &configure_line)?;
    fs::write(bin_dir.join("hashes.toml"), hashes)?;
    fs::write(bin_dir.join("NOTICE"), notice(m, &source))?;
    for f in ["COPYING.LGPLv2.1", "LICENSE.md"] {
        fs::copy(src.join(f), bin_dir.join("licenses").join(f))?;
    }
    fs::copy(&zlib_licence, bin_dir.join("licenses/zlib-LICENSE"))?;

    // The source asset: the archive as fetched, tapestry's change to it
    // (the VERSION file version.sh reads), zlib's tarball and the build
    // inputs of this commit.
    fs::copy(
        root.join("target")
            .join("ffmpeg-dl")
            .join(format!("ffmpeg-{}.tar.gz", m.source.commit)),
        src_dir.join(format!("ffmpeg-{}.tar.gz", m.source.commit)),
    )?;
    fs::write(
        src_dir.join("changes.diff"),
        format!(
            "--- /dev/null\n+++ b/VERSION\n@@ -0,0 +1 @@\n+{}\n",
            m.source.describe
        ),
    )?;
    fs::copy(
        zlib_src,
        src_dir.join(format!("zlib-{}.tar.gz", m.zlib.version)),
    )?;
    fs::copy(&zlib_licence, src_dir.join("zlib-LICENSE"))?;
    for f in [
        "ffmpeg.toml",
        "flake.nix",
        "flake.lock",
        "xtask/src/ffmpeg.rs",
    ] {
        let name = f.rsplit('/').next().unwrap_or(f);
        fs::copy(root.join(f), src_dir.join("build").join(name))?;
    }
    fs::write(src_dir.join("build/CONFIGURE"), &configure_line)?;
    fs::write(
        src_dir.join("README"),
        format!(
            "The corresponding source of {tag}: FFmpeg's archive as fetched, the change\n\
             tapestry makes to it (changes.diff), zlib's release tarball and the build\n\
             inputs of the commit that built it (build/). Rebuild with\n\
             `cargo xtask ffmpeg build` in that commit's `ffmpeg` shell\n\
             (`nix develop .#ffmpeg`), which works at {dir}.\n",
            dir = m.build.dir
        ),
    )?;

    // Fixed times and a sorted order, so a repack gives the same bytes when
    // the tools do.
    let date = format!("@{}", m.source.date);
    run_plain(
        Command::new("find")
            .arg(&stage)
            .args(["-exec", "touch", "-h", "-d", &date, "{}", "+"]),
        "touch",
    )?;
    let zip = out.join(&binary);
    let _ = fs::remove_file(&zip);
    run_plain(
        Command::new("sh")
            .arg("-c")
            .arg("find \"$1\" -type f | LC_ALL=C sort | TZ=UTC zip -X -q -@ \"$2\"")
            .arg("sh")
            .arg(&base)
            .arg(&zip)
            .current_dir(&stage),
        "zip",
    )?;
    let xz = out.join(&source);
    run_plain(
        Command::new("tar")
            .args([
                "--sort=name",
                "--owner=0",
                "--group=0",
                "--numeric-owner",
                "--mtime",
                &date,
                "-cJf",
            ])
            .arg(&xz)
            .arg(format!("{base}-source"))
            .current_dir(&stage),
        "tar (source asset)",
    )?;
    println!("ffmpeg: release {tag}, assets in {}:", out.display());
    for a in [&zip, &xz] {
        println!(
            "{}  {}",
            sha256(&fs::read(a)?),
            a.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        );
    }
    Ok(())
}

/// The steps of a fetch that reach outside the workspace: downloading a URL
/// to a path, unpacking a zip into a directory, and making an import library
/// from a `.def` file. `fetch` passes the real ones ([`curl_download`],
/// [`unzip_asset`], [`import_library`]); tests pass their own.
pub struct FetchSteps<'a> {
    pub download: &'a dyn Fn(&str, &Path) -> anyhow::Result<()>,
    pub unpack: &'a dyn Fn(&Path, &Path) -> anyhow::Result<()>,
    pub import_library: &'a dyn Fn(&Path, &Path) -> anyhow::Result<()>,
}

/// `cargo xtask ffmpeg fetch`: [`install_release`] into `target/` with the
/// real steps.
fn fetch(root: &Path) -> anyhow::Result<()> {
    let m = read_manifest(&root.join("ffmpeg.toml"))?;
    install_release(
        &root.join("target"),
        &m,
        &FetchSteps {
            download: &curl_download,
            unpack: &unzip_asset,
            import_library: &import_library,
        },
    )
}

/// The directory the pinned release's binary asset unpacks to: the asset's
/// name without `-win64-lgpl-shared.zip`, as the build packs it.
fn release_top(release: &Release) -> anyhow::Result<&str> {
    release
        .binary
        .name
        .strip_suffix("-win64-lgpl-shared.zip")
        .with_context(|| {
            format!(
                "[release]'s binary asset {} is no `<name>-win64-lgpl-shared.zip`",
                release.binary.name
            )
        })
}

/// The `.def` files of `[release.files]` (`lib/<dll>.def`), each with the
/// import library made from it (`lib/<name>.lib`, the DLL's name without its
/// major version). A path that is neither `bin/<dll>.dll` nor
/// `lib/<dll>.def` is refused.
fn release_defs(release: &Release) -> anyhow::Result<Vec<(String, String)>> {
    let mut defs = Vec::new();
    for path in release.files.keys() {
        let (dir, name) = path.split_once('/').unwrap_or(("", path));
        let plain = !name.contains(['/', '\\']) && !name.starts_with('.');
        match (dir, name.rsplit_once('.')) {
            ("bin", Some((dll, "dll"))) if plain && !dll.is_empty() => {}
            ("lib", Some((dll, "def"))) if plain && !dll.is_empty() => {
                let base = dll.split('-').next().unwrap_or(dll);
                defs.push((path.clone(), format!("lib/{base}.lib")));
            }
            _ => bail!("[release.files] has `{path}`, neither bin/<dll>.dll nor lib/<dll>.def"),
        }
    }
    ensure!(!defs.is_empty(), "[release.files] lists no .def file");
    Ok(defs)
}

/// The files of `[release.files]` that `dir` does not hold with their pinned
/// sha256, one line each, naming the file.
pub fn unpinned_files(dir: &Path, release: &Release) -> Vec<String> {
    release
        .files
        .iter()
        .filter_map(|(path, pinned)| match fs::read(dir.join(path)) {
            Ok(bytes) => {
                let got = sha256(&bytes);
                (got != *pinned).then(|| format!("{path}: sha256 {got}, pinned {pinned}"))
            }
            Err(_) => Some(format!("{path}: missing")),
        })
        .collect()
}

/// Installs the release `m` pins into `<target>/ffmpeg`, all of it taken from
/// `[release]` (the asset, the directory it unpacks to, its files), never
/// from `[build]`, which may already build a newer revision.
///
/// An installation whose `TAG` is the release's is kept only when every file
/// of `[release.files]` in it has its pinned sha256; its import libraries
/// are then made again from those `.def` files. Otherwise the binary asset
/// is downloaded, refused unless its sha256 is the pinned one before
/// anything is unpacked, unpacked, and refused unless every pinned file in
/// it has its pinned sha256; only then are its import libraries made and its
/// `TAG` written, and it replaces the directory. With no release pinned,
/// nothing is done.
pub fn install_release(target: &Path, m: &Manifest, steps: &FetchSteps<'_>) -> anyhow::Result<()> {
    let Some(release) = &m.release else {
        println!("ffmpeg: ffmpeg.toml pins no release yet; nothing to fetch");
        return Ok(());
    };
    let defs = release_defs(release)?;
    let make_libraries = |dir: &Path| -> anyhow::Result<()> {
        for (def, lib) in &defs {
            (steps.import_library)(&dir.join(def), &dir.join(lib))?;
        }
        Ok(())
    };
    let dir = target.join("ffmpeg");
    if fs::read_to_string(dir.join("TAG")).is_ok_and(|t| t.trim() == release.tag) {
        let unpinned = unpinned_files(&dir, release);
        if unpinned.is_empty() {
            make_libraries(&dir)?;
            println!(
                "ffmpeg: {} already in {}, every file as pinned",
                release.tag,
                dir.display()
            );
            return Ok(());
        }
        println!(
            "ffmpeg: {} in {} differs from its pins; installing it again:\n  {}",
            release.tag,
            dir.display(),
            unpinned.join("\n  ")
        );
    }
    fs::create_dir_all(target)?;
    let download = target.join(&release.binary.name);
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        release.repository, release.tag, release.binary.name
    );
    (steps.download)(&url, &download)?;
    let staging = target.join("ffmpeg.new");
    let _ = fs::remove_dir_all(&staging);
    unpack_checked(&download, &release.binary.sha256, &staging, steps.unpack)?;
    let top = staging.join(release_top(release)?);
    let unpinned = unpinned_files(&top, release);
    ensure!(
        unpinned.is_empty(),
        "{}: its files differ from ffmpeg.toml's [release.files]: refused, nothing installed:\n  {}",
        release.binary.name,
        unpinned.join("\n  ")
    );
    make_libraries(&top)?;
    fs::write(top.join("TAG"), format!("{}\n", release.tag))?;
    let _ = fs::remove_dir_all(&dir);
    fs::rename(&top, &dir)?;
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_file(&download);
    println!(
        "ffmpeg: {} in {}; its corresponding source is the release's {} (sha256 {})",
        release.tag,
        dir.display(),
        release.source.name,
        release.source.sha256
    );
    Ok(())
}

fn curl_download(url: &str, to: &Path) -> anyhow::Result<()> {
    run_plain(
        Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(to)
            .arg(url),
        "curl",
    )
}

/// Unpacks the zip at `zip` into `into`: Windows' own tar reads zip; Linux
/// has unzip in the setup.
fn unzip_asset(zip: &Path, into: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(into)?;
    if cfg!(windows) {
        run_plain(
            Command::new("tar").arg("-xf").arg(zip).arg("-C").arg(into),
            "tar",
        )
    } else {
        run_plain(
            Command::new("unzip").arg("-q").arg(zip).arg("-d").arg(into),
            "unzip",
        )
    }
}

/// An import library for the DLL `def` describes: MSVC's `lib.exe` on
/// Windows (found as the `cc` crate finds MSVC's tools), `llvm-dlltool` on
/// Linux. The release's `.def` files hold only `EXPORTS`, no `LIBRARY`
/// line, so the DLL's name is the `.def` file's own (`avutil-61.def` names
/// `avutil-61.dll`): `lib.exe` takes it from the file name, `llvm-dlltool`
/// only from `-D`, refusing the `.def` without it.
fn import_library(def: &Path, lib: &Path) -> anyhow::Result<()> {
    if cfg!(windows) {
        let mut cmd = cc::windows_registry::find("x86_64-pc-windows-msvc", "lib.exe")
            .context("MSVC's lib.exe was not found")?;
        let mut def_arg = OsString::from("/def:");
        def_arg.push(def);
        let mut out_arg = OsString::from("/out:");
        out_arg.push(lib);
        run_plain(
            cmd.args(["/nologo", "/machine:x64"])
                .arg(def_arg)
                .arg(out_arg),
            "lib.exe",
        )
    } else {
        let mut dll = def
            .file_stem()
            .with_context(|| format!("{} has no file name", def.display()))?
            .to_owned();
        dll.push(".dll");
        run_plain(
            Command::new("llvm-dlltool")
                .args(["-m", "i386:x86-64", "-d"])
                .arg(def)
                .arg("-D")
                .arg(&dll)
                .arg("-l")
                .arg(lib),
            "llvm-dlltool",
        )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    const MANIFEST: &str = r#"
version = 1
[source]
commit = "c"
describe = "n9"
archive = "https://example.invalid/a.tar.gz"
sha256 = "s"
tree = "t"
date = 1
[build]
revision = 2
dir = "/tmp/x"
dlls = ["avutil-61", "avcodec-63"]
configure = ["--enable-shared"]
[build.components]
decoders = ["h264", "vp8"]
parsers = ["h264"]
bsfs = []
demuxers = ["mov"]
protocols = []
[zlib]
version = "1.4"
sha256 = "z"
"#;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|&(f, h)| (f.to_owned(), h.to_owned()))
            .collect()
    }

    fn release(revision: i64) -> String {
        format!(
            "{MANIFEST}[release]\nrevision = {revision}\nrepository = \"o/r\"\ntag = \"t\"\n\
             binary = {{ name = \"b.zip\", sha256 = \"1\" }}\nsource = {{ name = \"s.tar.xz\", sha256 = \"2\" }}\n\
             [release.files]\n\"bin/avutil-61.dll\" = \"aa\"\n\"lib/avutil-61.def\" = \"bb\"\n"
        )
    }

    /// A PE32+ image whose import directory names `imports` and whose
    /// delay-import directory names `delayed`.
    fn pe(imports: &[&str], delayed: &[&str]) -> Vec<u8> {
        let mut b = vec![0_u8; 0x800];
        let put16 =
            |b: &mut Vec<u8>, at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let put32 =
            |b: &mut Vec<u8>, at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        b[..2].copy_from_slice(b"MZ");
        put32(&mut b, 0x3c, 0x40);
        b[0x40..0x44].copy_from_slice(b"PE\0\0");
        put16(&mut b, 0x44, 0x8664);
        put16(&mut b, 0x46, 1);
        put16(&mut b, 0x54, 240);
        let optional = 0x58;
        put16(&mut b, optional, 0x20b);
        put32(&mut b, optional + 108, 16);
        // One section: RVA 0x1000 at file offset 0x200, 0x600 bytes.
        let section = optional + 240;
        b[section..section + 6].copy_from_slice(b".idata");
        put32(&mut b, section + 8, 0x600);
        put32(&mut b, section + 12, 0x1000);
        put32(&mut b, section + 16, 0x600);
        put32(&mut b, section + 20, 0x200);
        let rva = |file: usize| u32::try_from(file - 0x200 + 0x1000).unwrap();
        // Descriptors at 0x200 (imports) and 0x300 (delay), names from 0x500.
        let mut name_at = 0x500;
        for (dir, table, size, field, names) in
            [(1, 0x200, 20, 12, imports), (13, 0x300, 32, 4, delayed)]
        {
            if names.is_empty() {
                continue;
            }
            put32(&mut b, optional + 112 + 8 * dir, rva(table));
            for (i, n) in names.iter().enumerate() {
                put32(&mut b, table + size * i + field, rva(name_at));
                b[name_at..name_at + n.len()].copy_from_slice(n.as_bytes());
                name_at += n.len() + 1;
            }
        }
        b
    }

    #[test]
    fn the_manifest_is_read_and_another_version_refused() {
        let m = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(m.version(), "n9-2");
        assert_eq!(
            m.names(),
            (
                "ffmpeg-n9-2".to_owned(),
                "tapestry-ffmpeg-n9-2-win64-lgpl-shared.zip".to_owned(),
                "tapestry-ffmpeg-n9-2-source.tar.xz".to_owned()
            )
        );
        assert!(m.release.is_none());
        assert_eq!(m.build.components.len(), COMPONENT_LISTS.len());
        assert_eq!(
            m.build.components["decoders"],
            BTreeSet::from(["h264".to_owned(), "vp8".to_owned()])
        );
        let other = MANIFEST.replace("version = 1", "version = 2");
        assert!(Manifest::parse(&other).is_err());
        let missing = MANIFEST.replace("protocols = []\n", "");
        assert!(Manifest::parse(&missing).is_err(), "every kind is listed");
        let unknown = MANIFEST.replace("protocols = []", "protocols = []\nencoders = []");
        assert!(Manifest::parse(&unknown).is_err(), "no other kind");
    }

    /// One of configure's generated lists, holding `entries`.
    fn list(entries: &[&str]) -> String {
        let mut s = String::from("static const FFCodec * const codec_list[] = {\n");
        for e in entries {
            let _ = writeln!(s, "    &ff_{e},");
        }
        s.push_str("    NULL };\n");
        s
    }

    /// The build's component check (design.md "Libraries", the FFmpeg row):
    /// what configure registered passes only when every kind holds exactly
    /// the manifest's names; an off-list decoder built again (flashsv) or a
    /// listed one left out (vp8) fails, naming it. Encoders in the codec
    /// list are not decoders; a line of another form, or a list cut short,
    /// is refused rather than read as fewer components.
    #[test]
    fn ffmpeg_build_registers_exactly_the_listed_components() {
        let listed = Manifest::parse(MANIFEST).unwrap().build.components;
        let built = |decoders: &[&str]| -> Components {
            let lists = [
                ("decoders", list(decoders), "_decoder"),
                ("parsers", list(&["h264_parser"]), "_parser"),
                ("bsfs", list(&[]), "_bsf"),
                ("demuxers", list(&["mov_demuxer"]), "_demuxer"),
                ("protocols", list(&[]), "_protocol"),
            ];
            lists
                .iter()
                .map(|(kind, text, suffix)| ((*kind).to_owned(), registered(text, suffix).unwrap()))
                .collect()
        };
        let same = built(&["h264_decoder", "mpeg4_encoder", "vp8_decoder"]);
        assert!(compare_components(&listed, &same).is_empty());

        let extra = built(&["flashsv_decoder", "h264_decoder", "vp8_decoder"]);
        assert_eq!(
            compare_components(&listed, &extra),
            ["decoders: flashsv is registered and not listed"]
        );
        let short = built(&["h264_decoder"]);
        assert_eq!(
            compare_components(&listed, &short),
            ["decoders: vp8 is listed and not registered"]
        );

        assert!(registered(&list(&["file_protocol"]), "_bsf").is_err());
        let open = "static const URLProtocol * const url_protocols[] = {\n";
        assert!(registered(open, "_protocol").is_err(), "no closing");
        let after = list(&[]) + "    &ff_file_protocol,\n";
        assert!(registered(&after, "_protocol").is_err(), "past the closing");
        assert!(registered("#define X 1\n    NULL };\n", "_bsf").is_err());
    }

    #[test]
    fn imports_are_read_from_both_directories() {
        let image = pe(&["KERNEL32.dll", "avutil-61.dll"], &["bcrypt.dll"]);
        assert_eq!(
            pe_imports(&image).unwrap(),
            ["KERNEL32.dll", "avutil-61.dll", "bcrypt.dll"]
        );
        assert!(pe_imports(b"MZ").is_err());
    }

    /// The build step's import check (design.md "Threat Model" T02): the
    /// allow-list's DLLs, the C runtime's API sets and the build's own DLLs
    /// pass; a DLL importing USER32, or a toolchain runtime, fails.
    #[test]
    fn ffmpeg_build_imports_only_the_allowed_dlls() {
        let dlls = ["avutil-61".to_owned(), "avcodec-63".to_owned()];
        let allowed = pe(
            &[
                "KERNEL32.dll",
                "WS2_32.dll",
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "avutil-61.dll",
            ],
            &["bcrypt.dll"],
        );
        let imports = pe_imports(&allowed).unwrap();
        assert!(disallowed_imports(&dlls, &imports).is_empty());
        for bad in ["USER32.dll", "libwinpthread-1.dll"] {
            let image = pe(&["KERNEL32.dll", bad], &[]);
            let imports = pe_imports(&image).unwrap();
            assert_eq!(disallowed_imports(&dlls, &imports), [bad], "{bad}");
        }
        let delayed = pe(&["KERNEL32.dll"], &["GDI32.dll"]);
        let imports = pe_imports(&delayed).unwrap();
        assert_eq!(disallowed_imports(&dlls, &imports), ["GDI32.dll"]);
    }

    /// The executables' import check (design.md "Threat Model" T02): the
    /// worker imports the system's core, its C runtime and the build's DLLs,
    /// and fails with a graphics DLL, imported or delay-loaded; the game
    /// imports no DLL of the build.
    #[test]
    fn executables_import_what_the_worker_split_allows() {
        let dlls = ["avutil-61".to_owned(), "avcodec-63".to_owned()];
        let worker = pe(
            &[
                "KERNEL32.dll",
                "ntdll.dll",
                "VCRUNTIME140.dll",
                "api-ms-win-core-synch-l1-2-0.dll",
                "api-ms-win-crt-heap-l1-1-0.dll",
                "AVCODEC-63.dll",
            ],
            &["avutil-61.dll"],
        );
        let imports = pe_imports(&worker).unwrap();
        assert!(worker_disallowed_imports(&dlls, &imports).is_empty());
        assert_eq!(
            game_ffmpeg_imports(&dlls, &imports),
            ["AVCODEC-63.dll", "avutil-61.dll"]
        );
        for bad in ["USER32.dll", "gdi32.dll", "bcrypt.dll"] {
            let image = pe(&["KERNEL32.dll", bad], &[]);
            let imports = pe_imports(&image).unwrap();
            assert_eq!(worker_disallowed_imports(&dlls, &imports), [bad], "{bad}");
        }
        let delayed = pe(&["KERNEL32.dll"], &["GDI32.dll"]);
        let imports = pe_imports(&delayed).unwrap();
        assert_eq!(worker_disallowed_imports(&dlls, &imports), ["GDI32.dll"]);
        let game = pe(&["USER32.dll", "GDI32.dll", "KERNEL32.dll"], &[]);
        let imports = pe_imports(&game).unwrap();
        assert!(game_ffmpeg_imports(&dlls, &imports).is_empty());
        let game = pe(&["USER32.dll"], &["avutil-61.dll"]);
        let imports = pe_imports(&game).unwrap();
        assert_eq!(game_ffmpeg_imports(&dlls, &imports), ["avutil-61.dll"]);
    }

    /// The build's fixed directory: a claim starts it empty with its marker,
    /// a second claim while the first is held is refused and leaves it alone,
    /// a claim after the first is released empties it again, and a directory
    /// without the marker is never removed.
    #[test]
    fn the_build_dir_is_claimed_by_one_build_and_starts_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("build");
        let first = claim_build_dir(&dir).unwrap();
        assert!(dir.join(MARKER).is_file());
        fs::write(dir.join("left.o"), "x").unwrap();
        let e = claim_build_dir(&dir).unwrap_err();
        assert!(e.to_string().contains("another build holds"), "{e}");
        assert!(
            dir.join("left.o").exists(),
            "a refused claim touches nothing"
        );
        drop(first);
        let _second = claim_build_dir(&dir).unwrap();
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [MARKER], "the next build starts empty");

        let foreign = tmp.path().join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("keep"), "x").unwrap();
        assert!(claim_build_dir(&foreign).is_err());
        assert!(foreign.join("keep").exists());
    }

    /// `cargo xtask ffmpeg fetch` checks the asset's sha256 before unpacking
    /// anything: an asset one byte off is refused and nothing is unpacked.
    #[test]
    fn ffmpeg_asset_is_checked_by_its_pinned_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let asset = dir.path().join("a.zip");
        let bytes = b"the asset's bytes".to_vec();
        fs::write(&asset, &bytes).unwrap();
        let pinned = sha256(&bytes);
        let into = dir.path().join("out");
        let mut unpacked = false;
        unpack_checked(&asset, &pinned, &into, |_, into| {
            unpacked = true;
            Ok(fs::create_dir_all(into)?)
        })
        .unwrap();
        assert!(unpacked && into.exists());

        let mut off = bytes.clone();
        off[0] ^= 1;
        fs::write(&asset, &off).unwrap();
        let into = dir.path().join("refused");
        let mut called = false;
        let e = unpack_checked(&asset, &pinned, &into, |_, _| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
        assert!(!called, "nothing unpacked");
        assert!(!into.exists());
    }

    const DLL: &[u8] = b"the pinned avutil-61.dll";
    const DEF: &[u8] = b"EXPORTS\n av_malloc\n";

    /// A manifest whose release (revision 2, tag `ffmpeg-n9-2`) pins `asset`
    /// as its binary asset and [`DLL`] and [`DEF`] as `bin/avutil-61.dll` and
    /// `lib/avutil-61.def`, while `[build]` builds `build_revision` with
    /// `build_dlls`.
    fn pinning(asset: &[u8], build_revision: i64, build_dlls: &str) -> Manifest {
        let text = MANIFEST
            .replace("revision = 2", &format!("revision = {build_revision}"))
            .replace(
                r#"dlls = ["avutil-61", "avcodec-63"]"#,
                &format!("dlls = {build_dlls}"),
            );
        let release = format!(
            "[release]\nrevision = 2\nrepository = \"o/r\"\ntag = \"ffmpeg-n9-2\"\n\
             binary = {{ name = \"tapestry-ffmpeg-n9-2-win64-lgpl-shared.zip\", sha256 = \"{}\" }}\n\
             source = {{ name = \"tapestry-ffmpeg-n9-2-source.tar.xz\", sha256 = \"s\" }}\n\
             [release.files]\n\"bin/avutil-61.dll\" = \"{}\"\n\"lib/avutil-61.def\" = \"{}\"\n",
            sha256(asset),
            sha256(DLL),
            sha256(DEF)
        );
        Manifest::parse(&format!("{text}{release}")).unwrap()
    }

    /// [`install_release`] with its outside steps faked: the download writes
    /// `asset` and counts itself, the unpack writes the release's directory
    /// with `dll` and [`DEF`], and an import library is `lib of ` and its
    /// `.def`.
    struct Fake {
        asset: Vec<u8>,
        dll: RefCell<Vec<u8>>,
        downloads: Cell<u32>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                asset: b"the binary asset".to_vec(),
                dll: RefCell::new(DLL.to_vec()),
                downloads: Cell::new(0),
            }
        }

        fn install(&self, target: &Path, m: &Manifest) -> anyhow::Result<()> {
            let download = |_: &str, to: &Path| -> anyhow::Result<()> {
                self.downloads.set(self.downloads.get() + 1);
                Ok(fs::write(to, &self.asset)?)
            };
            let unpack = |_: &Path, into: &Path| -> anyhow::Result<()> {
                let top = into.join("tapestry-ffmpeg-n9-2");
                fs::create_dir_all(top.join("bin"))?;
                fs::create_dir_all(top.join("lib"))?;
                fs::write(top.join("bin/avutil-61.dll"), &*self.dll.borrow())?;
                Ok(fs::write(top.join("lib/avutil-61.def"), DEF)?)
            };
            let import_library = |def: &Path, lib: &Path| -> anyhow::Result<()> {
                Ok(fs::write(
                    lib,
                    [b"lib of ".as_slice(), &fs::read(def)?].concat(),
                )?)
            };
            install_release(
                target,
                m,
                &FetchSteps {
                    download: &download,
                    unpack: &unpack,
                    import_library: &import_library,
                },
            )
        }
    }

    /// `cargo xtask ffmpeg fetch` installs the release `[release]` pins while
    /// `[build]` already builds a newer revision with other DLLs: the
    /// asset's directory and the files it checks and makes import libraries
    /// for are the release's. A pinned path outside `bin/` and `lib/` is
    /// refused.
    #[test]
    fn fetch_installs_the_pinned_release_while_a_newer_revision_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let m = pinning(&fake.asset, 3, r#"["avutil-62", "avcodec-64"]"#);
        fake.install(tmp.path(), &m).unwrap();
        let dir = tmp.path().join("ffmpeg");
        assert_eq!(
            fs::read_to_string(dir.join("TAG")).unwrap(),
            "ffmpeg-n9-2\n"
        );
        assert_eq!(fs::read(dir.join("bin/avutil-61.dll")).unwrap(), DLL);
        assert_eq!(
            fs::read(dir.join("lib/avutil.lib")).unwrap(),
            [b"lib of ".as_slice(), DEF].concat()
        );
        assert_eq!(fake.downloads.get(), 1);

        let mut outside = pinning(&fake.asset, 2, r#"["avutil-61"]"#);
        let files = &mut outside.release.as_mut().unwrap().files;
        files.insert("../avutil-61.dll".to_owned(), sha256(DLL));
        let e = fake
            .install(&tmp.path().join("other"), &outside)
            .unwrap_err();
        assert!(e.to_string().contains("`../avutil-61.dll`"), "{e}");
    }

    /// `cargo xtask ffmpeg fetch` keeps an installation only while every
    /// pinned file in it has its pinned sha256: intact, nothing is downloaded
    /// and its import library is made again; a changed DLL or a missing
    /// `.def` under the release's `TAG` is installed again, its import
    /// library made from the restored `.def` rather than left stale; an
    /// asset whose files differ from `[release.files]` is refused and
    /// nothing is installed.
    #[test]
    fn fetch_installs_again_a_release_that_differs_from_its_pins() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Fake::new();
        let m = pinning(&fake.asset, 2, r#"["avutil-61"]"#);
        let dir = tmp.path().join("ffmpeg");
        let (dll, def, lib) = (
            dir.join("bin/avutil-61.dll"),
            dir.join("lib/avutil-61.def"),
            dir.join("lib/avutil.lib"),
        );
        let made = [b"lib of ".as_slice(), DEF].concat();
        fake.install(tmp.path(), &m).unwrap();
        fs::write(&lib, "stale").unwrap();
        fake.install(tmp.path(), &m).unwrap();
        assert_eq!(fake.downloads.get(), 1, "an intact installation is kept");
        assert_eq!(fs::read(&lib).unwrap(), made);

        fs::write(&dll, "changed").unwrap();
        fake.install(tmp.path(), &m).unwrap();
        assert_eq!(fake.downloads.get(), 2, "a changed DLL is installed again");
        assert_eq!(fs::read(&dll).unwrap(), DLL);

        fs::write(&lib, "stale").unwrap();
        fs::remove_file(&def).unwrap();
        fake.install(tmp.path(), &m).unwrap();
        assert_eq!(fake.downloads.get(), 3, "a missing .def is installed again");
        assert_eq!(fs::read(&def).unwrap(), DEF);
        assert_eq!(fs::read(&lib).unwrap(), made);

        *fake.dll.borrow_mut() = b"another build's DLL".to_vec();
        fs::write(&dll, "changed").unwrap();
        let e = fake.install(tmp.path(), &m).unwrap_err().to_string();
        assert!(
            e.contains("refused") && e.contains("bin/avutil-61.dll"),
            "{e}"
        );
        assert_eq!(fs::read(&dll).unwrap(), b"changed", "nothing installed");
    }

    /// The fetch's real import-library step, not a fake, on a `.def` file
    /// like the release's (`EXPORTS` alone, no `LIBRARY` line): the library
    /// it makes imports from the versioned DLL the `.def` file is named for.
    /// Windows always has `lib.exe` here; Linux runs it where `llvm-dlltool`
    /// is on `PATH` (the `llvm` package of the cargo-xwin check).
    #[test]
    fn import_library_imports_from_the_dll_its_def_is_named_for() {
        if !cfg!(windows)
            && Command::new("llvm-dlltool")
                .arg("--version")
                .output()
                .is_err()
        {
            eprintln!("llvm-dlltool is not on PATH: the import library is not made");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let def = tmp.path().join("avutil-61.def");
        fs::write(&def, "EXPORTS\n    av_malloc\n    av_free\n").unwrap();
        let lib = tmp.path().join("avutil.lib");
        import_library(&def, &lib).unwrap();
        let bytes = fs::read(&lib).unwrap();
        for name in ["avutil-61.dll", "av_malloc", "av_free"] {
            assert!(
                bytes.windows(name.len()).any(|w| w == name.as_bytes()),
                "{} does not name {name}",
                lib.display()
            );
        }
    }

    /// The build step's comparison: the manifest's own hashes pass; one DLL's
    /// hash one character off fails, naming the file; a manifest pinning the
    /// revision being built is compared with, one pinning another revision
    /// leaves the hashes recorded and nothing compared.
    #[test]
    fn ffmpeg_hash_comparison_fails_on_any_difference() {
        let pinned = files(&[("bin/avutil-61.dll", "aa"), ("lib/avutil-61.def", "bb")]);
        assert!(compare_hashes(&pinned, &pinned).is_empty());
        let off = files(&[("bin/avutil-61.dll", "ab"), ("lib/avutil-61.def", "bb")]);
        let diff = compare_hashes(&pinned, &off);
        assert_eq!(diff.len(), 1, "{diff:?}");
        assert!(diff[0].starts_with("bin/avutil-61.dll:"), "{diff:?}");
        let more = files(&[
            ("bin/avutil-61.dll", "aa"),
            ("lib/avutil-61.def", "bb"),
            ("bin/x.dll", "cc"),
        ]);
        assert_eq!(compare_hashes(&pinned, &more).len(), 1);

        let same = Manifest::parse(&release(2)).unwrap();
        assert_eq!(expectation(&same, None), Expect::Compare(&pinned));
        let other = Manifest::parse(&release(1)).unwrap();
        assert_eq!(expectation(&other, None), Expect::Record);
        let unpinned = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(expectation(&unpinned, None), Expect::Record);
        assert_eq!(expectation(&unpinned, Some(&off)), Expect::Compare(&off));
    }

    #[test]
    fn expected_files_come_from_a_record_or_a_pinning_manifest() {
        let m = Manifest::parse(MANIFEST).unwrap();
        let f = files(&[("bin/avutil-61.dll", "aa"), ("lib/avutil-61.def", "bb")]);
        let text = record(&m, &[("nixpkgs", "r".to_owned())], &f);
        assert_eq!(expected_files(&text, 2).unwrap(), f);
        assert!(text.contains("version = \"n9-2\""));
        assert!(expected_files(&text, 3).is_err());
        let pinned = files(&[("bin/avutil-61.dll", "aa"), ("lib/avutil-61.def", "bb")]);
        assert_eq!(expected_files(&release(2), 2).unwrap(), pinned);
        assert!(expected_files(&release(1), 2).is_err());
    }
}
