//! Build-time fetch + compile of the `sqlite-vec` `vec0` extension, only when
//! `--features compare-sqlite-vec` is enabled (`docs/BENCHMARKS.md` §1).
//!
//! The `sqlite-vec` crate on crates.io (0.1.10-alpha.4, the only version
//! published) ships a `sqlite-vec.c` that `#include`s sibling files
//! (`sqlite-vec-diskann.c`, `sqlite-vec-rescore.c`) the published package does
//! not contain, so depending on that crate directly fails to build on every
//! platform. Instead this script fetches the upstream `sqlite-vec.c` straight
//! from its pinned source commit and verifies it against a pinned SHA-256
//! before compiling — the same download-and-verify pattern
//! `crates/embedmind-core/build.rs` uses for the ONNX model, so a truncated
//! download or a tampered mirror can never be linked in. `sqlite-vec.h` is not
//! fetched (it isn't published at that commit — the crates.io package
//! generates it from a template); it is reproduced verbatim below since it is
//! a ~40-line, logic-free public header.
//!
//! The DiskANN and rescore experimental sub-modules (the ones with the
//! missing includes) are compiled out via `SQLITE_VEC_ENABLE_DISKANN=0` /
//! `SQLITE_VEC_ENABLE_RESCORE=0`; the harness only needs the default
//! brute-force `vec0` virtual table (`competitors.rs`'s pinned note already
//! says "brute-force KNN", sqlite-vec's own recommended small-scale path).
//!
//! This only runs behind `compare-sqlite-vec`; the default harness build (and
//! the CI regression guard, BENCHMARKS.md §5) never touches the network.

#[cfg(feature = "compare-sqlite-vec")]
use std::env;
#[cfg(feature = "compare-sqlite-vec")]
use std::path::PathBuf;

/// Cargo prints a non-empty `Err` from a build script's `main` and fails the
/// build — the sanctioned way to abort without a lint-forbidden `panic!`
/// (same pattern as `crates/embedmind-core/build.rs`).
fn main() -> Result<(), String> {
    #[cfg(feature = "compare-sqlite-vec")]
    fetch_and_compile()?;
    Ok(())
}

// The whole fetch-verify-compile pipeline (including the `cc` build-dependency
// call) is gated on the feature, not just the `main` dispatch above: `cc` is
// only present as a build-dependency when `compare-sqlite-vec` is on (it is
// `dep:cc` in Cargo.toml's optional deps), so an unconditional call into it
// would fail name resolution when the feature — and thus the dependency — is
// off.
#[cfg(feature = "compare-sqlite-vec")]
fn fetch_and_compile() -> Result<(), String> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|_| "OUT_DIR unset".to_string())?);
    let c_path = out_dir.join("sqlite-vec.c");

    if !(c_path.is_file() && verify(&c_path)) {
        download_to(&c_path)?;
        if !verify(&c_path) {
            return Err(format!(
                "downloaded sqlite-vec.c SHA-256 mismatch — refusing to compile an \
                 unexpected source (expected {SQLITE_VEC_C_SHA256}, from {SQLITE_VEC_C_URL})"
            ));
        }
    }

    let h_path = out_dir.join("sqlite-vec.h");
    std::fs::write(&h_path, SQLITE_VEC_H)
        .map_err(|e| format!("cannot write sqlite-vec.h stub: {e}"))?;

    // `libsqlite3-sys` (a direct dependency under this feature, `links =
    // "sqlite3"`) exports its include dir this way — no hardcoded registry
    // path (docs/BENCHMARKS.md §1 methodology note in Cargo.toml).
    let sqlite_include = env::var("DEP_SQLITE3_INCLUDE").map_err(|_| {
        "DEP_SQLITE3_INCLUDE not set — is libsqlite3-sys a direct dependency?".to_string()
    })?;

    cc::Build::new()
        .file(&c_path)
        .include(&out_dir)
        .include(&sqlite_include)
        .define("SQLITE_CORE", None)
        .define("SQLITE_VEC_ENABLE_DISKANN", "0")
        .define("SQLITE_VEC_ENABLE_RESCORE", "0")
        .warnings(false)
        .try_compile("sqlite_vec0")
        .map_err(|e| format!("cannot compile sqlite-vec.c: {e}"))?;

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// Upstream source: `asg017/sqlite-vec` at the commit embedded in the
/// crates.io `0.1.10-alpha.4` package's own header (`SQLITE_VEC_SOURCE`), so
/// this is exactly the source that version was built from — a real pin, not a
/// moving branch.
#[cfg(feature = "compare-sqlite-vec")]
const SQLITE_VEC_C_URL: &str = "https://raw.githubusercontent.com/asg017/sqlite-vec/04d28bd21773981e2d266bbf6aa4efbd011eb4f6/sqlite-vec.c";

/// SHA-256 of the file at [`SQLITE_VEC_C_URL`] — verified byte-for-byte
/// identical to the `sqlite-vec.c` vendored inside the crates.io
/// `sqlite-vec = "=0.1.10-alpha.4"` package.
#[cfg(feature = "compare-sqlite-vec")]
const SQLITE_VEC_C_SHA256: &str =
    "905a4bc025a63553aff1da9af0b9efcd5c18beec6e6ab2c25428f24a696aa19b";

#[cfg(feature = "compare-sqlite-vec")]
fn verify(path: &PathBuf) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    sha256_hex(&bytes) == SQLITE_VEC_C_SHA256
}

/// Downloads [`SQLITE_VEC_C_URL`] into `dest` atomically (temp file + rename)
/// so a killed build never leaves a half-written file that later passes the
/// existence check but fails verification. Uses the `curl` CLI (present on
/// all three CI platforms), matching `embedmind-core/build.rs`'s approach.
#[cfg(feature = "compare-sqlite-vec")]
fn download_to(dest: &PathBuf) -> Result<(), String> {
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
        .arg(SQLITE_VEC_C_URL)
        .status()
        .map_err(|e| format!("failed to run `curl` to download sqlite-vec.c: {e}"))?;
    if !status.success() {
        return Err(format!(
            "curl exited with {status} downloading {SQLITE_VEC_C_URL}"
        ));
    }
    std::fs::rename(&tmp, dest)
        .map_err(|e| format!("cannot move downloaded sqlite-vec.c into place: {e}"))?;
    Ok(())
}

/// `sqlite-vec.h` at the pinned commit, reproduced verbatim (see module docs:
/// it is not published at that commit, only generated at release time). Just
/// the public `vec0` init entrypoint declaration and version macros — no
/// logic, so no drift risk from hand-copying it once here.
#[cfg(feature = "compare-sqlite-vec")]
const SQLITE_VEC_H: &str = r#"#ifndef SQLITE_VEC_H
#define SQLITE_VEC_H

#ifndef SQLITE_CORE
#include "sqlite3ext.h"
#else
#include "sqlite3.h"
#endif

#ifdef SQLITE_VEC_STATIC
  #define SQLITE_VEC_API
#else
  #ifdef _WIN32
    #define SQLITE_VEC_API __declspec(dllexport)
  #else
    #define SQLITE_VEC_API
  #endif
#endif

#define SQLITE_VEC_VERSION "v0.1.10-alpha.4"
#define SQLITE_VEC_DATE "2026-05-18T06:52:37Z+0000"
#define SQLITE_VEC_SOURCE "04d28bd21773981e2d266bbf6aa4efbd011eb4f6"

#define SQLITE_VEC_VERSION_MAJOR 0
#define SQLITE_VEC_VERSION_MINOR 1
#define SQLITE_VEC_VERSION_PATCH 10

#ifdef __cplusplus
extern "C" {
#endif

SQLITE_VEC_API int sqlite3_vec_init(sqlite3 *db, char **pzErrMsg,
                  const sqlite3_api_routines *pApi);

#ifdef __cplusplus
}
#endif

#endif
"#;

// --- Minimal SHA-256 (no build-dependency; identical implementation to
// `crates/embedmind-core/build.rs`, FIPS 180-4) ---

#[cfg(feature = "compare-sqlite-vec")]
struct Sha256 {
    state: [u32; 8],
    len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

#[cfg(feature = "compare-sqlite-vec")]
impl Sha256 {
    #[allow(clippy::unreadable_literal)]
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

    #[allow(clippy::needless_range_loop)]
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

#[cfg(feature = "compare-sqlite-vec")]
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.hex()
}
