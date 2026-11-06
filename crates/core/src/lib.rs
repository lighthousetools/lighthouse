//! Shared types of Lighthouse. Performs no IO.

pub mod agent;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifies a tenant. Every table carries this ID, even in single mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(Uuid);

impl TenantId {
    /// The only tenant in single mode.
    pub const SINGLE: Self = Self(Uuid::nil());

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
    fn single_tenant_is_stable() {
        assert_eq!(TenantId::SINGLE.as_uuid(), Uuid::nil());
    }

    #[test]
    fn run_ids_are_unique_and_time_ordered() {
        let first = RunId::new();
        let second = RunId::default();
        assert_ne!(first, second);
        assert!(first.as_uuid() < second.as_uuid());
    }
}
