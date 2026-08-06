//! HTTP API. axum — ergonomics win here, this is not the hot path.
//!
//! The eval worker (Python, V1) talks to this over the same API a customer
//! would use. Dogfooding the public API is how it stays good (PRD §10).

use axum::Router;

/// MVP surface, deliberately thin:
///   GET  /healthz
///   GET  /v1/snapshot            — current spec + generation, for the poll backstop
///   GET  /v1/prompts/{hash}      — lazy prompt body fetch
///   GET  /v1/checkpoints/{chain} — signed checkpoints for the verifier
///   GET  /v1/pubkeys             — every public key that was ever valid, with windows
///
/// Registry CRUD, oversight, and evidence-pack export are V1.
pub fn router() -> Router {
    todo!("M4")
}
