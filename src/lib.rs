//! bigyo — a syntax-aware three-way merge viewer.
//!
//! The pipeline: `mergiraf` produces a diff3-marked merge, [`merge::diff3`]
//! splits it into resolved runs and conflict hunks, `difft` reports which bytes
//! actually changed inside each conflict, syntect supplies foregrounds, and
//! [`render::span::overlay_spans`] fuses the two into styled spans that the
//! ratatui viewer draws.

pub mod external;
pub mod merge;
pub mod render;
pub mod tui;
