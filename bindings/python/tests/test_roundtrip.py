"""Cross-language round-trip: a `.mind` written by the Rust CLI must read back
identically through the Python bindings, and vice-versa. This is the heart of
the B5 DoD — "same `.mind` files readable by Rust and Python" — and it holds
for free because both are shells over the *same* engine, not two format
implementations. If it ever breaks, the binary format compatibility promise
(CLAUDE.md: never break the format) has been violated.

The Rust side is driven through the installed `embedmind` CLI binary. Point
`EMBEDMIND_BIN` at it (the release build does); the tests skip cleanly if no
binary is found, so `pytest` still passes on a machine that only built the
wheel.
"""

import os
import subprocess
from pathlib import Path

import pytest

import embedmind


def _find_cli():
    """Locate the `embedmind` CLI binary: `EMBEDMIND_BIN` first, then the
    workspace's release build. Returns None if neither exists."""
    env = os.environ.get("EMBEDMIND_BIN")
    if env and Path(env).exists():
        return env
    # bindings/python/tests/ -> repo root is three parents up.
    root = Path(__file__).resolve().parents[3]
    for name in ("embedmind.exe", "embedmind"):
        candidate = root / "target" / "release" / name
        if candidate.exists():
            return str(candidate)
    return None


CLI = _find_cli()
requires_cli = pytest.mark.skipif(
    CLI is None,
    reason="embedmind CLI binary not found (set EMBEDMIND_BIN or build --release)",
)


def _cli(mind_path, *args):
    """Runs `embedmind --file <path> <args>` and returns stdout, raising on a
    non-zero exit with the captured stderr for diagnosis."""
    result = subprocess.run(
        [CLI, "--file", str(mind_path), *args],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise AssertionError(
            f"CLI {' '.join(args)} failed ({result.returncode}): {result.stderr}"
        )
    return result.stdout


@requires_cli
def test_rust_writes_python_reads(tmp_path):
    """Remember via the Rust CLI, recall via Python over the same file."""
    mind = tmp_path / "rust-to-py.mind"
    # `--global` so the CLI does not scope the memory to a detected project,
    # keeping the assertion independent of the cwd's git root.
    _cli(mind, "remember", "the WAL is checkpointed on clean close", "--global")

    store = embedmind.Store(str(mind))
    hits = store.recall("write-ahead log durability")
    assert len(hits) >= 1
    assert any("WAL is checkpointed" in h.content for h in hits)
    # The CLI stamps agent "cli" (main.rs `remember`): provenance survives the
    # crossing.
    assert any(h.agent == "cli" for h in hits)


@requires_cli
def test_python_writes_rust_reads(tmp_path):
    """Remember via Python, recall + stats via the Rust CLI over the same file."""
    mind = tmp_path / "py-to-rust.mind"
    store = embedmind.Store(str(mind))
    mid = store.remember(
        "HNSW graph lives in the single file", agent="python", session_id="s9"
    )
    del store  # release the writer lock so the CLI can open it

    # Recall through the CLI finds the Python-written memory.
    out = _cli(mind, "recall", "approximate nearest neighbor index", "--all")
    assert mid in out

    # Stats through the CLI counts it and attributes it to the "python" agent.
    stats_out = _cli(mind, "stats")
    assert "live memories:      1" in stats_out
    assert "python" in stats_out


@requires_cli
def test_forget_crosses_the_boundary(tmp_path):
    """A memory written in Python and forgotten via the CLI is gone when read
    back in Python — tombstones are the same bytes on both sides."""
    mind = tmp_path / "forget-cross.mind"
    store = embedmind.Store(str(mind))
    mid = store.remember("temporary note", agent="python")
    del store

    _cli(mind, "forget", mid)

    store = embedmind.Store(str(mind))
    assert store.stats().live_memories == 0
    assert store.stats().forgotten_memories == 1
    assert all(h.id != mid for h in store.recall("temporary note"))
