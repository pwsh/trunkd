// SPDX-License-Identifier: GPL-3.0-or-later
//! IDEN (frequency band identifier) storage and channel→frequency math
//! (spec §10).

/// A frequency band definition from an identifier-update message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdenBand {
    /// 4-bit band identifier.
    pub id: u8,
    /// Base frequency in Hz (raw × 5).
    pub base_hz: u64,
    /// Channel spacing in Hz (raw × 125).
    pub spacing_hz: u32,
    /// Signed transmit offset in Hz (0 if none).
    pub offset_hz: i64,
    /// Channel bandwidth in Hz.
    pub bandwidth_hz: u32,
    /// Slots per carrier (1 for FDMA).
    pub slots_per_carrier: u8,
    /// True for TDMA channel types.
    pub tdma: bool,
}

impl IdenBand {
    /// Downlink frequency for a channel number in this band.
    ///
    /// FDMA: `base + spacing × channel`.
    /// TDMA: `base + spacing × floor(channel / slotsPerCarrier)`.
    pub fn downlink_hz(&self, channel: u16) -> u64 {
        let ch = if self.tdma && self.slots_per_carrier > 1 {
            u64::from(channel) / u64::from(self.slots_per_carrier)
        } else {
            u64::from(channel)
        };
        self.base_hz + u64::from(self.spacing_hz) * ch
    }

    /// Uplink frequency: downlink + signed transmit offset (0/absent → equal
    /// to downlink).
    pub fn uplink_hz(&self, channel: u16) -> u64 {
        self.downlink_hz(channel)
            .saturating_add_signed(self.offset_hz)
    }

    /// TDMA timeslot for a channel number: `channel % slots == 1` → 2, else 1.
    pub fn timeslot(&self, channel: u16) -> u8 {
        if self.slots_per_carrier > 1 && channel % u16::from(self.slots_per_carrier) == 1 {
            2
        } else {
            1
        }
    }
}

/// Storage for the 16 possible band identifiers.
#[derive(Debug, Clone, Default)]
pub struct BandMap {
    bands: [Option<IdenBand>; 16],
}

impl BandMap {
    /// Empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store/replace a band definition.
    pub fn insert(&mut self, band: IdenBand) {
        self.bands[usize::from(band.id & 0xF)] = Some(band);
    }

    /// Look up a band by identifier.
    pub fn get(&self, id: u8) -> Option<&IdenBand> {
        self.bands[usize::from(id & 0xF)].as_ref()
    }

    /// Downlink frequency for (band, channel); 0 when the band is unknown
    /// (spec: such grants are silently unusable until the IDEN arrives).
    pub fn downlink_hz(&self, band: u8, channel: u16) -> u64 {
        self.get(band).map_or(0, |b| b.downlink_hz(channel))
    }

    /// Clear all bands (control-channel retune).
    pub fn clear(&mut self) {
        self.bands = [None; 16];
    }

    /// Iterate over the stored bands.
    pub fn iter(&self) -> impl Iterator<Item = &IdenBand> {
        self.bands.iter().flatten()
    }
}

/// TDMA channel-type table (opcode 51), spec §10.
pub fn tdma_channel_type(value: u8) -> (bool, u32, u8) {
    // (tdma, bandwidth Hz, slots/carrier)
    match value {
        0 | 1 => (false, 12_500, 1),
        2 => (false, 6_250, 1),
        3 => (true, 12_500, 2),
        4 => (true, 25_000, 4),
        5 => (true, 12_500, 2), // H-D8PSK simulcast
        _ => (false, 0, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-computed from the spec's formulas: a standard 800 MHz band.
    /// base raw 170_235_100 (×5 = 851_175_500 Hz), spacing raw 100 (×125 =
    /// 12 500 Hz), offset −45 MHz.
    #[test]
    fn fdma_channel_math() {
        let b = IdenBand {
            id: 1,
            base_hz: 170_235_100 * 5,
            spacing_hz: 100 * 125,
            offset_hz: -45_000_000,
            bandwidth_hz: 12_500,
            slots_per_carrier: 1,
            tdma: false,
        };
        assert_eq!(b.base_hz, 851_175_500);
        assert_eq!(b.downlink_hz(0), 851_175_500);
        // channel 320: 851_175_500 + 12_500 × 320 = 855_175_500
        assert_eq!(b.downlink_hz(320), 855_175_500);
        assert_eq!(b.uplink_hz(320), 855_175_500 - 45_000_000);
        assert_eq!(b.timeslot(320), 1);
    }

    #[test]
    fn tdma_channel_math() {
        let b = IdenBand {
            id: 2,
            base_hz: 851_006_250,
            spacing_hz: 12_500,
            offset_hz: 0,
            bandwidth_hz: 12_500,
            slots_per_carrier: 2,
            tdma: true,
        };
        // floor(channel / slots): channels 0,1 share a carrier.
        assert_eq!(b.downlink_hz(0), 851_006_250);
        assert_eq!(b.downlink_hz(1), 851_006_250);
        assert_eq!(b.downlink_hz(2), 851_018_750);
        assert_eq!(b.timeslot(0), 1);
        assert_eq!(b.timeslot(1), 2);
        assert_eq!(b.timeslot(2), 1);
        assert_eq!(b.timeslot(3), 2);
    }

    #[test]
    fn unknown_band_yields_zero() {
        let m = BandMap::new();
        assert_eq!(m.downlink_hz(7, 100), 0);
    }
}
