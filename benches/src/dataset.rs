//! Committed benchmark dataset specs and their on-disk vector materialization
//! (`docs/BENCHMARKS.md` §2).
//!
//! A dataset is committed as a **tiny spec** — a name, a fixed seed and a
//! memory count — not as a multi-megabyte vector blob (100k × 384 × 4 bytes is
//! ~150 MB; committing that would blow the repo and the crates.io budget). The
//! reproducibility guarantee comes from two deterministic stages:
//!
//! 1. [`crate::corpus::generate`] turns `(seed, count)` into the exact same text
//!    corpus everywhere, forever.
//! 2. The shipped ONNX model (`embedmind-core`, CPU-only, no network) turns
//!    that text into the exact same vectors — the *same embeddings fed to
//!    every benchmarked system*, which is the methodology's core rule.
//!
//! So the committed artifact is [`DATASETS`]; the vectors are a build product,
//! materialized on demand into `benches/data/<name>.vec` (git-ignored) and the
//! searchable store into `benches/data/<name>.mind`. The `.vec` file records
//! its `(name, seed, count, dims, model_id)` in a header, so a stale
//! materialization from a different model or seed is rejected on load rather
//! than silently benchmarked.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use embedmind_core::api::{MemoryDraft, Store, StoreOptions};
use embedmind_core::embed::{Embedder, OnnxEmbedder};
use embedmind_core::index::normalize;
use ulid::Ulid;

use crate::corpus::{self, GenMemory};

/// A committed benchmark dataset: reproducible from these three fields alone.
#[derive(Debug, Clone, Copy)]
pub struct DatasetSpec {
    /// Stable name (`agent-mem-10k`, `agent-mem-100k`), used for file paths and
    /// results tables.
    pub name: &'static str,
    /// Fixed generation seed — **recorded here so it is version-controlled**
    /// (the "seeds registered" rule, docs/BENCHMARKS.md §2).
    pub seed: u64,
    /// Number of memories.
    pub count: usize,
}

/// The two committed datasets, always reported side by side (`docs/BENCHMARKS.md`
/// §4 rule 2). The 100k set shares the 10k set's seed, so the 10k corpus is a
/// genuine prefix of the 100k one (see `corpus` tests) — the smaller run is never
/// a different distribution, just a truncation.
pub const DATASETS: &[DatasetSpec] = &[
    DatasetSpec {
        name: "agent-mem-10k",
        seed: 0xEDB1_A9C7_2026_0708,
        count: 10_000,
    },
    DatasetSpec {
        name: "agent-mem-100k",
        seed: 0xEDB1_A9C7_2026_0708,
        count: 100_000,
    },
];

impl DatasetSpec {
    /// Looks a dataset up by name (for CLI dispatch).
    pub fn by_name(name: &str) -> Option<&'static DatasetSpec> {
        DATASETS.iter().find(|d| d.name == name)
    }

    /// The generated text corpus for this dataset — deterministic.
    pub fn corpus(&self) -> Vec<GenMemory> {
        corpus::generate(self.seed, self.count)
    }

    /// Path of the materialized vector file (git-ignored build product).
    pub fn vec_path(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(format!("{}.vec", self.name))
    }

    /// Path of the materialized `.mind` store (git-ignored build product).
    pub fn mind_path(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(format!("{}.mind", self.name))
    }
}

/// The vectors of one dataset, in memory: each memory's id, its (already
/// L2-normalized) embedding, and its project. This is what both the
/// brute-force baseline and the recall harness operate on.
pub struct VectorSet {
    /// Embedding dimensionality (matches the model).
    pub dims: u16,
    /// One entry per memory, in generation order.
    pub entries: Vec<VectorEntry>,
}

/// One memory's vector row.
pub struct VectorEntry {
    /// Record id assigned when the memory was stored (ties the vector back to
    /// the `.mind` record so recall results can be cross-checked).
    pub id: Ulid,
    /// L2-normalized embedding (cosine similarity is then a plain dot product).
    pub vector: Vec<f32>,
    /// Project scope, for scope-filtered recall realism.
    pub project: String,
}

/// Magic prefixing the `.vec` file, so a truncated or foreign file is caught
/// on load instead of silently benchmarked.
const VEC_MAGIC: &[u8; 8] = b"MINDBEN1";

/// Materializes a dataset: embeds every memory with the shipped ONNX model,
/// writes them into a fresh `.mind` store (so HNSW recall can be measured
/// against a real file) **and** dumps the same normalized vectors to `.vec`
/// for the brute-force baseline. Returns the in-memory [`VectorSet`].
///
/// Both outputs come from one embedding pass over one corpus, so the store and
/// the baseline see identical vectors — the invariant that makes recall@10
/// meaningful (`docs/BENCHMARKS.md`: "same embeddings for all").
pub fn materialize(spec: &DatasetSpec, data_dir: &Path) -> io::Result<VectorSet> {
    std::fs::create_dir_all(data_dir)?;
    // One embedder, shared with the store: loading the ONNX session is the
    // slow part, and materialize + the store must see identical vectors.
    let embedder: Arc<dyn Embedder> = Arc::new(
        OnnxEmbedder::load().map_err(|e| io::Error::other(format!("model load failed: {e}")))?,
    );
    let model_id = embedder.id();

    let corpus = spec.corpus();

    // Fresh store: remove any stale materialization first so `Store::create`
    // (which refuses to clobber) starts clean.
    let mind_path = spec.mind_path(data_dir);
    let _ = std::fs::remove_file(&mind_path);
    let _ = std::fs::remove_file(mind_path.with_extension("mind-wal"));
    let opts = StoreOptions {
        embedder: Some(Arc::clone(&embedder)),
        ..StoreOptions::default()
    };
    let mut store = Store::create_with(
        Arc::new(embedmind_core::storage::vfs::RealVfs),
        &mind_path,
        opts,
    )
    .map_err(|e| io::Error::other(format!("store create failed: {e}")))?;

    let set = ingest_corpus(&mut store, embedder.as_ref(), &corpus)
        .map_err(|e| io::Error::other(format!("ingest failed: {e}")))?;
    store
        .close()
        .map_err(|e| io::Error::other(format!("store close failed: {e}")))?;

    write_vec_file(spec, &set, &spec.vec_path(data_dir), model_id)?;
    Ok(set)
}

/// `remember`s every memory of `corpus` into `store` and returns the parallel
/// [`VectorSet`] of their normalized embeddings — the shared core of both
/// `materialize` (real file) and the in-memory harness tests. `store` and the
/// returned set are guaranteed to hold the *same* vectors, which is what makes
/// recall@k against them meaningful.
///
/// Backlog idea (not started): this calls `Store::remember` once per memory,
/// each a full commit (embed + HNSW insert + FTS + graph + btree write + WAL
/// fsync) — measured at ~64 mem/s, so materializing `agent-mem-100k` from
/// scratch costs ~26 min of ingest alone. A `remember_batch` on the engine
/// (one transaction/fsync for N drafts) would remove that per-call commit
/// cost, but only pays off for bulk scenarios — dataset generation exactly
/// like this, initial import from another system, or a full reindex after
/// changing embedders — not the product's real hot path (one memory at a
/// time, incrementally, during a session). Lower priority than anything on
/// the engine's actual read/write path; also needs a partial-failure story
/// (today one bad draft rolls back to nothing — a batch of 1000 needs a
/// deliberate all-or-nothing-vs-best-effort call, not an implementation
/// detail).
pub fn ingest_corpus(
    store: &mut Store,
    embedder: &dyn Embedder,
    corpus: &[GenMemory],
) -> embedmind_core::Result<VectorSet> {
    let mut entries = Vec::with_capacity(corpus.len());
    for (i, mem) in corpus.iter().enumerate() {
        // Heartbeat on (unbuffered) stderr: the 100k ingest runs for ~half an
        // hour with no other output, and from outside that silence is
        // indistinguishable from a hang.
        if i > 0 && i % 5_000 == 0 {
            eprintln!("  ingest: {i}/{} memories", corpus.len());
        }
        let stored = store.remember(
            MemoryDraft::new(mem.content.clone())
                .project(mem.project.clone())
                .agent("bench-gen"),
        )?;
        // The baseline compares against the memory's *primary* embedding — the
        // whole-content vector recall ultimately returns a record for. Short
        // agent memories are one chunk, so this is the same vector the store
        // indexed; the recall harness dedupes by record id either way.
        let mut vector = embedder.embed(&mem.content)?;
        normalize(&mut vector);
        entries.push(VectorEntry {
            id: stored.id,
            vector,
            project: mem.project.clone(),
        });
    }
    Ok(VectorSet {
        dims: embedder.dims(),
        entries,
    })
}

/// Writes the `.vec` sidecar: a small header (magic, seed, count, dims, model
/// id) then the raw normalized vectors and ids. Little-endian throughout, to
/// match the engine's on-disk convention (`docs/FORMAT.md`).
fn write_vec_file(
    spec: &DatasetSpec,
    set: &VectorSet,
    path: &Path,
    model_id: &str,
) -> io::Result<()> {
    let mut w = io::BufWriter::new(std::fs::File::create(path)?);
    w.write_all(VEC_MAGIC)?;
    w.write_all(&spec.seed.to_le_bytes())?;
    w.write_all(&(set.entries.len() as u64).to_le_bytes())?;
    w.write_all(&set.dims.to_le_bytes())?;
    let model = model_id.as_bytes();
    w.write_all(&(model.len() as u16).to_le_bytes())?;
    w.write_all(model)?;
    for e in &set.entries {
        w.write_all(&e.id.to_bytes())?;
        for &x in &e.vector {
            w.write_all(&x.to_le_bytes())?;
        }
    }
    w.flush()
}

/// Loads a previously materialized `.vec` file, refusing it if its recorded
/// seed/dims/model do not match this spec + embedder — a stale file from a
/// different model or seed is a silent-wrong-answer trap, so it is a hard
/// error, not a warning.
pub fn load_vec_file(
    spec: &DatasetSpec,
    path: &Path,
    dims: u16,
    model_id: &str,
) -> io::Result<VectorSet> {
    let mut r = io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != VEC_MAGIC {
        return Err(io::Error::other("not a bench .vec file (bad magic)"));
    }
    let seed = read_u64(&mut r)?;
    if seed != spec.seed {
        return Err(io::Error::other("stale .vec: seed mismatch — regenerate"));
    }
    let count = read_u64(&mut r)? as usize;
    let file_dims = read_u16(&mut r)?;
    if file_dims != dims {
        return Err(io::Error::other("stale .vec: dims mismatch — regenerate"));
    }
    let model_len = read_u16(&mut r)? as usize;
    let mut model_buf = vec![0u8; model_len];
    r.read_exact(&mut model_buf)?;
    if model_buf != model_id.as_bytes() {
        return Err(io::Error::other("stale .vec: model mismatch — regenerate"));
    }

    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let mut id_bytes = [0u8; 16];
        r.read_exact(&mut id_bytes)?;
        let id = Ulid::from_bytes(id_bytes);
        let mut vector = vec![0f32; usize::from(dims)];
        for x in &mut vector {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            *x = f32::from_le_bytes(b);
        }
        // Project is not stored in the .vec file (it lives in the .mind
        // record); the baseline recall harness reads it back from the store
        // when scope filtering is needed. Left empty here.
        entries.push(VectorEntry {
            id,
            vector,
            project: String::new(),
        });
    }
    Ok(VectorSet { dims, entries })
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
