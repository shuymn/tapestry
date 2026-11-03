//! A Lua skin's import memory, measured (docs/stage3.md §3, Lua
//! evidence): a counting allocator keeps the live bytes and their peak on
//! the thread that opts in, a reallocation counted as a new buffer, a copy
//! and a free. The VM allocates through it (`mlua`'s allocator is Rust's
//! global one), which the positive control proves. Each fixture runs
//! through [`read_lua_header`] and [`LuaHeader::import`]: its peak covers
//! the source snapshots, each VM, the conversion, the common import and
//! the teardown, and every byte is freed again once the results are
//! dropped. The process's peak resident memory is recorded too. The
//! fixtures run in one test, one after another.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use skin_beatoraja::lua::MAX_CONVERTED_BYTES;
use skin_beatoraja::lua::limits::MAX_LUA_MEMORY;
use skin_beatoraja::{
    Cancel, ImportError, InputLimit, LuaHeader, LuaImported, LuaLimit, LuaWhy, read_lua_header,
};

/// The recorded bound: the largest measured peak over the fixtures plus
/// 25 %.
const BOUND: i64 = 136_365_007;

struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
}

fn tracked() -> bool {
    TRACK.try_with(Cell::get).unwrap_or(false)
}

fn grow(n: usize) {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    let live = LIVE.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

fn shrink(n: usize) {
    LIVE.fetch_sub(i64::try_from(n).unwrap_or(i64::MAX), Ordering::Relaxed);
}

// SAFETY: delegates to `System`; only counts sizes, which never
// allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if tracked() {
            grow(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if tracked() {
            shrink(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if tracked() {
            // The new buffer, then the old freed: both live at the peak.
            grow(new_size);
            shrink(layout.size());
        }
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// `f`'s peak live bytes above where it started, and the live bytes left
/// once its result is dropped.
fn measure<T>(f: impl FnOnce() -> T) -> (i64, i64) {
    LIVE.store(0, Ordering::Relaxed);
    PEAK.store(0, Ordering::Relaxed);
    TRACK.with(|t| t.set(true));
    let out = f();
    drop(out);
    TRACK.with(|t| t.set(false));
    (PEAK.load(Ordering::Relaxed), LIVE.load(Ordering::Relaxed))
}

fn header(root: &Path) -> Result<LuaHeader, ImportError> {
    read_lua_header(root, "s.luaskin", &Cancel::new())
}

fn both(root: &Path) -> Result<LuaImported, ImportError> {
    common::lua_import(root)
}

/// A folder holding `entry` as `s.luaskin` and a 64×64 `a.png`.
fn folder(entry: &str) -> tempfile::TempDir {
    let d = common::skin(&[("s.luaskin", entry)]);
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// A skin whose configured pass runs `body` and draws `a.png`.
fn configured(body: &str) -> String {
    format!(
        "local skin = {{ type = 0, w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         {body}\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", w = 64, h = 64 }} }}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ w = 64, h = 64 }} }} }} }}\n\
         return skin\n"
    )
}

/// [`configured`] running `after` once its destinations are set.
fn drawing(body: &str, after: &str) -> String {
    configured(body).replace("\nreturn skin\n", &format!("\n{after}\nreturn skin\n"))
}

#[test]
fn lua_import_memory_is_bounded_and_freed() {
    let mut peaks = Vec::new();
    let mut record = |name: &str, (peak, left): (i64, i64)| {
        println!("{name}: peak {peak} bytes, {left} left");
        assert_eq!(left, 0, "{name}: bytes left after the results are dropped");
        peaks.push((name.to_owned(), peak));
        peak
    };

    // The smallest skin: one VM a pass, the arena and the import.
    let small = folder(&configured(""));
    let base = record("small", measure(|| both(small.path()).unwrap()));

    // The positive control: a 32 MiB string the VM builds appears in the
    // count.
    let control = folder(&configured(
        "local s = \"x\"\nfor i = 1, 25 do s = s .. s end",
    ));
    let p = record("control", measure(|| both(control.path()).unwrap()));
    assert!(
        p - base >= 32 << 20,
        "the counter misses VM allocation: {p}"
    );

    // Caught errors keep their values in Lua: ten caught errors carrying
    // an 8 MiB string hold that one string, not ten copies outside the VM
    // (each message is charged as native work, so ten fit the budget).
    let errors = folder(&configured(
        "local big = \"x\"\nfor i = 1, 23 do big = big .. big end\n\
         local kept = {}\n\
         for i = 1, 5 do local ok, e = pcall(error, big, 0) kept[i] = e end\n\
         for i = 1, 5 do local ok, e = pcall(table.sort, { 2, 1 }, function() error(big, 0) end) kept[5 + i] = e end",
    ));
    let p = record("retained_errors", measure(|| both(errors.path()).unwrap()));
    assert!(p < 40 << 20, "caught errors grew memory: {p}");

    // A bridge's error is a Lua string too, whatever the names a
    // traceback would find: 20,000 kept errors with an 8 MiB name in
    // every table the skin reaches.
    let bridge = folder(&configured(
        "local big = \"x\"\nfor i = 1, 23 do big = big .. big end\n\
         for _, t in ipairs({ _G, string, table, math, skin_config }) do t[big] = pcall end\n\
         local kept = {}\n\
         for i = 1, 20000 do local ok, e = pcall(skin_config.get_path, i) kept[i] = e end",
    ));
    let p = record("bridge_errors", measure(|| both(bridge.path()).unwrap()));
    assert!(p < 48 << 20, "bridge errors grew memory: {p}");

    // The VM at its cap: refused, everything freed.
    let vm_cap = folder(&configured(
        "local t = {}\nlocal s = \"x\"\nfor i = 1, 20 do s = s .. s end\nfor i = 1, 100 do t[i] = s .. i end",
    ));
    let p = record(
        "memory_cap",
        measure(|| {
            let e = both(vm_cap.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Lua {
                        why: LuaWhy::Limit(LuaLimit::Memory),
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );
    let limit = i64::try_from(MAX_LUA_MEMORY).unwrap();
    assert!(p <= limit + (8 << 20), "the VM passed its cap: {p}");

    // A few lines that claim the node cap: refused by the conversion
    // before the table is kept whole.
    let nodes = folder(&configured(
        "local t = { 1, 2, 3, 4, 5, 6, 7, 8 }\n\
         for i = 1, 6 do t = { t, t, t, t, t, t, t, t } end\nskin.extra = t",
    ));
    record(
        "node_cap",
        measure(|| {
            let e = both(nodes.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Input {
                        limit: InputLimit::Nodes,
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );

    // The decoded bytes at their cap.
    let decoded = folder(&configured(
        "local s = \"x\"\nfor i = 1, 16 do s = s .. s end\nlocal t = {}\n\
         for i = 1, 520 do t[i] = s end\nskin.extra = t",
    ));
    record(
        "decoded_cap",
        measure(|| {
            let e = both(decoded.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Input {
                        limit: InputLimit::DecodedBytes,
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );

    // A fault under nested handlers, at a call site named by a 4 MiB
    // constant: the hook's one error carries a traceback naming it, and no
    // handler fails again (each failure would add another, outside the
    // VM's limit, while the handlers recurse).
    let name = "n".repeat(4 << 20);
    let long_names = folder(&format!(
        "local t = {{}}\n\
         t[\"{name}\"] = function() while true do end end\n\
         local function deep(n)\n\
           if n == 0 then local r = t[\"{name}\"]() return r end\n\
           return xpcall(deep, function(e) return e end, n - 1)\n\
         end\n\
         deep(2)\nreturn {{ type = 0 }}"
    ));
    let p = record(
        "long_names",
        measure(|| {
            let e = header(long_names.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Lua {
                        why: LuaWhy::Limit(LuaLimit::Instructions),
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );
    assert!(p < 48 << 20, "the handlers' errors grew memory: {p}");

    // The instruction cap, refused in the header pass.
    let looping = folder("while true do end");
    record(
        "instruction_cap",
        measure(|| header(looping.path()).unwrap_err()),
    );

    // The callbacks' analysis near its caps: a module only the configured
    // pass parses, one table short of the node cap; then one past it.
    let near = format!(
        "local t = {{ {} }}
return t",
        "1,".repeat(340_000)
    );
    let analysed = folder(&configured("local big = require(\"big\")"));
    common::write(analysed.path(), "big.lua", near.as_bytes());
    record("ast_nodes", measure(|| both(analysed.path()).unwrap()));
    let past = format!(
        "local t = {{ {} }}
return t",
        "1,".repeat(360_000)
    );
    common::write(analysed.path(), "big.lua", past.as_bytes());
    record(
        "ast_node_cap",
        measure(|| {
            let e = both(analysed.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Lua {
                        why: LuaWhy::Limit(LuaLimit::AstNodes),
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );
    // 2 000 callbacks lowered through small helpers, then helpers that
    // expand past the expansion cap.
    let mut helpers = String::from(
        "local ms = require(\"main_state\")\nlocal function p0() return ms.number(110) > 0 end\n",
    );
    for k in 1..=31 {
        helpers.push_str(&format!(
            "local function p{k}() return p{0}() or p{0}() end\n",
            k - 1
        ));
    }
    let callbacks: String = (0..2_000)
        .map(|k| {
            format!(
                "table.insert(skin.destination, {{ id = \"i\", draw = function() return p{}() end, dst = {{ {{ w = 64, h = 64 }} }} }})\n",
                k % 5
            )
        })
        .collect();
    let expanded = folder(&drawing(&helpers, &callbacks));
    record("callbacks", measure(|| both(expanded.path()).unwrap()));
    let blowup = folder(&drawing(
        &helpers,
        "table.insert(skin.destination, { id = \"i\", draw = function() return p31() end, dst = { { w = 64, h = 64 } } })",
    ));
    record(
        "expansion_cap",
        measure(|| {
            let e = both(blowup.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Lua {
                        why: LuaWhy::Limit(LuaLimit::Expansion),
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );

    // 600 keys of 50 004 bytes, shared by 20 nested tables through their
    // first key `a`: the keys the conversion keeps are charged as they
    // are copied, so it stops near the decoded-bytes cap instead of
    // copying the shared keys once a table.
    let shared = folder(&configured(
        "local p = \"x\"\nwhile #p < 50000 do p = p .. p end\np = string.sub(p, 1, 50000)\n\
         local keys = {}\nfor i = 1, 600 do keys[i] = \"b\" .. p .. string.format(\"%03d\", i) end\n\
         local t = {}\nlocal top = t\n\
         for level = 1, 20 do\n\
           for i = 1, 600 do t[keys[i]] = true end\n\
           local next = {}\n\
           t.a = next\n\
           t = next\n\
         end\n\
         skin.extra = top",
    ));
    let p = record(
        "shared_keys",
        measure(|| {
            let e = both(shared.path()).unwrap_err();
            assert!(
                matches!(
                    e,
                    ImportError::Input {
                        limit: InputLimit::DecodedBytes,
                        ..
                    }
                ),
                "{e:?}"
            );
        }),
    );
    assert!(p < 160 << 20, "the shared keys were copied again: {p}");

    let worst = peaks.iter().map(|(_, p)| *p).max().unwrap_or(0);
    println!("worst peak {worst} bytes; converted table at most {MAX_CONVERTED_BYTES} bytes");
    assert!(worst <= BOUND, "{peaks:?}");
    // The process's own peak, which also holds the test harness: recorded
    // beside the counted peaks, never compared with them.
    let process = process_peak().expect("the process's peak memory");
    println!("process peak {process} bytes");
    assert!(process > 0);
}

/// The process's peak resident memory in bytes: `VmHWM` on Linux, the
/// peak working set on Windows.
fn process_peak() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    }
    #[cfg(windows)]
    {
        let query = format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id());
        let out = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &query])
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}
