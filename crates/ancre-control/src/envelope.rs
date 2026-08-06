//! Publication of sealed snapshots.
//!
//! The envelope type itself lives in `ancre-types`: both ends need it, and the
//! gateway must not depend on the control plane to read its own configuration.
//! What is here is where a sealed envelope *goes*.

use ancre_types::SnapshotEnvelope;

use crate::registry::ControlError;

/// A trait so publication is testable without a broker, exactly as `EventSink`
/// is in the gateway.
///
/// The NATS implementation publishes JSON on `SNAPSHOT_SUBJECT`, retained, so
/// a gateway that starts after the last publish gets the current generation
/// without waiting for the next one. The poll backstop covers the rest.
pub trait SnapshotBus: Send + Sync {
    fn publish(
        &self,
        envelope: SnapshotEnvelope,
    ) -> impl std::future::Future<Output = Result<(), ControlError>> + Send;
}

#[cfg(feature = "testing")]
pub mod testing {
    //! An in-memory bus that can be knocked over on command.

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{ControlError, SnapshotBus, SnapshotEnvelope};

    #[derive(Debug, Default)]
    pub struct MemoryBus {
        sent: Mutex<Vec<SnapshotEnvelope>>,
        down: AtomicBool,
    }

    impl MemoryBus {
        pub fn kill(&self) {
            self.down.store(true, Ordering::SeqCst);
        }

        pub fn revive(&self) {
            self.down.store(false, Ordering::SeqCst);
        }

        #[must_use]
        pub fn count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        #[must_use]
        pub fn last(&self) -> Option<SnapshotEnvelope> {
            self.sent.lock().unwrap().last().cloned()
        }
    }

    impl SnapshotBus for MemoryBus {
        async fn publish(&self, envelope: SnapshotEnvelope) -> Result<(), ControlError> {
            if self.down.load(Ordering::SeqCst) {
                return Err(ControlError::Bus("no connection".into()));
            }
            self.sent.lock().unwrap().push(envelope);
            Ok(())
        }
    }
}
