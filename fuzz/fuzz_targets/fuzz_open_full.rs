#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    embedmind_core::fuzz::fuzz_open_full(data);
});
