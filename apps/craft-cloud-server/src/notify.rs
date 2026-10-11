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
    /// A receiver that sees every later `publish` for the space. A new space starts at sequence 0.
    pub fn subscribe(&self, space: SpaceId) -> watch::Receiver<u64> {
        let mut spaces = self.spaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        spaces.entry(space).or_insert_with(|| watch::channel(0).0).subscribe()
    }

    /// Raises the space's sequence to `seq` and wakes its subscribers. Lower values are ignored, and a
    /// space nobody watches is a no-op.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscriber_is_woken_by_a_publish() {
        let notifier = Notifier::default();
        let space = SpaceId::generate();
        let mut rx = notifier.subscribe(space);
        notifier.publish(space, 5);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow_and_update(), 5);
    }

    #[test]
    fn a_publish_to_an_unwatched_space_is_a_no_op() {
        let notifier = Notifier::default();
        notifier.publish(SpaceId::generate(), 9);
        let space = SpaceId::generate();
        assert_eq!(*notifier.subscribe(space).borrow(), 0);
    }

    #[test]
    fn a_lower_sequence_never_lowers_the_value() {
        let notifier = Notifier::default();
        let space = SpaceId::generate();
        let rx = notifier.subscribe(space);
        notifier.publish(space, 7);
        notifier.publish(space, 3);
        assert_eq!(*rx.borrow(), 7);
    }
}
