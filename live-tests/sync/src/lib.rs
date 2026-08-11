//! Wallet-driven chain-sync tests with Zaino as the system under test.
//!
//! The suite lives in `tests/`; each test is a `#[ztest::sync_test]` profile —
//! a continuous-monitor run that a wallet drives to tip through a validator +
//! Zaino, asserting chain invariants throughout and the note-commitment-tree
//! roots at completion. Profiles are launched *detached* via
//! `ztest sync start <name>` (design §"Execution model: ztest-owned pods"), so
//! they outlive the launching terminal.
//!
//! This crate carries no library code; it exists to host the sync suite on the
//! ztest harness, driven by ztest's default in-process librustzcash wallet (no
//! zingolib in the graph — see `Cargo.toml`).
