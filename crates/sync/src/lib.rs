//! Caches mail, calendar, issues and pull requests from connectors, with search.
//!
//! Each source calls a few connector tools. An adapter in the configuration, or
//! a bundled preset, maps their results to items. The services stay the truth:
//! the cache only makes reading fast and searchable, and every write goes
//! through the owner's approval elsewhere.

pub mod engine;
pub mod mapping;
pub mod numbers;
pub mod orgs;
pub mod scheduling;
pub mod sources;
pub mod splits;
pub mod store;

pub use engine::{
    CheckChange, PreparedAction, SourceList, SyncError, SyncEvent, SyncReport, Syncer, prune_empty,
};
pub use mapping::{MappedItem, MappingError, Owner, Titles, map_item, map_item_with};
pub use numbers::{
    CurrencyForecast, CurrencyNumbers, ForecastMonth, Numbers, PeriodAmount, StageTotal,
    roll_forward,
};
pub use orgs::{Organization, OrganizationError, OrganizationInput};
pub use sources::{DOMAINS, PRESETS, Source, SourceError, personal_key, preset, resolve};
pub use splits::{Split, SplitFilter, SplitInput};
pub use store::{ItemFilter, ItemPage, Order, PERSONAL_DOMAINS, SyncStatus, SyncStore, SyncedItem};

#[cfg(test)]
mod tests;
