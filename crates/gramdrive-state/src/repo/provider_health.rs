//! Aggregate File Provider callback health (TASK-260729-376m7o).
//!
//! This module intentionally has no item, account, chat, filename, path or
//! source-error field. The durable state answers whether the extension and
//! engine disagree about failures without turning diagnostics into a record
//! of a user's activity.

use rusqlite::params;

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn};

/// Privacy-safe aggregate counters for File Provider fetch callbacks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderFetchHealthCounters {
    /// Callbacks observed by the provider.
    pub callback_count: u64,
    /// Callbacks that returned verified content.
    pub success_count: u64,
    /// Failures reported by the hydration engine or its transport.
    pub engine_failure_count: u64,
    /// Non-success provider error mappings returned to macOS.
    pub provider_mapping_count: u64,
    /// Provider mappings that specifically asserted `noSuchItem`.
    pub no_such_item_count: u64,
    /// Callback results that macOS may retry.
    pub retryable_count: u64,
}

/// One counter increment request from the coordinator-owned control path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderFetchHealthObservation {
    /// The callback returned verified content.
    pub succeeded: bool,
    /// Hydration engine or transport returned a failure.
    pub engine_failure: bool,
    /// The extension mapped an error onto a provider error surface.
    pub provider_mapping: bool,
    /// The mapping specifically asserted `noSuchItem`.
    pub no_such_item: bool,
    /// macOS may retry this callback result.
    pub retryable: bool,
    /// Wall-clock observation timestamp, used only for aggregate freshness.
    pub observed_at_ms: i64,
}

impl ReadTxn<'_> {
    /// Reads the one identity-free File Provider health row.
    pub fn provider_fetch_health(&self) -> Result<ProviderFetchHealthCounters, StateError> {
        self.conn()
            .prepare_cached(
                "SELECT callback_count, success_count, engine_failure_count,
                        provider_mapping_count, no_such_item_count, retryable_count
                   FROM provider_fetch_health WHERE singleton = 1",
            )?
            .query_row([], |row| {
                Ok(ProviderFetchHealthCounters {
                    callback_count: row.get::<_, i64>(0)? as u64,
                    success_count: row.get::<_, i64>(1)? as u64,
                    engine_failure_count: row.get::<_, i64>(2)? as u64,
                    provider_mapping_count: row.get::<_, i64>(3)? as u64,
                    no_such_item_count: row.get::<_, i64>(4)? as u64,
                    retryable_count: row.get::<_, i64>(5)? as u64,
                })
            })
            .map_err(StateError::from)
    }
}

impl WriteTxn<'_> {
    /// Atomically adds one provider callback result to the aggregate row.
    pub fn record_provider_fetch_health(
        &self,
        observation: ProviderFetchHealthObservation,
    ) -> Result<(), StateError> {
        self.conn()
            .prepare_cached(
                "UPDATE provider_fetch_health
                    SET callback_count = callback_count + 1,
                        success_count = success_count + ?1,
                        engine_failure_count = engine_failure_count + ?2,
                        provider_mapping_count = provider_mapping_count + ?3,
                        no_such_item_count = no_such_item_count + ?4,
                        retryable_count = retryable_count + ?5,
                        last_updated_at_ms = ?6
                  WHERE singleton = 1",
            )?
            .execute(params![
                i64::from(observation.succeeded),
                i64::from(observation.engine_failure),
                i64::from(observation.provider_mapping),
                i64::from(observation.no_such_item),
                i64::from(observation.retryable),
                observation.observed_at_ms,
            ])?;
        Ok(())
    }
}
