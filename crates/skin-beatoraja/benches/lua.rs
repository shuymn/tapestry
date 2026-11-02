//! CPU benchmarks of importing a Lua skin, each stage apart
//! (docs/stage3.md §3, Lua evidence), on a generated skin: a header of
//! 16 options, a configured pass that builds 1,000 images and their
//! destinations through a local module, with a decorative draw function
//! on every tenth.
//!
//! - `skin_beatoraja_lua_header_pass`: [`read_lua_header`], the header
//!   pass's VM, its conversion and the header.
//! - `skin_beatoraja_lua_configured_execution`, `_conversion`,
//!   `_teardown` and `_common_import`: the stages of
//!   [`LuaHeader::timed`], each timed inside the configured pass.
//! - `skin_beatoraja_lua_refusal_instructions`: a header pass that loops
//!   until the instruction cap refuses it.

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use common::{png, write};
use criterion::{Criterion, criterion_group, criterion_main};
use skin_beatoraja::lua::Timings;
use skin_beatoraja::{Cancel, Config, read_lua_header};

fn typical(root: &Path) {
    png(root, "a.png", 64, 64);
    let options: String = (0..16)
        .map(|i| {
            format!(
                "{{ name = \"o{i}\", item = {{ {{ name = \"a\", op = {} }}, {{ name = \"b\", op = {} }} }} }},",
                900 + 2 * i,
                901 + 2 * i
            )
        })
        .collect();
    let entry = format!(
        "local skin = {{ type = 0, name = \"bench\", w = 1920, h = 1080, property = {{ {options} }} }}\n\
         if not skin_config then return skin end\n\
         local parts = require(\"parts\")\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image, skin.destination = parts.build(1000)\n\
         return skin\n"
    );
    write(root, "s.luaskin", entry.as_bytes());
    write(
        root,
        "parts.lua",
        b"local m = {}\n\
          local function shown() return true end\n\
          function m.build(n)\n\
            local images, dsts = {}, {}\n\
            for i = 1, n do\n\
              local id = \"i\" .. i\n\
              images[i] = { id = id, src = \"a\", x = i % 32, y = 0, w = 32, h = 32 }\n\
              local d = { id = id, dst = { { x = i % 1000, y = 10, w = 32, h = 32 } } }\n\
              if i % 10 == 0 then d.draw = shown end\n\
              dsts[i] = d\n\
            end\n\
            return images, dsts\n\
          end\n\
          return m\n",
    );
}

fn stage(c: &mut Criterion, name: &str, root: &Path, pick: fn(&Timings) -> Duration) {
    let cancel = Cancel::new();
    let h = read_lua_header(root, "s.luaskin", &cancel).expect("the header pass runs");
    let config = Config::defaults(h.header());
    c.bench_function(name, |b| {
        b.iter_custom(|iters| {
            (0..iters)
                .map(|_| pick(&h.timed(&config, &cancel).expect("the skin imports")))
                .sum()
        });
    });
}

fn benches(c: &mut Criterion) {
    let d = tempfile::tempdir().expect("a temporary folder");
    typical(d.path());
    c.bench_function("skin_beatoraja_lua_header_pass", |b| {
        b.iter(|| {
            black_box(read_lua_header(d.path(), "s.luaskin", &Cancel::new()).expect("header"))
        });
    });
    stage(
        c,
        "skin_beatoraja_lua_configured_execution",
        d.path(),
        |t| t.execution,
    );
    stage(c, "skin_beatoraja_lua_conversion", d.path(), |t| {
        t.conversion
    });
    stage(c, "skin_beatoraja_lua_teardown", d.path(), |t| t.teardown);
    stage(c, "skin_beatoraja_lua_common_import", d.path(), |t| {
        t.import
    });

    let looping = tempfile::tempdir().expect("a temporary folder");
    write(looping.path(), "s.luaskin", b"while true do end");
    c.bench_function("skin_beatoraja_lua_refusal_instructions", |b| {
        b.iter(|| black_box(read_lua_header(looping.path(), "s.luaskin", &Cancel::new()).is_err()));
    });
}

criterion_group! {
    name = lua;
    config = Criterion::default().sample_size(20);
    targets = benches
}
criterion_main!(lua);
