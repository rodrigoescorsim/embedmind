//! Embedding pipeline behind the [`Embedder`] trait (`docs/adr/0004`).
//!
//! Default implementation (M1 item 1.3): all-MiniLM-L6-v2, quantized int8,
//! via ONNX Runtime (`ort`, CPU-only), tokenizer + vocab + model weights
//! embedded in the binary with `include_bytes!` — no network call, no
//! separate download step, consistent with "nothing leaves the machine"
//! (CLAUDE.md). Swapping models is a config change behind this trait, never
//! a code change in callers (`docs/adr/0004`); the store's header records
//! `model_id` + `dims` so mixing embeddings from different models in one
//! file is caught, not silently corrupted (`docs/FORMAT.md` §6).
//!
//! **Chunking (DESIGN §6):** the model sees at most 512 positions, so text
//! longer than one window is split into overlapping token windows
//! ([`WINDOW_TOKENS`] content tokens, [`OVERLAP_TOKENS`] overlap) and
//! embedded chunk by chunk ([`Embedder::embed_chunks`]). Chunks exist only
//! in the vector index — the memory record stays whole; each chunk becomes
//! one more index entry pointing at the same record, and recall dedupes by
//! record id (returns the memory, never a chunk).

use std::sync::Mutex;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::error::{Error, Result};

/// Identifies which model produced an embedding. Stored verbatim in the
/// store header (`embedding_model_id`, `docs/FORMAT.md` §4); changing models
/// requires `embedmind reembed` into a new file (`docs/adr/0004`).
pub type ModelId = &'static str;

/// The default embedded model's identifier, written to fresh stores'
/// headers.
pub const DEFAULT_MODEL_ID: ModelId = "all-MiniLM-L6-v2-int8";

/// The default embedded model's output dimensionality.
pub const DEFAULT_MODEL_DIMS: u16 = 384;

/// Content tokens per chunk window (DESIGN §6): the model's 512-position
/// limit minus the `[CLS]`/`[SEP]` specials added per window.
pub const WINDOW_TOKENS: usize = 510;

/// Tokens shared between consecutive chunk windows (DESIGN §6), so a
/// sentence straddling a boundary is fully seen by at least one chunk.
pub const OVERLAP_TOKENS: usize = 64;

/// Hard cap on chunks embedded per memory. At 510-token windows this covers
/// ~57k tokens (~230 KB of English text) — far beyond any sane agent memory.
/// Past it, `remember` fails with a typed error rather than silently
/// indexing a truncated view or spending minutes embedding one record
/// (`MAX_RECORD_LEN` alone would allow ~8M tokens).
pub const MAX_CHUNKS_PER_MEMORY: usize = 128;

/// Turns text into fixed-size embedding vectors. Implementations are
/// plugable (BYO model) — the engine only depends on this trait, never on
/// `ort`/`tokenizers` directly outside this module (`docs/adr/0004`).
pub trait Embedder: Send + Sync {
    /// Embeds one piece of text as a single vector. Text longer than the
    /// model's window is truncated to it — right for *queries* (short by
    /// nature) and for models without a window limit; stored content goes
    /// through [`Embedder::embed_chunks`] instead. The returned vector
    /// always has [`Embedder::dims`] elements; callers L2-normalize
    /// separately (`index::normalize`) rather than trusting the model's own
    /// output scale.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embeds one piece of text as one vector **per chunk window**
    /// (DESIGN §6). Always returns at least one vector; short text yields
    /// exactly one, identical to [`Embedder::embed`]. The default forwards
    /// to `embed` — implementations with an input-length limit should
    /// override with real windowing.
    fn embed_chunks(&self, text: &str) -> Result<Vec<Vec<f32>>> {
        Ok(vec![self.embed(text)?])
    }

    /// This embedder's stable identifier, written to the store header.
    fn id(&self) -> ModelId;

    /// Output dimensionality, written to the store header.
    fn dims(&self) -> u16;
}

/// Model assets, embedded in the binary (`docs/adr/0004`) so installation
/// stays a single command with no post-install download step. ~23 MB
/// (int8-quantized ONNX weights) + tokenizer, well inside the < 40 MB binary
/// budget (DESIGN.md §1).
///
/// The tokenizer is embedded straight from the crate tree. The ONNX weights
/// come from a path resolved by `build.rs` (`EMBEDMIND_MODEL_ONNX`): the
/// in-tree asset in a dev/CI checkout, or a checksum-verified download when
/// building from the published crate — the 22 MB file is `exclude`d from the
/// package to stay under the crates.io 10 MiB ceiling (docs/RELEASING.md).
/// Either way the bytes are identical and `include_bytes!` embeds them at
/// compile time, so runtime behavior is unchanged.
mod assets {
    pub const MODEL_ONNX: &[u8] = include_bytes!(env!("EMBEDMIND_MODEL_ONNX"));
    pub const TOKENIZER_JSON: &[u8] = include_bytes!("../assets/all-MiniLM-L6-v2/tokenizer.json");
}

/// The default embedder: all-MiniLM-L6-v2 int8 via ONNX Runtime, CPU-only
/// (`docs/adr/0004`). `Session::run` takes `&mut self` in `ort`, so the
/// session sits behind a [`Mutex`] — `remember` is not a hot loop (DESIGN.md
/// latency budget is dominated by embedding compute itself, not lock
/// contention on a single-writer store).
pub struct OnnxEmbedder {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
    cls_id: i64,
    sep_id: i64,
}

impl OnnxEmbedder {
    /// Loads the embedded MiniLM model and tokenizer. This runs ONNX Runtime
    /// session initialization (graph load + optimization), so callers should
    /// build one `OnnxEmbedder` and reuse it — not construct one per call.
    pub fn load() -> Result<Self> {
        let mut tokenizer = Tokenizer::from_bytes(assets::TOKENIZER_JSON)
            .map_err(|e| Error::Internal(leak_reason("tokenizer load failed", &e)))?;
        // The bundled tokenizer.json pads/truncates to 128 tokens by default
        // (Xenova's export). Both are disabled explicitly: windowing over
        // the full token stream (DESIGN §6) is this module's only source of
        // truncation, never hidden tokenizer config.
        tokenizer
            .with_truncation(None)
            .map_err(|e| Error::Internal(leak_reason("tokenizer truncation config failed", &e)))?;
        tokenizer.with_padding(None);
        let cls_id = tokenizer
            .token_to_id("[CLS]")
            .ok_or(Error::Internal("tokenizer vocab missing [CLS]"))?;
        let sep_id = tokenizer
            .token_to_id("[SEP]")
            .ok_or(Error::Internal("tokenizer vocab missing [SEP]"))?;

        let session = Session::builder()
            .map_err(|e| Error::Internal(leak_reason("onnx session builder failed", &e)))?
            .commit_from_memory(assets::MODEL_ONNX)
            .map_err(|e| Error::Internal(leak_reason("onnx model load failed", &e)))?;

        Ok(OnnxEmbedder {
            tokenizer,
            session: Mutex::new(session),
            cls_id: i64::from(cls_id),
            sep_id: i64::from(sep_id),
        })
    }

    /// Tokenizes `text` into content token ids (no specials — windows add
    /// their own `[CLS]`/`[SEP]`).
    fn content_ids(&self, text: &str) -> Result<Vec<i64>> {
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| Error::Internal(leak_reason("tokenization failed", &e)))?;
        Ok(encoding.get_ids().iter().map(|&x| i64::from(x)).collect())
    }

    /// Runs the model on `[CLS] + window + [SEP]` and mean-pools the token
    /// embeddings (the standard sentence-transformers pooling for this model
    /// family — MiniLM's ONNX export has no built-in pooler output). With no
    /// padding the attention mask is all ones, so plain mean over the
    /// sequence is exactly the mask-weighted mean.
    fn embed_window(&self, window: &[i64]) -> Result<Vec<f32>> {
        debug_assert!(window.len() <= WINDOW_TOKENS);
        let mut ids = Vec::with_capacity(window.len() + 2);
        ids.push(self.cls_id);
        ids.extend_from_slice(window);
        ids.push(self.sep_id);
        let seq_len = ids.len();

        let input_ids = Tensor::from_array(([1, seq_len], ids))
            .map_err(|e| Error::Internal(leak_reason("input_ids tensor failed", &e)))?;
        let attention_mask = Tensor::from_array(([1, seq_len], vec![1i64; seq_len]))
            .map_err(|e| Error::Internal(leak_reason("attention_mask tensor failed", &e)))?;
        let token_type_ids = Tensor::from_array(([1, seq_len], vec![0i64; seq_len]))
            .map_err(|e| Error::Internal(leak_reason("token_type_ids tensor failed", &e)))?;

        let mut session = self
            .session
            .lock()
            .map_err(|_| Error::Internal("onnx session lock poisoned"))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
                "token_type_ids" => token_type_ids,
            ])
            .map_err(|e| Error::Internal(leak_reason("onnx inference failed", &e)))?;

        let last_hidden_state = outputs
            .get("last_hidden_state")
            .ok_or(Error::Internal("onnx output missing last_hidden_state"))?;
        let (shape, data) = last_hidden_state
            .try_extract_tensor::<f32>()
            .map_err(|e| Error::Internal(leak_reason("output extraction failed", &e)))?;
        let dims = usize::from(self.dims());
        if shape.as_ref() != [1, seq_len as i64, dims as i64] {
            return Err(Error::Internal("unexpected onnx output shape"));
        }

        let mut pooled = vec![0.0f32; dims];
        for t in 0..seq_len {
            let row = &data[t * dims..(t + 1) * dims];
            for (p, &v) in pooled.iter_mut().zip(row) {
                *p += v;
            }
        }
        for p in &mut pooled {
            *p /= seq_len as f32;
        }
        Ok(pooled)
    }
}

/// Splits `ids` into windows of at most [`WINDOW_TOKENS`], consecutive
/// windows sharing [`OVERLAP_TOKENS`]. Empty input yields no windows. Pure —
/// unit-testable without the model.
fn chunk_windows(ids: &[i64]) -> Vec<&[i64]> {
    let stride = WINDOW_TOKENS - OVERLAP_TOKENS;
    let mut out = Vec::new();
    let mut start = 0;
    while start < ids.len() {
        let end = (start + WINDOW_TOKENS).min(ids.len());
        out.push(&ids[start..end]);
        if end == ids.len() {
            break;
        }
        start += stride;
    }
    out
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.content_ids(text)?;
        if ids.is_empty() {
            return Ok(vec![0.0; usize::from(DEFAULT_MODEL_DIMS)]);
        }
        // Queries are short by nature; anything past the first window is
        // truncated (stored content goes through `embed_chunks`).
        let window = &ids[..ids.len().min(WINDOW_TOKENS)];
        self.embed_window(window)
    }

    fn embed_chunks(&self, text: &str) -> Result<Vec<Vec<f32>>> {
        let ids = self.content_ids(text)?;
        if ids.is_empty() {
            return Ok(vec![vec![0.0; usize::from(DEFAULT_MODEL_DIMS)]]);
        }
        let windows = chunk_windows(&ids);
        if windows.len() > MAX_CHUNKS_PER_MEMORY {
            return Err(Error::InvalidArgument(
                "memory too long to embed (exceeds MAX_CHUNKS_PER_MEMORY windows); split it into smaller memories",
            ));
        }
        windows.iter().map(|w| self.embed_window(w)).collect()
    }

    fn id(&self) -> ModelId {
        DEFAULT_MODEL_ID
    }

    fn dims(&self) -> u16 {
        DEFAULT_MODEL_DIMS
    }
}

/// Formats an opaque third-party error into a `'static` reason string for
/// [`Error::Internal`], which only carries `&'static str` (the engine's
/// error type is deliberately allocation-free — DESIGN.md's "typed errors,
/// never unwrap/panic" rule doesn't extend to leaking one string per failure
/// class here, since these are all "should never happen in a working
/// install" paths, not routine control flow).
fn leak_reason(context: &str, err: &impl std::fmt::Display) -> &'static str {
    // Deliberately leaked: these are rare, non-hot-path failures (model/tokenizer
    // load, malformed ONNX output) and Error::Internal's contract is a
    // `&'static str`. Leaking a handful of short diagnostic strings over a
    // process lifetime is not a practical memory concern.
    Box::leak(format!("{context}: {err}").into_boxed_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn onnx_embedder_produces_normalizable_dims_and_is_deterministic() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let a = embedder
            .embed("the founder prefers explicit errors")
            .unwrap();
        assert_eq!(a.len(), usize::from(DEFAULT_MODEL_DIMS));
        assert!(a.iter().any(|&x| x != 0.0));

        let b = embedder
            .embed("the founder prefers explicit errors")
            .unwrap();
        assert_eq!(a, b, "same input must yield the same embedding");
    }

    #[test]
    fn onnx_embedder_similar_text_scores_higher_than_unrelated() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let mut a = embedder.embed("the cat sat on the mat").unwrap();
        let mut b = embedder.embed("a cat was sitting on a mat").unwrap();
        let mut c = embedder.embed("quarterly tax filing deadline").unwrap();
        crate::index::normalize(&mut a);
        crate::index::normalize(&mut b);
        crate::index::normalize(&mut c);
        let dot = |x: &[f32], y: &[f32]| -> f32 { x.iter().zip(y).map(|(p, q)| p * q).sum() };
        let sim_related = dot(&a, &b);
        let sim_unrelated = dot(&a, &c);
        assert!(
            sim_related > sim_unrelated,
            "related sentences ({sim_related}) should score above unrelated ({sim_unrelated})"
        );
    }

    #[test]
    fn onnx_embedder_handles_empty_string() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let v = embedder.embed("").unwrap();
        assert_eq!(v.len(), usize::from(DEFAULT_MODEL_DIMS));
    }

    #[test]
    fn model_id_and_dims_are_stable() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        assert_eq!(embedder.id(), DEFAULT_MODEL_ID);
        assert_eq!(embedder.dims(), DEFAULT_MODEL_DIMS);
    }

    #[test]
    fn chunk_windows_splits_with_overlap() {
        assert!(chunk_windows(&[]).is_empty());

        let short: Vec<i64> = (0..WINDOW_TOKENS as i64).collect();
        let w = chunk_windows(&short);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0], short.as_slice());

        let long: Vec<i64> = (0..(WINDOW_TOKENS as i64) + 1).collect();
        let w = chunk_windows(&long);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].len(), WINDOW_TOKENS);
        // Second window starts one stride in, so consecutive windows share
        // exactly OVERLAP_TOKENS tokens.
        let stride = WINDOW_TOKENS - OVERLAP_TOKENS;
        assert_eq!(w[1][0], long[stride]);
        assert_eq!(&w[0][stride..], &w[1][..OVERLAP_TOKENS]);

        // Every token appears in at least one window, in order.
        let big: Vec<i64> = (0..2000).collect();
        let w = chunk_windows(&big);
        let mut covered = std::collections::BTreeSet::new();
        for win in &w {
            assert!(win.len() <= WINDOW_TOKENS);
            covered.extend(win.iter().copied());
        }
        assert_eq!(covered.len(), big.len());
    }

    #[test]
    fn embed_chunks_short_text_matches_embed() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let chunks = embedder.embed_chunks("a short memory").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], embedder.embed("a short memory").unwrap());
    }

    #[test]
    fn embed_chunks_long_text_produces_multiple_chunks() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        // ~700 single-token words: past one 510-token window, so two chunks.
        let long = "cat ".repeat(700);
        let chunks = embedder.embed_chunks(&long).unwrap();
        assert_eq!(chunks.len(), 2);
        for c in &chunks {
            assert_eq!(c.len(), usize::from(DEFAULT_MODEL_DIMS));
            assert!(c.iter().any(|&x| x != 0.0));
        }
    }

    #[test]
    fn embed_chunks_empty_text_yields_one_zero_vector() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let chunks = embedder.embed_chunks("").unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn embed_chunks_rejects_absurdly_long_text_before_any_inference() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        // > MAX_CHUNKS_PER_MEMORY windows of content tokens; the typed error
        // fires before any model run, so this is tokenization-only fast.
        let stride = WINDOW_TOKENS - OVERLAP_TOKENS;
        let tokens_over_cap = WINDOW_TOKENS + stride * MAX_CHUNKS_PER_MEMORY;
        let text = "cat ".repeat(tokens_over_cap);
        match embedder.embed_chunks(&text) {
            Err(Error::InvalidArgument(_)) => {}
            Err(e) => panic!("expected InvalidArgument, got {e}"),
            Ok(_) => panic!("expected an error for over-cap text"),
        }
    }

    #[test]
    fn embed_truncates_long_text_instead_of_failing() {
        let embedder = OnnxEmbedder::load().expect("model assets must load");
        let long = "cat ".repeat(700);
        let v = embedder.embed(&long).unwrap();
        assert_eq!(v.len(), usize::from(DEFAULT_MODEL_DIMS));
        // Truncated to the first window: identical to embedding just that
        // window's worth of text.
        let first_window = "cat ".repeat(WINDOW_TOKENS);
        assert_eq!(v, embedder.embed(&first_window).unwrap());
    }
}
