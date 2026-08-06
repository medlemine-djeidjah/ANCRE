//! Ingester entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    // TODO(M4): NATS JetStream consumer and the ClickHouse `EventStore` impl.
    // The chaining pipeline, the retry-and-rollback behaviour and the daily
    // heartbeat are implemented and tested — this is the transport wiring.
    eprintln!(
        "ancre-ingester: transport not wired yet. The chaining pipeline is \
         implemented and tested — see `cargo test -p ancre-ingester`."
    );
    ExitCode::FAILURE
}
