//! AirRadar - a small, self-hosted anime airing schedule index.
//!
//! Collects the current-season subject list from the official Bangumi API,
//! discovers schedule pages through pluggable web-search providers, extracts
//! weekday + JST broadcast time with rules first and an optional LLM fallback,
//! cross-checks independent sources and publishes JSON consumed by MiruPlay.

pub mod api;
pub mod app;
pub mod atomic;
pub mod bangumi;
pub mod cache;
pub mod config;
pub mod extract;
pub mod fetch;
pub mod html;
pub mod llm;
pub mod log;
pub mod model;
pub mod pipeline;
pub mod scheduler;
pub mod search;
pub mod season;
pub mod store;
pub mod util;
pub mod verify;

/// Crate version from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default User-Agent used when `BANGUMI_UA` / `FETCH_UA` are not configured.
pub const DEFAULT_UA: &str = concat!(
    "AirRadar/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ModerRAS/AirRadar)"
);
