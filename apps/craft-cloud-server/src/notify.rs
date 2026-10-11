//! Wakes long-polling change-feed requests when a space gets a new sequence number.

use std::collections::HashMap;
use std::sync::Mutex;

use craft_cloud_proto::SpaceId;
use tokio::sync::watch;

#[derive(Default)]
pub struct Notifier {
    spaces: Mutex<HashMap<SpaceId, watch::Sender<u64>>>,
}

impl Notifier {
    /// A receiver that sees every later `publish` for the space.
    pub fn subscribe(&self, space: SpaceId, current: u64) -> watch::Receiver<u64> {
        let mut spaces = self.spaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        spaces.entry(space).or_insert_with(|| watch::channel(current).0).subscribe()
    }

    pub fn publish(&self, space: SpaceId, seq: u64) {
        let spaces = self.spaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = spaces.get(&space) {
            sender.send_if_modified(|value| {
                let newer = seq > *value;
                if newer {
                    *value = seq;
                }
                newer
            });
        }
    }
}
