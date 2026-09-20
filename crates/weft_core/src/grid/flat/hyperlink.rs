//! Run-length-encoded storage for OSC 8 hyperlink ids, parallel to
//! [`super::style::FgColorMap`] and [`super::style::BgAndStyleMap`]
//! (aligned with Warp `hyperlink.rs`).
//!
//! A typical OSC 8 span covers many adjacent cells with the same id, so the
//! interval map collapses a 100-cell hyperlink into one entry, and runs of
//! plain output (`None`) cost no entries at all.
//!
//! weft adaptation: ids are plain `u32` (matching
//! `RowExtras::hyperlink_id`), and the uri ↔ id table deliberately stays in
//! `crate::hyperlink::HyperlinkRegistry` — flat storage only persists ids,
//! never uri strings, so materialized rows resolve links through the same
//! registry the viewport uses.

use super::attribute_map::AttributeMap;

/// Map holding each cell's OSC 8 hyperlink id (`None` outside link spans).
pub(crate) type HyperlinkIdMap = AttributeMap<Option<u32>>;
