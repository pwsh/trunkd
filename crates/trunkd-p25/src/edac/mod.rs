// SPDX-License-Identifier: GPL-3.0-or-later
//! Error detection and correction suite for P25 Phase 1 (spec §8).

pub mod bch;
pub mod crc;
pub mod gf64;
pub mod golay;
pub mod hamming;
pub mod lsd;
pub mod rs;
pub mod trellis;
