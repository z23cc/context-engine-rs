use super::*;

pub struct FastembedEmbeddingBackend {
    model: EmbeddingModel,
    dimension: usize,
    cache_dir: PathBuf,
    inner: Mutex<Option<TextEmbedding>>,
}

impl FastembedEmbeddingBackend {
    pub(super) fn new(model: EmbeddingModel, cache_dir: PathBuf) -> Self {
        Self {
            dimension: embedding_dimension(&model),
            model,
            cache_dir,
            inner: Mutex::new(None),
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        let mut guard = self.inner.lock().expect("fastembed embedding lock");
        if guard.is_none() {
            let options = TextInitOptions::new(self.model.clone())
                .with_cache_dir(self.cache_dir.clone())
                .with_show_download_progress(false);
            *guard = Some(TextEmbedding::try_new(options).map_err(|err| {
                CtxError::Semantic(format!("embedding model init failed: {err}"))
            })?);
        }
        guard
            .as_mut()
            .expect("embedding initialized")
            .embed(texts, None)
            .map_err(|err| CtxError::Semantic(format!("embedding failed: {err}")))
    }
}

impl EmbeddingBackend for FastembedEmbeddingBackend {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        self.embed(texts)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError> {
        let embeddings = self.embed(&[query.to_string()])?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| CtxError::Semantic("embedding backend returned no query vector".into()))
    }
}

pub struct FastembedRerankerBackend {
    model: RerankerModel,
    cache_dir: PathBuf,
    inner: Mutex<Option<TextRerank>>,
}

impl FastembedRerankerBackend {
    pub(super) fn new(model: RerankerModel, cache_dir: PathBuf) -> Self {
        Self {
            model,
            cache_dir,
            inner: Mutex::new(None),
        }
    }
}

impl RerankerBackend for FastembedRerankerBackend {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, CtxError> {
        let mut guard = self.inner.lock().expect("fastembed reranker lock");
        if guard.is_none() {
            let options = RerankInitOptions::new(self.model.clone())
                .with_cache_dir(self.cache_dir.clone())
                .with_show_download_progress(false);
            *guard =
                Some(TextRerank::try_new(options).map_err(|err| {
                    CtxError::Semantic(format!("reranker model init failed: {err}"))
                })?);
        }
        let ranked = guard
            .as_mut()
            .expect("reranker initialized")
            .rerank(query.to_string(), documents, false, None)
            .map_err(|err| CtxError::Semantic(format!("rerank failed: {err}")))?;
        let mut scores = vec![0.0; documents.len()];
        for result in ranked {
            if result.index < scores.len() {
                scores[result.index] = result.score;
            }
        }
        Ok(scores)
    }
}

pub(super) fn parse_embedding_model(model: Option<&str>) -> Result<EmbeddingModel, CtxError> {
    match model.unwrap_or("jina-embeddings-v2-base-code") {
        "jina-embeddings-v2-base-code" | "jinaai/jina-embeddings-v2-base-code" => {
            Ok(EmbeddingModel::JinaEmbeddingsV2BaseCode)
        }
        "bge-small-en-v1.5" | "BAAI/bge-small-en-v1.5" => Ok(EmbeddingModel::BGESmallENV15),
        "bge-large-en-v1.5" | "BAAI/bge-large-en-v1.5" => Ok(EmbeddingModel::BGELargeENV15),
        "gte-large-en-v1.5" | "Alibaba-NLP/gte-large-en-v1.5" => Ok(EmbeddingModel::GTELargeENV15),
        "mxbai-embed-large-v1" | "mixedbread-ai/mxbai-embed-large-v1" => {
            Ok(EmbeddingModel::MxbaiEmbedLargeV1)
        }
        "modernbert-embed-large" | "lightonai/modernbert-embed-large" => {
            Ok(EmbeddingModel::ModernBertEmbedLarge)
        }
        "nomic-embed-text-v1.5" | "nomic-ai/nomic-embed-text-v1.5" => {
            Ok(EmbeddingModel::NomicEmbedTextV15)
        }
        "snowflake-arctic-embed-l" | "snowflake/snowflake-arctic-embed-l" => {
            Ok(EmbeddingModel::SnowflakeArcticEmbedL)
        }
        "embeddinggemma-300m" | "onnx-community/embeddinggemma-300m-ONNX" => {
            Ok(EmbeddingModel::EmbeddingGemma300M)
        }
        other => Err(CtxError::Semantic(format!(
            "unsupported embedding model for semantic_search: {other}"
        ))),
    }
}

pub(super) fn parse_reranker_model(model: Option<&str>) -> Result<RerankerModel, CtxError> {
    match model.unwrap_or("bge-reranker-base") {
        "bge-reranker-base" | "BAAI/bge-reranker-base" => Ok(RerankerModel::BGERerankerBase),
        "bge-reranker-v2-m3" | "BAAI/bge-reranker-v2-m3" => Ok(RerankerModel::BGERerankerV2M3),
        "jina-reranker-v1-turbo-en" | "jinaai/jina-reranker-v1-turbo-en" => {
            Ok(RerankerModel::JINARerankerV1TurboEn)
        }
        "jina-reranker-v2-base-multilingual" | "jinaai/jina-reranker-v2-base-multilingual" => {
            Ok(RerankerModel::JINARerankerV2BaseMultiligual)
        }
        other => Err(CtxError::Semantic(format!(
            "unsupported reranker model for semantic_search: {other}"
        ))),
    }
}

pub(super) fn embedding_dimension(model: &EmbeddingModel) -> usize {
    match model {
        EmbeddingModel::JinaEmbeddingsV2BaseCode => 768,
        EmbeddingModel::BGESmallENV15 => 384,
        EmbeddingModel::BGELargeENV15
        | EmbeddingModel::GTELargeENV15
        | EmbeddingModel::MxbaiEmbedLargeV1
        | EmbeddingModel::ModernBertEmbedLarge
        | EmbeddingModel::SnowflakeArcticEmbedL => 1024,
        EmbeddingModel::NomicEmbedTextV15 | EmbeddingModel::EmbeddingGemma300M => 768,
        _ => 768,
    }
}

pub(super) fn semantic_model_cache_dir(configured: Option<&Path>) -> PathBuf {
    // Machine-level by default so the embedding model downloads once per machine
    // and is shared across every workspace (not re-downloaded per directory).
    configured.map_or_else(
        || super::persistence::semantic_cache_root().join("models"),
        Path::to_path_buf,
    )
}

#[derive(Default)]
pub struct MockEmbeddingBackend {
    dimension: usize,
}

impl MockEmbeddingBackend {
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        let dimension = self.dimension();
        let mut vector = vec![0.0; dimension];
        for token in tokenize_text(text, false) {
            let idx = stable_bucket(&token, dimension);
            vector[idx] += 1.0;
        }
        normalize(&mut vector);
        vector
    }
}

impl EmbeddingBackend for MockEmbeddingBackend {
    fn dimension(&self) -> usize {
        self.dimension.max(32)
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        Ok(texts.iter().map(|text| self.embed_text(text)).collect())
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError> {
        Ok(self.embed_text(query))
    }
}

pub struct MockRerankerBackend;

impl RerankerBackend for MockRerankerBackend {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, CtxError> {
        let query_terms: HashSet<String> = tokenize_text(query, false).into_iter().collect();
        Ok(documents
            .iter()
            .map(|doc| {
                let doc_terms: HashSet<String> = tokenize_text(doc, false).into_iter().collect();
                query_terms.intersection(&doc_terms).count() as f32
            })
            .collect())
    }
}
