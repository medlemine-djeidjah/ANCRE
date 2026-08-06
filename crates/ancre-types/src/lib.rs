//! Shared domain types.
//!
//! Everything here is either hashed into an audit event or read on the hot
//! path, so two rules apply throughout:
//!
//! 1. **Never `Option<T>` for a pin.** Unknown is the literal string
//!    `"unknown"` plus a risk flag. NULL hides a gap; `unknown` shows up in a
//!    `GROUP BY` and becomes a line item on an invoice (PRD §6.3).
//! 2. **`Arc<str>`, not `String`.** Cloning pins into the request context must
//!    be a refcount increment, not an allocation.

pub mod config;
pub mod event;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod pins;
pub mod risk;
pub mod time;

pub use ancre_canon::{CANON_VERSION, Hash32};
pub use config::{
    ConfigSnapshot, IngressMeta, KeyBinding, KeyBindingSpec, Matcher, PinOverrides, PromptRef,
    Route, RouteSpec, SnapshotContent, SnapshotError, SnapshotSpec, SystemConfig, SystemConfigSpec,
};
pub use event::{AuditEvent, EmittedEvent, EventType, HashedBody, Metrics, Outcome};
pub use pins::{Pins, RequestCtx, UNKNOWN, UNRESOLVED_PREFIX};
pub use risk::{ChangeClass, RiskClass, RiskFlag};
pub use time::Timestamp;
