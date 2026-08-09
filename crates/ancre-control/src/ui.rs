//! The dashboard, served out of the binary.
//!
//! No fourth container. PRD §10 caps production at three Ancre services and
//! treats that ceiling as a customer-adoption constraint rather than a
//! preference, so the dashboard is static assets compiled into `ancre-control`
//! and served from the axum router it already runs. One origin, so no CORS and
//! no second place for a session cookie to go wrong; one artefact, so the UI
//! and the API it talks to cannot be different versions of themselves.
//!
//! React, Vite and Tailwind stay **build-time** dependencies. Nothing in the
//! shipped image needs Node.
//!
//! ## When the assets are not there
//!
//! `ui/dist` is a build artefact, and `cargo build` has to work in a checkout
//! where nobody has run `npm run build`. So the directory is committed empty
//! and a missing `index.html` is answered with instructions rather than a 404 —
//! the failure is "you have not built the UI", and saying so beats a blank page
//! that reads as a broken deployment.

use axum::Router;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "ui/dist"]
struct Assets;

/// The SPA, plus every asset it references.
///
/// A catch-all rather than a file-server directory: the dashboard is a
/// single-page app, so `/chains/acme/hr-screening` is a route the *client*
/// resolves and the server has never heard of. Anything that is not a real
/// asset gets `index.html` and lets the router in the browser decide — except
/// under `/v1` and `/api`, which are the server's own and must 404 honestly
/// rather than hand a JSON client a page of HTML.
pub fn router() -> Router {
    Router::new().fallback(get(serve))
}

async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // An API path that reached the fallback is a genuine 404. Returning the
    // SPA here would mean a mistyped endpoint answers 200 with HTML, and the
    // caller parses it as JSON and reports something unrelated.
    if path.starts_with("v1/") || path.starts_with("api/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"no such endpoint"}"#,
        )
            .into_response();
    }

    if let Some(asset) = Assets::get(path) {
        return with_headers(path, asset);
    }

    match Assets::get("index.html") {
        Some(index) => with_headers("index.html", index),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            UNBUILT,
        )
            .into_response(),
    }
}

fn with_headers(path: &str, asset: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();

    // Vite fingerprints every asset filename, so an immutable year-long cache
    // is safe for them and wrong for `index.html` — which keeps the same name
    // and is what points at the new fingerprints after a deploy.
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };

    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache),
        ],
        asset.data,
    )
        .into_response()
}

/// Shown when the binary was built without the UI having been built first.
const UNBUILT: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>Ancre — dashboard not built</title>
<style>
  body { font: 15px/1.6 ui-monospace, SFMono-Regular, Menlo, monospace;
         max-width: 46rem; margin: 12vh auto; padding: 0 1.5rem;
         color: #e7e5e4; background: #0c0a09; }
  h1 { font-size: 1.1rem; font-weight: 600; letter-spacing: -0.01em; }
  code { background: #1c1917; padding: 0.15rem 0.4rem; border-radius: 4px; }
  pre { background: #1c1917; padding: 1rem; border-radius: 8px; overflow-x: auto; }
  a { color: #a8a29e; }
</style>
<h1>The dashboard was not built into this binary.</h1>
<p>The API is running normally — this only affects the web interface.</p>
<pre>cd ui
npm install
npm run build
cargo build -p ancre-control</pre>
<p><code>deploy/compose/Dockerfile</code> does this for you; a local
<code>cargo run</code> does not.</p>
<p>The API is unaffected: <a href="/healthz">/healthz</a>,
<code>/v1/pubkeys</code>, <code>/v1/chains</code>.</p>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_path(path: &str) -> (StatusCode, String) {
        let response = router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The property that keeps a mistyped endpoint honest. Without it, every
    /// wrong API path answers 200 with HTML and the client reports a JSON
    /// parse error somewhere far away from the cause.
    #[tokio::test]
    async fn an_unknown_api_path_is_a_json_404_not_the_app_shell() {
        let (status, body) = get_path("/v1/nonsense").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("no such endpoint"), "{body}");
        assert!(!body.contains("<!doctype html>"), "{body}");

        let (status, _) = get_path("/api/nonsense").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A client-side route must reach the app shell, not a 404 — that is the
    /// whole point of a single-page app served from one binary.
    ///
    /// In a checkout with no built UI the honest answer is the instructions,
    /// so this asserts on the pair rather than on one of them: either the
    /// shell, or a page that says how to produce it. Never a bare 404.
    #[tokio::test]
    async fn a_client_side_route_gets_the_app_or_an_explanation() {
        let (status, body) = get_path("/chains/acme/hr-screening").await;
        if status == StatusCode::OK {
            assert!(body.contains("<!doctype html>") || body.contains("<!DOCTYPE html>"));
        } else {
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert!(body.contains("was not built"), "{body}");
        }
    }
}
