#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = fuzz::bms_load(data);
});
