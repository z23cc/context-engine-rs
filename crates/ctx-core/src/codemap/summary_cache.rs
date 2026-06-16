use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, OnceLock},
};

use super::summarize::{SummaryOptions, SummaryResult, SummarySegment};

const SUMMARY_CACHE_CAP: usize = 128;
const SUMMARY_CACHE_MAX_RETAINED_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ContentFingerprint {
    hash: u128,
    len: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SummaryCacheKey {
    path: String,
    content_hash: ContentFingerprint,
    options: SummaryOptions,
}

#[derive(Clone)]
enum SummaryCacheValue {
    Elided(SummaryResult),
    FullContent {
        language: Option<String>,
        parsed: bool,
        total_lines: usize,
    },
}

struct SummaryCacheEntry {
    value: SummaryCacheValue,
    retained_bytes: usize,
}

#[derive(Default)]
struct SummaryCache {
    entries: HashMap<SummaryCacheKey, SummaryCacheEntry>,
    order: VecDeque<SummaryCacheKey>,
    retained_bytes: usize,
}

pub(super) fn get_or_insert_with(
    path: &str,
    source: &str,
    options: SummaryOptions,
    compute: impl FnOnce() -> SummaryResult,
) -> SummaryResult {
    let key = SummaryCacheKey {
        path: path.to_string(),
        content_hash: content_hash(source),
        options,
    };
    if let Some(result) = cached_result(&key, source) {
        return result;
    }

    let result = compute();
    store_result(key, &result);
    result
}

fn cached_result(key: &SummaryCacheKey, source: &str) -> Option<SummaryResult> {
    let mut cache = cache().lock().expect("summary cache lock");
    let value = cache.entries.get(key).map(|entry| entry.value.clone())?;
    touch_key(&mut cache.order, key.clone());
    Some(value.to_result(source))
}

fn store_result(key: SummaryCacheKey, result: &SummaryResult) {
    let value = SummaryCacheValue::from(result);
    let retained_bytes = value.retained_bytes();
    if retained_bytes > SUMMARY_CACHE_MAX_RETAINED_BYTES {
        return;
    }

    let mut cache = cache().lock().expect("summary cache lock");
    if let Some(previous) = cache.entries.insert(
        key.clone(),
        SummaryCacheEntry {
            value,
            retained_bytes,
        },
    ) {
        cache.retained_bytes = cache.retained_bytes.saturating_sub(previous.retained_bytes);
    }
    cache.retained_bytes += retained_bytes;
    touch_key(&mut cache.order, key);
    evict_over_budget(&mut cache);
}

fn evict_over_budget(cache: &mut SummaryCache) {
    while cache.entries.len() > SUMMARY_CACHE_CAP
        || cache.retained_bytes > SUMMARY_CACHE_MAX_RETAINED_BYTES
    {
        let Some(oldest) = cache.order.pop_front() else {
            break;
        };
        if let Some(removed) = cache.entries.remove(&oldest) {
            cache.retained_bytes = cache.retained_bytes.saturating_sub(removed.retained_bytes);
        }
    }
}

fn touch_key(order: &mut VecDeque<SummaryCacheKey>, key: SummaryCacheKey) {
    order.retain(|existing| existing != &key);
    order.push_back(key);
}

fn cache() -> &'static Mutex<SummaryCache> {
    static CACHE: OnceLock<Mutex<SummaryCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SummaryCache::default()))
}

fn content_hash(source: &str) -> ContentFingerprint {
    let mut low = 0xcbf2_9ce4_8422_2325u64;
    let mut high = 0x9e37_79b9_7f4a_7c15u64;
    for byte in source.as_bytes() {
        low ^= u64::from(*byte);
        low = low.wrapping_mul(0x0000_0100_0000_01b3);
        high ^= u64::from(byte.rotate_left(1));
        high = high.wrapping_mul(0x0000_0001_0000_01b3);
    }
    ContentFingerprint {
        hash: (u128::from(high) << 64) | u128::from(low),
        len: source.len(),
    }
}

impl SummaryCacheValue {
    fn to_result(&self, source: &str) -> SummaryResult {
        match self {
            Self::Elided(result) => result.clone(),
            Self::FullContent {
                language,
                parsed,
                total_lines,
            } => full_content_result(source, language.clone(), *parsed, *total_lines),
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Elided(result) => result
                .segments
                .iter()
                .filter_map(|segment| segment.text.as_ref())
                .map(String::len)
                .sum(),
            Self::FullContent { language, .. } => language.as_ref().map_or(0, String::len),
        }
    }
}

impl From<&SummaryResult> for SummaryCacheValue {
    fn from(result: &SummaryResult) -> Self {
        if result.elided {
            Self::Elided(result.clone())
        } else {
            Self::FullContent {
                language: result.language.clone(),
                parsed: result.parsed,
                total_lines: result.total_lines,
            }
        }
    }
}

fn full_content_result(
    source: &str,
    language: Option<String>,
    parsed: bool,
    total_lines: usize,
) -> SummaryResult {
    let segments = if source.is_empty() {
        Vec::new()
    } else {
        vec![SummarySegment {
            kind: "kept".to_string(),
            start_line: 1,
            end_line: total_lines,
            text: Some(source.to_string()),
        }]
    };
    SummaryResult {
        language,
        parsed,
        elided: false,
        total_lines,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn repeated_identical_summary_reads_hit_cache() {
        let calls = AtomicUsize::new(0);
        let path = "cache-hit-summary-test.rs";
        let source = "fn demo() {\n    one();\n    two();\n}\n";
        let first = get_or_insert_with(path, source, SummaryOptions::default(), || {
            calls.fetch_add(1, Ordering::Relaxed);
            elided_result("first")
        });
        let second = get_or_insert_with(path, source, SummaryOptions::default(), || {
            calls.fetch_add(1, Ordering::Relaxed);
            elided_result("second")
        });

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(first, second);
        assert_eq!(second.segments[0].text.as_deref(), Some("first\n"));
    }

    #[test]
    fn summary_cache_key_includes_content_hash_and_options() {
        let calls = AtomicUsize::new(0);
        let path = "cache-key-summary-test.rs";
        let source_a = "fn demo() {\n    one();\n}\n";
        let source_b = "fn demo() {\n    two();\n}\n";
        let unfolded = SummaryOptions {
            unfold_until_lines: 2,
            unfold_limit_lines: 8,
            ..SummaryOptions::default()
        };

        cache_probe(path, source_a, SummaryOptions::default(), &calls);
        cache_probe(path, source_b, SummaryOptions::default(), &calls);
        cache_probe(path, source_a, unfolded, &calls);
        cache_probe(path, source_a, SummaryOptions::default(), &calls);
        cache_probe(path, source_b, SummaryOptions::default(), &calls);
        cache_probe(path, source_a, unfolded, &calls);

        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    fn cache_probe(path: &str, source: &str, options: SummaryOptions, calls: &AtomicUsize) {
        get_or_insert_with(path, source, options, || {
            calls.fetch_add(1, Ordering::Relaxed);
            elided_result("cached")
        });
    }

    fn elided_result(label: &str) -> SummaryResult {
        SummaryResult {
            language: Some("rust".to_string()),
            parsed: true,
            elided: true,
            total_lines: 3,
            segments: vec![
                SummarySegment {
                    kind: "kept".to_string(),
                    start_line: 1,
                    end_line: 1,
                    text: Some(format!("{label}\n")),
                },
                SummarySegment {
                    kind: "elided".to_string(),
                    start_line: 2,
                    end_line: 3,
                    text: None,
                },
            ],
        }
    }
}
