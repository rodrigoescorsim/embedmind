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

/// Turns text into a fixed-size embedding vector. Implementations are
/// plugable (BYO model) — the engine only depends on this trait, never on
/// `ort`/`tokenizers` directly outside this module (`docs/adr/0004`).
pub trait Embedder: Send + Sync {
    /// Embeds one piece of text. The returned vector always has
    /// [`Embedder::dims`] elements; callers L2-normalize separately
    /// (`index::normalize`) rather than trusting the model's own output
    /// scale.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// This embedder's stable identifier, written to the store header.
    fn id(&self) -> ModelId;

    /// Output dimensionality, written to the store header.
    fn dims(&self) -> u16;
}

/// Model assets, embedded in the binary (`docs/adr/0004`) so installation
/// stays a single command with no post-install download step. ~23 MB
/// (int8-quantized ONNX weights) + tokenizer, well inside the < 40 MB binary
/// budget (DESIGN.md §1).
mod assets {
    pub const MODEL_ONNX: &[u8] =
        include_bytes!("../assets/all-MiniLM-L6-v2/onnx/model_quantized.onnx");
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
}

impl OnnxEmbedder {
    /// Loads the embedded MiniLM model and tokenizer. This runs ONNX Runtime
    /// session initialization (graph load + optimization), so callers should
    /// build one `OnnxEmbedder` and reuse it — not construct one per call.
    pub fn load() -> Result<Self> {
        let mut tokenizer = Tokenizer::from_bytes(assets::TOKENIZER_JSON)
            .map_err(|e| Error::Internal(leak_reason("tokenizer load failed", &e)))?;
        // The bundled tokenizer.json pads/truncates to 128 tokens by default
        // (Xenova's export); override explicitly so behavior does not depend
        // on hidden config and short memories are not padded to 128 for no
        // reason (`docs/DESIGN.md` §6 chunking is a separate, later concern
        // for text > 512 tokens — this just controls this call's batch).
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| Error::Internal(leak_reason("tokenizer truncation config failed", &e)))?;
        tokenizer.with_padding(None);

        let session = Session::builder()
            .map_err(|e| Error::Internal(leak_reason("onnx session builder failed", &e)))?
            .commit_from_memory(assets::MODEL_ONNX)
            .map_err(|e| Error::Internal(leak_reason("onnx model load failed", &e)))?;

        Ok(OnnxEmbedder {
            tokenizer,
            session: Mutex::new(session),
        })
    }
}

impl Embedder for OnnxEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| Error::Internal(leak_reason("tokenization failed", &e)))?;
        let ids: Vec<i64> = encoding.get_ids().iter().map(|&x| i64::from(x)).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| i64::from(x))
            .collect();
        let type_ids: Vec<i64> = encoding
            .get_type_ids()
            .iter()
            .map(|&x| i64::from(x))
            .collect();
        let seq_len = ids.len();
        if seq_len == 0 {
            return Ok(vec![0.0; usize::from(DEFAULT_MODEL_DIMS)]);
        }

        let input_ids = Tensor::from_array(([1, seq_len], ids))
            .map_err(|e| Error::Internal(leak_reason("input_ids tensor failed", &e)))?;
        let attention_mask = Tensor::from_array(([1, seq_len], mask.clone()))
            .map_err(|e| Error::Internal(leak_reason("attention_mask tensor failed", &e)))?;
        let token_type_ids = Tensor::from_array(([1, seq_len], type_ids))
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

        // Mean-pool token embeddings, weighted by the attention mask (the
        // standard sentence-transformers pooling for this model family —
        // MiniLM's ONNX export has no built-in pooler output).
        let mut pooled = vec![0.0f32; dims];
        let mut mask_sum = 0.0f32;
        for (t, &m) in mask.iter().enumerate() {
            let m = m as f32;
            mask_sum += m;
            let row = &data[t * dims..(t + 1) * dims];
            for (p, &v) in pooled.iter_mut().zip(row) {
                *p += v * m;
            }
        }
        if mask_sum > 0.0 {
            for p in &mut pooled {
                *p /= mask_sum;
            }
        }
        Ok(pooled)
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
}
