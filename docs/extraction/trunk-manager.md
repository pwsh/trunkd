# P25 P1 trunk manager — extraction

> **Porting notes, derived from sdrtrunk.** These notes summarize how
> [sdrtrunk](https://github.com/DSheirer/sdrtrunk) (© Dennis Sheirer, GPL-3.0)
> implements this part of the P25 Phase 1 decoder, written while porting it to
> `trunkd-p25`. Class, field and constant names refer to sdrtrunk's Java
> sources. They are paraphrased facts, not copied source; trunkd is
> GPL-3.0-or-later as well. See `NOTICE.md` for full attribution.

Key: tracking keyed by (downlink FREQUENCY, timeslot) NOT talkgroup. Phase1 always TS1.
- mAllocatedTrafficChannelMap[freq]=Channel (control freq inserted so never allocated as traffic).
- mTS1ChannelGrantEventMap[freq]=tracker. mFrequencyBandMap[idenId]=band.
- Grant: freq=channel.getDownlinkFrequency() (0 if IDEN band unknown → grant silently dropped at >0 guards). tracker=getTrackerRemoveIfStale. If tracker && isSameCallCheckingToOnly: if isDifferentTalker→new CONTINUE event+replace tracker; else merge FROM + updateDurationControl. Re-check allocation (may be tracked-but-not-started). Else new-call: new event+addTracker; if freq not allocated: poll pool→null=MAX TRAFFIC CHANNELS EXCEEDED (no start), else requestTrafficChannelStart.
- requestTrafficChannelStart: SourceConfigTuner@downlink; INSERT freq into allocated map BEFORE posting (dup guard); ChannelStartProcessingRequest with PatchGroupPreLoadDataContent + P25FrequencyBandPreloadDataContent(all bands) — traffic decoder starts with empty band map, needs bands to resolve channel numbers.
- Channel→freq: P25Channel.getDownlinkFrequency = band.getDownlinkFrequency(chanNum) else 0. Bands learned from IDEN_UP applied by P25P1MessageProcessor.processForFrequencyBands.
- Tracker (ChannelEventTracker): STALE_EVENT_THRESHOLD_MS=3000; isStale = now-timeEnd>3000. updateDurationControl updates end ONLY if not started (hand-off freeze). updateDurationTraffic sets Started+end. completeTraffic sets Complete (only if isStarted). MAX_TDMA_DATA_CHANNEL_EVENT_DURATION_MS=15000.
- GRANT_UPDATE for untracked call → treated as fresh grant (picks up missed in-progress calls).
- Traffic paths: processP1TrafficCallStart(HDU), LDU1/CurrentUser (merge enc/gps/talkeralias, updateDurationTraffic, isSameCallCheckingToAndFrom), processP1TrafficCallEnd(TDU/TDULC → completeTraffic only if started, does NOT remove tracker).
- Pool default TRAFFIC_CHANNEL_LIMIT_DEFAULT=20. Teardown via TrafficChannelTeardownMonitor on ChannelEvent: PROCESSING_STOP→remove freq from allocated, removeTracker, return channel to pool; START_REJECTED→remove alloc+return pool but leave tracker stamped CHANNEL START REJECTED. Control retune: processControlFrequencyUpdate disables all traffic, clears maps+bands, reseeds control freq.
- No timer thread; staleness lazy on next access. Sentinels: "MAX TRAFFIC CHANNELS EXCEEDED", "CHANNEL START REJECTED". Latent precedence bug in details string ~:1290 — replicate behavior not the quirk.
