//! Python bindings for the EmbedMind engine (B5, roadmap 2.5).
//!
//! A thin shell over `embedmind_core::api`, exactly like the CLI and MCP
//! server (CLAUDE.md decision 2): no domain logic lives here. `Store` wraps
//! the engine's `Store`, translating typed metadata to/from native Python
//! values and typed engine errors into Python exceptions. Files produced or
//! read here are byte-for-byte the same `.mind` the Rust `Store` writes — the
//! bindings call the same code, so cross-language round-trips are inherent,
//! not an extra compatibility layer.
//!
//! `remember`, `recall`, `forget`, `stats` mirror the MCP tools and CLI
//! subcommands. The default `Store()` loads the embedded ONNX model, so
//! `recall` works out of the box.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use embedmind_core::{
    Error as CoreError, Filter, MemoryDraft, Query, Recalled, Scalar, Scope, Store as CoreStore,
    StoreStats, Ulid,
};
use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyString};

/// Maps a typed engine error onto the closest Python exception. I/O and
/// corruption become `IOError` (something is wrong with the file); everything
/// the caller can fix by changing arguments becomes `ValueError`.
fn to_py_err(err: CoreError) -> PyErr {
    match err {
        CoreError::Io(e) => PyIOError::new_err(e.to_string()),
        CoreError::CorruptPage { .. }
        | CoreError::BadHeader
        | CoreError::MalformedPage { .. }
        | CoreError::MalformedRecord(_)
        | CoreError::PageOutOfBounds { .. } => PyIOError::new_err(err.to_string()),
        other => PyValueError::new_err(other.to_string()),
    }
}

/// Reads one Python object into the engine's typed [`Scalar`]. The mapping is
/// the natural one: `bool`→`Bool`, `int`→`I64`, `float`→`F64`, `str`→`Str`,
/// `None`→`Null`. `bool` is checked before `int` because Python's `bool` is a
/// subclass of `int`. Any other type is a `ValueError` — metadata is typed
/// (`docs/FORMAT.md` §5), not arbitrary objects.
fn scalar_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Scalar> {
    if obj.is_none() {
        Ok(Scalar::Null)
    } else if obj.is_instance_of::<PyBool>() {
        Ok(Scalar::Bool(obj.extract::<bool>()?))
    } else if obj.is_instance_of::<PyInt>() {
        Ok(Scalar::I64(obj.extract::<i64>()?))
    } else if obj.is_instance_of::<PyFloat>() {
        Ok(Scalar::F64(obj.extract::<f64>()?))
    } else if obj.is_instance_of::<PyString>() {
        Ok(Scalar::Str(obj.extract::<String>()?))
    } else {
        Err(PyValueError::new_err(
            "metadata values must be str, int, float, bool or None",
        ))
    }
}

/// Renders an engine [`Scalar`] back as a native Python object, the inverse of
/// [`scalar_from_py`], so a value stored from Python round-trips to the same
/// type it went in as.
fn scalar_to_py(py: Python<'_>, scalar: &Scalar) -> Py<PyAny> {
    match scalar {
        Scalar::Null => py.None(),
        Scalar::Bool(b) => b
            .into_pyobject(py)
            .map_or_else(|_| py.None(), |v| v.to_owned().into()),
        Scalar::I64(i) => i.into_pyobject(py).map_or_else(|_| py.None(), Into::into),
        Scalar::F64(f) => f.into_pyobject(py).map_or_else(|_| py.None(), Into::into),
        Scalar::Str(s) => s.into_pyobject(py).map_or_else(|_| py.None(), Into::into),
    }
}

/// Parses one recall filter value into an engine [`Filter`]. Two forms, mirroring
/// the CLI's `--filter` semantics (S10):
///
/// - a scalar (`str`/`int`/`float`/`bool`) → [`Filter::Eq`] against that value;
/// - a 2-tuple `(min, max)` of numbers-or-`None` → [`Filter::Range`], each side
///   open when its element is `None`.
fn filter_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Filter> {
    // A tuple/list of length 2 is a numeric range [min, max]; None on a side
    // means that side is unbounded.
    if let Ok(seq) = obj.extract::<(Option<f64>, Option<f64>)>() {
        return Ok(Filter::Range {
            min: seq.0,
            max: seq.1,
        });
    }
    Ok(Filter::Eq(scalar_from_py(obj)?))
}

/// The writer's identity for `remember`, matching the CLI's `agent("cli")`
/// stamp so Python-written memories carry honest provenance (basic provenance
/// is part of the free tier — CLAUDE.md decision 3). Callers can override it.
const DEFAULT_AGENT: &str = "python";

/// One recalled memory returned to Python: the memory fields plus its fused
/// relevance score (`docs/adr/0005`). Attribute-only, read-only — the store is
/// the single writer.
#[pyclass(frozen, name = "Recalled")]
struct PyRecalled {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    content: String,
    #[pyo3(get)]
    project: Option<String>,
    #[pyo3(get)]
    score: f32,
    #[pyo3(get)]
    agent: String,
    #[pyo3(get)]
    session_id: Option<String>,
    #[pyo3(get)]
    created_at_micros: i64,
    metadata: BTreeMap<String, Scalar>,
}

#[pymethods]
impl PyRecalled {
    /// Metadata as a plain `dict[str, str|int|float|bool|None]`.
    #[getter]
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (k, v) in &self.metadata {
            dict.set_item(k, scalar_to_py(py, v))?;
        }
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "Recalled(id={:?}, score={:.3}, content={:?})",
            self.id, self.score, self.content
        )
    }
}

impl PyRecalled {
    fn from_hit(hit: Recalled) -> PyRecalled {
        let Recalled { memory, score } = hit;
        PyRecalled {
            id: memory.id.to_string(),
            content: memory.content,
            project: memory.project,
            score,
            agent: memory.provenance.agent,
            session_id: memory.provenance.session_id,
            created_at_micros: memory.provenance.created_at_micros,
            metadata: memory.metadata,
        }
    }
}

/// The provenance breakdown for one agent inside [`PyStats`] (S14).
#[pyclass(frozen, name = "AgentStats")]
struct PyAgentStats {
    #[pyo3(get)]
    live_memories: u64,
    #[pyo3(get)]
    sessions: u64,
}

/// What `Store.stats()` returns — the same numbers as `embedmind stats`.
#[pyclass(frozen, name = "Stats")]
struct PyStats {
    #[pyo3(get)]
    live_memories: u64,
    #[pyo3(get)]
    forgotten_memories: u64,
    #[pyo3(get)]
    index_entries: u64,
    #[pyo3(get)]
    fts_documents: u64,
    #[pyo3(get)]
    graph_entities: u64,
    #[pyo3(get)]
    graph_relations: u64,
    #[pyo3(get)]
    page_size: u32,
    #[pyo3(get)]
    page_count: u64,
    #[pyo3(get)]
    file_bytes: u64,
    #[pyo3(get)]
    embedding_model_id: Option<String>,
    #[pyo3(get)]
    embedding_dims: u16,
    by_agent: BTreeMap<String, (u64, u64)>,
}

#[pymethods]
impl PyStats {
    /// `dict[str, AgentStats]` keyed by writing agent (the empty string groups
    /// memories with unknown provenance) — the S14 provenance breakdown.
    #[getter]
    fn by_agent<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (agent, (live, sessions)) in &self.by_agent {
            let stats = PyAgentStats {
                live_memories: *live,
                sessions: *sessions,
            };
            dict.set_item(agent, stats.into_pyobject(py)?)?;
        }
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "Stats(live_memories={}, forgotten_memories={}, file_bytes={})",
            self.live_memories, self.forgotten_memories, self.file_bytes
        )
    }
}

impl PyStats {
    fn from_stats(s: StoreStats) -> PyStats {
        PyStats {
            live_memories: s.live_memories,
            forgotten_memories: s.forgotten_memories,
            index_entries: s.index_entries,
            fts_documents: s.fts_documents,
            graph_entities: s.graph_entities,
            graph_relations: s.graph_relations,
            page_size: s.page_size,
            page_count: s.page_count,
            file_bytes: s.file_bytes,
            embedding_model_id: s.embedding_model_id,
            embedding_dims: s.embedding_dims,
            by_agent: s
                .by_agent
                .into_iter()
                .map(|(agent, a)| (agent, (a.live_memories, a.sessions.len() as u64)))
                .collect(),
        }
    }
}

/// A crash-safe memory store backed by one `.mind` file — the Python face of
/// `embedmind_core::Store`. Opens (creating if absent) with the embedded ONNX
/// model, so vector recall works immediately.
///
/// The engine is a single-writer, `!Sync` type; the `Mutex` makes the Python
/// wrapper `Send + Sync` (PyO3 requires it) and serializes concurrent access
/// from Python threads onto the one writer, matching the engine's contract
/// (`docs/adr/0006`).
#[pyclass(name = "Store")]
struct PyStore {
    inner: Mutex<CoreStore>,
}

#[pymethods]
impl PyStore {
    /// Opens the store at `path`, creating it if it does not exist. Vector
    /// recall is enabled via the default embedded model.
    #[new]
    fn new(path: PathBuf) -> PyResult<PyStore> {
        let store = CoreStore::open_or_create(&path).map_err(to_py_err)?;
        Ok(PyStore {
            inner: Mutex::new(store),
        })
    }

    /// Stores one memory durably and returns its id. `metadata` is a
    /// `dict[str, str|int|float|bool|None]` (typed, `docs/FORMAT.md` §5).
    /// `project` scopes it (DESIGN §7); `agent`/`session_id` record provenance
    /// (defaults to agent `"python"`).
    #[pyo3(signature = (content, *, project=None, metadata=None, agent=None, session_id=None))]
    fn remember(
        &self,
        content: String,
        project: Option<String>,
        metadata: Option<&Bound<'_, PyDict>>,
        agent: Option<String>,
        session_id: Option<String>,
    ) -> PyResult<String> {
        let mut draft =
            MemoryDraft::new(content).agent(agent.unwrap_or_else(|| DEFAULT_AGENT.to_string()));
        if let Some(project) = project {
            draft = draft.project(project);
        }
        if let Some(session_id) = session_id {
            draft = draft.session(session_id);
        }
        if let Some(metadata) = metadata {
            for (key, value) in metadata.iter() {
                let key: String = key.extract()?;
                draft = draft.meta(key, scalar_from_py(&value)?);
            }
        }
        let mut store = self.inner.lock().map_err(lock_poisoned)?;
        let memory = store.remember(draft).map_err(to_py_err)?;
        Ok(memory.id.to_string())
    }

    /// Hybrid recall (vector + full-text, RRF-fused — `docs/adr/0005`), best
    /// first. `project` narrows the scope (omit for all projects); `filters` is
    /// a `dict[str, value]` where a scalar means exact match and a `(min, max)`
    /// tuple means a numeric range (S10); `agent` filters by writing agent
    /// (S14). Returns a list of `Recalled`.
    #[pyo3(signature = (query, *, limit=8, project=None, filters=None, agent=None))]
    fn recall(
        &self,
        query: String,
        limit: usize,
        project: Option<String>,
        filters: Option<&Bound<'_, PyDict>>,
        agent: Option<String>,
    ) -> PyResult<Vec<PyRecalled>> {
        let mut q = Query::new(query).limit(limit);
        if let Some(project) = project {
            q = q.scope(Scope::Project(project));
        }
        if let Some(agent) = agent {
            q = q.agent(agent);
        }
        if let Some(filters) = filters {
            let mut map = BTreeMap::new();
            for (key, value) in filters.iter() {
                let key: String = key.extract()?;
                map.insert(key, filter_from_py(&value)?);
            }
            q = q.filters(map);
        }
        // Recall does not mutate, but the engine's `Store` is `!Sync`, so it
        // still goes through the same lock (which serializes Python threads
        // onto the single writer, `docs/adr/0006`).
        let hits = {
            let store = self.inner.lock().map_err(lock_poisoned)?;
            store.recall(q).map_err(to_py_err)?
        };
        Ok(hits.into_iter().map(PyRecalled::from_hit).collect())
    }

    /// Soft-deletes one memory by id (tombstone; space returns on `vacuum`,
    /// `docs/adr/0003`). Returns `True` if a live memory was forgotten, `False`
    /// if the id was unknown or already forgotten. A malformed id is a
    /// `ValueError`.
    fn forget(&self, id: &str) -> PyResult<bool> {
        let id = Ulid::from_string(id)
            .map_err(|_| PyValueError::new_err(format!("'{id}' is not a valid memory id")))?;
        let mut store = self.inner.lock().map_err(lock_poisoned)?;
        store.forget(id).map_err(to_py_err)
    }

    /// File size, counts and index health — the same numbers as
    /// `embedmind stats`, including the S14 per-agent provenance breakdown.
    fn stats(&self) -> PyResult<PyStats> {
        let store = self.inner.lock().map_err(lock_poisoned)?;
        let stats = store.stats().map_err(to_py_err)?;
        Ok(PyStats::from_stats(stats))
    }

    /// Reclaims space from forgotten memories and rebuilds the indexes
    /// (`docs/adr/0003`, S11). The store stays usable afterward.
    fn vacuum(&self) -> PyResult<()> {
        let mut store = self.inner.lock().map_err(lock_poisoned)?;
        store.vacuum().map_err(to_py_err)
    }

    fn __repr__(&self) -> String {
        "Store(<embedmind .mind>)".to_string()
    }
}

/// A poisoned lock means a prior call panicked while holding the store — an
/// engine bug (production paths never panic). Surface it as a `RuntimeError`
/// rather than propagating a Rust panic across the FFI boundary.
fn lock_poisoned<T>(_: std::sync::PoisonError<T>) -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(
        "store is in an inconsistent state after a prior failure",
    )
}

/// The `embedmind` extension module.
#[pymodule]
fn embedmind(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyStore>()?;
    m.add_class::<PyRecalled>()?;
    m.add_class::<PyStats>()?;
    m.add_class::<PyAgentStats>()?;
    Ok(())
}
