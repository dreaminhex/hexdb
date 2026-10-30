// HexDB Core Engine: vector search and embeddings
//
// Similarity queries (`similar_documents`) walk a vector index's nearest
// neighbors best first and keep those that pass the query's filter, narrowed
// to what the caller may read, until enough are found; they're returned with
// their `_score`. A query by text is embedded first, within the asking user's
// daily allowance (`ai.embeddings.user_daily_tokens`) and the monthly budget.
//
// Vector indexes with source fields fill their field themselves: writes whose
// source text changed (or that have no embedding from the model in use) queue
// the document, and a background task on the Overseer embeds the queue in
// batches and writes `{"vector", "model", "source_hash"}` into the field, as
// a guarded update (a document changed meanwhile is embedded again later).
// Changing `ai.embeddings.model` rebuilds those indexes and re-embeds.

use super::{writes::BatchItem, HexDBEngine};
use crate::{
    ai::EmbedPurpose,
    auth::Action,
    document::{Document, FieldValue},
    filter::Filter,
    hex::DocKey,
    wal::WalOp,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Most results one similarity query returns.
pub const MAX_K: usize = 100;

/// A similarity query: the `k` documents nearest to a vector, or to a text's
/// embedding, in `field` (which needs a vector index), among those matching `filter`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimilarQuery {
    pub field: String,
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default = "default_k")]
    pub k: usize,
    /// Leave out documents scoring below this.
    #[serde(default)]
    pub min_score: Option<f32>,
    #[serde(skip)]
    pub filter: Option<Filter>,
}

fn default_k() -> usize {
    10
}

/// The embedding queue and how it's going.
#[derive(Default)]
pub(crate) struct EmbedState {
    queue: Mutex<BTreeSet<DocKey>>,
    notify: tokio::sync::Notify,
    status: Mutex<EmbedStatus>,
    /// Today's (UTC) query-embedding tokens per user.
    spent: Mutex<HashMap<(String, String), u64>>,
    model: Mutex<Option<String>>,
}

#[derive(Default, Clone)]
struct EmbedStatus {
    embedded: u64,
    last_error: Option<String>,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    crate::engine::EngineError::Invalid(message.into()).into()
}

impl HexDBEngine {
    /// Queue a document for embedding if one of its tessellation's vector
    /// indexes embeds source fields and its embedding is missing or stale.
    /// Only the Overseer embeds (it writes the result).
    pub(crate) fn note_for_embedding(&self, tess: &str, doc: &Document) {
        if !self.is_writable() {
            return;
        }
        let indexes = self.indexes.read().unwrap();
        let Some(list) = indexes.by_tessellation.get(tess) else { return };
        if !list.iter().any(|i| i.embeds()) {
            return;
        }
        let doc = crate::sealed::unsealed(doc);
        if list.iter().any(|i| i.needs_embedding(&doc).is_some()) {
            self.embeddings.queue.lock().unwrap().insert(DocKey::new(tess, doc.id));
            self.embeddings.notify.notify_one();
        }
    }

    /// Queue every document of a tessellation's embedding indexes that needs it.
    pub(crate) async fn sweep_embeddings(&self, tess: &str) -> Result<()> {
        if !self.indexes.read().unwrap().by_tessellation.get(tess).is_some_and(|l| l.iter().any(|i| i.embeds())) {
            return Ok(());
        }
        for doc in self.matching_documents_scan(tess, &Filter::all()).await? {
            self.note_for_embedding(tess, &doc);
        }
        Ok(())
    }

    /// Embeddings status for `/status`.
    pub fn embeddings_status(&self) -> Value {
        let status = self.embeddings.status.lock().unwrap().clone();
        json!({
            "model": self.embedding_model(),
            "pending": self.embeddings.queue.lock().unwrap().len(),
            "embedded_since_start": status.embedded,
            "last_error": status.last_error,
        })
    }

    /// After the lattice configuration changed: if the embedding model did,
    /// rebuild the auto-embedding indexes and queue their documents again.
    pub(crate) async fn check_embedding_model(&self) {
        let model = self.embedding_model();
        let changed = {
            let mut last = self.embeddings.model.lock().unwrap();
            let changed = *last != model;
            *last = model.clone();
            changed
        };
        if !changed {
            return;
        }
        let tessellations: Vec<String> = {
            let mut indexes = self.indexes.write().unwrap();
            for list in indexes.by_tessellation.values_mut() {
                for index in list.iter_mut() {
                    index.set_embedding_model(model.clone());
                }
            }
            indexes.by_tessellation.iter().filter(|(_, l)| l.iter().any(|i| i.embeds())).map(|(t, _)| t.clone()).collect()
        };
        if !self.indexes_loaded.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        for tess in tessellations {
            info!("🧭 The embedding model is now {}; rebuilding the vector indexes of '{}'.", model.as_deref().unwrap_or("(none)"), tess);
            self.rebuild_tessellation_indexes(&tess).await;
            if let Err(e) = self.sweep_embeddings(&tess).await {
                warn!("⚠️ Couldn't queue the documents of '{}' for embedding: {:#}", tess, e);
            }
        }
    }

    /// Embed one batch from the queue. Returns false when there was nothing to do.
    async fn embed_batch(&self) -> Result<bool> {
        let keys: Vec<DocKey> = {
            let mut queue = self.embeddings.queue.lock().unwrap();
            let keys: Vec<DocKey> = queue.iter().take(crate::ai::EMBED_BATCH).cloned().collect();
            keys.iter().for_each(|k| {
                queue.remove(k);
            });
            keys
        };
        if keys.is_empty() {
            return Ok(false);
        }
        // What each document needs: (key, seq, doc, index field, text, hash).
        let mut work = Vec::new();
        for key in &keys {
            let (Some(doc), seq) = self.read_latest(key).await? else { continue };
            let indexes = self.indexes.read().unwrap();
            for index in indexes.by_tessellation.get(&key.tessellation).into_iter().flatten() {
                if let Some((text, hash)) = index.needs_embedding(&doc) {
                    work.push((key.clone(), seq, doc.clone(), index.def.fields[0].clone(), text, hash));
                }
            }
        }
        if work.is_empty() {
            return Ok(true);
        }
        let texts: Vec<String> = work.iter().map(|w| w.4.clone()).collect();
        let embedded = match self.ai_embed(&texts, EmbedPurpose::Document, "embeddings").await {
            Ok(embedded) => embedded,
            Err(e) => {
                // Try again later.
                let mut queue = self.embeddings.queue.lock().unwrap();
                keys.into_iter().for_each(|k| {
                    queue.insert(k);
                });
                return Err(e);
            }
        };
        // One write per document, with every embedding it got.
        let mut by_doc: HashMap<DocKey, (u64, Document)> = HashMap::new();
        for ((key, seq, doc, field, _, hash), vector) in work.into_iter().zip(embedded.vectors) {
            let entry = by_doc.entry(key).or_insert((seq, doc));
            let mut json = entry.1.data_json();
            set_path(&mut json, &field, json!({ "vector": vector, "model": embedded.model, "source_hash": hash }));
            entry.1.data = crate::document::infer_fields_from_json(&json);
        }
        let count = by_doc.len() as u64;
        for (key, (seq, doc)) in by_doc {
            let item = BatchItem { key: key.clone(), op: WalOp::Put(doc), expected_seq: Some(seq) };
            match self.commit(vec![item]).await {
                Ok(true) => {}
                // Changed meanwhile: the new version was queued by its own write.
                Ok(false) => debug!("{} changed while it was embedded; it's queued again.", key.id),
                Err(e) => warn!("⚠️ Couldn't store the embedding of {} in '{}': {:#}", key.id, key.tessellation, e),
            }
        }
        let mut status = self.embeddings.status.lock().unwrap();
        status.embedded += count;
        status.last_error = None;
        Ok(true)
    }

    fn note_embedding_error(&self, message: String) {
        let mut status = self.embeddings.status.lock().unwrap();
        if status.last_error.as_deref() != Some(message.as_str()) {
            warn!("⚠️ Embedding documents failed; retrying in a minute: {}", message);
        }
        status.last_error = Some(message);
    }

    /// The `k` documents most similar to the query's vector (or text), best
    /// first, among those its filter matches and the caller may read, each
    /// with `_score`.
    pub async fn similar_documents(&self, tess: &str, query: &SimilarQuery) -> Result<crate::engine::QueryPage> {
        if query.k == 0 || query.k > MAX_K {
            return Err(invalid(format!("k must be 1-{}.", MAX_K)));
        }
        let scope = self.caller_scope(tess, Action::Read);
        let filter = query.filter.clone().unwrap_or_else(Filter::all);
        self.check_scope_fields(tess, &scope, &filter, &[query.field.as_str()])?;
        let filter = self.prepare_filter(tess, &scope.narrow(&filter)).0;
        let index_name = {
            let indexes = self.indexes.read().unwrap();
            let found = indexes
                .by_tessellation
                .get(tess)
                .and_then(|l| l.iter().find(|i| i.def.kind == crate::index::IndexKind::Vector && i.def.fields == [query.field.clone()]));
            match found {
                Some(index) if index.ready => index.def.name.clone(),
                Some(_) => return Err(crate::engine::EngineError::Unavailable(format!("The vector index on '{}' is still being built; try again shortly.", query.field)).into()),
                None => return Err(invalid(format!("'{}' has no vector index on '{}'. Create one (kind \"vector\") to search it by similarity.", tess, query.field))),
            }
        };
        let vector = match (&query.vector, query.text.as_deref().map(str::trim)) {
            (Some(v), None) => v.clone(),
            (None, Some(text)) if !text.is_empty() => self.embed_query(text).await?,
            _ => return Err(invalid("Give either vector (numbers) or text to search by.")),
        };

        let prepared = {
            let indexes = self.indexes.read().unwrap();
            indexes.by_tessellation.get(tess).and_then(|l| l.iter().find(|i| i.def.name == index_name)).and_then(|i| i.prepare_query(vector.clone()))
        };
        let mut documents: Vec<Document> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut scanned = 0;
        let mut want = (query.k * 4).max(50);
        loop {
            let (ranked, held) = {
                let indexes = self.indexes.read().unwrap();
                let index = indexes.by_tessellation.get(tess).and_then(|l| l.iter().find(|i| i.def.name == index_name));
                let Some(index) = index else { break };
                let ranked = index.nearest(vector.clone(), want, want).ok_or_else(|| invalid("The vector doesn't fit the index (wrong number of dimensions, or all zeros)."))?;
                (ranked, index.vector_count())
            };
            let mut below = false;
            for (id, score) in &ranked {
                if !seen.insert(*id) {
                    continue;
                }
                if query.min_score.is_some_and(|m| *score < m) {
                    below = true;
                    break;
                }
                scanned += 1;
                let Some(doc) = self.read_latest(&DocKey::new(tess, *id)).await?.0 else { continue };
                if !filter.matches(&doc) {
                    continue;
                }
                // A quantized index ranks approximately; the document's own vector gives the exact score.
                let exact = prepared.as_ref().and_then(|q| {
                    let indexes = self.indexes.read().unwrap();
                    indexes.by_tessellation.get(tess).and_then(|l| l.iter().find(|i| i.def.name == index_name)).and_then(|i| i.exact_score(q, &doc))
                });
                let score = exact.unwrap_or(*score);
                let mut doc = scope.mask(doc);
                doc.data.insert("_score".into(), FieldValue::Float((score as f64 * 1e6).round() / 1e6));
                documents.push(doc);
                if documents.len() == query.k {
                    break;
                }
            }
            if documents.len() == query.k || below || ranked.len() < want || want >= held {
                break;
            }
            want = (want * 4).min(held);
        }
        // Exact scores may reorder what an approximate (quantized) ranking found.
        let key = |d: &Document| match d.data.get("_score") {
            Some(FieldValue::Float(f)) => *f,
            _ => f64::MIN,
        };
        documents.sort_by(|a, b| key(b).total_cmp(&key(a)));
        // How many documents were read would reveal how many the caller can't see.
        let scanned = if scope.is_full() { scanned } else { documents.len() };
        Ok(crate::engine::QueryPage { documents, total: None, next: None, indexes: vec![index_name], scanned })
    }

    /// Embed a query text as the calling user, within their daily allowance.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let user = crate::access::caller().map(|p| p.login.clone()).unwrap_or_else(|| "system".into());
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let cap = self.config_value("ai.embeddings.user_daily_tokens").as_u64().unwrap_or(0);
        let key = (user, day.clone());
        if cap > 0 && self.embeddings.spent.lock().unwrap().get(&key).copied().unwrap_or(0) >= cap {
            let (_, seconds) = crate::ai::next_month_utc();
            return Err(crate::engine::EngineError::RateLimited(
                format!("Your similarity searches by text used today's allowance ({} tokens, ai.embeddings.user_daily_tokens). Search by vector, or try again tomorrow (UTC).", cap),
                seconds.min(86_400),
            )
            .into());
        }
        let text: String = text.chars().take(8_000).collect();
        let embedded = self.ai_embed(&[text], EmbedPurpose::Query, "embeddings.query").await?;
        {
            let mut spent = self.embeddings.spent.lock().unwrap();
            spent.retain(|(_, d), _| *d == day);
            *spent.entry(key).or_default() += embedded.tokens;
        }
        embedded.vectors.into_iter().next().ok_or_else(|| anyhow::anyhow!("the embeddings API returned no vector"))
    }
}

fn set_path(json: &mut Value, path: &str, value: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = json;
    for (i, part) in parts.iter().enumerate() {
        let Value::Object(map) = current else { return };
        if i == parts.len() - 1 {
            map.insert(part.to_string(), value);
            return;
        }
        current = map.entry(part.to_string()).or_insert_with(|| Value::Object(Default::default()));
    }
}

/// Embed queued documents while this hex is the Overseer.
pub fn spawn_embedding_task(engine: Arc<HexDBEngine>, shutdown_rx: watch::Receiver<()>) {
    crate::tasks::supervise("embeddings", shutdown_rx.clone(), move || {
        let (engine, mut shutdown_rx) = (engine.clone(), shutdown_rx.clone());
        async move {
            let mut swept = false;
            loop {
                if engine.is_writable() {
                    if !swept {
                        // A new Overseer queues what the last one didn't finish.
                        let tessellations: Vec<String> = engine.indexes.read().unwrap().by_tessellation.keys().cloned().collect();
                        for tess in tessellations {
                            if let Err(e) = engine.sweep_embeddings(&tess).await {
                                warn!("⚠️ Couldn't queue the documents of '{}' for embedding: {:#}", tess, e);
                            }
                        }
                        swept = true;
                    }
                    match engine.embed_batch().await {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            engine.note_embedding_error(format!("{:#}", e));
                            tokio::select! {
                                _ = shutdown_rx.changed() => break,
                                _ = tokio::time::sleep(Duration::from_secs(60)) => continue,
                            }
                        }
                    }
                } else {
                    swept = false;
                    engine.embeddings.queue.lock().unwrap().clear();
                }
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    _ = engine.embeddings.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
            }
        }
    });
}
