//! A short-lived in-memory cache for the public catalogue (products, categories, offers, reviews,
//! the sample campaign). These answers are the same for every shopper, so serving them from memory
//! for a few seconds lets hundreds of shoppers share one database query instead of each making
//! their own. Stock shown in the shop can lag by up to the cache time; checkout always re-checks
//! real stock in the database.
//!
//! - Only successful GET answers are kept.
//! - When many shoppers ask for the same thing at once, one request loads it and the rest wait for
//!   that answer (no stampede on the database).
//! - Any successful staff change clears everything at once (see `clear_after_admin_change`).
//! - The number of entries is capped, so made-up addresses can't fill the memory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;

use crate::state::AppState;

/// Most entries kept before the cache starts over (the catalogue itself needs a few dozen).
const MAX_ENTRIES: usize = 2_000;
/// Largest answer worth keeping (the full product list is ~20 KB).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Header that says whether an answer came from memory, for checking the cache works.
pub const CACHE_HEADER: &str = "x-cache";

#[derive(Clone)]
struct Entry {
    stored: Instant,
    content_type: Option<HeaderValue>,
    body: Bytes,
}

pub struct ResponseCache {
    ttl: Duration,
    entries: RwLock<HashMap<String, Entry>>,
    /// One lock per address so only one request at a time loads a missing answer.
    loading: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Bumped on every clear, so an answer loaded before a staff change is never stored after it.
    generation: AtomicU64,
}

impl ResponseCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: RwLock::default(),
            loading: Mutex::default(),
            generation: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.ttl.is_zero()
    }

    fn fresh(&self, key: &str) -> Option<Entry> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        entries
            .get(key)
            .filter(|e| e.stored.elapsed() < self.ttl)
            .cloned()
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut loading = self.loading.lock().unwrap_or_else(|e| e.into_inner());
        if loading.len() >= MAX_ENTRIES {
            loading.retain(|_, l| Arc::strong_count(l) > 1);
        }
        loading.entry(key.to_owned()).or_default().clone()
    }

    fn store(&self, key: String, generation: u64, entry: Entry) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        if self.generation.load(Ordering::SeqCst) != generation {
            return; // a staff change happened while this was loading
        }
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&key) {
            entries.retain(|_, e| e.stored.elapsed() < self.ttl);
            if entries.len() >= MAX_ENTRIES {
                entries.clear();
            }
        }
        entries.insert(key, entry);
    }

    /// Forgets everything, e.g. after a staff member edits a product, price, offer or stock.
    pub fn clear(&self) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        self.generation.fetch_add(1, Ordering::SeqCst);
        entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn respond(&self, e: Entry, status: &'static str) -> Response {
        let age = e.stored.elapsed().as_secs();
        let mut res = Response::new(Body::from(e.body));
        let h = res.headers_mut();
        if let Some(ct) = e.content_type {
            h.insert(header::CONTENT_TYPE, ct);
        }
        // Browsers and CDNs may reuse it for the same short time; it is public, non-personal data.
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_str(&format!(
                "public, max-age={}, stale-while-revalidate={}",
                self.ttl.as_secs(),
                self.ttl.as_secs() * 3
            ))
            .expect("valid header"),
        );
        h.insert(header::AGE, HeaderValue::from(age));
        h.insert(CACHE_HEADER, HeaderValue::from_static(status));
        res
    }
}

/// Serves public catalogue GETs from memory when a fresh copy exists; otherwise loads it once.
pub async fn serve_public_from_cache(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let cache = &s.cache;
    if !cache.enabled() || req.method() != Method::GET {
        return next.run(req).await;
    }
    let key = req.uri().to_string();
    if let Some(e) = cache.fresh(&key) {
        return cache.respond(e, "HIT");
    }

    let lock = cache.lock_for(&key);
    let _loading = lock.lock().await;
    if let Some(e) = cache.fresh(&key) {
        return cache.respond(e, "HIT"); // another request loaded it while this one waited
    }

    let generation = cache.generation.load(Ordering::SeqCst);
    let res = next.run(req).await;
    if res.status() != StatusCode::OK {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY_BYTES).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let entry = Entry {
        stored: Instant::now(),
        content_type: parts.headers.get(header::CONTENT_TYPE).cloned(),
        body,
    };
    cache.store(key, generation, entry.clone());
    cache.respond(entry, "MISS")
}

/// After any successful change by staff (products, prices, offers, stock, orders), drop the
/// cached catalogue so shoppers see the change on their next page.
pub async fn clear_after_admin_change(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let changes = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let res = next.run(req).await;
    if changes && res.status().is_success() {
        s.cache.clear();
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &'static str) -> Entry {
        Entry { stored: Instant::now(), content_type: None, body: Bytes::from_static(body.as_bytes()) }
    }

    #[test]
    fn fresh_entries_are_found_and_old_ones_are_not() {
        let c = ResponseCache::new(Duration::from_millis(50));
        c.store("/products".into(), 0, entry("[1]"));
        assert_eq!(c.fresh("/products").unwrap().body, "[1]");
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.fresh("/products").is_none());
    }

    #[test]
    fn clearing_forgets_everything_and_blocks_answers_loaded_before_it() {
        let c = ResponseCache::new(Duration::from_secs(10));
        c.store("/a".into(), 0, entry("a"));
        let before = c.generation.load(Ordering::SeqCst);
        c.clear();
        assert!(c.is_empty());
        c.store("/b".into(), before, entry("stale")); // loaded before the clear
        assert!(c.fresh("/b").is_none());
        c.store("/b".into(), c.generation.load(Ordering::SeqCst), entry("new"));
        assert_eq!(c.fresh("/b").unwrap().body, "new");
    }

    #[test]
    fn the_number_of_entries_is_capped() {
        let c = ResponseCache::new(Duration::from_secs(10));
        for i in 0..MAX_ENTRIES + 50 {
            c.store(format!("/reviews?product_id={i}"), 0, entry("[]"));
        }
        assert!(c.len() <= MAX_ENTRIES);
    }

    #[test]
    fn a_zero_ttl_means_switched_off() {
        assert!(!ResponseCache::new(Duration::ZERO).enabled());
        assert!(ResponseCache::new(Duration::from_secs(1)).enabled());
    }
}
