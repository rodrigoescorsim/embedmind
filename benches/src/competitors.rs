//! Pinned competitor registry and comparison adapters (`docs/BENCHMARKS.md`
//! §1/§4).
//!
//! The methodology's contract is strict: competitors are compared "in **pinned
//! and recorded** versions, under the same load", and we "publish every metric
//! we measure, **including where EmbedMind loses**" — but equally we never
//! fabricate a number. So this module does two things:
//!
//! 1. **Pins the versions in version-controlled constants** ([`COMPETITORS`]),
//!    so the target version is recorded in the repo and rendered into every
//!    results table, whether or not the competitor actually ran on a given
//!    machine.
//! 2. Runs each competitor **only when its build feature is enabled and its
//!    native dependency is present**; otherwise it reports
//!    [`CompetitorOutcome::NotMeasured`] with the reason, so the table shows an
//!    honest "not measured on this run (target vX.Y)" instead of a made-up row.
//!
//! sqlite-vec (a SQLite extension, C) and zvec (Zig) both need a native
//! toolchain that a pure-`cargo bench` box may not have. Gating them behind
//! `--features compare-sqlite-vec` / `compare-zvec` keeps the default harness
//! buildable everywhere (the CI regression guard, BENCHMARKS.md §5, runs the
//! EmbedMind-only metrics), while a release-run box with the toolchains flips
//! the features on to fill the comparison columns.
//!
//! When a real adapter is wired (see [`run_sqlite_vec`]/[`run_zvec`]), it must
//! obey the same rules the EmbedMind path does: **same normalized vectors, same
//! queries, same k**, default/recommended settings for the competitor (no
//! de-tuning), and it fills a [`CompetitorMetrics`] measured identically.

use crate::dataset::VectorSet;
#[cfg(any(
    feature = "compare-sqlite-vec",
    feature = "compare-zvec",
    feature = "compare-chroma"
))]
use crate::metrics::{self, Latencies};

/// A benchmarked competitor and the exact version this harness targets. The
/// version string is what lands in the results table's "version" cell — it is
/// the recorded pin, independent of whether the adapter ran.
#[derive(Debug, Clone, Copy)]
pub struct Competitor {
    /// Display name for the results table.
    pub name: &'static str,
    /// Pinned target version (`docs/BENCHMARKS.md` §1: "pinned versions,
    /// recorded in results"). Update this in lockstep with the adapter.
    pub version: &'static str,
    /// The build feature that enables this competitor's adapter. When the
    /// feature is off, the harness records [`CompetitorOutcome::NotMeasured`].
    pub feature: &'static str,
    /// One-line note on settings used / why it is the fair comparison, shown
    /// under the table.
    pub note: &'static str,
    /// What a query returns and what ingest persists (`docs/BENCHMARKS.md` §4
    /// rule 6): a smaller on-disk file or a faster query that does less is not
    /// a win row, so every comparison row states its scope explicitly.
    pub scope: Scope,
}

/// A system's scope in the comparison (`docs/BENCHMARKS.md` §4 rule 6):
/// what a query returns, and what ingest persists to disk.
#[derive(Debug, Clone, Copy)]
pub struct Scope {
    /// What a query returns, e.g. "ids only" vs. "full content + metadata".
    pub returns: &'static str,
    /// What ingest persists, e.g. "vectors only" vs. "text + metadata + full-text + graph".
    pub persists: &'static str,
}

/// The competitors the methodology names (`docs/BENCHMARKS.md` §1), with the
/// versions this harness is pinned to. **Version-controlled** — bumping a
/// competitor is a reviewed commit, and the number in the table always traces
/// back to this constant.
pub const COMPETITORS: &[Competitor] = &[
    Competitor {
        name: "sqlite-vec",
        // The incumbent "embedded vector search in one file". This is the
        // version `benches/build.rs` actually compiles (pinned upstream commit,
        // SHA-256-verified — the source the crates.io 0.1.10-alpha.4 package
        // was built from); recorded here so the table's version cell always
        // matches the binary that produced the numbers.
        version: "0.1.10-alpha.4",
        feature: "compare-sqlite-vec",
        note: "SQLite extension, default page size, vec0 virtual table, brute-force KNN (its recommended small-scale path).",
        scope: Scope {
            returns: "rowid + distance only (no content/metadata store)",
            persists: "vectors only",
        },
    },
    Competitor {
        name: "zvec",
        // The closest new embedded vector store (alibaba/zvec), driven through
        // its official `zvec-rust` binding pinned to the same version in
        // Cargo.toml (`zvec-rust = "=0.5.1"`).
        version: "0.5.1",
        feature: "compare-zvec",
        note: "Embedded vector store (alibaba/zvec) via the official zvec-rust binding, default HNSW settings (M=16, ef_construction=200, cosine).",
        scope: Scope {
            returns: "primary key + distance only (no content/metadata store)",
            persists: "vectors + primary key only",
        },
    },
    Competitor {
        name: "Chroma",
        // The product-category competitor (docs/03-tasks.md BQ4 / story S18):
        // a local vector store that, in real agent-dev use, also embeds —
        // unlike sqlite-vec/zvec, which are index-layer-only baselines. This
        // harness drives it with the *same* pre-computed all-MiniLM-L6-v2
        // vectors as every other system (BENCHMARKS.md Sec1: same embeddings
        // fed to all systems), so the comparison isolates the store, not the
        // embedding step — Chroma's own embedding functions are never called.
        version: "1.5.9",
        feature: "compare-chroma",
        note: "Local/embedded mode (PersistentClient), cosine space (its documented default for embedding search), driven via a pinned Python subprocess (benches/chroma_bench.py) — no server, no network.",
        scope: Scope {
            returns: "ids only (queried by vector; metadata/documents optional and unused here)",
            persists: "vectors + ids (a local Chroma collection can also store documents/metadata, unused in this comparison)",
        },
    },
];

/// The metrics a competitor is graded on — the same shape as EmbedMind's own
/// row so the renderer can lay them side by side. All optional: an adapter that
/// cannot measure something (e.g. a store that does not embed) leaves it
/// `None`, rendered as `—`.
#[derive(Debug, Clone, Default)]
pub struct CompetitorMetrics {
    /// recall@10 vs. the shared brute-force baseline.
    pub recall_at_10: Option<f64>,
    /// Warm query latency p50 / p99, milliseconds.
    pub query_p50_ms: Option<f64>,
    pub query_p99_ms: Option<f64>,
    /// Cold-open first-query latency, milliseconds.
    pub cold_open_ms: Option<f64>,
    /// Ingest throughput, memories/sec (vectors only — competitors don't embed).
    pub ingest_vecs_per_sec: Option<f64>,
    /// On-disk file size after ingest, bytes.
    pub file_bytes: Option<u64>,
    /// Peak RSS during the run, mebibytes.
    pub peak_rss_mib: Option<f64>,
}

/// Outcome of attempting a competitor comparison: either real numbers, or an
/// honest record of why they are absent (never a fabricated row).
#[derive(Debug, Clone)]
pub enum CompetitorOutcome {
    /// The adapter ran and produced numbers.
    Measured(CompetitorMetrics),
    /// The adapter did not run; `reason` is shown in the table
    /// (e.g. "feature `compare-sqlite-vec` disabled").
    NotMeasured { reason: String },
}

/// Runs every registered competitor over the same `set`/`queries`/`k` and
/// returns each outcome paired with its pin. Adapters that are not compiled in
/// return [`CompetitorOutcome::NotMeasured`] — so the caller always gets one
/// entry per competitor and the table is complete and honest.
pub fn run_all(
    set: &VectorSet,
    queries: &[Vec<f32>],
    k: usize,
) -> Vec<(&'static Competitor, CompetitorOutcome)> {
    COMPETITORS
        .iter()
        .map(|c| {
            let outcome = match c.name {
                "sqlite-vec" => run_sqlite_vec(c, set, queries, k),
                "zvec" => run_zvec(c, set, queries, k),
                "Chroma" => run_chroma(c, set, queries, k),
                _ => CompetitorOutcome::NotMeasured {
                    reason: "no adapter".to_string(),
                },
            };
            (c, outcome)
        })
        .collect()
}

/// sqlite-vec adapter. Real implementation lives behind
/// `--features compare-sqlite-vec` (needs the SQLite `vec0` extension /
/// `rusqlite` bundled build). Without the feature it records why it is absent.
#[cfg(not(feature = "compare-sqlite-vec"))]
fn run_sqlite_vec(
    c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: format!(
            "feature `{}` disabled (build with it + the sqlite-vec {} extension to fill this row)",
            c.feature, c.version
        ),
    }
}

/// zvec adapter. Real implementation lives behind `--features compare-zvec`
/// (needs the Zig toolchain to build zvec). Without the feature it records why.
#[cfg(not(feature = "compare-zvec"))]
fn run_zvec(
    c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: format!(
            "feature `{}` disabled (build with it + a zvec {} build to fill this row)",
            c.feature, c.version
        ),
    }
}

/// Chroma adapter. Real implementation lives behind `--features compare-chroma`
/// (needs a Python 3 interpreter with `chromadb` installed, driven as a
/// subprocess — see `benches/chroma_bench.py`). Without the feature it records
/// why it is absent.
#[cfg(not(feature = "compare-chroma"))]
fn run_chroma(
    c: &Competitor,
    _set: &VectorSet,
    _queries: &[Vec<f32>],
    _k: usize,
) -> CompetitorOutcome {
    CompetitorOutcome::NotMeasured {
        reason: format!(
            "feature `{}` disabled (build with it + Python 3 + `pip install chromadb=={}` to fill this row)",
            c.feature, c.version
        ),
    }
}

// The real adapters, compiled only when their feature is on. Both obey the
// same rules the EmbedMind path does: same normalized vectors, same queries,
// same k, default/recommended settings (no de-tuning), fresh on-disk file per
// run so `file_bytes` is a real "after ingest" number.

/// sqlite-vec adapter: the `vec0` virtual table, ingested one row at a time
/// (the fair comparison to EmbedMind's one-at-a-time `remember`), queried with
/// its default brute-force KNN (`ORDER BY distance LIMIT k`, its documented
/// small-scale path — no de-tuning).
///
/// Registering `vec0` is only possible through the C entrypoint
/// (`sqlite3_auto_extension`, `rusqlite`'s own documented pattern for
/// statically-linked extensions), which is inherently `unsafe` — the crate
/// does not expose a safe wrapper. That one call is isolated here and behind
/// `compare-sqlite-vec` only, the same targeted-exception shape
/// `bindings/python` already uses for PyO3's generated glue; the rest of the
/// workspace (and the rest of this crate) keeps `unsafe_code = "forbid"`.
#[cfg(feature = "compare-sqlite-vec")]
#[allow(unsafe_code)]
fn run_sqlite_vec(
    c: &Competitor,
    set: &VectorSet,
    queries: &[Vec<f32>],
    k: usize,
) -> CompetitorOutcome {
    use rusqlite::Connection;
    use rusqlite::ffi::sqlite3_auto_extension;

    // The real `sqlite3_vec_init(sqlite3*, char**, const sqlite3_api_routines*)
    // -> c_int` signature (`sqlite-vec.h`, reproduced in `build.rs`) — matches
    // `sqlite3_auto_extension`'s expected entry-point type exactly, so no
    // transmute is needed (unlike the upstream crate's own binding, which
    // deliberately mis-declares the signature and transmutes it back).
    unsafe extern "C" {
        fn sqlite3_vec_init(
            db: *mut rusqlite::ffi::sqlite3,
            pz_err_msg: *mut *mut std::os::raw::c_char,
            p_api: *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
    }

    // Registered once per process is enough, but `sqlite3_auto_extension` is
    // idempotent about duplicate registrations, so calling it per run is safe
    // and keeps this adapter self-contained (no shared init state to manage).
    // SAFETY: `sqlite3_vec_init` is the vec0 extension entry point compiled by
    // `build.rs` from the pinned, checksum-verified upstream source; its
    // signature is declared to match what `sqlite3_auto_extension` requires.
    unsafe {
        sqlite3_auto_extension(Some(sqlite3_vec_init));
    }

    let db_path = std::env::temp_dir().join(format!("embedmind-bench-sqlite-vec-{k}.sqlite3"));
    let _ = std::fs::remove_file(&db_path);

    let run = || -> rusqlite::Result<CompetitorMetrics> {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE items USING vec0(embedding float[{}])",
            set.dims
        ))?;

        // --- ingest, one row at a time (fair comparison to `remember`) ---
        let mut ingest_lat = Latencies::with_capacity(set.entries.len());
        let ingest_started = std::time::Instant::now();
        {
            let mut stmt = conn.prepare("INSERT INTO items(rowid, embedding) VALUES (?, ?)")?;
            for (i, e) in set.entries.iter().enumerate() {
                let started = std::time::Instant::now();
                stmt.execute(rusqlite::params![i as i64, vec_to_blob(&e.vector)])?;
                ingest_lat.push(started.elapsed());
            }
        }
        let ingest_per_sec = metrics::ops_per_sec(set.entries.len(), ingest_started.elapsed());

        // --- recall@k against the shared brute-force baseline ---
        let mut recall_sum = 0.0;
        let mut recall_n = 0usize;
        // --- warm query latency, same query set/k as EmbedMind's own run ---
        let mut warm = Latencies::with_capacity(queries.len());
        {
            let mut stmt = conn.prepare(
                "SELECT rowid, distance FROM items WHERE embedding MATCH ? \
                 ORDER BY distance LIMIT ?",
            )?;
            for q in queries {
                let started = std::time::Instant::now();
                let rows: Vec<i64> = stmt
                    .query_map(rusqlite::params![vec_to_blob(q), k as i64], |row| {
                        row.get(0)
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                warm.push(started.elapsed());

                let exact = crate::baseline::top_k(set, q, k, |_| true);
                let exact_ids: std::collections::HashSet<i64> = exact
                    .iter()
                    .map(|hit| id_index(set, hit.record_id) as i64)
                    .collect();
                let hit_count = rows.iter().filter(|r| exact_ids.contains(r)).count();
                if !exact_ids.is_empty() {
                    recall_sum += hit_count as f64 / exact_ids.len() as f64;
                    recall_n += 1;
                }
            }
        }

        let file_bytes = std::fs::metadata(&db_path).map(|m| m.len()).ok();

        Ok(CompetitorMetrics {
            recall_at_10: if recall_n > 0 {
                Some(recall_sum / recall_n as f64)
            } else {
                None
            },
            query_p50_ms: warm.p50_ms(),
            query_p99_ms: warm.p99_ms(),
            cold_open_ms: None,
            ingest_vecs_per_sec: Some(ingest_per_sec),
            file_bytes,
            peak_rss_mib: None,
        })
    };

    let outcome = match run() {
        Ok(m) => CompetitorOutcome::Measured(m),
        Err(e) => CompetitorOutcome::NotMeasured {
            reason: format!("sqlite-vec adapter failed: {e}"),
        },
    };
    let _ = std::fs::remove_file(&db_path);
    if matches!(outcome, CompetitorOutcome::Measured(_)) {
        outcome
    } else {
        // Keep the pinned-version note even on failure, per module contract.
        let CompetitorOutcome::NotMeasured { reason } = outcome else {
            unreachable!()
        };
        CompetitorOutcome::NotMeasured {
            reason: format!("{reason} (target {})", c.version),
        }
    }
}

/// Finds `id`'s position in `set.entries` — both adapters key their rows by
/// that same position (sqlite-vec's `rowid`, zvec's string pk), so this maps
/// an exact-baseline hit back to the id space each adapter's query returns,
/// letting recall be computed as a plain set overlap.
#[cfg(any(
    feature = "compare-sqlite-vec",
    feature = "compare-zvec",
    feature = "compare-chroma"
))]
fn id_index(set: &VectorSet, id: ulid::Ulid) -> usize {
    set.entries
        .iter()
        .position(|e| e.id == id)
        .unwrap_or(usize::MAX)
}

/// Encodes a `f32` vector as the little-endian byte blob `vec0` expects for a
/// `float[N]` column.
#[cfg(feature = "compare-sqlite-vec")]
fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(v.len() * 4);
    for x in v {
        buf.extend_from_slice(&x.to_le_bytes());
    }
    buf
}

/// zvec adapter: an HNSW-indexed collection with its documented default
/// parameters (`M=16`, `ef_construction=200`, cosine metric — the values the
/// crate's own quick-start example uses), ingested one document at a time.
#[cfg(feature = "compare-zvec")]
fn run_zvec(c: &Competitor, set: &VectorSet, queries: &[Vec<f32>], k: usize) -> CompetitorOutcome {
    use zvec_rust::{
        Collection, CollectionSchema, DataType, Doc, FieldSchema, IndexParams, MetricType,
        SearchQuery,
    };

    let dir = std::env::temp_dir().join(format!("embedmind-bench-zvec-{k}"));
    let _ = std::fs::remove_dir_all(&dir);

    let run = || -> zvec_rust::Result<CompetitorMetrics> {
        zvec_rust::config::initialize(None).or_else(|e| {
            // Re-running the suite in the same process re-initializes; zvec
            // treats that as "already exists", which is fine to ignore.
            if e.is_already_exists() {
                Ok(())
            } else {
                Err(e)
            }
        })?;

        // zvec's `create_and_open` creates the collection directory itself and
        // rejects a path that already exists ("path validate failed: … exists").
        // So the adapter must only guarantee the path is *absent* (the
        // `remove_dir_all` above), and hand zvec a clean, non-existent path —
        // never pre-create it. The parent (the OS temp dir) already exists.
        let schema = CollectionSchema::builder("bench")
            .add_field(FieldSchema::new("id", DataType::Int64, false, 0)?)
            .add_vector_field(
                "embedding",
                DataType::VectorFp32,
                set.dims as u32,
                IndexParams::hnsw(MetricType::Cosine, 16, 200)?,
            )
            .build()?;

        let collection = Collection::create_and_open(dir.to_str().unwrap_or("."), &schema, None)?;

        // --- ingest, one document at a time ---
        let mut ingest_lat = Latencies::with_capacity(set.entries.len());
        let ingest_started = std::time::Instant::now();
        for (i, e) in set.entries.iter().enumerate() {
            let started = std::time::Instant::now();
            let mut doc = Doc::new()?;
            doc.set_pk(&i.to_string());
            doc.add_i64("id", i as i64)?;
            doc.add_vector_f32("embedding", &e.vector)?;
            collection.insert(&[&doc])?;
            ingest_lat.push(started.elapsed());
        }
        let ingest_per_sec = metrics::ops_per_sec(set.entries.len(), ingest_started.elapsed());
        collection.flush()?;

        // --- recall@k + warm query latency ---
        let mut recall_sum = 0.0;
        let mut recall_n = 0usize;
        let mut warm = Latencies::with_capacity(queries.len());
        for q in queries {
            let started = std::time::Instant::now();
            let query = SearchQuery::new("embedding", q, k as i32)?;
            let results = collection.query(&query)?;
            warm.push(started.elapsed());

            let got: std::collections::HashSet<i64> = results
                .iter()
                .filter_map(|d| d.get_pk().and_then(|pk| pk.parse::<i64>().ok()))
                .collect();

            let exact = crate::baseline::top_k(set, q, k, |_| true);
            let exact_ids: std::collections::HashSet<i64> = exact
                .iter()
                .map(|hit| id_index(set, hit.record_id) as i64)
                .collect();
            let hit_count = got.iter().filter(|r| exact_ids.contains(r)).count();
            if !exact_ids.is_empty() {
                recall_sum += hit_count as f64 / exact_ids.len() as f64;
                recall_n += 1;
            }
        }

        let file_bytes = dir_size(&dir);

        collection.close()?;

        Ok(CompetitorMetrics {
            recall_at_10: if recall_n > 0 {
                Some(recall_sum / recall_n as f64)
            } else {
                None
            },
            query_p50_ms: warm.p50_ms(),
            query_p99_ms: warm.p99_ms(),
            cold_open_ms: None,
            ingest_vecs_per_sec: Some(ingest_per_sec),
            file_bytes,
            peak_rss_mib: None,
        })
    };

    let outcome = match run() {
        Ok(m) => CompetitorOutcome::Measured(m),
        Err(e) => CompetitorOutcome::NotMeasured {
            reason: format!("zvec adapter failed: {e} (target {})", c.version),
        },
    };
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Total on-disk size of everything zvec wrote for the collection — it stores
/// a collection as a directory of segment files, not a single file, so
/// `file_bytes` sums the directory recursively (best-effort: unreadable
/// entries are skipped rather than failing the whole measurement).
#[cfg(feature = "compare-zvec")]
fn dir_size(dir: &std::path::Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    stack.push(path);
                } else {
                    total += meta.len();
                }
            }
        }
    }
    Some(total)
}

/// Chroma adapter: drives `benches/chroma_bench.py` as a subprocess over
/// newline-delimited JSON on stdin/stdout (the script's own docstring is the
/// protocol reference). No native FFI, no `unsafe` — the boundary is a plain
/// child process, the same shape as `Command::new` anywhere else in the crate.
///
/// The request carries the *same* pre-computed vectors/queries every other
/// competitor receives (never re-embedded by Chroma itself, matching
/// `docs/BENCHMARKS.md` Sec1 "same embeddings fed to all systems"). Ids are
/// stringified indices into `set.entries`, which the script echoes back in
/// `results` so `id_index`'s position-based recall math applies unchanged.
#[cfg(feature = "compare-chroma")]
fn run_chroma(
    c: &Competitor,
    set: &VectorSet,
    queries: &[Vec<f32>],
    k: usize,
) -> CompetitorOutcome {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let run = || -> Result<CompetitorMetrics, String> {
        let ids: Vec<String> = (0..set.entries.len()).map(|i| i.to_string()).collect();
        let vectors: Vec<&[f32]> = set.entries.iter().map(|e| e.vector.as_slice()).collect();

        let request = serde_json::json!({
            "dims": set.dims,
            "ids": ids,
            "vectors": vectors,
            "queries": queries,
            "k": k,
        });
        let request_bytes =
            serde_json::to_vec(&request).map_err(|e| format!("encoding request: {e}"))?;

        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("chroma_bench.py");
        let mut child = Command::new(python_interpreter())
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawning python for {}: {e}", script.display()))?;

        child
            .stdin
            .take()
            .ok_or("no stdin handle on spawned python")?
            .write_all(&request_bytes)
            .map_err(|e| format!("writing request to python stdin: {e}"))?;

        let output = child
            .wait_with_output()
            .map_err(|e| format!("waiting for python: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "chroma_bench.py exited with {}: {stderr}",
                output.status
            ));
        }

        let response: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("parsing python response: {e}"))?;
        if let Some(err) = response.get("error").and_then(|v| v.as_str()) {
            return Err(err.to_string());
        }

        let ingest_ms: Vec<f64> = response["ingest_ms_per_op"]
            .as_array()
            .ok_or("response missing ingest_ms_per_op")?
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .collect();
        let query_ms: Vec<f64> = response["query_ms"]
            .as_array()
            .ok_or("response missing query_ms")?
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .collect();
        let results: Vec<Vec<String>> = response["results"]
            .as_array()
            .ok_or("response missing results")?
            .iter()
            .map(|row| {
                row.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .collect();
        let file_bytes = response["file_bytes"].as_u64();

        let ingest_total_secs: f64 = ingest_ms.iter().sum::<f64>() / 1000.0;
        let ingest_per_sec = metrics::ops_per_sec(
            set.entries.len(),
            std::time::Duration::from_secs_f64(ingest_total_secs.max(0.0)),
        );

        let mut warm = Latencies::with_capacity(query_ms.len());
        for ms in &query_ms {
            warm.push(std::time::Duration::from_secs_f64((ms / 1000.0).max(0.0)));
        }

        // --- recall@k against the shared brute-force baseline, same pattern
        // as run_sqlite_vec/run_zvec: Chroma's returned ids are stringified
        // positions into `set.entries`, mapped back through `id_index`. ---
        let mut recall_sum = 0.0;
        let mut recall_n = 0usize;
        for (q, got_ids) in queries.iter().zip(results.iter()) {
            let got: std::collections::HashSet<i64> = got_ids
                .iter()
                .filter_map(|s| s.parse::<i64>().ok())
                .collect();
            let exact = crate::baseline::top_k(set, q, k, |_| true);
            let exact_ids: std::collections::HashSet<i64> = exact
                .iter()
                .map(|hit| id_index(set, hit.record_id) as i64)
                .collect();
            let hit_count = got.iter().filter(|r| exact_ids.contains(r)).count();
            if !exact_ids.is_empty() {
                recall_sum += hit_count as f64 / exact_ids.len() as f64;
                recall_n += 1;
            }
        }

        Ok(CompetitorMetrics {
            recall_at_10: if recall_n > 0 {
                Some(recall_sum / recall_n as f64)
            } else {
                None
            },
            query_p50_ms: warm.p50_ms(),
            query_p99_ms: warm.p99_ms(),
            cold_open_ms: None,
            ingest_vecs_per_sec: Some(ingest_per_sec),
            file_bytes,
            peak_rss_mib: None,
        })
    };

    match run() {
        Ok(m) => CompetitorOutcome::Measured(m),
        Err(e) => CompetitorOutcome::NotMeasured {
            reason: format!("Chroma adapter failed: {e} (target {})", c.version),
        },
    }
}

/// Resolves the Python interpreter to invoke: `EMBEDMIND_BENCH_PYTHON` if set
/// (a pinned venv on a release box), else `python3`, the portable name on
/// every platform this harness targets except Windows, where the launcher is
/// conventionally just `python` — `python3` is not guaranteed to exist there.
#[cfg(feature = "compare-chroma")]
fn python_interpreter() -> String {
    if let Ok(p) = std::env::var("EMBEDMIND_BENCH_PYTHON") {
        return p;
    }
    if cfg!(windows) { "python" } else { "python3" }.to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_competitor_has_a_pinned_version() {
        for c in COMPETITORS {
            assert!(!c.version.is_empty(), "{} has no pinned version", c.name);
            assert!(!c.feature.is_empty());
        }
    }

    #[test]
    fn every_competitor_states_its_scope() {
        // BENCHMARKS.md §4 rule 6: every comparison row must state what it
        // returns and what it persists — never a silent "smaller/faster" claim.
        for c in COMPETITORS {
            assert!(
                !c.scope.returns.is_empty(),
                "{} has no scope.returns",
                c.name
            );
            assert!(
                !c.scope.persists.is_empty(),
                "{} has no scope.persists",
                c.name
            );
        }
    }

    #[test]
    fn run_all_returns_one_outcome_per_competitor() {
        let set = VectorSet {
            dims: 2,
            entries: vec![],
        };
        let outcomes = run_all(&set, &[], 10);
        assert_eq!(outcomes.len(), COMPETITORS.len());
        // A competitor whose feature is off is honestly NotMeasured — never
        // fake. (With the feature on, the real adapter runs — even on this
        // empty set — and reports whatever it actually measured.)
        for (c, o) in &outcomes {
            let enabled = match c.feature {
                "compare-sqlite-vec" => cfg!(feature = "compare-sqlite-vec"),
                "compare-zvec" => cfg!(feature = "compare-zvec"),
                "compare-chroma" => cfg!(feature = "compare-chroma"),
                _ => false,
            };
            if !enabled {
                assert!(matches!(o, CompetitorOutcome::NotMeasured { .. }));
            }
        }
    }
}
