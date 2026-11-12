//! Instrument cache: Nautilus instrument id, venue asset index and the venue's names.

use dashmap::DashMap;
use nautilus_model::{
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
};

/// Shared by the HTTP client, the data client and the execution client.
///
/// The venue names one market in three ways (`"1"`, `"BTC-USDC"`, `"BTC"`), depending on the
/// endpoint, so [`Self::lookup`] accepts all of them.
#[derive(Debug, Default)]
pub struct InstrumentCache {
    by_id: DashMap<InstrumentId, (u32, InstrumentAny)>,
    by_key: DashMap<String, InstrumentId>,
}

impl InstrumentCache {
    /// `asset` is `perpMeta.index`; `name` is `perpMeta.name`; `base` is the base currency code.
    pub fn insert(&self, asset: u32, name: &str, base: &str, instrument: InstrumentAny) {
        let id = instrument.id();
        self.by_key.insert(asset.to_string(), id);
        self.by_key.insert(name.to_string(), id);
        // Two markets may share a base currency; the first one keeps the short name.
        self.by_key.entry(base.to_string()).or_insert(id);
        self.by_id.insert(id, (asset, instrument));
    }

    #[must_use]
    pub fn get(&self, id: &InstrumentId) -> Option<InstrumentAny> {
        self.by_id.get(id).map(|entry| entry.1.clone())
    }

    /// The integer the venue calls `a` / `asset`, and `coin` (as a string) in requests.
    #[must_use]
    pub fn asset(&self, id: &InstrumentId) -> Option<u32> {
        self.by_id.get(id).map(|entry| entry.0)
    }

    /// Finds an instrument by asset index, market name or base currency.
    #[must_use]
    pub fn lookup(&self, key: &str) -> Option<InstrumentAny> {
        let id = *self.by_key.get(key)?;
        self.get(&id)
    }

    #[must_use]
    pub fn all(&self) -> Vec<InstrumentAny> {
        self.by_id.iter().map(|entry| entry.1.clone()).collect()
    }
}
