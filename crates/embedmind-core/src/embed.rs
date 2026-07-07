//! Embedding pipeline behind the `Embedder` trait (`docs/adr/0004`).
//!
//! Default implementation (M1 item 1.3): all-MiniLM-L6-v2 int8 via ONNX
//! Runtime, CPU-only, tokenizer embedded in the binary. The `ort` and
//! `tokenizers` dependencies are added here — and only here — when this module
//! is implemented, keeping the rest of the engine dependency-light.
