#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| nautilus_txflow::fuzz::ws_parse(data));
