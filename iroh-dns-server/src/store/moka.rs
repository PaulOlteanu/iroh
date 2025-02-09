use std::time::Duration;

use anyhow::Result;
use iroh_metrics::inc;
use pkarr::SignedPacket;
use tracing::info;

use crate::{metrics::Metrics, util::PublicKeyBytes};

#[derive(Debug)]
pub struct MokaStore {
    store: moka::future::Cache<PublicKeyBytes, SignedPacket>,
}

impl MokaStore {
    pub fn new() -> Self {
        info!("using in-memory packet database");

        let cache = moka::future::Cache::builder()
            .time_to_live(Duration::from_secs(300))
            .build();

        Self { store: cache }
    }

    pub async fn upsert(&self, packet: SignedPacket) -> Result<bool> {
        let key = PublicKeyBytes::from_signed_packet(&packet);
        let mut replaced = false;
        if let Some(existing) = self.store.get(&key).await {
            if existing.more_recent_than(&packet) {
                return Ok(false);
            } else {
                replaced = true;
            }
        }
        self.store.insert(key, packet).await;
        if replaced {
            inc!(Metrics, store_packets_updated);
        } else {
            inc!(Metrics, store_packets_inserted);
        }
        Ok(true)
    }

    pub async fn get(&self, key: &PublicKeyBytes) -> Result<Option<SignedPacket>> {
        Ok(self.store.get(key).await)
    }

    pub async fn remove(&self, key: &PublicKeyBytes) -> Result<bool> {
        let updated = self.store.remove(key).await.is_some();
        if updated {
            inc!(Metrics, store_packets_removed)
        }
        Ok(updated)
    }
}
