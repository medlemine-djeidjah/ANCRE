//! The hyper serve loop.

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::proxy::{GatewayState, handle};
use crate::upstream::Upstream;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A concrete error for the service.
///
/// `service_fn` needs an error that converts to a boxed error for *any*
/// lifetime, which a `Box<dyn Error>` cannot satisfy. Flattening to a message
/// at the connection boundary is fine — the error is logged and the connection
/// closed; nothing downstream inspects it.
#[derive(Debug)]
pub struct ServeError(String);

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ServeError {}

/// Bind and serve until `shutdown` resolves.
///
/// The bind happens **after** the resolver is installed, never before. A
/// listening socket with no snapshot behind it would accept traffic it can
/// only refuse — cold start fails closed, and the honest way to express that
/// at the socket level is not to be listening yet (spec §6).
///
/// # Errors
/// If the address cannot be bound.
pub async fn serve<U>(
    addr: SocketAddr,
    state: Arc<GatewayState<U>>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), BoxError>
where
    U: Upstream + 'static,
    U::Body: Send,
    <U::Body as http_body::Body>::Error: std::fmt::Display + Into<BoxError>,
{
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "gateway listening");

    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = &mut shutdown => {
                tracing::info!("shutting down; in-flight requests will finish");
                return Ok(());
            }
        };

        let state = Arc::clone(&state);
        tokio::task::spawn(async move {
            let service = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move {
                    handle(&state, req)
                        .await
                        .map_err(|e| ServeError(e.to_string()))
                }
            });

            if let Err(e) = http1::Builder::new()
                // Streaming is the norm here, and Nagle's algorithm delays the
                // first token by up to 40ms waiting for a full segment. That
                // alone would blow the TTFT budget the product is sold on.
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                // A client hanging up mid-stream is normal traffic, not an
                // incident. The audit event is still written by the tap.
                tracing::debug!(%peer, error = %e, "connection closed");
            }
        });
    }
}
