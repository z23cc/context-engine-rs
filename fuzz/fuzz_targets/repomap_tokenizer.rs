#![no_main]

use ctx_core::fuzzing::repomap_identifier_counts;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let source = String::from_utf8_lossy(data);
    for language in ["rust", "python", "javascript", "unknown"] {
        let _ = repomap_identifier_counts(&source, language);
    }
});
