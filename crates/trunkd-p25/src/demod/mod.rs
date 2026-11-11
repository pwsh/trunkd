// SPDX-License-Identifier: GPL-3.0-or-later
//! Demodulators: LSM (§6, primary) and C4FM (§5).

pub mod c4fm;
pub mod lsm;

pub use c4fm::C4fmDemodulator;
pub use lsm::{LsmDemodulator, Symbol};
