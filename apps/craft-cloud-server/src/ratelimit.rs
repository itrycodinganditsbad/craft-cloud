//! Login and join rate limiting, and the client address behind trusted proxies.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// Keys kept at most. When the map is full (after a sweep), attempts from new keys are refused.
const DEFAULT_CAPACITY: usize = 100_000;

struct Windows {
    entries: HashMap<String, (Instant, u32)>,
    /// When expired entries were last swept. `None` until the first attempt.
    last_sweep: Option<Instant>,
}

/// Fixed-window counters keyed by a string (`ip:…`, `user:…`).
pub struct Limiter {
    windows: Mutex<Windows>,
    period: Duration,
    capacity: usize,
}

impl Limiter {
    pub fn new(period: Duration) -> Self {
        Self::with_capacity(period, DEFAULT_CAPACITY)
    }

    pub fn with_capacity(period: Duration, capacity: usize) -> Self {
        Self { windows: Mutex::new(Windows { entries: HashMap::new(), last_sweep: None }), period, capacity }
    }

    /// Counts one attempt for `key`; false once `max` attempts were made in the current window, or when
    /// the key is new and the limiter is full.
    pub fn allow(&self, key: &str, max: u32, now: Instant) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let sweep_due = windows.last_sweep.is_none_or(|last| now.saturating_duration_since(last) >= self.period);
        if sweep_due {
            let period = self.period;
            windows.entries.retain(|_, (start, _)| now.saturating_duration_since(*start) < period);
            windows.last_sweep = Some(now);
        }
        if !windows.entries.contains_key(key) && windows.entries.len() >= self.capacity {
            return false;
        }
        let entry = windows.entries.entry(key.to_owned()).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= self.period {
            *entry = (now, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= max
    }

    /// Whether `key` already made `max` attempts in its current window, without counting one (or the limiter is
    /// full and `key` is new): a caller that counts only failures asks this first, and [`Limiter::allow`] after one.
    pub fn full(&self, key: &str, max: u32, now: Instant) -> bool {
        let windows = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match windows.entries.get(key) {
            Some((start, count)) => now.saturating_duration_since(*start) < self.period && *count >= max,
            None => windows.entries.len() >= self.capacity,
        }
    }
}

/// The rate-limit key for a client address: the IPv4 address, or for IPv6 the first 64 bits (a home or
/// office network usually owns a whole /64, so one client can't dodge the limit by changing addresses).
pub fn ip_key(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => format!("ip:{v4}"),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let network = Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
            format!("ip:{network}/64")
        }
    }
}

/// The client's address: the peer, unless the peer is a trusted proxy. Then the right-most
/// `X-Forwarded-For` entry that isn't itself a trusted proxy. If any entry on the way is malformed (or the
/// header isn't text), the peer is returned: nothing to the left of a bad entry is believed.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    let peer_ip = peer.ip().to_canonical();
    let is_trusted = |ip: IpAddr| trusted.iter().any(|t| t.to_canonical() == ip);
    if !is_trusted(peer_ip) {
        return peer_ip;
    }
    let mut entries = Vec::new();
    for value in headers.get_all("x-forwarded-for") {
        let Ok(text) = value.to_str() else { return peer_ip };
        entries.extend(text.split(','));
    }
    for entry in entries.iter().rev() {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else { return peer_ip };
        let ip = ip.to_canonical();
        if !is_trusted(ip) {
            return ip;
        }
    }
    peer_ip
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
    fn full_looks_without_counting() {
        let limiter = Limiter::with_capacity(Duration::from_secs(60), 2);
        let t0 = Instant::now();
        assert!(!limiter.full("ip:a", 2, t0));
        assert!(limiter.allow("ip:a", 2, t0));
        assert!(!limiter.full("ip:a", 2, t0), "one of two");
        assert!(!limiter.full("ip:a", 2, t0), "looking didn't count");
        assert!(limiter.allow("ip:a", 2, t0));
        assert!(limiter.full("ip:a", 2, t0));
        assert!(!limiter.full("ip:a", 2, t0 + Duration::from_secs(60)), "a new window");
        assert!(limiter.allow("ip:b", 2, t0));
        assert!(limiter.full("ip:c", 2, t0), "a new key when the limiter is full");
    }

    #[test]
    fn a_full_limiter_refuses_new_keys_but_counts_existing_ones() {
        let limiter = Limiter::with_capacity(Duration::from_secs(60), 2);
        let t0 = Instant::now();
        assert!(limiter.allow("a", 5, t0));
        assert!(limiter.allow("b", 5, t0));
        assert!(!limiter.allow("c", 5, t0));
        assert!(limiter.allow("a", 5, t0));
        assert_eq!(limiter.windows.lock().unwrap().entries.len(), 2);
    }

    #[test]
    fn expired_entries_are_swept_so_new_keys_fit_again() {
        let limiter = Limiter::with_capacity(Duration::from_secs(60), 2);
        let t0 = Instant::now();
        assert!(limiter.allow("a", 5, t0));
        assert!(limiter.allow("b", 5, t0));
        assert!(limiter.allow("c", 5, t0 + Duration::from_secs(61)));
        assert_eq!(limiter.windows.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn sweeps_run_at_most_once_per_period() {
        let limiter = Limiter::with_capacity(Duration::from_secs(60), 10);
        let t0 = Instant::now();
        assert!(limiter.allow("a", 5, t0));
        assert!(limiter.allow("b", 5, t0 + Duration::from_secs(61)));
        assert!(limiter.allow("c", 5, t0 + Duration::from_secs(62)));
        assert_eq!(limiter.windows.lock().unwrap().last_sweep, Some(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn ip_keys_are_per_address_for_v4_and_per_64_bits_for_v6() {
        assert_eq!(ip_key("203.0.113.9".parse().unwrap()), "ip:203.0.113.9");
        assert_eq!(ip_key("::ffff:203.0.113.9".parse().unwrap()), "ip:203.0.113.9");
        assert_eq!(ip_key("2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap()), "ip:2001:db8:1:2::/64");
        assert_eq!(ip_key("2001:db8:1:2::1".parse().unwrap()), ip_key("2001:db8:1:2:ffff::9".parse().unwrap()));
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

    #[test]
    fn a_malformed_entry_stops_the_scan_and_the_peer_is_used() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, garbage".parse().unwrap());
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        let peer = SocketAddr::new(proxy, 1234);
        assert_eq!(client_ip(peer, &headers, &[proxy]), proxy);
    }

    #[test]
    fn a_malformed_header_in_a_second_line_also_falls_back_to_the_peer() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", "203.0.113.9".parse().unwrap());
        headers.append("x-forwarded-for", "garbage".parse().unwrap());
        let proxy: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(SocketAddr::new(proxy, 1), &headers, &[proxy]), proxy);
    }

    #[test]
    fn an_ipv4_mapped_peer_matches_a_trusted_v4_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        let mapped_peer = SocketAddr::new("::ffff:10.0.0.1".parse().unwrap(), 1234);
        let trusted: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(mapped_peer, &headers, &[trusted]), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(mapped_peer, &HeaderMap::new(), &[]), trusted);
    }
}
