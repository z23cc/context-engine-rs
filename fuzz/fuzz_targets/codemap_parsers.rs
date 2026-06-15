#![no_main]

use ctx_core::fuzzing::codemap_symbols_for_path;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    for rel_path in ["fuzz.rs", "fuzz.py", "fuzz.js", "fuzz.ts", "fuzz.tsx"] {
        let _ = codemap_symbols_for_path(&source, rel_path);
    }
});
