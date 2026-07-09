//! Build-time resolver for the embedded ONNX model weights (ADR 0004).
//!
//! The observable behavior is unchanged: the release binary still embeds the
//! model via `include_bytes!`, so `cargo install embedmind` works with no
//! network at runtime and nothing leaves the machine (CLAUDE.md, ADR 0004).
//! What changed is what ships *inside the published source package*: the
//! 22 MB `model_quantized.onnx` is excluded from the crate (`Cargo.toml`
//! `exclude`) so the package stays under the crates.io 10 MiB ceiling
//! (docs/RELEASING.md). The much smaller tokenizer/vocab stay embedded in the
//! crate source as before.
//!
//! At build time this script guarantees the model file exists on disk and
//! exports its absolute path to the compiler via `EMBEDMIND_MODEL_ONNX`, which
//! `embed.rs` feeds to `include_bytes!`. Resolution order:
//!
//! 1. **Checkout / CI build** — the asset is present in the crate tree
//!    (`assets/all-MiniLM-L6-v2/onnx/model_quantized.onnx`). Use it directly.
//! 2. **Build from the published crate** — the asset was excluded, so download
//!    it once into a local cache and use the cached copy.
//!
//! Either way the bytes are verified against a pinned SHA-256 before use, so a
//! truncated download, a mirror swap, or a tampered cache can never be linked
//! into the binary. The cache lives under `CARGO_HOME` (or `OUT_DIR` as a
//! fallback), keyed by the checksum, so it is shared across builds and never
//! re-downloaded once populated.

// The SHA-256 core (FIPS 180-4) is inherently index-driven over the message
// schedule and working variables; rewriting it to iterator form would obscure
// the standard, not clarify it.
#![allow(clippy::needless_range_loop)]

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// SHA-256 of `all-MiniLM-L6-v2` int8 `model_quantized.onnx` (22_972_370
/// bytes). Identical to the copy on Hugging Face's `Xenova/all-MiniLM-L6-v2`
/// export — verified byte-for-byte — so a download and a checkout produce the
/// same binary. Bump both this and [`MODEL_URL`] together if the model is ever
/// re-quantized (would also require `embedmind reembed`, ADR 0004).
const MODEL_SHA256: &str = "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1";

/// Expected size in bytes — a cheap first gate before hashing.
const MODEL_LEN: u64 = 22_972_370;

/// Canonical download source for builds from the published crate (where the
/// asset was `exclude`d). Pinned to a content-addressed export; the checksum
/// above is the real trust anchor regardless of the URL.
const MODEL_URL: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx";

/// Path of the in-tree asset, relative to the crate root, when present.
const IN_TREE_REL: &str = "assets/all-MiniLM-L6-v2/onnx/model_quantized.onnx";

/// Cargo prints a non-empty `Err` from a build script's `main` and fails the
/// build — the sanctioned way to abort without a lint-forbidden `panic!`.
fn main() -> Result<(), String> {
    let out = resolve_model().map_err(|e| {
        format!(
            "could not obtain the ONNX model weights: {e}\n\
             Expected the file in-tree at `{IN_TREE_REL}` (dev/CI checkout), or a \
             network reachable to download it from `{MODEL_URL}` (build from the \
             published crate). Set EMBEDMIND_MODEL_ONNX to a pre-fetched copy to \
             build fully offline."
        )
    })?;

    // include_bytes! wants a path literal; hand it an absolute one via env.
    println!("cargo:rustc-env=EMBEDMIND_MODEL_ONNX={}", out.display());
    Ok(())
}

/// Returns the absolute path to a checksum-verified model file, fetching it if
/// necessary. Never returns an unverified path.
fn resolve_model() -> Result<PathBuf, String> {
    // Escape hatch for fully offline / air-gapped builds and vendoring: point
    // at a pre-fetched copy. Still checksum-verified — no bypass of integrity.
    if let Ok(explicit) = env::var("EMBEDMIND_MODEL_ONNX") {
        let path = PathBuf::from(&explicit);
        println!("cargo:rerun-if-changed={explicit}");
        verify(&path)?;
        return Ok(path);
    }
    println!("cargo:rerun-if-env-changed=EMBEDMIND_MODEL_ONNX");

    // 1. In-tree asset (dev/CI checkout): use it directly, no network.
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR unset".to_string())?,
    );
    let in_tree = manifest_dir.join(IN_TREE_REL);
    println!("cargo:rerun-if-changed={}", in_tree.display());
    if in_tree.is_file() {
        verify(&in_tree)?;
        return Ok(in_tree);
    }

    // 2. Build from the published crate: the asset was excluded. Fetch once
    //    into a checksum-keyed cache and reuse it forever after.
    let cached = cache_path()?;
    if cached.is_file() && verify(&cached).is_ok() {
        return Ok(cached);
    }
    download_to(&cached)?;
    verify(&cached)?;
    Ok(cached)
}

/// Location of the shared, checksum-keyed cache entry. Prefers `CARGO_HOME`
/// (persists across builds and target dirs); falls back to `OUT_DIR` (always
/// writable, but per-build). Keyed by the checksum so a re-quantized model
/// lands in a fresh slot instead of colliding.
fn cache_path() -> Result<PathBuf, String> {
    let dir = if let Some(cargo_home) = cargo_home() {
        cargo_home.join("embedmind").join("models")
    } else {
        PathBuf::from(env::var("OUT_DIR").map_err(|_| "OUT_DIR unset".to_string())?)
            .join("embedmind-models")
    };
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create cache dir {dir:?}: {e}"))?;
    Ok(dir.join(format!("all-MiniLM-L6-v2-{}.onnx", &MODEL_SHA256[..16])))
}

fn cargo_home() -> Option<PathBuf> {
    if let Ok(h) = env::var("CARGO_HOME") {
        return Some(PathBuf::from(h));
    }
    // Default: ~/.cargo. HOME on unix, USERPROFILE on Windows.
    let home = env::var("HOME").or_else(|_| env::var("USERPROFILE")).ok()?;
    Some(PathBuf::from(home).join(".cargo"))
}

/// Verifies size then SHA-256 of `path` against the pinned constants.
fn verify(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot stat {path:?}: {e}"))?;
    if meta.len() != MODEL_LEN {
        return Err(format!(
            "{path:?} is {} bytes, expected {MODEL_LEN} — refusing to embed a truncated model",
            meta.len()
        ));
    }
    let mut file = fs::File::open(path).map_err(|e| format!("cannot open {path:?}: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read error on {path:?}: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hasher.hex();
    if got != MODEL_SHA256 {
        return Err(format!(
            "{path:?} SHA-256 {got} != expected {MODEL_SHA256} — refusing to embed a \
             model with an unexpected checksum"
        ));
    }
    Ok(())
}

/// Downloads [`MODEL_URL`] into `dest` atomically (temp file + rename) so a
/// killed build never leaves a half-written cache entry that later passes the
/// existence check but fails verification. Uses the `curl` CLI, which is
/// present on all three CI platforms and on any machine that could `cargo
/// install` this crate; keeping a heavyweight HTTP crate out of build deps
/// keeps the source package small (the whole point of this change).
fn download_to(dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("part");
    let status = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
            "--output",
        ])
        .arg(&tmp)
        .arg(MODEL_URL)
        .status()
        .map_err(|e| {
            format!(
                "failed to run `curl` to download the model: {e}. Install curl, or set \
                 EMBEDMIND_MODEL_ONNX to a pre-fetched copy."
            )
        })?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        return Err(format!("curl exited with {status} downloading {MODEL_URL}"));
    }
    fs::rename(&tmp, dest).map_err(|e| format!("cannot move downloaded model into place: {e}"))?;
    Ok(())
}

// --- Minimal SHA-256 (no build-dependency, keeps the source package small) ---
//
// A dependency here would be a *build* dep of a published crate, pulled by
// every downstream `cargo install`. A ~90-line self-contained implementation
// is cheaper than that and has no supply-chain surface. Standard FIPS 180-4.

struct Sha256 {
    state: [u32; 8],
    len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            len: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut h = self.state;
        for i in 0..64 {
            let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
            let ch = (h[4] & h[5]) ^ ((!h[4]) & h[6]);
            let t1 = h[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
            let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
            let t2 = s0.wrapping_add(maj);
            h[7] = h[6];
            h[6] = h[5];
            h[5] = h[4];
            h[4] = h[3].wrapping_add(t1);
            h[3] = h[2];
            h[2] = h[1];
            h[1] = h[0];
            h[0] = t1.wrapping_add(t2);
        }
        for i in 0..8 {
            self.state[i] = self.state[i].wrapping_add(h[i]);
        }
    }

    fn hex(mut self) -> String {
        let bit_len = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        self.update(&bit_len.to_be_bytes());
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }
}
