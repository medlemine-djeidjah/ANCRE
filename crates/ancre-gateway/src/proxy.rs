//! The hot path.
//!
//! Raw hyper rather than axum here: axum's extractors cost more than they are
//! worth on a proxy that does not want to parse the body at all. axum is used
//! in the control plane, where ergonomics win (PRD §10).

use ancre_types::RequestCtx;

/// Serve one request.
///
/// Order matters and is not negotiable:
/// 1. hash the key
/// 2. **resolve pins once** into `RequestCtx`
/// 3. call the provider, streaming straight through
/// 4. fork telemetry — after the response is on its way, never before
///
/// Step 2 happens exactly once. Re-resolving before writing the audit event is
/// the bug that eats this whole design (spec §5).
pub async fn handle() -> Result<(), ProxyError> {
    todo!("M3")
}

/// Streamed. The response body is forwarded byte-for-byte as it arrives; the
/// pin-capture pass reads frames on their way past and never holds one back.
async fn stream_through(_ctx: &RequestCtx) -> Result<(), ProxyError> {
    todo!("M3: no buffering — first byte out before the second is read")
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("resolve: {0}")]
    Resolve(#[from] ancre_resolver::ResolveError),
    #[error("provider: {0}")]
    Provider(#[from] ancre_provider::ProviderError),
    #[error("client disconnected mid-stream")]
    ClientGone,
}

// M3 done-when (mvp-plan §5): an end-to-end p99 overhead measurement against a
// **null-gateway baseline** — the same binary with pinning compiled out.
//
// Measuring against direct-to-provider conflates the gateway's cost with
// network variance and produces a number that falls apart the first time a
// prospect's own engineer reproduces it. Overhead only means anything as a
// delta against an identical path.
