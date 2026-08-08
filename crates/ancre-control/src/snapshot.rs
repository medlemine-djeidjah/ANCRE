//! Snapshot build and publication.
//!
//! Registry → `SnapshotSpec` → canonical encoding → `content_hash` →
//! generation bump → bus. Gateway nodes also poll every 10s as a backstop,
//! because a missed message must never mean indefinite staleness.

use ancre_canon::Hash32;
use ancre_types::{ConfigSnapshot, SnapshotEnvelope, SnapshotSpec};

use crate::api::SnapshotApi;
use crate::envelope::SnapshotBus;
use crate::registry::{ControlError, Registry};

#[derive(Debug)]
pub struct SnapshotBuilder<R, B> {
    registry: R,
    bus: B,
    gateway_version: String,
}

/// What a publish attempt did. The distinction is the whole reason the poll
/// loop can run every 10 seconds without minting a generation every 10 seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// New content, new generation, published.
    Bumped { generation: u64 },
    /// The registry built to the same bytes as the last publish. No generation
    /// was allocated and nothing was sent.
    ///
    /// Note this is not the same as "nothing happened": it is a positive
    /// statement that the configuration on the wire is current, which is what
    /// makes a quiet control plane distinguishable from a stuck one.
    Unchanged { generation: u64 },
}

impl Published {
    #[must_use]
    pub fn generation(self) -> u64 {
        match self {
            Self::Bumped { generation } | Self::Unchanged { generation } => generation,
        }
    }
}

impl<R: Registry, B: SnapshotBus> SnapshotBuilder<R, B> {
    pub fn new(registry: R, bus: B, gateway_version: impl Into<String>) -> Self {
        Self {
            registry,
            bus,
            gateway_version: gateway_version.into(),
        }
    }

    /// The registry behind this builder. For a caller driving both ends of
    /// the publish path in one process.
    #[must_use]
    pub fn registry(&self) -> &R {
        &self.registry
    }

    #[must_use]
    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// Build the wire form from the registry.
    ///
    /// `generation` is left at 0 here: it is not part of the content, and
    /// allocating it before knowing whether the content changed would burn a
    /// generation on every poll. `publish` stamps the real one.
    ///
    /// The spec is **validated by building a real `ConfigSnapshot` from it**
    /// before anything is published. A snapshot that `ConfigSnapshot::build`
    /// would reject — a key bound to a system that no longer exists, a
    /// `default_route` out of bounds — must fail here, in the control plane,
    /// where the failure is one alert. Published, it is a fleet-wide cold
    /// resolver: every gateway node refusing every request, at once.
    pub async fn build(&self) -> Result<SnapshotSpec, ControlError> {
        let mut spec = SnapshotSpec {
            generation: 0,
            gateway_version: self.gateway_version.clone(),
            systems: self.registry.systems().await?,
            keys: self.registry.keys().await?,
        };
        spec.canonicalize_order();

        // Build and discard. The cost is one snapshot build per publish, off
        // the request path, against the alternative of finding out from a
        // customer that their gateway fleet stopped serving.
        ConfigSnapshot::build(spec.clone()).map_err(|e| ControlError::Invalid(e.to_string()))?;
        Ok(spec)
    }

    /// Content hash of a built spec, over everything but `generation`.
    pub fn content_hash(spec: &SnapshotSpec) -> Result<Hash32, ControlError> {
        ancre_canon::content_hash(&spec.content_view())
            .map_err(|e| ControlError::Canon(e.to_string()))
    }

    /// Build, and publish only if the content actually changed.
    ///
    /// Order matters and is not obvious: the generation is allocated
    /// **before** the bus send, and a failed send is not rolled back. A
    /// generation that was allocated but never reached a gateway is a gap in a
    /// counter, which is harmless — the next publish moves past it. Reusing it
    /// would not be: two different configurations could then share a
    /// generation number, and every pin carrying that number would be
    /// ambiguous forever. Burn the number.
    pub async fn publish(&self) -> Result<Published, ControlError> {
        let mut spec = self.build().await?;
        let hash = Self::content_hash(&spec)?;

        if let Some((generation, published_hash)) = self.registry.published().await?
            && published_hash == hash
        {
            return Ok(Published::Unchanged { generation });
        }

        let generation = self.registry.allocate_generation(hash).await?;
        spec.generation = generation;

        self.bus.publish(SnapshotEnvelope::seal(spec, hash)).await?;
        Ok(Published::Bumped { generation })
    }

    /// For the API's `GET /v1/snapshot`, the poll backstop's source.
    pub async fn current(&self) -> Result<SnapshotEnvelope, ControlError> {
        let mut spec = self.build().await?;
        let hash = Self::content_hash(&spec)?;
        let generation = match self.registry.published().await? {
            // The registry has moved on since the last publish. Serving the
            // new content under the old generation would let a gateway install
            // content its `config_generation` pin does not identify, so serve
            // what was published: the poll is a backstop for a missed message,
            // not a second publication path.
            Some((generation, published_hash)) if published_hash == hash => generation,
            Some((generation, _)) => {
                return Err(ControlError::Invalid(format!(
                    "registry has unpublished changes since generation {generation}; \
                     publish before serving"
                )));
            }
            None => return Err(ControlError::Invalid("nothing published yet".into())),
        };
        spec.generation = generation;
        Ok(SnapshotEnvelope::seal(spec, hash))
    }
}

/// The builder *is* the read API's snapshot source. Both endpoints answer from
/// the registry, and giving the API its own path to it would let the two
/// disagree about what is published.
impl<R: Registry + 'static, B: SnapshotBus + 'static> SnapshotApi for SnapshotBuilder<R, B> {
    async fn current(&self) -> Result<SnapshotEnvelope, ControlError> {
        // The inherent method, which shadows this one at every call site.
        Self::current(self).await
    }

    /// A hash that is not 64 hex characters is a 404 rather than a 503: the
    /// request named something that cannot exist, which is the client's
    /// mistake and not the control plane's condition. The poll backstop
    /// distinguishes the two, so the distinction has to be real.
    async fn prompt(&self, hash: &str) -> Result<Option<Vec<u8>>, ControlError> {
        let Ok(hash) = Hash32::from_hex(hash) else {
            return Ok(None);
        };
        self.registry.prompt(hash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::testing::MemoryBus;
    use crate::registry::testing::MemoryRegistry;

    fn registry() -> MemoryRegistry {
        let spec = ancre_resolver::testing::spec(0);
        MemoryRegistry::new(spec.systems, spec.keys)
    }

    fn builder() -> SnapshotBuilder<MemoryRegistry, MemoryBus> {
        SnapshotBuilder::new(registry(), MemoryBus::default(), "0.1.0+abc123")
    }

    /// Resolver spec test 6, from the control plane's side: the registry hands
    /// rows back in a hostile order, and the hash must not notice.
    #[tokio::test]
    async fn two_builds_of_the_same_registry_produce_the_same_content_hash() {
        let b = builder();
        let first =
            SnapshotBuilder::<MemoryRegistry, MemoryBus>::content_hash(&b.build().await.unwrap())
                .unwrap();

        for _ in 0..100 {
            let again = SnapshotBuilder::<MemoryRegistry, MemoryBus>::content_hash(
                &b.build().await.unwrap(),
            )
            .unwrap();
            assert_eq!(first, again);
        }
    }

    #[tokio::test]
    async fn a_build_is_ordered_regardless_of_what_the_registry_returns() {
        let spec = builder().build().await.unwrap();
        let ids: Vec<_> = spec.systems.iter().map(|s| s.system_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn the_first_publish_allocates_generation_one() {
        let b = builder();
        assert_eq!(
            b.publish().await.unwrap(),
            Published::Bumped { generation: 1 }
        );
        assert_eq!(b.bus.count(), 1);
    }

    /// The 10s poll must not turn into a generation counter that climbs
    /// forever: `config_generation` is a pin, and a pin that changes when
    /// nothing changed makes every substantial-modification review start with
    /// discarding noise.
    #[tokio::test]
    async fn republishing_unchanged_content_does_not_bump_the_generation() {
        let b = builder();
        b.publish().await.unwrap();

        for _ in 0..10 {
            assert_eq!(
                b.publish().await.unwrap(),
                Published::Unchanged { generation: 1 }
            );
        }
        assert_eq!(b.bus.count(), 1, "nothing more may reach the bus");
    }

    #[tokio::test]
    async fn a_real_change_bumps_and_publishes() {
        let b = builder();
        b.publish().await.unwrap();

        b.registry
            .edit(|systems| systems[0].routes[0].model_id = "claude-opus-5".into());

        assert_eq!(
            b.publish().await.unwrap(),
            Published::Bumped { generation: 2 }
        );
        assert_eq!(b.bus.count(), 2);
        assert_eq!(b.bus.last().unwrap().spec.generation, 2);
    }

    /// A snapshot the gateway would refuse must never leave the control plane.
    #[tokio::test]
    async fn a_spec_that_would_not_build_is_refused_before_publication() {
        let b = builder();
        // Delete a system but leave the key bound to it — the exact shape
        // `ConfigSnapshot::build` rejects.
        b.registry.edit(|systems| {
            systems.remove(0);
        });

        let err = b.publish().await.unwrap_err();
        assert!(
            matches!(err, ControlError::Invalid(_)),
            "expected a validation refusal, got {err:?}"
        );
        assert_eq!(b.bus.count(), 0, "nothing may reach the bus");
    }

    #[tokio::test]
    async fn a_dead_registry_publishes_nothing_rather_than_publishing_less() {
        let b = builder();
        b.publish().await.unwrap();
        b.registry.kill();

        assert!(matches!(b.publish().await, Err(ControlError::Db(_))));
        assert_eq!(b.bus.count(), 1);
    }

    /// The generation is allocated before the send, and a failed send does not
    /// give it back — two configurations sharing a generation number would
    /// make every pin carrying it ambiguous.
    #[tokio::test]
    async fn a_generation_lost_to_a_bus_failure_is_never_reused() {
        let b = SnapshotBuilder::new(registry(), MemoryBus::default(), "0.1.0+abc123");
        b.bus.kill();
        assert!(matches!(b.publish().await, Err(ControlError::Bus(_))));

        b.bus.revive();
        b.registry
            .edit(|systems| systems[0].routes[0].model_id = "claude-opus-5".into());

        assert_eq!(
            b.publish().await.unwrap(),
            Published::Bumped { generation: 2 },
            "generation 1 was burned by the failed send"
        );
    }

    #[tokio::test]
    async fn current_serves_what_was_published_and_refuses_to_invent_a_generation() {
        let b = builder();
        assert!(matches!(b.current().await, Err(ControlError::Invalid(_))));

        b.publish().await.unwrap();
        let env = b.current().await.unwrap();
        assert_eq!(env.generation, 1);
        assert!(env.verify().is_ok());

        // Registry edited, not yet published: the poll backstop must not serve
        // content the published generation does not identify.
        b.registry
            .edit(|systems| systems[0].routes[0].model_id = "claude-opus-5".into());
        assert!(matches!(b.current().await, Err(ControlError::Invalid(_))));
    }
}
