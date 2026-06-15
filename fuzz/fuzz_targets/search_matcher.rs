#![no_main]

use ctx_core::fuzzing::search_match_content;
use libfuzzer_sys::fuzz_target;

fn split_input(data: &[u8]) -> (&[u8], &[u8], bool, bool) {
    if data.is_empty() {
        return (&[], &[], false, false);
    }
    let flags = data[0];
    let rest = &data[1..];
    let split = if rest.is_empty() {
        0
    } else {
        usize::from(flags) % (rest.len() + 1)
    };
    let (pattern, content) = rest.split_at(split);
    (content, pattern, flags & 0x01 != 0, flags & 0x02 != 0)
}

fuzz_target!(|data: &[u8]| {
    let (content_bytes, pattern_bytes, regex, whole_word) = split_input(data);
    let content = String::from_utf8_lossy(content_bytes);
    let pattern = String::from_utf8_lossy(pattern_bytes);
    let _ = search_match_content(&content, &pattern, regex, whole_word);
});
