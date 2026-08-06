//! Control plane entry point.

use std::process::ExitCode;

fn main() -> ExitCode {
    // TODO(M5): the Postgres `Registry`, the ClickHouse `ChainSource` and the
    // NATS `SnapshotBus`. Snapshot build and publication, generation
    // allocation, checkpoint scheduling, key rotation and the read API are
    // implemented and tested against in-memory implementations of those three
    // traits — this is the transport wiring, and it lands with the packaging
    // that makes it testable against real infrastructure.
    eprintln!(
        "ancre-control: no datastores wired yet. Snapshot build, publication, \
         checkpointing and the API are implemented and tested — see \
         `cargo test -p ancre-control`."
    );
    ExitCode::FAILURE
}
