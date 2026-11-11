//! A play skin's hi-speed readouts under the speed policy's fix
//! (design.md "beatoraja Lua Skins", Numbers; docs/stage3.md "The
//! presentation program's numerics past ±2^53"): self-made Lua of the
//! shape a real skin uses, a helper that rescales the duration by the
//! current tempo over the fix's reference tempo (the MAX or MAIN BPM)
//! while the hidden cover and a tempo change are on, and seven values
//! over it: the hi-speed relative and absolute (a skin option picks one
//! pair, each a value and its two decimals), the lane cover's green
//! number and duration (two destinations each) and the adjusted rate. The
//! product of the duration and the tempo can pass 2^53 over the reads'
//! ranges, where the source computes it as a double; each value is the
//! source's, hand-computed with its interpreter's rules (int products
//! rounded once into doubles, `/` and `math.floor` of doubles, `toint`'s
//! low 32 bits).

mod common;

use bms_model::Tempo;
use common::play::Drawn;
use skin_beatoraja::lua::Disposition;
use skin_beatoraja::{Cancel, Config, LuaImported, read_lua_header};
use skin_model::{BpmRange, CoverSettings, DurationRatio, FrameReadouts};
use tempfile::TempDir;

/// The skin: option 905 shows the relative pair, 906 the absolute one.
/// Its value definitions, in order: the hi-speed's value and decimals
/// (relative `h · 0.6` and `⌊h · 60 + 0.5⌋`, or absolute `h` and
/// `⌊h · 100 + 0.5⌋`), the green number `h · 0.6`, the duration `h` and
/// the adjusted rate.
const SKIN: &str = r#"local skin = { type = 0, name = "t", w = 1920, h = 1080,
  property = { { name = "type", item = { { name = "relative", op = 905 }, { name = "absolute", op = 906 } } } } }
if not skin_config then return skin end
local ms = require("main_state")
local relative = skin_config.option["type"] == 905

-- The duration rescaled by the fix's reference tempo.
local function speed()
  local fix = ms.event_index(55)
  if ms.option(177) and ms.option(273) then
    if fix == 2 then
      local top = ms.number(90)
      if top == 0 then return ms.number(312) end
      return ms.number(312) * ms.number(160) / top
    elseif fix == 3 then
      local main = ms.number(92)
      if main == 0 then return ms.number(312) end
      return ms.number(312) * ms.number(160) / main
    end
  end
  return ms.number(312)
end

skin.source = { { id = "a", path = "a.png" } }
skin.value = {}
if relative then
  table.insert(skin.value, { id = "hs", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,
    value = function() return speed() * 0.6 end })
  table.insert(skin.value, { id = "hs-dot", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,
    value = function() return math.floor(speed() * 60 + 0.5) end })
else
  table.insert(skin.value, { id = "hs", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,
    value = function() return speed() end })
  table.insert(skin.value, { id = "hs-dot", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,
    value = function() return math.floor(speed() * 100 + 0.5) end })
end
table.insert(skin.value, { id = "green", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,
  value = function() return speed() * 0.6 end })
table.insert(skin.value, { id = "duration", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 4,
  value = function() return speed() end })
table.insert(skin.value, { id = "rate", src = "a", x = 0, y = 0, w = 60, h = 6, divx = 10, digit = 2,
  value = function()
    local fix = ms.event_index(55)
    local on = ms.option(400) or ms.option(273)
    if fix == 2 and on then
      return math.floor(ms.number(160) / ms.number(90) * 100 + 0.5)
    elseif fix == 3 and on then
      local r = math.floor(ms.number(160) / ms.number(92) * 100 + 0.5)
      if r > 100 then return 100 end
      return r
    end
  end })

skin.destination = {
  { id = "hs", dst = { { x = 0, y = 100, w = 6, h = 6 } } },
  { id = "hs-dot", dst = { { x = 30, y = 100, w = 6, h = 6 } } },
}
for k, offset in ipairs({ 4, 3 }) do
  table.insert(skin.destination, { id = "green", op = { 270 }, offset = offset,
    dst = { { x = 0, y = 200 + 100 * k, w = 6, h = 6 } } })
  table.insert(skin.destination, { id = "duration", op = { 270 }, offset = offset,
    dst = { { x = 30, y = 200 + 100 * k, w = 6, h = 6 } } })
end
table.insert(skin.destination, { id = "rate", dst = { { x = 0, y = 600, w = 6, h = 6 } },
  draw = function()
    local fix = ms.event_index(55)
    return (ms.option(400) or ms.option(273)) and ms.option(177) and (fix == 2 or fix == 3)
  end })
return skin
"#;

/// The skin's folder.
fn folder() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    common::write(d.path(), "s.luaskin", SKIN.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    d
}

/// The skin imported with the hi-speed type `op` (905 relative, 906
/// absolute).
fn import(d: &TempDir, op: i32) -> LuaImported {
    let cancel = Cancel::new();
    let h = read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = Config {
        options: vec![Some(op)],
        ..Config::default()
    };
    common::lua_play(&h, &config, &cancel).unwrap()
}

/// One frame's reads: the fix (55), the hidden cover (273), the tempo
/// change (177), the duration (312), the current, maximum and MAIN BPM
/// (160, 90, 92).
#[derive(Clone, Copy, Debug)]
struct Reads {
    fix: u8,
    hidden: bool,
    change: bool,
    duration: u128,
    current: u32,
    max: u32,
    main: u32,
}

/// Each output's value and availability, `lua.<k>` and `lua.<k>.ok`, of
/// the frame `reads` give, in destination order: the hi-speed's value and
/// decimals, the green number and duration at the cover's and the lift's
/// offsets, and the adjusted rate. Seven: every destination is retained.
fn outputs(i: &LuaImported, r: Reads) -> Vec<[i32; 2]> {
    let range = BpmRange {
        min_x100: 0,
        max_x100: r.max * 100,
    };
    let main = Tempo::from_milli(i64::from(r.main) * 1_000);
    let mut p = Drawn::over_tempos(
        i.imported.skin.clone(),
        &common::play::chart(8),
        Some(range),
        Some(r.change),
        main,
    );
    let covers = CoverSettings {
        cover: 0,
        lift: 0,
        hidden: 0,
        cover_enabled: false,
        lift_enabled: false,
        hidden_enabled: r.hidden,
        adjusting: false,
    };
    p.readouts = Some(FrameReadouts {
        bpm_current_x100: Some(r.current * 100),
        duration: Some(DurationRatio {
            numerator: r.duration,
            denominator: 1,
        }),
        covers,
        fix_hispeed: r.fix,
        ..FrameReadouts::UNAVAILABLE
    });
    p.frame(1_000, &[]);
    let f = p.play.state().fields;
    let base = skin_model::field::COUNT;
    assert_eq!(f.len(), base + 2 * 7, "{r:?}");
    (0..7)
        .map(|k| [f[base + 2 * k], f[base + 2 * k + 1]])
        .collect()
}

/// A case: its name, its reads, and the source's `toint` of the relative
/// value and decimals, the absolute value and decimals, the green number,
/// the duration and the adjusted rate.
type Case = (&'static str, Reads, [i32; 7]);

const fn reads(fix: u8, on: bool, duration: u128, current: u32, max: u32, main: u32) -> Reads {
    Reads {
        fix,
        hidden: on,
        change: on,
        duration,
        current,
        max,
        main,
    }
}

/// The cases, each value hand-computed with the source's rules.
const CASES: [Case; 13] = [
    // The hidden cover and the tempo change off: the duration whatever
    // the fix, `625 · 0.6` 375 (shown `375.00`); the rate nil, 0.
    (
        "default",
        reads(2, false, 625, 120, 200, 180),
        [375, 37_500, 625, 62_500, 375, 625, 0],
    ),
    // MAX: 500 · 150 / 200 = 375; the rate ⌊75.5⌋.
    (
        "fix MAX",
        reads(2, true, 500, 150, 200, 180),
        [225, 22_500, 375, 37_500, 225, 375, 75],
    ),
    // MAX with the duration the source's own fix gives at 150 (667):
    // 500.25, shown `300.15` and `500.25`.
    (
        "fix MAX, coupled",
        reads(2, true, 667, 150, 200, 180),
        [300, 30_015, 500, 50_025, 300, 500, 75],
    ),
    // MAIN: 75 000 / 180 = 416.666…, its relative 250 exactly; the rate
    // ⌊83.83…⌋.
    (
        "fix MAIN",
        reads(3, true, 500, 150, 200, 180),
        [250, 25_000, 416, 41_667, 250, 416, 83],
    ),
    // MAIN's rate clamped at 100 (⌊111.6…⌋).
    (
        "fix MAIN, rate clamped",
        reads(3, true, 500, 200, 250, 180),
        [333, 33_333, 555, 55_556, 333, 555, 100],
    ),
    // 120 / 150 · 100 is 80 exactly.
    (
        "fix MAIN, 150 and 120",
        reads(3, true, 500, 120, 250, 150),
        [240, 24_000, 400, 40_000, 240, 400, 80],
    ),
    (
        "fix MAX, 200 and 120",
        reads(2, true, 500, 120, 200, 180),
        [180, 18_000, 300, 30_000, 180, 300, 60],
    ),
    (
        "fix MAX, 150 and 120",
        reads(2, true, 500, 120, 150, 180),
        [240, 24_000, 400, 40_000, 240, 400, 80],
    ),
    // A maximum of 0: the duration itself; the rate ⌊+inf⌋, `toint` −1.
    (
        "fix MAX, maximum 0",
        reads(2, true, 312, 150, 0, 180),
        [187, 18_720, 312, 31_200, 187, 312, -1],
    ),
    // 0 / 0 is NaN, `toint` 0.
    (
        "fix MAX, 0 over 0",
        reads(2, true, 312, 0, 0, 180),
        [187, 18_720, 312, 31_200, 187, 312, 0],
    ),
    // The product past 2^53, a multiple of 8, exact: 2 147 483 646; the
    // decimals' floors past `i32`, their low 32 bits.
    (
        "past 2^53, exact",
        reads(2, true, 2_147_483_646, 21_474_836, 21_474_836, 180),
        [
            1_288_490_187,
            -120,
            2_147_483_646,
            -200,
            1_288_490_187,
            2_147_483_646,
            100,
        ],
    ),
    // 46 116 859 089 057 220 rounds to …216, so 2 147 483 644.9999998,
    // where exact integers give 2 147 483 645.
    (
        "past 2^53, rounded",
        reads(2, true, 2_147_483_645, 21_474_836, 21_474_836, 180),
        [
            1_288_490_186,
            -180,
            2_147_483_644,
            -300,
            1_288_490_186,
            2_147_483_644,
            100,
        ],
    ),
    // MAIN 1: the rounded product itself, its low 32 bits −64 424 512
    // (exactly, −64 424 508); the absolute decimals `i32::MIN`, which no
    // number draws.
    (
        "past 2^53, MAIN 1",
        reads(3, true, 2_147_483_645, 21_474_836, 21_474_836, 1),
        [
            -897_648_168,
            429_496_320,
            -64_424_512,
            i32::MIN,
            -897_648_168,
            -64_424_512,
            100,
        ],
    ),
];

/// Under either hi-speed type every value compiles into the program, its
/// destinations retained, and gives the source's value: the relative or
/// the absolute pair, then the green number and duration twice and the
/// adjusted rate, each available but `i32::MIN`, which the source draws
/// as nothing.
#[test]
fn the_hispeed_values_are_the_source_s() {
    let d = folder();
    for (op, pair) in [(905, 0), (906, 2)] {
        let i = import(&d, op);
        // The rate's draw lowers; the five values compile.
        let mut want = vec![Disposition::Lowered];
        want.extend([Disposition::Compiled; 5]);
        assert_eq!(i.callbacks, want, "{op}");
        for (name, r, v) in CASES {
            let want: Vec<[i32; 2]> = [v[pair], v[pair + 1], v[4], v[5], v[4], v[5], v[6]]
                .into_iter()
                // `i32::MIN` is no value a number draws: unavailable.
                .map(|x| [x, i32::from(x != i32::MIN)])
                .collect();
            assert_eq!(outputs(&i, r), want, "{op}: {name}");
        }
    }
}
