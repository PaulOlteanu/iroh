use anyhow::Result;
use dashmap::DashMap;
use iroh_metrics::inc;
use pkarr::SignedPacket;
use tracing::info;

use crate::{metrics::Metrics, util::PublicKeyBytes};

#[derive(Debug)]
pub struct SignedPacketStore {
    store: DashMap<PublicKeyBytes, SignedPacket>,
}

impl SignedPacketStore {
    pub fn in_memory() -> Result<Self> {
        info!("using in-memory packet database");
        Self::open()
    }

    pub fn open() -> Result<Self> {
        Ok(Self {
            store: DashMap::new(),
        })
    }

    pub async fn upsert(&self, packet: SignedPacket) -> Result<bool> {
        let key = PublicKeyBytes::from_signed_packet(&packet);

        let mut replaced = false;
        if let Some(existing) = self.store.get(&key) {
            if existing.more_recent_than(&packet) {
                return Ok(false);
            } else {
                replaced = true;
            }
        }
        self.store.insert(key, packet);
        if replaced {
            inc!(Metrics, store_packets_updated);
        } else {
            inc!(Metrics, store_packets_inserted);
        }
        Ok(true)
    }

    pub async fn get(&self, key: &PublicKeyBytes) -> Result<Option<SignedPacket>> {
        let packet = self.store.get(key).map(|x| x.to_owned());
        Ok(packet)
    }

    pub async fn remove(&self, key: &PublicKeyBytes) -> Result<bool> {
        let existed = self.store.remove(key).is_some();
        if existed {
            inc!(Metrics, store_packets_removed)
        }
        Ok(existed)
    }
}
