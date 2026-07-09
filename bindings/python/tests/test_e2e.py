"""End-to-end tests for the Python bindings (B5), mirroring the CLI's E2E
coverage: remember → recall → forget → stats → vacuum, plus the metadata,
provenance and filter surfaces the CLI exercises. These are the pytest half of
the story's DoD; the Rust↔Python round-trip over one shared `.mind` lives in
``test_roundtrip.py``.

Each test gets its own throwaway `.mind` in a tmp dir, so nothing leaks between
tests and the embedded model is the only shared (process-wide) state.
"""

import pytest

import embedmind


@pytest.fixture
def store(tmp_path):
    """A fresh store on a per-test `.mind` file."""
    return embedmind.Store(str(tmp_path / "memory.mind"))


def test_module_surface():
    assert hasattr(embedmind, "__version__")
    for name in ("Store", "Recalled", "Stats", "AgentStats"):
        assert hasattr(embedmind, name)


def test_remember_returns_ulid(store):
    mid = store.remember("hello world")
    # ULIDs are 26-char Crockford base32.
    assert isinstance(mid, str)
    assert len(mid) == 26


def test_remember_recall_roundtrip(store):
    store.remember(
        "prefers explicit errors over panics",
        project="embedmind",
        metadata={"topic": "conventions", "weight": 3},
        agent="claude-code",
        session_id="sess-42",
    )
    hits = store.recall("error handling style", project="embedmind")
    assert len(hits) == 1
    hit = hits[0]
    assert hit.content == "prefers explicit errors over panics"
    assert hit.project == "embedmind"
    assert hit.agent == "claude-code"
    assert hit.session_id == "sess-42"
    assert hit.score > 0.0
    assert hit.created_at_micros > 0
    # Metadata round-trips to the same Python types it went in as.
    assert hit.metadata == {"topic": "conventions", "weight": 3}
    assert isinstance(hit.metadata["weight"], int)


def test_metadata_types_roundtrip(store):
    store.remember(
        "typed metadata",
        metadata={"s": "text", "i": 7, "f": 1.5, "b": True, "n": None},
    )
    (hit,) = store.recall("typed metadata")
    md = hit.metadata
    assert md["s"] == "text" and isinstance(md["s"], str)
    assert md["i"] == 7 and isinstance(md["i"], int)
    assert md["f"] == 1.5 and isinstance(md["f"], float)
    assert md["b"] is True
    assert md["n"] is None


def test_recall_default_agent_is_python(store):
    store.remember("no explicit agent")
    (hit,) = store.recall("no explicit agent")
    assert hit.agent == "python"


def test_recall_limit_caps_results(store):
    for i in range(10):
        store.remember(f"note number {i}")
    hits = store.recall("note number", limit=3)
    assert len(hits) == 3


def test_recall_project_scope_isolates(store):
    store.remember("alpha memory", project="alpha")
    store.remember("beta memory", project="beta")
    # Scoped to alpha: beta must not appear.
    hits = store.recall("memory", project="alpha")
    assert [h.project for h in hits] == ["alpha"]
    # No project arg searches everything.
    all_hits = store.recall("memory")
    assert {h.project for h in all_hits} == {"alpha", "beta"}


def test_recall_metadata_eq_filter(store):
    store.remember("first", metadata={"kind": "a"})
    store.remember("second", metadata={"kind": "b"})
    hits = store.recall("first second", filters={"kind": "a"})
    assert [h.content for h in hits] == ["first"]


def test_recall_metadata_range_filter(store):
    store.remember("light", metadata={"weight": 1})
    store.remember("heavy", metadata={"weight": 9})
    # Closed range.
    hits = store.recall("light heavy", filters={"weight": (5, 10)})
    assert [h.content for h in hits] == ["heavy"]
    # Open upper bound.
    hits = store.recall("light heavy", filters={"weight": (None, 5)})
    assert [h.content for h in hits] == ["light"]


def test_recall_filter_type_mismatch_raises(store):
    store.remember("stringy", metadata={"weight": "not a number"})
    with pytest.raises(ValueError):
        store.recall("stringy", filters={"weight": (0, 10)})


def test_recall_agent_filter(store):
    store.remember("by alice", agent="alice")
    store.remember("by bob", agent="bob")
    hits = store.recall("by", agent="alice")
    assert [h.content for h in hits] == ["by alice"]


def test_forget_tombstones(store):
    mid = store.remember("to forget")
    store.remember("to keep")
    assert store.forget(mid) is True
    # Forgetting again is a no-op (already gone), never an error.
    assert store.forget(mid) is False
    contents = [h.content for h in store.recall("to")]
    assert "to forget" not in contents
    assert "to keep" in contents


def test_forget_unknown_id_returns_false(store):
    # A well-formed but absent id: not an error, just False.
    assert store.forget("00000000000000000000000000") is False


def test_forget_malformed_id_raises(store):
    with pytest.raises(ValueError):
        store.forget("not-a-ulid")


def test_stats(store):
    a = store.remember("one", agent="alice", session_id="s1")
    store.remember("two", agent="alice", session_id="s2")
    store.remember("three", agent="bob")
    store.forget(a)

    s = store.stats()
    assert s.live_memories == 2
    assert s.forgotten_memories == 1
    assert s.file_bytes > 0
    assert s.page_count > 0
    assert s.embedding_model_id is not None
    assert s.embedding_dims == 384

    # Provenance breakdown (S14): alice has 1 live (the other was forgotten)
    # across the sessions still live; bob has 1.
    by_agent = s.by_agent
    assert set(by_agent) == {"alice", "bob"}
    assert by_agent["alice"].live_memories == 1
    assert by_agent["bob"].live_memories == 1
    assert isinstance(by_agent["alice"].sessions, int)


def test_vacuum_reclaims_forgotten(store):
    ids = [store.remember(f"memory {i}") for i in range(6)]
    for mid in ids[:3]:
        store.forget(mid)
    assert store.stats().forgotten_memories == 3
    store.vacuum()
    after = store.stats()
    assert after.forgotten_memories == 0
    assert after.live_memories == 3


def test_persists_across_reopen(tmp_path):
    path = str(tmp_path / "persist.mind")
    s1 = embedmind.Store(path)
    mid = s1.remember("durable memory", project="p")
    del s1  # drop the writer lock

    s2 = embedmind.Store(path)
    hits = s2.recall("durable memory", project="p")
    assert [h.id for h in hits] == [mid]
