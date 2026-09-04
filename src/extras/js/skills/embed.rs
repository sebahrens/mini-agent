//! Embedding generation and caching for learned-JS and Agent Skills retrieval.
//!
//! This module provides:
//! - One reusable, model-versioned embedding service initialized once per session
//! - Deterministic document builder from skill metadata
//! - Batched document embedding for admission/reindex
//! - Single query embedding with bounded caching
//! - Bounded blocking worker for CPU-bound inference
//!
//! The underlying inference backend is pluggable via the `EmbeddingBackend` trait.
//! A deterministic hash-based backend is used by default and in all tests.
//! The real `fastembed` backend is opt-in and only compiles with the `skills-embed` feature.

use sha2::Digest;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// Trait for embedding backends.
///
/// Implementations are responsible for:
/// - Initializing the model once
/// - Producing deterministic embeddings
/// - Validating output (non-empty, all-finite, correct dimension)
/// - Returning exact model metadata
pub trait EmbeddingBackend: Send + Sync {
    /// Embed a batch of documents.
    ///
    /// Returns one vector per document, all with the same dimension.
    /// All vectors must be normalized (unit norm).
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    /// Embed a single query.
    ///
    /// Returns a normalized vector.
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Model identifier (e.g., "BAAI/bge-small-en-v1.5").
    fn model_id(&self) -> &str;

    /// Exact model revision or version (e.g., a commit hash or release tag).
    /// Used as part of the cache key to detect model changes.
    fn model_revision(&self) -> &str;

    /// Number of dimensions in output vectors.
    fn dimensions(&self) -> usize;

    /// Whether output vectors are normalized (unit norm).
    fn normalized(&self) -> bool;
}

/// Error types for embedding operations.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmbeddingError {
    #[error("model initialization or download failed: {0}")]
    InitializationFailed(String),

    #[error("empty document provided")]
    EmptyDocument,

    #[error("empty query provided")]
    EmptyQuery,

    #[error("embedding contains non-finite value")]
    NonFiniteValue,

    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("embedding inference was cancelled")]
    Cancelled,

    #[error("embedding worker exhausted: too many concurrent requests")]
    WorkerSaturated,

    #[error("embedding worker panicked")]
    WorkerPanic,

    #[error("embedding request failed: {0}")]
    RequestFailed(String),

    #[error("invalid embedding configuration: {0}")]
    InvalidConfiguration(String),
}

/// Deterministic, dependency-free embedding backend using hashing.
///
/// Produces stable, finite, correctly-dimensioned, normalized vectors from text
/// without external dependencies. Suitable for all tests and default usage.
///
/// Implementation:
/// - Uses SHA-256 hash of (model_revision || document_index || normalized_text)
/// - Projects hash bytes into R^384 via modular arithmetic
/// - Normalizes to unit norm
pub struct DeterministicBackend {
    model_id: String,
    model_revision: String,
    dimensions: usize,
}

impl DeterministicBackend {
    /// Create a new deterministic backend.
    pub fn new() -> Self {
        Self {
            // Deliberately NOT a real model name. Stored vectors are keyed by
            // (model_id, model_revision) and compatibility is decided from that
            // key, so claiming to be BGE here would let hash vectors be compared
            // against real BGE vectors as if they were interchangeable.
            model_id: "deterministic-hash".to_string(),
            model_revision: "deterministic-v1".to_string(),
            dimensions: 384,
        }
    }

    /// Hash a string deterministically to a vector of f32.
    fn hash_to_vector(text: &str, model_revision: &str, doc_index: usize, dims: usize) -> Vec<f32> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(model_revision.as_bytes());
        hasher.update(b"\0");
        hasher.update(doc_index.to_le_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        // Project hash bytes into R^dims via modular arithmetic
        let mut vector = Vec::with_capacity(dims);
        for i in 0..dims {
            let byte_idx = i % hash.len();
            let base = hash[byte_idx] as f32 / 255.0;
            // Perturb with next byte to increase variation
            let perturb_idx = (i + 1) % hash.len();
            let perturb = hash[perturb_idx] as f32 / 255.0;
            // Mix to get value in approximately [0, 1]
            let val = (base + perturb) / 2.0;
            // Shift to approximately [-0.5, 0.5]
            vector.push(val - 0.5);
        }

        // Normalize to unit norm
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 1e-6 {
            for v in &mut vector {
                *v /= norm;
            }
        } else {
            // Degenerate case: very small norm. Use a default direction.
            vector = vec![0.0; dims];
            vector[0] = 1.0;
        }

        vector
    }
}

impl Default for DeterministicBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingBackend for DeterministicBackend {
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut embeddings = Vec::with_capacity(documents.len());
        for (i, doc) in documents.iter().enumerate() {
            if doc.trim().is_empty() {
                return Err(EmbeddingError::EmptyDocument);
            }
            embeddings.push(Self::hash_to_vector(
                doc,
                &self.model_revision,
                i,
                self.dimensions,
            ));
        }
        Ok(embeddings)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        if query.trim().is_empty() {
            return Err(EmbeddingError::EmptyQuery);
        }
        Ok(Self::hash_to_vector(
            query,
            &self.model_revision,
            0,
            self.dimensions,
        ))
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn model_revision(&self) -> &str {
        &self.model_revision
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn normalized(&self) -> bool {
        true
    }
}

/// Default external endpoint: the same OpenRouter API root the LLM provider uses,
/// so the skill library needs no separate provider setup.
pub const DEFAULT_EXTERNAL_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Default credential: the same environment variable the OpenRouter LLM provider
/// reads (see `ProviderKind::OpenRouter` in `src/auth.rs`).
pub const DEFAULT_EXTERNAL_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Default embedding model, routed through OpenRouter.
pub const DEFAULT_EXTERNAL_MODEL: &str = "openai/text-embedding-3-small";

/// Output width of [`DEFAULT_EXTERNAL_MODEL`]. Verified against the live endpoint.
pub const DEFAULT_EXTERNAL_DIMENSIONS: usize = 1536;

/// Embedding backend backed by an OpenAI-compatible embeddings HTTP API.
///
/// Selected with `[embedding] backend = "external"`. This is the practical way to
/// get real semantic embeddings on hosts where the local ONNX model cannot be
/// built, and it requires no model download.
///
/// The API key is read once from the environment variable named by
/// `api_key_env`; it is never stored in config and never appears in an error.
/// All calls are blocking and must run on a blocking worker, never on the async
/// executor or the QuickJS thread.
pub struct ExternalBackend {
    timeout: std::time::Duration,
    endpoint: String,
    api_key: String,
    model_id: String,
    model_revision: String,
    dimensions: usize,
    headers: Vec<(String, String)>,
}

/// Process-wide blocking HTTP client for external embedding requests.
///
/// `reqwest::blocking::Client` owns an internal tokio runtime, and dropping a
/// runtime from an async context panics with "Cannot drop a runtime in a context
/// where blocking is not allowed". Because the agent runner is async, a client
/// stored in `ExternalBackend` would panic whenever the embedder was dropped on
/// an async task. A `static` is never dropped, so keeping one shared client here
/// removes that failure mode entirely. Per-request timeouts are applied at the
/// `RequestBuilder`, so the shared client needs no default timeout.
fn external_http_client() -> Result<&'static reqwest::blocking::Client, EmbeddingError> {
    static CLIENT: std::sync::OnceLock<Result<reqwest::blocking::Client, String>> =
        std::sync::OnceLock::new();

    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| EmbeddingError::InitializationFailed(error.clone()))
}

/// Hand-written so the API key can never reach a log or panic message. Do not
/// replace this with `#[derive(Debug)]`.
impl std::fmt::Debug for ExternalBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalBackend")
            .field("endpoint", &self.endpoint)
            .field("model_id", &self.model_id)
            .field("model_revision", &self.model_revision)
            .field("dimensions", &self.dimensions)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl ExternalBackend {
    /// Build a backend from the `[embedding]` config section.
    ///
    /// Every field required by an external provider is validated up front so a
    /// misconfiguration surfaces at startup rather than as empty retrieval
    /// results during a turn.
    pub fn from_config(config: &crate::config::EmbeddingConfig) -> Result<Self, EmbeddingError> {
        let api_key_env = config
            .api_key_env
            .as_deref()
            .unwrap_or(DEFAULT_EXTERNAL_API_KEY_ENV);

        let api_key = std::env::var(api_key_env).map_err(|_| {
            EmbeddingError::InvalidConfiguration(format!(
                "environment variable `{api_key_env}` (from [embedding] api_key_env) is not set"
            ))
        })?;
        if api_key.trim().is_empty() {
            return Err(EmbeddingError::InvalidConfiguration(format!(
                "environment variable `{api_key_env}` (from [embedding] api_key_env) is empty"
            )));
        }

        Self::from_config_with_key(config, api_key)
    }

    /// Build a backend from config with the API key supplied directly.
    ///
    /// Split out from [`Self::from_config`] so the environment is read in exactly
    /// one place and so construction can be exercised without mutating process
    /// environment state.
    pub fn from_config_with_key(
        config: &crate::config::EmbeddingConfig,
        api_key: String,
    ) -> Result<Self, EmbeddingError> {
        // Default to the same OpenRouter endpoint and credential the LLM already
        // uses, so `backend = "external"` works with no duplicated provider config.
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_EXTERNAL_BASE_URL);
        let model_id = config.model.as_deref().unwrap_or(DEFAULT_EXTERNAL_MODEL);

        // Dimensions may only be inferred for the model whose width we actually
        // know. A custom model must state its width, because a wrong value would
        // silently mix incompatible vectors into an index generation.
        let dimensions = match config.dimensions {
            Some(dimensions) => dimensions,
            None if model_id == DEFAULT_EXTERNAL_MODEL => DEFAULT_EXTERNAL_DIMENSIONS,
            None => {
                return Err(EmbeddingError::InvalidConfiguration(format!(
                    "[embedding] `dimensions` is required for model `{model_id}`; it can only \
                     be inferred for the default `{DEFAULT_EXTERNAL_MODEL}`"
                )));
            }
        };

        if dimensions == 0 {
            return Err(EmbeddingError::InvalidConfiguration(
                "[embedding] `dimensions` must be greater than zero".to_string(),
            ));
        }

        let timeout = std::time::Duration::from_secs(config.timeout_secs.unwrap_or(30));

        // `base_url` is the API root; append the standard embeddings path.
        let endpoint = format!("{}/embeddings", base_url.trim_end_matches('/'));

        // Default the revision to the model name so vectors are still keyed by
        // something that changes when the operator switches models.
        let model_revision = config
            .model_revision
            .as_deref()
            .unwrap_or(model_id)
            .to_string();

        let mut headers: Vec<(String, String)> = config
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        // Deterministic request construction keeps fixtures stable.
        headers.sort();

        Ok(Self {
            timeout,
            endpoint,
            api_key,
            model_id: model_id.to_string(),
            model_revision,
            dimensions,
            headers,
        })
    }

    /// Issue one embeddings request and return vectors in input order.
    fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({ "model": self.model_id, "input": inputs });

        let mut request = external_http_client()?
            .post(&self.endpoint)
            .timeout(self.timeout)
            .bearer_auth(&self.api_key)
            .json(&body);
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        let response = request
            .send()
            .map_err(|error| EmbeddingError::RequestFailed(redact_url(&error.to_string())))?;

        let status = response.status();
        if !status.is_success() {
            // The body may echo the request; report only the status so a key or
            // document text cannot leak into logs.
            return Err(EmbeddingError::RequestFailed(format!(
                "embeddings endpoint returned HTTP {status}"
            )));
        }

        let payload: serde_json::Value = response
            .json()
            .map_err(|error| EmbeddingError::RequestFailed(redact_url(&error.to_string())))?;

        let data = payload
            .get("data")
            .and_then(|data| data.as_array())
            .ok_or_else(|| {
                EmbeddingError::RequestFailed("response has no `data` array".to_string())
            })?;

        if data.len() != inputs.len() {
            return Err(EmbeddingError::RequestFailed(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                data.len()
            )));
        }

        // The API does not guarantee response order, so place each vector by its
        // reported index rather than trusting array position.
        let mut ordered: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        for (position, entry) in data.iter().enumerate() {
            let index = entry
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|index| index as usize)
                .unwrap_or(position);
            if index >= ordered.len() {
                return Err(EmbeddingError::RequestFailed(format!(
                    "response index {index} out of range"
                )));
            }

            let values = entry
                .get("embedding")
                .and_then(|embedding| embedding.as_array())
                .ok_or_else(|| {
                    EmbeddingError::RequestFailed("response entry has no `embedding`".to_string())
                })?;

            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                let number = value.as_f64().ok_or(EmbeddingError::NonFiniteValue)?;
                let number = number as f32;
                if !number.is_finite() {
                    return Err(EmbeddingError::NonFiniteValue);
                }
                vector.push(number);
            }

            if vector.len() != self.dimensions {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.dimensions,
                    actual: vector.len(),
                });
            }

            if ordered[index].is_some() {
                return Err(EmbeddingError::RequestFailed(format!(
                    "response repeated index {index}"
                )));
            }
            ordered[index] = Some(normalize_vector(vector)?);
        }

        ordered.into_iter().enumerate().try_fold(
            Vec::with_capacity(inputs.len()),
            |mut acc, (index, vector)| {
                let vector = vector.ok_or_else(|| {
                    EmbeddingError::RequestFailed(format!("response missing index {index}"))
                })?;
                acc.push(vector);
                Ok(acc)
            },
        )
    }
}

impl EmbeddingBackend for ExternalBackend {
    fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if documents.iter().any(|document| document.trim().is_empty()) {
            return Err(EmbeddingError::EmptyDocument);
        }
        self.request(documents)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
        if query.trim().is_empty() {
            return Err(EmbeddingError::EmptyQuery);
        }
        let mut vectors = self.request(std::slice::from_ref(&query.to_string()))?;
        vectors.pop().ok_or_else(|| {
            EmbeddingError::RequestFailed("response contained no embedding".to_string())
        })
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn model_revision(&self) -> &str {
        &self.model_revision
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn normalized(&self) -> bool {
        true
    }
}

/// Scale a vector to unit norm, rejecting non-finite or zero-magnitude output.
fn normalize_vector(mut vector: Vec<f32>) -> Result<Vec<f32>, EmbeddingError> {
    if vector.is_empty() {
        return Err(EmbeddingError::DimensionMismatch {
            expected: 1,
            actual: 0,
        });
    }
    let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(EmbeddingError::NonFiniteValue);
    }
    for value in &mut vector {
        *value /= norm;
        if !value.is_finite() {
            return Err(EmbeddingError::NonFiniteValue);
        }
    }
    Ok(vector)
}

/// Strip any URL from an error string so an endpoint carrying a key in a query
/// parameter cannot reach a log.
fn redact_url(message: &str) -> String {
    message
        .split_whitespace()
        .map(|token| {
            if token.contains("://") {
                "<url>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fastembed-backed embedding backend (opt-in via feature gate).
#[cfg(feature = "skills-embed")]
pub mod fastembed_backend {
    use super::*;
    use fastembed::{EmbeddingModel, InitOptions};
    // `InitOptions` is #[non_exhaustive] in fastembed 5, so it must be built through
    // its builder rather than a struct expression.
    use std::sync::{Arc, Mutex};

    /// Real ONNX/BGE embedding backend using fastembed.
    ///
    /// Only available with the `skills-embed` feature.
    /// Targets BAAI/bge-small-en-v1.5 for 384-dimensional normalized embeddings.
    pub struct FastembedBackend {
        model: Arc<Mutex<fastembed::TextEmbedding>>,
        model_id: String,
        model_revision: String,
        dimensions: usize,
    }

    impl FastembedBackend {
        /// Initialize the fastembed model.
        ///
        /// This downloads the model on first run and caches it locally.
        pub fn new() -> Result<Self, EmbeddingError> {
            let model_id = "BAAI/bge-small-en-v1.5";
            let options = InitOptions::new(EmbeddingModel::BGESmallENV15);

            let model = fastembed::TextEmbedding::try_new(options)
                .map_err(|e| EmbeddingError::InitializationFailed(e.to_string()))?;

            Ok(Self {
                model: Arc::new(Mutex::new(model)),
                model_id: model_id.to_string(),
                model_revision: "v1.5".to_string(),
                dimensions: 384,
            })
        }
    }

    impl EmbeddingBackend for FastembedBackend {
        fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            // Validate input
            for doc in documents {
                if doc.trim().is_empty() {
                    return Err(EmbeddingError::EmptyDocument);
                }
            }

            let mut model = self.model.lock().map_err(|_| EmbeddingError::WorkerPanic)?;

            let embeddings = model
                .embed(documents, None)
                .map_err(|e| EmbeddingError::InitializationFailed(e.to_string()))?;

            // Validate output
            for embedding in &embeddings {
                if embedding.len() != self.dimensions {
                    return Err(EmbeddingError::DimensionMismatch {
                        expected: self.dimensions,
                        actual: embedding.len(),
                    });
                }
                if !embedding.iter().all(|v| v.is_finite()) {
                    return Err(EmbeddingError::NonFiniteValue);
                }
            }

            Ok(embeddings)
        }

        fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbeddingError> {
            if query.trim().is_empty() {
                return Err(EmbeddingError::EmptyQuery);
            }

            let mut model = self.model.lock().map_err(|_| EmbeddingError::WorkerPanic)?;

            let mut embeddings = model
                .embed(vec![query.to_string()], None)
                .map_err(|e| EmbeddingError::InitializationFailed(e.to_string()))?;

            if embeddings.is_empty() {
                return Err(EmbeddingError::EmptyDocument);
            }

            let embedding = embeddings.pop().unwrap();

            // Validate output
            if embedding.len() != self.dimensions {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.dimensions,
                    actual: embedding.len(),
                });
            }
            if !embedding.iter().all(|v| v.is_finite()) {
                return Err(EmbeddingError::NonFiniteValue);
            }

            Ok(embedding)
        }

        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn model_revision(&self) -> &str {
            &self.model_revision
        }

        fn dimensions(&self) -> usize {
            self.dimensions
        }

        fn normalized(&self) -> bool {
            true
        }
    }
}

/// Immutable metadata about an embedding model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMetadata {
    pub model_id: String,
    pub model_revision: String,
    pub dimensions: usize,
    pub normalized: bool,
}

/// Bounded query cache with O(1)-hit monotonic counter LRU eviction.
struct QueryCache {
    entries: HashMap<String, CacheEntry>,
    max_entries: usize,
    max_bytes: usize,
    current_bytes: usize,
    hits: u64,
    evictions: u64,
    /// Monotonic generation counter for last-used tracking.
    /// Incremented on each cache hit to avoid O(n) list manipulations.
    generation: u64,
}

/// A cache entry holding the embedding and tracking when it was last used.
/// The immutable slice makes cache hits cheap to share without exposing spare
/// vector capacity or allowing callers to mutate the cached value.
struct CacheEntry {
    embedding: Arc<[f32]>,
    size_bytes: usize,
    /// Generation at which this entry was last accessed (or inserted).
    last_used: u64,
}

impl QueryCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            max_bytes,
            current_bytes: 0,
            hits: 0,
            evictions: 0,
            generation: 0,
        }
    }

    fn next_generation(&mut self) -> u64 {
        if self.generation == u64::MAX {
            let mut entries: Vec<_> = self.entries.values_mut().collect();
            entries.sort_unstable_by_key(|entry| entry.last_used);
            for (index, entry) in entries.into_iter().enumerate() {
                entry.last_used = index as u64 + 1;
            }
            self.generation = self.entries.len() as u64;
        }
        self.generation += 1;
        self.generation
    }

    fn get(&mut self, key: &str) -> Option<Arc<[f32]>> {
        let generation = self.next_generation();
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = generation;
            self.hits += 1;
            Some(Arc::clone(&entry.embedding))
        } else {
            None
        }
    }

    fn insert(&mut self, key: String, embedding: Arc<[f32]>) {
        let size_bytes = embedding.len() * std::mem::size_of::<f32>();

        if self.max_entries == 0 || size_bytes > self.max_bytes {
            return;
        }

        let generation = self.next_generation();

        // If key exists, remove old entry first
        if let Some(old_entry) = self.entries.remove(&key) {
            self.current_bytes = self.current_bytes.saturating_sub(old_entry.size_bytes);
        }

        let entry = CacheEntry {
            embedding,
            size_bytes,
            last_used: generation,
        };

        // Evict entries if necessary to make room: O(n) but only on eviction
        while (self.current_bytes + size_bytes > self.max_bytes && !self.entries.is_empty())
            || (self.entries.len() >= self.max_entries && !self.entries.is_empty())
        {
            // Scan for the entry with minimum last_used
            if let Some(lru_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone())
            {
                if let Some(removed) = self.entries.remove(&lru_key) {
                    self.current_bytes = self.current_bytes.saturating_sub(removed.size_bytes);
                    self.evictions += 1;
                }
            } else {
                break;
            }
        }

        self.current_bytes += size_bytes;
        self.entries.insert(key, entry);
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.entries.len(),
            bytes: self.current_bytes,
            hits: self.hits,
            evictions: self.evictions,
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.current_bytes = 0;
    }
}

/// Observable cache statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub evictions: u64,
}

/// Reusable embedding service with model lifecycle and caching.
///
/// Initialized once and shared across multiple indexes.
/// Provides:
/// - Lazy model initialization with concurrent-safe setup
/// - Immutable model metadata
/// - Document batching for admission/reindex
/// - Query embedding with bounded cache
pub struct Embedder {
    backend: Arc<dyn EmbeddingBackend>,
    metadata: ModelMetadata,
    cache: Arc<tokio::sync::Mutex<QueryCache>>,
}

/// The backend is a trait object and the cache sits behind an async mutex, so
/// this reports the model identity only.
impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl Embedder {
    /// Create an embedder with the default deterministic backend.
    pub fn new() -> Result<Self, EmbeddingError> {
        let backend = Arc::new(DeterministicBackend::new());
        Self::with_backend(backend)
    }

    /// Create an embedder from the `[embedding]` config section.
    ///
    /// `None` selects the built-in deterministic backend. Configuration errors are
    /// returned rather than silently downgraded, so an operator who asked for a
    /// real model never gets meaningless hash vectors without being told.
    pub fn from_config(
        config: Option<&crate::config::EmbeddingConfig>,
    ) -> Result<Self, EmbeddingError> {
        use crate::config::EmbeddingBackendKind;

        let Some(config) = config else {
            return Self::new();
        };

        match config.backend {
            EmbeddingBackendKind::Deterministic => Self::new(),
            EmbeddingBackendKind::External => {
                Self::with_backend(Arc::new(ExternalBackend::from_config(config)?))
            }
            EmbeddingBackendKind::Local => {
                #[cfg(feature = "skills-embed")]
                {
                    Self::with_backend(Arc::new(fastembed_backend::FastembedBackend::new()?))
                }
                #[cfg(not(feature = "skills-embed"))]
                {
                    Err(EmbeddingError::InvalidConfiguration(
                        "[embedding] backend = \"local\" requires the `skills-embed` build \
                         feature; rebuild with it or use backend = \"external\""
                            .to_string(),
                    ))
                }
            }
        }
    }

    /// Create an embedder with a custom backend.
    pub fn with_backend(backend: Arc<dyn EmbeddingBackend>) -> Result<Self, EmbeddingError> {
        let metadata = ModelMetadata {
            model_id: backend.model_id().to_string(),
            model_revision: backend.model_revision().to_string(),
            dimensions: backend.dimensions(),
            normalized: backend.normalized(),
        };

        // Validate the advertised metadata only. Probing with a live embed call
        // would make construction perform network I/O for the external backend and
        // download a model for the local one, turning `new()` into a startup stall
        // or a hard failure on an offline machine.
        if metadata.dimensions == 0 {
            return Err(EmbeddingError::InvalidConfiguration(
                "backend reports zero dimensions".to_string(),
            ));
        }
        if metadata.model_revision.is_empty() {
            return Err(EmbeddingError::InvalidConfiguration(
                "backend reports an empty model revision, which would make stored \
                 vectors impossible to invalidate"
                    .to_string(),
            ));
        }

        Ok(Self {
            backend,
            metadata,
            cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(
                100,
                1024 * 1024 * 10,
            ))), // 100 entries, 10MB
        })
    }

    /// Get immutable model metadata.
    pub fn model_metadata(&self) -> &ModelMetadata {
        &self.metadata
    }

    /// Embed a batch of documents.
    ///
    /// All returned vectors have the same dimension and normalization as the model.
    pub fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        // Validate all non-empty
        for doc in documents {
            if doc.trim().is_empty() {
                return Err(EmbeddingError::EmptyDocument);
            }
        }

        let embeddings = self.backend.embed_documents(documents)?;

        // Validate all have correct dimension
        for embedding in &embeddings {
            if embedding.len() != self.metadata.dimensions {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: self.metadata.dimensions,
                    actual: embedding.len(),
                });
            }
            if !embedding.iter().all(|v| v.is_finite()) {
                return Err(EmbeddingError::NonFiniteValue);
            }
        }

        Ok(embeddings)
    }

    /// Embed a single query with caching.
    ///
    /// Returned embedding is normalized, has the correct dimension, and is
    /// cheaply shared across cache hits.
    pub async fn embed_query_cached(&self, query: &str) -> Result<Arc<[f32]>, EmbeddingError> {
        if query.trim().is_empty() {
            return Err(EmbeddingError::EmptyQuery);
        }

        // Compute cache key: (model_revision, sha256(query))
        let normalized_query = query.trim();
        let mut hasher = sha2::Sha256::new();
        hasher.update(normalized_query.as_bytes());
        let hash = hasher.finalize();
        let cache_key = format!("{}:{}", self.metadata.model_revision, hex_lower(&hash));

        // Check cache
        {
            let mut cache = self.cache.lock().await;
            if let Some(embedding) = cache.get(&cache_key) {
                return Ok(embedding);
            }
        }

        // Inference is CPU-bound (local model) or blocking I/O (external API), so it
        // must never run on the async executor. Hand it to a blocking worker and map
        // join failures onto the cancellation/panic error classes.
        let backend = Arc::clone(&self.backend);
        let owned_query = normalized_query.to_string();
        let embedding =
            crate::agent::runner::spawn_blocking_scoped(move || backend.embed_query(&owned_query))
                .await
                .map_err(|join_error| {
                    if join_error.is_cancelled() {
                        EmbeddingError::Cancelled
                    } else {
                        EmbeddingError::WorkerPanic
                    }
                })??;

        // Validate
        if embedding.len() != self.metadata.dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: self.metadata.dimensions,
                actual: embedding.len(),
            });
        }
        if !embedding.iter().all(|v| v.is_finite()) {
            return Err(EmbeddingError::NonFiniteValue);
        }

        let embedding: Arc<[f32]> = embedding.into();

        // Cache result without copying its allocation.
        {
            let mut cache = self.cache.lock().await;
            cache.insert(cache_key, Arc::clone(&embedding));
        }

        Ok(embedding)
    }

    /// Get observable cache statistics.
    pub async fn cache_stats(&self) -> CacheStats {
        self.cache.lock().await.stats()
    }

    /// Clear the cache (for testing).
    pub async fn clear_cache(&self) {
        self.cache.lock().await.clear();
    }
}

impl Default for Embedder {
    fn default() -> Self {
        Self::new().expect("failed to create default embedder")
    }
}

/// Build a deterministic retrieval document from skill metadata.
///
/// Format: `<description>\nExports: <signature>; ...\nTags: <tag>, ...\nIdentifiers: <identifiers>`
pub struct SkillDocument {
    pub description: String,
    pub exports: Vec<(String, String)>, // (name, signature)
    pub tags: Vec<String>,
    pub identifiers: Vec<String>,
}

impl SkillDocument {
    /// Create a new skill document with default values.
    pub fn new(description: String) -> Self {
        Self {
            description,
            exports: Vec::new(),
            tags: Vec::new(),
            identifiers: Vec::new(),
        }
    }

    /// Add an export.
    pub fn with_export(mut self, name: String, signature: String) -> Self {
        self.exports.push((name, signature));
        self
    }

    /// Add multiple exports.
    pub fn with_exports(mut self, exports: Vec<(String, String)>) -> Self {
        self.exports = exports;
        self
    }

    /// Add tags.
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Add identifiers (bounded and sorted).
    pub fn with_identifiers(mut self, mut identifiers: Vec<String>) -> Self {
        // Bounded to first 10 identifiers
        identifiers.sort();
        identifiers.dedup();
        if identifiers.len() > 10 {
            identifiers.truncate(10);
        }
        self.identifiers = identifiers;
        self
    }

    /// Render the document as a string for embedding.
    pub fn render(&self) -> String {
        let mut doc = self.description.clone();

        if !self.exports.is_empty() {
            doc.push_str("\nExports: ");
            let exports_str = self
                .exports
                .iter()
                .map(|(name, sig)| format!("{name}{sig}"))
                .collect::<Vec<_>>()
                .join("; ");
            doc.push_str(&exports_str);
        }

        if !self.tags.is_empty() {
            doc.push_str("\nTags: ");
            let tags_str = self.tags.join(", ");
            doc.push_str(&tags_str);
        }

        if !self.identifiers.is_empty() {
            doc.push_str("\nIdentifiers: ");
            let ids_str = self.identifiers.join(", ");
            doc.push_str(&ids_str);
        }

        doc
    }
}

/// Helper to convert bytes to lowercase hex.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_backend_creates_embeddings() {
        let backend = DeterministicBackend::new();
        let docs = vec!["hello world".to_string(), "goodbye world".to_string()];
        let embeddings = backend.embed_documents(&docs).unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 384);
        assert_eq!(embeddings[1].len(), 384);
    }

    #[test]
    fn test_deterministic_backend_normalized() {
        let backend = DeterministicBackend::new();
        let docs = vec!["test document".to_string()];
        let embeddings = backend.embed_documents(&docs).unwrap();
        let vec = &embeddings[0];
        let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "vector not normalized: norm={}",
            norm
        );
    }

    #[test]
    fn test_deterministic_backend_query() {
        let backend = DeterministicBackend::new();
        let query = backend.embed_query("what is rust?").unwrap();
        assert_eq!(query.len(), 384);
        let norm: f32 = query.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_deterministic_backend_empty_rejects() {
        let backend = DeterministicBackend::new();
        assert_eq!(backend.embed_documents(&[]).unwrap().len(), 0);
        assert_eq!(
            backend.embed_documents(&["  ".to_string()]),
            Err(EmbeddingError::EmptyDocument)
        );
        assert_eq!(backend.embed_query("  "), Err(EmbeddingError::EmptyQuery));
    }

    #[test]
    fn test_deterministic_backend_consistent() {
        let backend = DeterministicBackend::new();
        let doc = "same text".to_string();
        let emb1 = backend.embed_documents(std::slice::from_ref(&doc)).unwrap()[0].clone();
        let emb2 = backend.embed_documents(&[doc]).unwrap()[0].clone();
        assert_eq!(
            emb1, emb2,
            "deterministic backend not producing same embedding"
        );
    }

    #[test]
    fn test_model_metadata() {
        let embedder = Embedder::new().unwrap();
        let meta = embedder.model_metadata();
        // The offline backend advertises its own identity, not a real model's.
        assert_eq!(meta.model_id, "deterministic-hash");
        assert_eq!(meta.dimensions, 384);
        assert!(meta.normalized);
    }

    #[tokio::test]
    async fn test_embedder_embed_documents() {
        let embedder = Embedder::new().unwrap();
        let docs = vec!["hello".to_string(), "world".to_string()];
        let embeddings = embedder.embed_documents(&docs).unwrap();
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 384);
    }

    #[tokio::test]
    async fn test_embedder_cache_hit() {
        let embedder = Embedder::new().unwrap();
        embedder.clear_cache().await;

        let query = "what is rust?";
        let emb1 = embedder.embed_query_cached(query).await.unwrap();
        let stats1 = embedder.cache_stats().await;
        assert_eq!(stats1.entries, 1);
        assert_eq!(stats1.hits, 0);

        let emb2 = embedder.embed_query_cached(query).await.unwrap();
        let stats2 = embedder.cache_stats().await;
        assert_eq!(stats2.entries, 1);
        assert_eq!(stats2.hits, 1);
        assert_eq!(emb1, emb2);
    }

    #[tokio::test]
    async fn test_embedder_cache_eviction_by_count() {
        // Create a small cache with only 3 entries
        let backend = Arc::new(DeterministicBackend::new());
        let metadata = ModelMetadata {
            model_id: backend.model_id().to_string(),
            model_revision: backend.model_revision().to_string(),
            dimensions: backend.dimensions(),
            normalized: backend.normalized(),
        };

        let embedder = Embedder {
            backend: backend.clone(),
            metadata,
            cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(3, usize::MAX))),
        };

        let queries = vec!["query1", "query2", "query3", "query4"];
        for q in &queries {
            embedder.embed_query_cached(q).await.unwrap();
        }

        let stats = embedder.cache_stats().await;
        assert_eq!(stats.entries, 3, "cache should have evicted old entry");
        assert_eq!(stats.evictions, 1);
    }

    #[tokio::test]
    async fn test_embedder_cache_eviction_by_bytes() {
        let backend = Arc::new(DeterministicBackend::new());
        let metadata = ModelMetadata {
            model_id: backend.model_id().to_string(),
            model_revision: backend.model_revision().to_string(),
            dimensions: backend.dimensions(),
            normalized: backend.normalized(),
        };

        // Create cache with tiny byte limit: one 384-float vector is ~1.5KB
        // So only 1-2 vectors should fit in 2KB
        let embedder = Embedder {
            backend: backend.clone(),
            metadata,
            cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(usize::MAX, 2000))),
        };

        embedder.embed_query_cached("query1").await.unwrap();
        embedder.embed_query_cached("query2").await.unwrap();

        let stats = embedder.cache_stats().await;
        assert!(stats.evictions > 0, "cache should have evicted");
    }

    #[tokio::test]
    async fn test_cache_hit_does_not_reallocate_embedding() {
        // Test that the cached Arc is reused across hits, not reallocated.
        // We inspect the cache directly to verify Arc pointer equality.
        let backend = Arc::new(DeterministicBackend::new());
        let metadata = ModelMetadata {
            model_id: backend.model_id().to_string(),
            model_revision: backend.model_revision().to_string(),
            dimensions: backend.dimensions(),
            normalized: backend.normalized(),
        };

        let cache = Arc::new(tokio::sync::Mutex::new(QueryCache::new(100, usize::MAX)));
        let embedder = Embedder {
            backend,
            metadata,
            cache: Arc::clone(&cache),
        };

        let query = "test query for arc sharing";
        let emb1 = embedder.embed_query_cached(query).await.unwrap();

        let emb2 = embedder.embed_query_cached(query).await.unwrap();

        assert!(Arc::ptr_eq(&emb1, &emb2));
        assert_eq!(cache.lock().await.hits, 1);
    }

    #[test]
    fn test_query_cache_arc_sharing_on_hit() {
        // Unit test: verify that get() returns the same Arc, not a cloned vector.
        let mut cache = QueryCache::new(10, usize::MAX);
        let embedding = vec![1.0, 2.0, 3.0, 4.0];
        let key = "test_key".to_string();

        cache.insert(key.clone(), embedding.into());

        // First get
        let arc1 = cache.get(&key).expect("should find key");
        let ptr1 = Arc::as_ptr(&arc1);

        // Second get on same key
        let arc2 = cache.get(&key).expect("should find key");
        let ptr2 = Arc::as_ptr(&arc2);

        // Both should point to the same Arc allocation
        assert_eq!(ptr1, ptr2, "cache should return the same Arc on hit");

        // Verify the data is correct
        assert_eq!(arc1.len(), 4);
        assert_eq!(arc1[0], 1.0);
    }

    #[test]
    fn test_query_cache_rejects_entries_that_cannot_fit() {
        let embedding: Arc<[f32]> = vec![1.0, 2.0].into();

        let mut entry_limited = QueryCache::new(0, usize::MAX);
        entry_limited.insert("entry-limit".to_string(), Arc::clone(&embedding));
        assert_eq!(entry_limited.stats().entries, 0);
        assert_eq!(entry_limited.stats().bytes, 0);

        let mut byte_limited = QueryCache::new(1, std::mem::size_of::<f32>());
        byte_limited.insert("byte-limit".to_string(), embedding);
        assert_eq!(byte_limited.stats().entries, 0);
        assert_eq!(byte_limited.stats().bytes, 0);
    }

    #[tokio::test]
    async fn test_cache_lru_eviction_respects_recency() {
        // Test that LRU eviction is based on last used (via get), not insertion order.
        let backend = Arc::new(DeterministicBackend::new());
        let metadata = ModelMetadata {
            model_id: backend.model_id().to_string(),
            model_revision: backend.model_revision().to_string(),
            dimensions: backend.dimensions(),
            normalized: backend.normalized(),
        };

        // Create a small cache: max 2 entries
        let embedder = Embedder {
            backend,
            metadata,
            cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(2, usize::MAX))),
        };

        // Insert q1, q2
        embedder.embed_query_cached("query1").await.unwrap();
        embedder.embed_query_cached("query2").await.unwrap();
        let stats = embedder.cache_stats().await;
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.evictions, 0);

        // Access q1 again, making it recently used
        embedder.embed_query_cached("query1").await.unwrap();

        // Insert q3: should evict q2 (least recently used), not q1
        embedder.embed_query_cached("query3").await.unwrap();
        let stats = embedder.cache_stats().await;
        assert_eq!(stats.entries, 2, "should have evicted exactly one entry");
        assert_eq!(stats.evictions, 1, "should have evicted q2");

        // Verify q1 is still cached
        let stats_before = embedder.cache_stats().await;
        let hits_before = stats_before.hits;
        embedder.embed_query_cached("query1").await.unwrap();
        let stats_after = embedder.cache_stats().await;
        assert_eq!(
            stats_after.hits,
            hits_before + 1,
            "q1 should still be in cache"
        );

        // Verify q2 was evicted
        embedder.embed_query_cached("query2").await.unwrap();
        let stats_q2 = embedder.cache_stats().await;
        // One more eviction to make room for q2
        assert!(
            stats_q2.evictions >= 2,
            "q2 should have been evicted earlier"
        );
    }

    #[test]
    fn test_skill_document_builder() {
        let doc = SkillDocument::new("Parse JSON safely".to_string())
            .with_export(
                "parseJson".to_string(),
                "(text: string): unknown | null".to_string(),
            )
            .with_tags(vec!["json".to_string(), "utility".to_string()])
            .with_identifiers(vec!["parse_json_v1".to_string()]);

        let rendered = doc.render();
        assert!(rendered.contains("Parse JSON safely"));
        assert!(rendered.contains("Exports:"));
        assert!(rendered.contains("parseJson"));
        assert!(rendered.contains("Tags:"));
        assert!(rendered.contains("json"));
        assert!(rendered.contains("utility"));
        assert!(rendered.contains("Identifiers:"));
        assert!(rendered.contains("parse_json_v1"));
    }

    #[test]
    fn test_skill_document_sorted_ids() {
        let doc = SkillDocument::new("Test".to_string()).with_identifiers(vec![
            "z".to_string(),
            "a".to_string(),
            "m".to_string(),
        ]);

        let rendered = doc.render();
        // Should be sorted
        assert!(rendered.contains("Identifiers: a, m, z"));
    }

    #[test]
    fn test_skill_document_bounded_ids() {
        let ids: Vec<String> = (0..20).map(|i| format!("id{}", i)).collect();
        let doc = SkillDocument::new("Test".to_string()).with_identifiers(ids);

        let rendered = doc.render();
        let id_count = rendered.matches(',').count() + 1; // count commas + 1
        assert!(id_count <= 10, "should bound identifiers to 10");
    }

    #[test]
    fn test_embedding_error_display() {
        assert_eq!(
            format!("{}", EmbeddingError::EmptyDocument),
            "empty document provided"
        );
        assert_eq!(
            format!("{}", EmbeddingError::EmptyQuery),
            "empty query provided"
        );
        assert_eq!(
            format!("{}", EmbeddingError::NonFiniteValue),
            "embedding contains non-finite value"
        );
    }
}

#[cfg(test)]
mod external_backend_tests {
    use super::*;
    use crate::config::{EmbeddingBackendKind, EmbeddingConfig};

    /// A fully specified external config, so each test can invalidate one field.
    fn valid_config() -> EmbeddingConfig {
        EmbeddingConfig {
            backend: EmbeddingBackendKind::External,
            base_url: Some("https://api.example.com/v1".to_string()),
            model: Some("text-embedding-3-small".into()),
            api_key_env: Some("MINI_AGENT_TEST_EMBED_KEY".into()),
            dimensions: Some(1536),
            model_revision: None,
            timeout_secs: Some(5),
            headers: HashMap::new(),
        }
    }

    #[test]
    fn default_config_selects_the_deterministic_backend() {
        assert_eq!(
            EmbeddingConfig::default().backend,
            EmbeddingBackendKind::Deterministic
        );
    }

    #[test]
    fn absent_config_yields_the_deterministic_backend() {
        let embedder = Embedder::from_config(None).expect("default embedder must build");
        assert_eq!(embedder.model_metadata().model_id, "deterministic-hash");
    }

    #[test]
    fn deterministic_backend_does_not_impersonate_a_real_model() {
        // Vector compatibility is decided from (model_id, model_revision). If the
        // offline hash backend claimed to be BGE, its vectors would be treated as
        // interchangeable with real BGE vectors.
        let backend = DeterministicBackend::new();
        assert!(
            !backend.model_id().contains("bge") && !backend.model_id().contains("BAAI"),
            "deterministic backend must not claim a real model id, got {}",
            backend.model_id()
        );
    }

    #[test]
    fn external_backend_defaults_every_field_to_the_openrouter_llm_settings() {
        // A bare `backend = "external"` must work without repeating provider
        // config: it inherits the same endpoint and credential the LLM uses.
        let config = EmbeddingConfig {
            backend: EmbeddingBackendKind::External,
            ..EmbeddingConfig::default()
        };
        let backend = ExternalBackend::from_config_with_key(&config, "k".to_string())
            .expect("a bare external config must resolve against defaults");
        assert_eq!(backend.endpoint, "https://openrouter.ai/api/v1/embeddings");
        assert_eq!(backend.model_id(), "openai/text-embedding-3-small");
        assert_eq!(backend.dimensions(), 1536);
    }

    #[test]
    fn explicit_fields_override_the_defaults() {
        let mut config = valid_config();
        config.base_url = Some("https://api.openai.com/v1".to_string());
        config.model = Some("text-embedding-3-large".into());
        config.dimensions = Some(3072);
        let backend = ExternalBackend::from_config_with_key(&config, "k".to_string())
            .expect("explicit config must be honoured");
        assert_eq!(backend.endpoint, "https://api.openai.com/v1/embeddings");
        assert_eq!(backend.model_id(), "text-embedding-3-large");
        assert_eq!(backend.dimensions(), 3072);
    }

    #[test]
    fn external_backend_rejects_zero_dimensions() {
        let mut config = valid_config();
        config.dimensions = Some(0);
        let error = ExternalBackend::from_config_with_key(&config, "k".to_string())
            .expect_err("zero dims must be rejected");
        assert!(
            matches!(error, EmbeddingError::InvalidConfiguration(ref m) if m.contains("dimensions")),
            "got {error:?}"
        );
    }

    #[test]
    fn external_backend_reports_a_missing_api_key_env_var_by_name() {
        let mut config = valid_config();
        config.api_key_env = Some("MINI_AGENT_DEFINITELY_UNSET_KEY_VAR".into());
        let error = ExternalBackend::from_config(&config).expect_err("unset key must be rejected");
        match error {
            EmbeddingError::InvalidConfiguration(message) => {
                assert!(
                    message.contains("MINI_AGENT_DEFINITELY_UNSET_KEY_VAR"),
                    "error must name the variable so it is actionable, got: {message}"
                );
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[cfg(not(feature = "skills-embed"))]
    #[test]
    fn local_backend_without_its_feature_is_a_clear_error_not_a_silent_downgrade() {
        let config = EmbeddingConfig {
            backend: EmbeddingBackendKind::Local,
            ..EmbeddingConfig::default()
        };
        let error = Embedder::from_config(Some(&config))
            .expect_err("local backend must fail without its feature");
        assert!(
            matches!(error, EmbeddingError::InvalidConfiguration(ref m) if m.contains("skills-embed")),
            "error must name the missing feature, got {error:?}"
        );
    }

    #[test]
    fn embedding_config_round_trips_through_toml() {
        let toml_text = r#"
backend = "external"
base_url = "https://api.openai.com/v1"
model = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
dimensions = 1536
timeout_secs = 30

[headers]
X-Organization = "acme"
"#;
        let config: EmbeddingConfig = toml::from_str(toml_text).expect("config must parse");
        assert_eq!(config.backend, EmbeddingBackendKind::External);
        assert_eq!(config.dimensions, Some(1536));
        assert_eq!(config.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(
            config.headers.get("X-Organization").map(String::as_str),
            Some("acme")
        );

        let reserialized = toml::to_string(&config).expect("config must serialize");
        assert!(
            reserialized.contains("backend = \"external\""),
            "got: {reserialized}"
        );
        // The key itself is never persisted — only the variable name.
        assert!(
            !reserialized.contains("sk-"),
            "config must never carry a key"
        );
    }

    #[test]
    fn base_url_trailing_slash_is_tolerated() {
        let mut config = valid_config();
        config.base_url = Some("https://api.example.com/v1/".to_string());
        let backend =
            ExternalBackend::from_config_with_key(&config, "k".to_string()).expect("must build");
        assert_eq!(backend.endpoint, "https://api.example.com/v1/embeddings");
    }

    #[test]
    fn model_revision_defaults_to_the_model_name() {
        let backend = ExternalBackend::from_config_with_key(&valid_config(), "k".to_string())
            .expect("must build");
        assert_eq!(backend.model_revision(), "text-embedding-3-small");
        assert_eq!(backend.dimensions(), 1536);
    }

    #[test]
    fn explicit_model_revision_is_preserved() {
        let mut config = valid_config();
        config.model_revision = Some("2026-01-snapshot".into());
        let backend =
            ExternalBackend::from_config_with_key(&config, "k".to_string()).expect("must build");
        assert_eq!(backend.model_revision(), "2026-01-snapshot");
    }

    #[test]
    fn normalize_vector_produces_unit_norm() {
        let normalized = normalize_vector(vec![3.0, 4.0]).expect("must normalize");
        let norm: f32 = normalized.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm was {norm}");
    }

    #[test]
    fn normalize_vector_rejects_a_zero_vector() {
        assert_eq!(
            normalize_vector(vec![0.0, 0.0]),
            Err(EmbeddingError::NonFiniteValue)
        );
    }

    #[test]
    fn redact_url_strips_endpoints_from_error_text() {
        let redacted =
            redact_url("error sending request for url https://api.example.com/v1?key=sk-secret");
        assert!(!redacted.contains("sk-secret"), "got: {redacted}");
        assert!(!redacted.contains("api.example.com"), "got: {redacted}");
        assert!(redacted.contains("<url>"), "got: {redacted}");
    }
}

/// Live end-to-end tests against the configured external provider.
///
/// These are `#[ignore]` by default so the normal suite stays hermetic and
/// offline. Run them explicitly with a key present:
///
/// ```text
/// cargo test --features js,skills external_live -- --ignored --nocapture
/// ```
#[cfg(test)]
mod external_live_tests {
    use super::*;
    use crate::config::{EmbeddingBackendKind, EmbeddingConfig};

    /// Config with every field defaulted — the whole point is that pointing at
    /// OpenRouter requires nothing beyond selecting the backend.
    fn defaulted_external_config() -> EmbeddingConfig {
        EmbeddingConfig {
            backend: EmbeddingBackendKind::External,
            ..EmbeddingConfig::default()
        }
    }

    #[test]
    fn defaults_resolve_to_openrouter_without_any_extra_config() {
        let backend = ExternalBackend::from_config_with_key(
            &defaulted_external_config(),
            "placeholder".to_string(),
        )
        .expect("a bare external config must resolve against the OpenRouter defaults");

        assert_eq!(backend.endpoint, "https://openrouter.ai/api/v1/embeddings");
        assert_eq!(backend.model_id(), "openai/text-embedding-3-small");
        assert_eq!(backend.dimensions(), 1536);
        assert_eq!(DEFAULT_EXTERNAL_API_KEY_ENV, "OPENROUTER_API_KEY");
    }

    #[test]
    fn a_custom_model_must_declare_its_dimensions() {
        let mut config = defaulted_external_config();
        config.model = Some("some/other-embedding-model".into());
        let error = ExternalBackend::from_config_with_key(&config, "placeholder".to_string())
            .expect_err("dimensions must not be guessed for an unknown model");
        assert!(
            matches!(error, EmbeddingError::InvalidConfiguration(ref m) if m.contains("dimensions")),
            "got {error:?}"
        );
    }

    #[test]
    #[ignore = "performs a live network call; requires OPENROUTER_API_KEY"]
    fn live_openrouter_query_embedding_is_well_formed() {
        let backend = ExternalBackend::from_config(&defaulted_external_config())
            .expect("OPENROUTER_API_KEY must be set for the live test");

        let vector = backend
            .embed_query("parse JSON safely and return null on error")
            .expect("live embedding request must succeed");

        assert_eq!(vector.len(), 1536, "unexpected embedding width");
        assert!(
            vector.iter().all(|value| value.is_finite()),
            "non-finite value present"
        );
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "vector must be unit-normalized, norm was {norm}"
        );
    }

    #[test]
    #[ignore = "performs a live network call; requires OPENROUTER_API_KEY"]
    fn live_openrouter_batch_preserves_input_order_and_semantics() {
        let backend = ExternalBackend::from_config(&defaulted_external_config())
            .expect("OPENROUTER_API_KEY must be set for the live test");

        let documents = vec![
            "a function that parses JSON text".to_string(),
            "a recipe for sourdough bread".to_string(),
            "a helper that decodes JSON strings".to_string(),
        ];
        let vectors = backend
            .embed_documents(&documents)
            .expect("live batch request must succeed");

        assert_eq!(vectors.len(), 3);
        for vector in &vectors {
            assert_eq!(vector.len(), 1536);
        }

        let cosine = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };

        // Documents 0 and 2 are both about JSON parsing; 1 is unrelated. If the
        // response were misordered or the vectors meaningless, this would not hold.
        let json_pair = cosine(&vectors[0], &vectors[2]);
        let unrelated = cosine(&vectors[0], &vectors[1]);
        assert!(
            json_pair > unrelated,
            "semantically related documents should score higher: {json_pair} vs {unrelated}"
        );
    }

    #[tokio::test]
    #[ignore = "performs a live network call; requires OPENROUTER_API_KEY"]
    async fn live_openrouter_query_cache_avoids_a_second_request() {
        let embedder =
            Embedder::from_config(Some(&defaulted_external_config())).expect("embedder must build");

        let first = embedder
            .embed_query_cached("find duplicate files")
            .await
            .expect("first");
        let stats = embedder.cache_stats().await;
        assert_eq!(stats.hits, 0);

        let second = embedder
            .embed_query_cached("find duplicate files")
            .await
            .expect("second");
        let stats = embedder.cache_stats().await;
        assert_eq!(
            stats.hits, 1,
            "the second identical query must be served from cache"
        );
        assert_eq!(first, second);
        assert_eq!(embedder.model_metadata().dimensions, 1536);
    }
}

/// Live tests for the local ONNX/BGE backend.
///
/// Requires the `skills-embed` feature. On hosts where `ort-sys` has no prebuilt
/// binaries (notably x86_64-apple-darwin), build with `ort`'s `load-dynamic`
/// feature and point `ORT_DYLIB_PATH` at a local ONNX Runtime, e.g.
/// `brew install onnxruntime`. The first run downloads the model (~30 MiB).
///
/// ```text
/// ORT_DYLIB_PATH=$(brew --prefix onnxruntime)/lib/libonnxruntime.dylib \
///   cargo test --features js,skills,skills-embed skills_embed_live -- --ignored --nocapture
/// ```
#[cfg(all(test, feature = "skills-embed"))]
mod skills_embed_live_tests {
    use super::*;

    /// One shared backend for the whole module. Constructing a second model
    /// concurrently races on the shared model cache, and the production
    /// `Embedder` shares a single backend anyway.
    fn shared_backend() -> &'static fastembed_backend::FastembedBackend {
        static BACKEND: std::sync::OnceLock<fastembed_backend::FastembedBackend> =
            std::sync::OnceLock::new();
        BACKEND.get_or_init(|| {
            fastembed_backend::FastembedBackend::new().expect("fastembed model must initialize")
        })
    }

    #[test]
    #[ignore = "downloads the BGE model and requires ONNX Runtime; see module docs"]
    fn skills_embed_live_produces_normalized_bge_vectors() {
        let backend =
            fastembed_backend::FastembedBackend::new().expect("fastembed model must initialize");

        assert_eq!(backend.model_id(), "BAAI/bge-small-en-v1.5");
        assert_eq!(backend.dimensions(), 384);

        let vector = backend
            .embed_query("parse JSON safely and return null on error")
            .expect("query embedding must succeed");
        assert_eq!(vector.len(), 384);
        assert!(vector.iter().all(|v| v.is_finite()));
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "expected unit norm, got {norm}");
    }

    #[test]
    #[ignore = "downloads the BGE model and requires ONNX Runtime; see module docs"]
    fn skills_embed_live_batch_is_semantically_ordered() {
        let backend =
            fastembed_backend::FastembedBackend::new().expect("fastembed model must initialize");

        let documents = vec![
            "a function that parses JSON text".to_string(),
            "a recipe for sourdough bread".to_string(),
            "a helper that decodes JSON strings".to_string(),
        ];
        let vectors = backend
            .embed_documents(&documents)
            .expect("batch embedding must succeed");
        assert_eq!(vectors.len(), 3);

        let cosine = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        let related = cosine(&vectors[0], &vectors[2]);
        let unrelated = cosine(&vectors[0], &vectors[1]);
        assert!(
            related > unrelated,
            "related documents must score higher: {related} vs {unrelated}"
        );
    }
}
