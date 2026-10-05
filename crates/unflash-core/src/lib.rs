//! Photosensitive flash detection with WCAG 2.x / PEAT semantics.
//!
//! This crate is the portable heart of Unflash. It holds everything that
//! does not touch a GPU, a codec or a browser:
//!
//! * [`config`] — thresholds and the three profiles.
//! * [`pixel`] — the per-pixel state machine (monotonic-run trackers, flash
//!   pairing, rate rings) written so that the GPU shader in `unflash-gpu`
//!   and the CPU kernels here are the *same algorithm on the same integer
//!   clock*, and can be compared bit for bit.
//! * [`pattern`] — the regular-pattern (stripe) detector the flash tests
//!   cannot see, walking the picture along eight orientations.
//! * [`grid`] — the sliding-window geometry and the per-frame statistics
//!   that the pixel stage hands to the temporal stage.
//! * [`temporal`] — the window-mean coherence gate, event and hazard
//!   bookkeeping, and the final verdict (violations, extended flashes).
//! * [`sections`] — turning violations into padded work sections, the safe
//!   frame-rate bound, and the run-up length a section check needs.
//! * [`editing`] — replacement maps, edited timelines, thinning, the
//!   keep-light / keep-dark suggester and the classification of a check's
//!   violations relative to the section.
//! * [`detector`] — the [`detector::Detector`] that ties a pixel stage to the
//!   temporal stage, plus the fully-CPU [`detector::CpuDetector`].
//!
//! The reference behaviour is the Python implementation in `unflash/`
//! (see `DETECTION.md`); differences are documented where they exist.

pub mod config;
pub mod blend;
pub mod detector;
pub mod editing;
pub mod grid;
pub mod lut;
pub mod pattern;
pub mod pixel;
pub mod resample;
pub mod sections;
pub mod temporal;
pub mod time;
pub mod timeline;
pub mod yuv;

#[cfg(feature = "simd")]
pub mod pixel_simd;
