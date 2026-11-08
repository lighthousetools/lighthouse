//! Shared types of Lighthouse. Performs no IO.

pub mod agent;
pub mod automation;
pub mod proposal;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a tenant. Every table carries this ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TenantId(Uuid);

impl TenantId {
    /// The default tenant, created by the first migration. Instances of the
    /// single-user versions keep all their data here, and the admin token acts
    /// in it. Request paths never use it directly: they use the tenant of the
    /// caller's token.
    pub const DEFAULT: Self = Self(Uuid::nil());

    /// Wraps a stored tenant ID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Creates the ID of a new tenant.
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// Returns the underlying UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

/// Identifies one agent run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(Uuid);

impl RunId {
    /// Creates a new, time-ordered run ID.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps a stored run ID.
    pub const fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// Returns the underlying UUID.
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_tenant_is_stable() {
        assert_eq!(TenantId::DEFAULT.as_uuid(), Uuid::nil());
        let id = TenantId::generate();
        assert_eq!(TenantId::from_uuid(id.as_uuid()), id);
        assert_ne!(id, TenantId::DEFAULT);
    }

    #[test]
    fn run_ids_are_unique_and_time_ordered() {
        let first = RunId::new();
        let second = RunId::default();
        assert_ne!(first, second);
        assert!(first.as_uuid() < second.as_uuid());
    }
}
