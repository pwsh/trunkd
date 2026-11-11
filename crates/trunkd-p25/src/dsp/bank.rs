// SPDX-License-Identifier: GPL-3.0-or-later
//! [`ChannelBank`] — one wideband filterbank shared by every consumer of a
//! capture.
//!
//! The [`PolyphaseChannelizerM2`] is the expensive part of decoding a site:
//! at 20 Msps it is ~4 GFLOP/s (mix, FIR and one 1600-point FFT per 800
//! samples), and that cost is the same whether one channel is read out of it
//! or fifty. When every `[[site]]` on a tuner built its own, four sites paid
//! for four identical filterbanks over the same samples, and the ring
//! overflowed. A bank is the one filterbank a device runs, and every site
//! engine on the device is a consumer of it.
//!
//! # Ownership
//!
//! - **The device owns the bank.** One [`SharedChannelBank`] per sample
//!   stream, built with the device's rate, centre and prototype. Every
//!   [`SiteEngine`](crate::site::SiteEngine) on the device holds a clone of
//!   the `Rc` (the device's engines all live on its one DSP thread, so the
//!   sharing is `Rc<RefCell<_>>`, not a lock).
//! - **The device runs the filterbank, once per block**
//!   ([`process_block`](ChannelBank::process_block)); then every engine reads
//!   its channels ([`drain`](ChannelBank::drain)). An engine never runs the
//!   filterbank of a bank it shares.
//! - **Engines own their subscriptions.** Each subscribes exactly what it
//!   decodes — its control candidates while hunting, the control channel once
//!   locked, every traffic and data channel it follows — and releases them as
//!   before. Pool limits stay per site: the bank never refuses a subscription
//!   a site's own traffic pool admitted.
//! - **A channel is produced once however many consumers want it.**
//!   Subscriptions are reference-counted by exact centre frequency: the same
//!   control candidate hunted by two sites, or a neighbour's traffic channel,
//!   is one filterbank output read by each consumer. The last consumer to
//!   leave releases the channelizer slot.
//! - **Noise-floor windows are per consumer.** Each engine rolls its window on
//!   its own stats cadence and after a retune, so each opens its own
//!   ([`open_power_window`](ChannelBank::open_power_window)); the strided
//!   accumulation over the FFT output is the only per-engine cost left.
//!
//! # Per-block reads
//!
//! Output buffers hold the samples of the **most recent**
//! [`process_block`](ChannelBank::process_block) only: they are cleared as the
//! next block starts, and each consumer reads a block once. A consumer
//! subscribed after a block ran reads nothing until the next one — exactly
//! what a fresh subscription on a private channelizer would see — so an
//! engine that drains every channel every block (which a site engine does)
//! sees the same samples it would with a filterbank of its own. The one
//! difference is the residual NCO of an *off-grid* channel joined by a second
//! consumer: it continues from the first consumer's phase instead of
//! starting at zero, a constant phase rotation no demodulator in this crate
//! can see (every one is differential or phase-tracking). On-grid channels —
//! every P25 channel on a 12.5 kHz-aligned capture — have no NCO at all.

use std::cell::RefCell;
use std::rc::Rc;

use num_complex::Complex;

use super::channelizer::{
    BinSelection, ChannelHandle, Channelizer, ChannelizerError, PolyphaseChannelizerM2,
    DEFAULT_STOPBAND_DB,
};

/// A device's bank, as its site engines hold it.
pub type SharedChannelBank = Rc<RefCell<ChannelBank>>;

/// One consumer's subscription: a slot index plus a generation counter, so a
/// stale handle can never read a recycled slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BankHandle {
    slot: u32,
    generation: u32,
}

/// One produced channel and how many consumers read it.
struct Channel {
    /// `center_hz.to_bits()`: channels are shared on the exact frequency.
    center_bits: u64,
    handle: ChannelHandle,
    consumers: u32,
}

struct Consumer {
    /// Index into [`ChannelBank::channels`].
    channel: u32,
    /// The block this consumer last read (or subscribed after).
    read_seq: u64,
}

struct ConsumerSlot {
    generation: u32,
    consumer: Option<Consumer>,
}

/// One filterbank, many consumers. See the module docs.
pub struct ChannelBank {
    channelizer: PolyphaseChannelizerM2,
    sample_rate: u32,
    /// Produced channels; `None` is a free slot.
    channels: Vec<Option<Channel>>,
    consumers: Vec<ConsumerSlot>,
    /// `process_block` calls so far.
    seq: u64,
    /// Wideband samples consumed so far.
    samples: u64,
}

impl ChannelBank {
    /// A bank for a capture of `sample_rate` centred at `center_hz`, with a
    /// `taps_per_channel`-tap prototype (clamped to the filterbank's minimum
    /// of 2) and the default stopband.
    pub fn new(
        sample_rate: u32,
        center_hz: f64,
        taps_per_channel: usize,
    ) -> Result<Self, ChannelizerError> {
        let mut channelizer = PolyphaseChannelizerM2::with_taps(
            sample_rate,
            center_hz,
            taps_per_channel.max(2),
            DEFAULT_STOPBAND_DB,
        )?;
        // Every window belongs to a consumer that opened it; the filterbank's
        // default one is reopened by the first consumer to ask.
        channelizer.close_power_window(0);
        Ok(ChannelBank {
            channelizer,
            sample_rate,
            channels: Vec::new(),
            consumers: Vec::new(),
            seq: 0,
            samples: 0,
        })
    }

    /// [`new`](Self::new), wrapped for sharing between a device's engines.
    pub fn shared(
        sample_rate: u32,
        center_hz: f64,
        taps_per_channel: usize,
    ) -> Result<SharedChannelBank, ChannelizerError> {
        Ok(Rc::new(RefCell::new(Self::new(
            sample_rate,
            center_hz,
            taps_per_channel,
        )?)))
    }

    /// Capture rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Capture centre in Hz.
    pub fn center_hz(&self) -> f64 {
        self.channelizer.center_hz()
    }

    /// Prototype taps per polyphase arm.
    pub fn taps_per_channel(&self) -> usize {
        self.channelizer.taps_per_channel()
    }

    /// Per-channel output rate (25 kHz).
    pub fn output_rate(&self) -> u32 {
        self.channelizer.output_rate()
    }

    /// Which bin(s) a frequency maps to; `Err` outside the usable span or on
    /// the DC bin. Pure — the same answer a subscription would get.
    pub fn bin_for(&self, center_hz: f64) -> Result<(BinSelection, f64), ChannelizerError> {
        self.channelizer.bin_for(center_hz)
    }

    /// Run the filterbank over one block of wideband samples. Every channel's
    /// previous block is discarded first (see "Per-block reads").
    pub fn process_block(&mut self, wideband: &[Complex<f32>]) {
        self.channelizer.clear_outputs();
        self.channelizer.process_block(wideband);
        self.seq += 1;
        self.samples += wideband.len() as u64;
    }

    /// Subscribe to `center_hz`, sharing the channel with any consumer already
    /// reading that exact frequency.
    pub fn subscribe(&mut self, center_hz: f64) -> Result<BankHandle, ChannelizerError> {
        let bits = center_hz.to_bits();
        let existing = self
            .channels
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.center_bits == bits));
        let channel = match existing {
            Some(i) => i,
            None => {
                let handle = self.channelizer.subscribe(center_hz)?;
                let fresh = Channel {
                    center_bits: bits,
                    handle,
                    consumers: 0,
                };
                match self.channels.iter().position(Option::is_none) {
                    Some(i) => {
                        self.channels[i] = Some(fresh);
                        i
                    }
                    None => {
                        self.channels.push(Some(fresh));
                        self.channels.len() - 1
                    }
                }
            }
        };
        if let Some(c) = self.channels[channel].as_mut() {
            c.consumers += 1;
        }
        let consumer = Consumer {
            channel: channel as u32,
            // Nothing from a block that ran before the subscription.
            read_seq: self.seq,
        };
        let slot = match self.consumers.iter().position(|s| s.consumer.is_none()) {
            Some(i) => i,
            None => {
                self.consumers.push(ConsumerSlot {
                    generation: 0,
                    consumer: None,
                });
                self.consumers.len() - 1
            }
        };
        self.consumers[slot].consumer = Some(consumer);
        Ok(BankHandle {
            slot: slot as u32,
            generation: self.consumers[slot].generation,
        })
    }

    /// Drop a subscription; the channel stops being produced once its last
    /// consumer leaves. `false` for a stale handle.
    pub fn unsubscribe(&mut self, handle: BankHandle) -> bool {
        let Some(slot) = self.consumers.get_mut(handle.slot as usize) else {
            return false;
        };
        if slot.generation != handle.generation {
            return false;
        }
        let Some(consumer) = slot.consumer.take() else {
            return false;
        };
        slot.generation = slot.generation.wrapping_add(1);
        let idx = consumer.channel as usize;
        let last = match self.channels[idx].as_mut() {
            Some(c) => {
                c.consumers -= 1;
                c.consumers == 0
            }
            None => false,
        };
        if last {
            if let Some(c) = self.channels[idx].take() {
                self.channelizer.unsubscribe(c.handle);
            }
        }
        true
    }

    /// Append the latest block's samples for this subscription to `out`, once
    /// per block; returns the number appended (0 for a stale handle, a block
    /// already read, or a subscription newer than the block).
    pub fn drain(&mut self, handle: BankHandle, out: &mut Vec<Complex<f32>>) -> usize {
        let Some(slot) = self.consumers.get_mut(handle.slot as usize) else {
            return 0;
        };
        if slot.generation != handle.generation {
            return 0;
        }
        let Some(consumer) = slot.consumer.as_mut() else {
            return 0;
        };
        if consumer.read_seq >= self.seq {
            return 0;
        }
        consumer.read_seq = self.seq;
        let Some(channel) = self.channels[consumer.channel as usize].as_ref() else {
            return 0;
        };
        let samples = self.channelizer.peek(channel.handle).unwrap_or(&[]);
        out.extend_from_slice(samples);
        samples.len()
    }

    /// Centre frequency a subscription reads.
    pub fn subscribed_frequency(&self, handle: BankHandle) -> Option<f64> {
        let slot = self.consumers.get(handle.slot as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        let channel = slot.consumer.as_ref()?.channel as usize;
        self.channels[channel]
            .as_ref()
            .map(|c| f64::from_bits(c.center_bits))
    }

    /// Distinct channels the filterbank is producing.
    pub fn channel_count(&self) -> usize {
        self.channels.iter().filter(|c| c.is_some()).count()
    }

    /// Live subscriptions across every consumer (≥ [`channel_count`](Self::channel_count)).
    pub fn subscription_count(&self) -> usize {
        self.consumers
            .iter()
            .filter(|s| s.consumer.is_some())
            .count()
    }

    /// Wideband samples run through the filterbank so far.
    pub fn samples_consumed(&self) -> u64 {
        self.samples
    }

    /// Filterbank blocks run so far (one per `M/2` input samples): the
    /// channelizer's cost, paid once for every consumer.
    pub fn filterbank_blocks(&self) -> u64 {
        self.channelizer.blocks_run()
    }

    /// The filterbank itself (inspection).
    pub fn channelizer(&self) -> &PolyphaseChannelizerM2 {
        &self.channelizer
    }

    // -- noise-floor windows -------------------------------------------------

    /// Open a noise-floor window for one consumer; see
    /// [`PolyphaseChannelizerM2::bin_power_percentile_dbfs`] for what it
    /// measures.
    pub fn open_power_window(&mut self) -> usize {
        self.channelizer.open_power_window()
    }

    /// Close a consumer's window (the consumer is going away).
    pub fn close_power_window(&mut self, window: usize) {
        self.channelizer.close_power_window(window);
    }

    /// The `percentile`-th quietest bin's mean power over `window`, in dBFS;
    /// `None` before the window has folded in a block.
    pub fn power_percentile_dbfs(&mut self, window: usize, percentile: f64) -> Option<f64> {
        self.channelizer.window_percentile_dbfs(window, percentile)
    }

    /// Start a fresh measurement in `window`.
    pub fn reset_power_window(&mut self, window: usize) {
        self.channelizer.reset_window(window);
    }

    /// Blocks folded into `window` since its last reset.
    pub fn power_window_blocks(&self, window: usize) -> u64 {
        self.channelizer.window_blocks(window)
    }
}

#[cfg(test)]
mod tests {
    use super::super::channelizer::channelizers_built_on_this_thread;
    use super::*;

    /// 1 Msps → 80 bins of 12.5 kHz, cheap enough for unit tests.
    const RATE: u32 = 1_000_000;
    const CENTER: f64 = 852_000_000.0;

    fn tone(freq_offset: f64, start: u64, n: usize) -> Vec<Complex<f32>> {
        (0..n)
            .map(|k| {
                let ph = std::f64::consts::TAU * freq_offset * (start + k as u64) as f64
                    / f64::from(RATE);
                Complex::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn a_channel_two_consumers_want_is_produced_once_and_read_by_both() {
        let mut bank = ChannelBank::new(RATE, CENTER, 6).unwrap();
        let f = CENTER + 10.0 * 12_500.0;
        let a = bank.subscribe(f).unwrap();
        let b = bank.subscribe(f).unwrap();
        assert_ne!(a, b, "each consumer has its own handle");
        assert_eq!(bank.channel_count(), 1, "one produced channel");
        assert_eq!(bank.subscription_count(), 2);
        assert_eq!(
            bank.channelizer().subscription_count(),
            1,
            "the filterbank emits the channel once"
        );

        let (mut out_a, mut out_b) = (Vec::new(), Vec::new());
        let mut at = 0u64;
        for _ in 0..20 {
            let block = tone(10.0 * 12_500.0 + 1_000.0, at, 4_000);
            at += block.len() as u64;
            bank.process_block(&block);
            let na = bank.drain(a, &mut out_a);
            let nb = bank.drain(b, &mut out_b);
            assert_eq!(na, nb);
            assert!(na > 0);
            assert_eq!(bank.drain(a, &mut out_a), 0, "a block is read once");
        }
        assert_eq!(out_a, out_b, "both consumers read identical samples");

        // The channel outlives its first consumer and dies with its last.
        assert!(bank.unsubscribe(a));
        assert!(!bank.unsubscribe(a), "stale handle");
        assert_eq!(bank.channel_count(), 1);
        assert!(bank.unsubscribe(b));
        assert_eq!(bank.channel_count(), 0);
        assert_eq!(bank.channelizer().subscription_count(), 0);
    }

    #[test]
    fn a_late_consumer_reads_nothing_from_a_block_that_ran_before_it() {
        let mut bank = ChannelBank::new(RATE, CENTER, 6).unwrap();
        let f = CENTER - 20.0 * 12_500.0;
        let early = bank.subscribe(f).unwrap();
        bank.process_block(&tone(-20.0 * 12_500.0, 0, 4_000));
        let late = bank.subscribe(f).unwrap();
        let mut out = Vec::new();
        assert_eq!(bank.drain(late, &mut out), 0, "subscribed after the block");
        assert!(bank.drain(early, &mut out) > 0);
        out.clear();
        bank.process_block(&tone(-20.0 * 12_500.0, 4_000, 4_000));
        assert_eq!(
            bank.drain(late, &mut out),
            100,
            "4000 samples → 100 at 25 kHz"
        );
    }

    /// Two consumers of one bank read exactly what two private channelizers
    /// would, bit for bit, for an on-grid channel.
    #[test]
    fn shared_reads_match_private_filterbanks_bit_for_bit() {
        let f = CENTER + 7.0 * 12_500.0;
        let mut bank = ChannelBank::new(RATE, CENTER, 6).unwrap();
        let mut private = PolyphaseChannelizerM2::with_taps(RATE, CENTER, 6, 80.0).unwrap();
        let hb = bank.subscribe(f).unwrap();
        let hp = private.subscribe(f).unwrap();
        let (mut ob, mut op) = (Vec::new(), Vec::new());
        let mut at = 0u64;
        for n in [1usize, 39, 40, 41, 4_096, 800, 13] {
            let block = tone(7.0 * 12_500.0 + 2_345.0, at, n);
            at += n as u64;
            bank.process_block(&block);
            private.process_block(&block);
            bank.drain(hb, &mut ob);
            private.drain(hp, &mut op);
        }
        assert!(!ob.is_empty());
        assert_eq!(ob, op);
    }

    #[test]
    fn one_bank_builds_one_filterbank_and_windows_are_per_consumer() {
        let before = channelizers_built_on_this_thread();
        let mut bank = ChannelBank::new(RATE, CENTER, 6).unwrap();
        assert_eq!(channelizers_built_on_this_thread() - before, 1);

        let w1 = bank.open_power_window();
        let w2 = bank.open_power_window();
        assert_ne!(w1, w2);
        // 40 samples per filterbank block, one accumulation every 8 blocks.
        bank.process_block(&vec![Complex::new(0.5, 0.0); 40 * 8 * 3]);
        assert_eq!(bank.power_window_blocks(w1), 3);
        assert_eq!(bank.power_window_blocks(w2), 3);
        bank.reset_power_window(w1);
        assert_eq!(bank.power_window_blocks(w1), 0, "reset one window...");
        assert_eq!(bank.power_window_blocks(w2), 3, "...not its neighbour's");
        assert_eq!(bank.power_percentile_dbfs(w1, 20.0), None);
        assert!(bank.power_percentile_dbfs(w2, 20.0).is_some());

        // A closed window is recycled by the next consumer, freshly zeroed.
        bank.close_power_window(w2);
        bank.process_block(&vec![Complex::new(0.5, 0.0); 40 * 8]);
        assert_eq!(
            bank.power_window_blocks(w2),
            0,
            "closed windows do not accumulate"
        );
        assert_eq!(bank.open_power_window(), w2);
        assert_eq!(bank.filterbank_blocks(), 32);
    }
}
