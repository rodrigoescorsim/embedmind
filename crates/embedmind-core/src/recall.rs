//! Hybrid recall: score fusion across vector and full-text result lists.
//!
//! v0.1 is vector-only (plus project/tombstone filtering). From M2 on, fusion
//! uses Reciprocal Rank Fusion with k=60 (`docs/adr/0005`) — rank positions
//! only, no score normalization, no tunable weights.
