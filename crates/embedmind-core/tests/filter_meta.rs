//! FTOPT-1 equivalence suite (`docs/adr/0027`): `search_text` through the
//! filter-meta sidecar must return **exactly** what the pre-sidecar record
//! path returns — same ids, same scores, same order — for every query shape
//! `keep` distinguishes (scope, agent, metadata filters, tombstones,
//! superseded). The oracle is a genuine format_version-6 file (created via
//! `PagerOptions::format_version`, the same knob the FTS cross-version tests
//! use) fed the identical workload: its `keep` runs the full record path
//! because a v6 file has no sidecar.
//!
//! No embedder: `search_text` exercises the exact `keep`/`doc_len` closures
//! hybrid recall shares, without paying ONNX startup per test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use embedmind_core::Ulid;
use embedmind_core::api::{MemoryDraft, Query, Scope, Store, StoreOptions};
use embedmind_core::record::{Filter, Scalar};
use embedmind_core::storage::sim::SimVfs;
use embedmind_core::storage::vfs::{OpenMode, Vfs};
use embedmind_core::storage::{Pager, PagerOptions};

const STORE: &str = "memory.mind";

fn kv_options() -> StoreOptions {
    StoreOptions {
        page_size: 512, // small pages: the sidecar chains span real pages
        ..StoreOptions::default()
    }
}

/// A store on a fresh in-memory VFS at the current format version (7).
fn v7_store() -> (SimVfs, Store) {
    let vfs = SimVfs::new();
    let store = Store::create_with(Arc::new(vfs.clone()), Path::new(STORE), kv_options()).unwrap();
    (vfs, store)
}

/// A store over a genuine format_version-6 file: created raw via the pager
/// (the version knob lives there), then opened through the public API — the
/// same recipe the FTS cross-version tests use. Such a file has no sidecar,
/// so its `keep` is the full-record oracle.
fn v6_store() -> (SimVfs, Store) {
    let vfs = SimVfs::new();
    let shared: Arc<dyn Vfs> = Arc::new(vfs.clone());
    let pager = Pager::create(
        Arc::clone(&shared),
        Path::new(STORE),
        PagerOptions {
            page_size: 512,
            format_version: 6,
            ..Default::default()
        },
    )
    .unwrap();
    pager.close().unwrap();
    let store = Store::open_with(shared, Path::new(STORE), kv_options()).unwrap();
    (vfs, store)
}

/// The shared workload: several projects and agents, metadata on some
/// memories, one forget, one supersede — every `keep` branch has records on
/// both sides. Returns the id it forgot, for spot checks.
fn populate(store: &mut Store) -> Ulid {
    for i in 0..30 {
        let project = match i % 3 {
            0 => None,
            1 => Some("alpha"),
            _ => Some("beta"),
        };
        let agent = if i % 2 == 0 { "cli" } else { "claude-code" };
        let mut draft = MemoryDraft::new(format!(
            "shared corpus memory {i} — filter meta equivalence"
        ))
        .agent(agent);
        if let Some(p) = project {
            draft = draft.project(p);
        }
        if i % 5 == 0 {
            draft = draft.meta("topic", Scalar::Str("ops".into()));
        }
        if i % 7 == 0 {
            draft = draft.meta("stars", Scalar::I64(i));
        }
        store.remember(draft).unwrap();
    }
    let victim = store
        .remember(MemoryDraft::new("shared corpus memory to forget").project("alpha"))
        .unwrap();
    store.forget(victim.id).unwrap();
    let old = store
        .remember(MemoryDraft::new("shared corpus memory old version").project("beta"))
        .unwrap();
    store
        .remember(
            MemoryDraft::new("shared corpus memory new version")
                .project("beta")
                .supersede(old.id),
        )
        .unwrap();
    victim.id
}

/// The query shapes `keep` distinguishes: unscoped, scoped (hit and miss),
/// agent-filtered (hit, miss, and the empty-string edge), metadata-filtered
/// (matching, absent-key), and combinations.
fn query_matrix() -> Vec<Query> {
    vec![
        Query::new("shared corpus memory").limit(50),
        Query::new("shared corpus memory")
            .limit(50)
            .scope(Scope::Project("alpha".into())),
        Query::new("shared corpus memory")
            .limit(50)
            .scope(Scope::Project("beta".into())),
        Query::new("shared corpus memory")
            .limit(50)
            .scope(Scope::Project("no-such-project".into())),
        Query::new("shared corpus memory")
            .limit(50)
            .scope(Scope::Project(String::new())),
        Query::new("shared corpus memory").limit(50).agent("cli"),
        Query::new("shared corpus memory")
            .limit(50)
            .agent("no-such-agent"),
        Query::new("shared corpus memory").limit(50).agent(""),
        Query::new("shared corpus memory")
            .limit(50)
            .filter("topic", Filter::Eq(Scalar::Str("ops".into()))),
        Query::new("shared corpus memory").limit(50).filter(
            "stars",
            Filter::Range {
                min: Some(5.0),
                max: Some(25.0),
            },
        ),
        Query::new("shared corpus memory")
            .limit(50)
            .filter("absent-key", Filter::Eq(Scalar::Bool(true))),
        Query::new("shared corpus memory")
            .limit(50)
            .scope(Scope::Project("alpha".into()))
            .agent("cli")
            .filter("topic", Filter::Eq(Scalar::Str("ops".into()))),
    ]
}

/// `(id, score)` pairs — the full observable result, order included.
fn results(store: &Store, query: Query) -> Vec<(Ulid, f32)> {
    store
        .search_text(query)
        .unwrap()
        .into_iter()
        .map(|r| (r.memory.id, r.score))
        .collect()
}

/// Reads the on-disk `format_version` (header offset 8, little-endian).
fn file_format_version(vfs: &SimVfs) -> u32 {
    let file = vfs.open(Path::new(STORE), OpenMode::MustExist).unwrap();
    let mut prefix = [0u8; 12];
    file.read_at(&mut prefix, 0).unwrap();
    u32::from_le_bytes(prefix[8..12].try_into().unwrap())
}

#[test]
fn search_text_matches_the_v6_record_path_oracle_exactly() {
    // ULIDs are random per store, so the two stores' ids differ and ties
    // inside one BM25 score (broken by id) may order differently — compare
    // by (score, content) with ties normalized instead. Exact same-store
    // tie order under an arbitrary `keep` is already covered by the BMW-2
    // three-way equivalence proptest (`fts.rs`).
    let (_, mut v7) = v7_store();
    let (_, mut v6) = v6_store();
    populate(&mut v7);
    populate(&mut v6);
    let normalized = |store: &Store, query: Query| -> Vec<(u32, String)> {
        let mut out: Vec<(u32, String)> = store
            .search_text(query)
            .unwrap()
            .into_iter()
            .map(|r| (r.score.to_bits(), r.memory.content))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        out
    };
    for query in query_matrix() {
        let shape = format!("{query:?}");
        assert_eq!(
            normalized(&v7, query.clone()),
            normalized(&v6, query),
            "query {shape}"
        );
    }
}

#[test]
fn forget_supersede_and_reopen_are_reflected_through_the_sidecar() {
    let (vfs, mut store) = v7_store();
    let forgotten = populate(&mut store);
    store.verify_filter_meta_invariant().unwrap();

    let all = results(&store, Query::new("shared corpus memory").limit(50));
    assert!(!all.is_empty());
    assert!(
        all.iter().all(|(id, _)| *id != forgotten),
        "a forgotten memory must never surface"
    );
    let beta: Vec<(Ulid, f32)> = results(
        &store,
        Query::new("shared corpus memory old version new")
            .limit(50)
            .scope(Scope::Project("beta".into())),
    );
    let contents: Vec<String> = beta
        .iter()
        .map(|(id, _)| store.get(*id).unwrap().unwrap().content)
        .collect();
    assert!(
        contents.iter().any(|c| c.contains("new version")),
        "the superseding memory must be searchable: {contents:?}"
    );
    assert!(
        !contents.iter().any(|c| c.contains("old version")),
        "a superseded memory must never surface: {contents:?}"
    );

    // Reopen (WAL recovery + a cold sidecar cache): identical results.
    let before = results(&store, Query::new("shared corpus memory").limit(50));
    drop(store);
    let store = Store::open_with(Arc::new(vfs.clone()), Path::new(STORE), kv_options()).unwrap();
    store.verify_filter_meta_invariant().unwrap();
    assert_eq!(
        before,
        results(&store, Query::new("shared corpus memory").limit(50)),
        "reopen must not change results"
    );
}

#[test]
fn vacuum_upgrades_a_v6_file_and_preserves_results() {
    let (vfs, mut store) = v6_store();
    populate(&mut store);
    assert_eq!(file_format_version(&vfs), 6);
    // A v6 file has no sidecar: the invariant passes trivially (root 0).
    store.verify_filter_meta_invariant().unwrap();

    // Vacuum purges tombstoned docs from the full-text index, which shifts
    // BM25's doc_count/IDF — scores legitimately change. What the upgrade
    // must preserve is the *hit set* per query: nothing lost, nothing
    // resurrected.
    let id_set = |store: &Store, query: Query| -> Vec<Ulid> {
        let mut ids: Vec<Ulid> = results(store, query)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        ids
    };
    let mut expected: Vec<Vec<Ulid>> = Vec::new();
    for query in query_matrix() {
        expected.push(id_set(&store, query));
    }
    store.vacuum().unwrap();
    assert_eq!(
        file_format_version(&vfs),
        8,
        "vacuum's rebuild-by-copy is the upgrade path"
    );
    store.verify_filter_meta_invariant().unwrap();
    for (query, want) in query_matrix().into_iter().zip(expected) {
        let shape = format!("{query:?}");
        assert_eq!(id_set(&store, query), want, "query {shape}");
    }
}
