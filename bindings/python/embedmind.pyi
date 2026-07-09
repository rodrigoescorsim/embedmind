"""Type stubs for the EmbedMind Python bindings (B5).

Kept in sync by hand with `src/lib.rs` — the pyclass/pymethods surface there is
the source of truth. `Scalar` is the typed-metadata union the engine stores
(`docs/FORMAT.md` §5); `FilterValue` is what `recall(filters=...)` accepts.
"""

from typing import Optional, Union

__version__: str

# Typed metadata values (docs/FORMAT.md §5): str, int, float, bool, or None.
Scalar = Union[str, int, float, bool, None]
# A recall filter is either an exact-match scalar or an inclusive numeric
# range `(min, max)`; either bound may be None for an open side.
FilterValue = Union[Scalar, tuple[Optional[float], Optional[float]]]

class Recalled:
    """One recall hit: a memory plus its fused relevance score."""

    id: str
    content: str
    project: Optional[str]
    score: float
    agent: str
    session_id: Optional[str]
    created_at_micros: int
    metadata: dict[str, Scalar]
    def __repr__(self) -> str: ...

class AgentStats:
    """One agent's slice of the provenance breakdown (S14)."""

    live_memories: int
    sessions: int

class Stats:
    """Counts, file size and index health — the `embedmind stats` numbers."""

    live_memories: int
    forgotten_memories: int
    index_entries: int
    fts_documents: int
    graph_entities: int
    graph_relations: int
    page_size: int
    page_count: int
    file_bytes: int
    embedding_model_id: Optional[str]
    embedding_dims: int
    by_agent: dict[str, AgentStats]
    def __repr__(self) -> str: ...

class Store:
    """A crash-safe memory store backed by one `.mind` file."""

    def __init__(self, path: str) -> None:
        """Open the store at `path`, creating it if absent."""

    def remember(
        self,
        content: str,
        *,
        project: Optional[str] = ...,
        metadata: Optional[dict[str, Scalar]] = ...,
        agent: Optional[str] = ...,
        session_id: Optional[str] = ...,
    ) -> str:
        """Store one memory durably; returns its ULID id."""

    def recall(
        self,
        query: str,
        *,
        limit: int = ...,
        project: Optional[str] = ...,
        filters: Optional[dict[str, FilterValue]] = ...,
        agent: Optional[str] = ...,
    ) -> list[Recalled]:
        """Hybrid (vector + full-text) recall, best first."""

    def forget(self, id: str) -> bool:
        """Soft-delete one memory by id; True if a live memory was forgotten."""

    def stats(self) -> Stats:
        """File size, counts and index health."""

    def vacuum(self) -> None:
        """Reclaim space from forgotten memories and rebuild the indexes."""

    def __repr__(self) -> str: ...
