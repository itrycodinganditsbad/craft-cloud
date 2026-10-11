//! Login rate limiting and the client address behind trusted proxies.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// Fixed-window counters keyed by a string (`ip:…`, `user:…`).
pub struct Limiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
    period: Duration,
}

/// Entries kept before expired ones are swept.
const SWEEP_AT: usize = 10_000;

impl Limiter {
    pub fn new(period: Duration) -> Self {
        Self { windows: Mutex::new(HashMap::new()), period }
    }

    /// Counts one attempt for `key`; false once `max` attempts were made in the current window.
    pub fn allow(&self, key: &str, max: u32, now: Instant) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if windows.len() >= SWEEP_AT {
            let period = self.period;
            windows.retain(|_, (start, _)| now.saturating_duration_since(*start) < period);
        }
        let entry = windows.entry(key.to_owned()).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= self.period {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= max
    }
}

/// The client's address: the peer, unless the peer is a trusted proxy, in which case the right-most
/// `X-Forwarded-For` entry that isn't itself a trusted proxy.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    let peer_ip = peer.ip();
    if !trusted.contains(&peer_ip) {
        return peer_ip;
    }
    let forwarded = headers.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).collect::<Vec<_>>();
    forwarded.iter().rev().filter_map(|s| s.trim().parse::<IpAddr>().ok()).find(|ip| !trusted.contains(ip)).unwrap_or(peer_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_count_and_reset() {
        let limiter = Limiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(limiter.allow("user:ana", 3, t0));
        }
        assert!(!limiter.allow("user:ana", 3, t0));
        assert!(limiter.allow("user:bob", 3, t0));
        assert!(limiter.allow("user:ana", 3, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.2".parse().unwrap());
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        let inner: IpAddr = "10.0.0.2".parse().unwrap();
        let peer = SocketAddr::new(proxy, 1234);
        assert_eq!(client_ip(peer, &headers, &[]), proxy);
        assert_eq!(client_ip(peer, &headers, &[proxy, inner]), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(peer, &headers, &[proxy]), inner);
        let stranger = SocketAddr::new("198.51.100.7".parse().unwrap(), 1);
        assert_eq!(client_ip(stranger, &headers, &[proxy]), "198.51.100.7".parse::<IpAddr>().unwrap());
    }
}
