//! Wakes long-polling change-feed requests when a space gets a new sequence number.

use std::collections::HashMap;
use std::sync::Mutex;

use craft_cloud_proto::SpaceId;
use tokio::sync::watch;

/// Counters of spaces' members kept before the ones nobody watches are dropped on the next subscription.
const PRUNE_MEMBERS_AT: usize = 64;

/// Most long-polls one space may have waiting at once.
pub const MAX_WAITERS_PER_SPACE: usize = 16;

pub struct Notifier {
    spaces: Mutex<HashMap<SpaceId, watch::Sender<u64>>>,
    /// Counts the changes to a space's members, roles and life cycle, for the waiters to re-check their access.
    members: Mutex<HashMap<SpaceId, watch::Sender<u64>>>,
    waiters: Mutex<HashMap<SpaceId, usize>>,
    shutdown: watch::Sender<bool>,
}

impl Default for Notifier {
    fn default() -> Self {
        Self { spaces: Mutex::default(), members: Mutex::default(), waiters: Mutex::default(), shutdown: watch::channel(false).0 }
    }
}

/// One waiting long-poll; frees its slot when dropped.
pub struct WaitGuard<'a> {
    notifier: &'a Notifier,
    space: SpaceId,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        let mut waiters = self.notifier.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = waiters.get_mut(&self.space) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                waiters.remove(&self.space);
            }
        }
    }
}

impl Notifier {
    /// Takes one of the space's waiting slots, or `None` when `MAX_WAITERS_PER_SPACE` are already taken.
    pub fn begin_wait(&self, space: SpaceId) -> Option<WaitGuard<'_>> {
        let mut waiters = self.waiters.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = waiters.entry(space).or_insert(0);
        if *count >= MAX_WAITERS_PER_SPACE {
            return None;
        }
        *count += 1;
        Some(WaitGuard { notifier: self, space })
    }

    /// Tells every waiting long-poll (now and later) to answer at once. The server calls this when it starts
    /// shutting down, so a graceful shutdown doesn't wait out the long-polls.
    pub fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// A receiver whose value turns `true` once [`Notifier::begin_shutdown`] has been called.
    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// A receiver that sees every later `publish` for the space. A new space starts at sequence 0.
    pub fn subscribe(&self, space: SpaceId) -> watch::Receiver<u64> {
        let mut spaces = self.spaces.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        spaces.entry(space).or_insert_with(|| watch::channel(0).0).subscribe()
    }

    /// A receiver that sees every later [`Notifier::members_changed`] for the space.
    pub fn subscribe_members(&self, space: SpaceId) -> watch::Receiver<u64> {
        let mut members = self.members.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // A space nobody watches any more is forgotten (checked now and then, not at every call).
        if members.len() >= PRUNE_MEMBERS_AT {
            members.retain(|_, sender| sender.receiver_count() > 0);
        }
        members.entry(space).or_insert_with(|| watch::channel(0).0).subscribe()
    }

    /// Tells the space's waiting long-polls that its members, a role or its life cycle (archived) changed, so each looks
    /// at its own access again at once. A space nobody watches is a no-op. Another server process on the same database is
    /// not told: its waiters notice at the end of their wait (at most a minute).
    pub fn members_changed(&self, space: SpaceId) {
        let mut members = self.members.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = members.get(&space) {
            if sender.receiver_count() == 0 {
                members.remove(&space);
            } else {
                sender.send_modify(|count| *count = count.wrapping_add(1));
            }
        }
    }

    /// How many spaces have a members counter (for tests).
    #[cfg(test)]
    fn members_len(&self) -> usize {
        self.members.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
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
    fn waiters_are_counted_per_space_and_released_on_drop() {
        let notifier = Notifier::default();
        let (a, b) = (SpaceId::generate(), SpaceId::generate());
        let guards: Vec<_> = (0..MAX_WAITERS_PER_SPACE).map(|_| notifier.begin_wait(a).unwrap()).collect();
        assert!(notifier.begin_wait(a).is_none());
        // Another space has its own allowance.
        assert!(notifier.begin_wait(b).is_some());
        drop(guards);
        assert!(notifier.begin_wait(a).is_some());
    }

    #[tokio::test]
    async fn shutdown_reaches_late_and_early_listeners() {
        let notifier = Notifier::default();
        let mut early = notifier.shutdown_signal();
        assert!(!*early.borrow());
        notifier.begin_shutdown();
        early.wait_for(|stop| *stop).await.unwrap();
        notifier.shutdown_signal().wait_for(|stop| *stop).await.unwrap();
    }

    #[test]
    fn counters_of_members_nobody_watches_are_forgotten() {
        let notifier = Notifier::default();
        let first = SpaceId::generate();
        let watcher = notifier.subscribe_members(first);
        notifier.members_changed(first);
        assert_eq!(notifier.members_len(), 1, "watched: kept");
        drop(watcher);
        notifier.members_changed(first);
        assert_eq!(notifier.members_len(), 0, "unwatched: a change drops it");
        // Many spaces that were watched once: the next subscription clears the ones nobody watches.
        for _ in 0..(PRUNE_MEMBERS_AT + 10) {
            drop(notifier.subscribe_members(SpaceId::generate()));
        }
        let kept = notifier.subscribe_members(SpaceId::generate());
        assert!(notifier.members_len() <= PRUNE_MEMBERS_AT + 1, "{}", notifier.members_len());
        drop(kept);
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
