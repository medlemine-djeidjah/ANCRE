//! Checkpoint signer.
//!
//! Every N = 10 000 events or T = 5 minutes: read the range from ClickHouse,
//! compute the tree root, sign, write to Postgres.

use crate::snapshot::ControlError;

#[derive(Debug)]
pub struct Checkpointer {
    _signer: (),
}

impl Checkpointer {
    /// Runs forever. Falling behind is not an error — a checkpoint covering a
    /// larger range is still valid — but it *is* a metric, because the gap
    /// between the chain head and the last signed checkpoint is the window in
    /// which tampering would go unattested. Alert on it.
    pub async fn run(self) -> Result<(), ControlError> {
        todo!("M4")
    }
}

/// Retention floor. Un-lowerable by configuration — Articles 19 and 26(6) put
/// a ≥6-month duty on providers and deployers, and a product that lets a
/// customer set 30 days has helped them break the law with a form field.
///
/// V1-7. The constant lives here now so no MVP code path can quietly assume a
/// shorter one.
pub const RETENTION_FLOOR_DAYS: u32 = 180;
pub const RETENTION_DEFAULT_CEILING_YEARS: u32 = 7;
