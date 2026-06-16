//! Semantic chunk construction.

use crate::{
    cancel::CancelToken,
    codemap::{CodeSymbol, block_span},
    models::{CatalogEntry, CtxError, Diagnostic},
    port::CatalogProvider,
    ranking::is_binary,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

const FALLBACK_WINDOW_LINES: usize = 80;
const FALLBACK_OVERLAP_LINES: usize = 16;
const SYMBOL_CONTEXT_LINES: usize = 2;
pub(crate) const CHUNKER_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SemanticChunk {
    pub(crate) id: String,
    pub(crate) root_id: String,
    pub(crate) path: String,
    pub(crate) display_path: String,
    pub(crate) line_start: usize,
    pub(crate) line_end: usize,
    pub(crate) symbol: Option<String>,
    pub(crate) signature: Option<String>,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkManifest {
    pub(crate) fingerprint: String,
    pub(crate) file_chunks: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkBuild {
    pub(crate) chunks: Vec<SemanticChunk>,
    pub(crate) manifest: ChunkManifest,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
pub(crate) fn build_chunks<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &crate::snapshot::CatalogSnapshot,
    cancel: &CancelToken,
) -> Result<ChunkBuild, CtxError> {
    let entries: Vec<&CatalogEntry> = snapshot.entries.iter().collect();
    build_chunks_for_entries(provider, &entries, snapshot.generation, cancel)
}

pub(crate) fn build_chunks_for_entries<P: CatalogProvider + Sync>(
    provider: &P,
    entries: &[&CatalogEntry],
    generation: u64,
    cancel: &CancelToken,
) -> Result<ChunkBuild, CtxError> {
    let mut chunks = Vec::new();
    let mut diagnostics = Vec::new();
    let mut file_chunks = BTreeMap::new();
    let mut manifest_hasher = Sha256::new();
    manifest_hasher.update(generation.to_le_bytes());

    for entry in entries {
        cancel.check_cancelled()?;
        let bytes = provider.read_bytes(Path::new(&entry.abs_path))?;
        manifest_hasher.update(entry.rel_path.as_bytes());
        manifest_hasher.update(entry.size.to_le_bytes());
        manifest_hasher.update(Sha256::digest(&bytes));

        if is_binary(&bytes) {
            diagnostics.push(Diagnostic {
                path: Some(entry.rel_path.clone().into()),
                message: format!(
                    "skipped binary file during semantic indexing: {}",
                    entry.rel_path
                ),
            });
            continue;
        }

        let source = String::from_utf8_lossy(&bytes);
        let mut ids = chunks_for_entry(provider, entry, &source, &mut chunks, cancel)?;
        if ids.is_empty() {
            ids = fallback_chunks_for_entry(provider, entry, &source, &mut chunks);
        }
        file_chunks.insert(entry.rel_path.clone(), ids);
    }

    Ok(ChunkBuild {
        chunks,
        manifest: ChunkManifest {
            fingerprint: format!("{:x}", manifest_hasher.finalize()),
            file_chunks,
        },
        diagnostics,
    })
}

fn chunks_for_entry<P: CatalogProvider + Sync>(
    provider: &P,
    entry: &CatalogEntry,
    source: &str,
    chunks: &mut Vec<SemanticChunk>,
    cancel: &CancelToken,
) -> Result<Vec<String>, CtxError> {
    let parsed = match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
        Ok(Some(parsed)) => parsed,
        Ok(None) | Err(_) => return Ok(Vec::new()),
    };
    let lines: Vec<&str> = source.lines().collect();
    let mut ids = Vec::new();
    for symbol in &parsed.symbols {
        cancel.check_cancelled()?;
        let (mut start, mut end) = match block_span(&entry.rel_path, source, symbol.line) {
            Some(span) => span,
            None => {
                let span = fixed_span(symbol.line, lines.len());
                if span.0 > lines.len() {
                    continue;
                }
                span
            }
        };
        start = start.saturating_sub(SYMBOL_CONTEXT_LINES).max(1);
        end = (end + SYMBOL_CONTEXT_LINES).min(lines.len().max(1));
        let text = slice_lines(&lines, start, end);
        if text.trim().is_empty() {
            continue;
        }
        let chunk = make_chunk(provider, entry, start, end, Some(symbol), text);
        ids.push(chunk.id.clone());
        chunks.push(chunk);
    }
    Ok(ids)
}

fn fallback_chunks_for_entry<P: CatalogProvider + Sync>(
    provider: &P,
    entry: &CatalogEntry,
    source: &str,
    chunks: &mut Vec<SemanticChunk>,
) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let step = FALLBACK_WINDOW_LINES
        .saturating_sub(FALLBACK_OVERLAP_LINES)
        .max(1);
    let mut ids = Vec::new();
    let mut start = 1usize;
    while start <= lines.len() {
        let end = (start + FALLBACK_WINDOW_LINES - 1).min(lines.len());
        let text = slice_lines(&lines, start, end);
        if !text.trim().is_empty() {
            let chunk = make_chunk(provider, entry, start, end, None, text);
            ids.push(chunk.id.clone());
            chunks.push(chunk);
        }
        if end == lines.len() {
            break;
        }
        start += step;
    }
    ids
}

fn fixed_span(line: usize, total_lines: usize) -> (usize, usize) {
    let half = FALLBACK_WINDOW_LINES / 2;
    let start = line.saturating_sub(half).max(1);
    let end = (start + FALLBACK_WINDOW_LINES - 1).min(total_lines.max(1));
    (start, end)
}

fn make_chunk<P: CatalogProvider + Sync>(
    provider: &P,
    entry: &CatalogEntry,
    line_start: usize,
    line_end: usize,
    symbol: Option<&CodeSymbol>,
    text: String,
) -> SemanticChunk {
    let mut hasher = Sha256::new();
    hasher.update(entry.rel_path.as_bytes());
    hasher.update(line_start.to_le_bytes());
    hasher.update(line_end.to_le_bytes());
    hasher.update(text.as_bytes());
    SemanticChunk {
        id: format!("{:x}", hasher.finalize()),
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
        display_path: provider.display_path(&entry.abs_path),
        line_start,
        line_end,
        symbol: symbol.map(|symbol| symbol.name.clone()),
        signature: symbol.and_then(|symbol| symbol.signature.clone()),
        text,
    }
}

fn slice_lines(lines: &[&str], start: usize, end: usize) -> String {
    lines[start.saturating_sub(1)..end.min(lines.len())].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostFile, MemoryCatalogProvider};

    #[test]
    fn symbol_chunks_include_metadata_and_file_map() {
        let provider = MemoryCatalogProvider::new(vec![HostFile::new(
            "lib.rs",
            b"pub fn alpha() {\n    println!(\"alpha\");\n}\n\npub fn beta() {}\n".to_vec(),
        )])
        .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        let build = build_chunks(&provider, &snapshot, &CancelToken::never()).expect("chunks");
        assert!(
            build
                .chunks
                .iter()
                .any(|chunk| chunk.symbol.as_deref() == Some("alpha"))
        );
        assert!(build.manifest.file_chunks.contains_key("lib.rs"));
    }

    #[test]
    fn unsupported_files_fall_back_to_windows() {
        let provider = MemoryCatalogProvider::new(vec![HostFile::new(
            "notes.txt",
            b"one\ntwo\nthree\n".to_vec(),
        )])
        .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        let build = build_chunks(&provider, &snapshot, &CancelToken::never()).expect("chunks");
        assert_eq!(build.chunks.len(), 1);
        assert_eq!(build.chunks[0].line_start, 1);
        assert_eq!(build.chunks[0].line_end, 3);
    }
}
