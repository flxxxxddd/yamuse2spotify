//! getting a working client for each side.
//!
//! both flows are interactive exactly once. after that the credentials live in
//! the config file (yandex) and rspotify's token cache (spotify), and a rerun
//! goes straight through.

pub mod spotify;
pub mod yandex;
