# trunkd-mbe test vectors

| File | What it is | Used by | Public tree |
|---|---|---|---|
| `jtransforms_fft_probe.txt` | JTransforms FFT outputs for fixed inputs (pure math) | `src/fft/tests.rs` | shipped |
| `imbe_frames_encrypted.hex` | 206 IMBE frames from the middle of an AES-256 call (ciphertext, no speech) | `tests/jmbe_oracle.rs` | shipped |
| `jmbe_pcm_encrypted.i16` | JMBE 1.0.9's decode of those frames (noise) | `tests/jmbe_oracle.rs` | shipped |
| `imbe_frames_clear.hex` | 270 IMBE frames (5.4 s) of a clear, ALGID 0x80 dispatch call | `tests/jmbe_oracle.rs` | **not shipped** by default |
| `jmbe_pcm_clear.i16` | JMBE 1.0.9's decode of those frames (intelligible speech) | `tests/jmbe_oracle.rs` | **not shipped** by default |

## Provenance

Both frame sets are real off-air P25 Phase 1 traffic, sliced out of a wideband IQ capture by
`trunkd-p25`'s `p25_decode` example (its `{"type":"imbe",...,"frame":"<hex>"}` lines) at the raw
144-bit vocoder boundary. `.hex` files hold one frame per line as 36 hex characters.

The `.i16` files are what JMBE 1.0.9 (`github.com/DSheirer/jmbe`) produces for exactly those frames:
every frame fed in capture order through **one** `jmbe.codec.imbe.IMBEAudioCodec` instance (decoder
state carries across frames), each returned `float[160]` converted with
`(short)(sample * Short.MAX_VALUE)` and written as little-endian `i16`, 160 samples per frame, 8 kHz.

## Regenerating

1. Decode an IQ capture with `cargo run --release -p trunkd-p25 --example p25_decode -- ...` and keep
   the `frame` field of the `imbe` lines for one call, in order, one per line, as `<name>.hex`.
2. In a JMBE checkout, run a short Java harness that reads that file, calls
   `IMBEAudioCodec.getAudio(byte[])` once per frame on a single codec instance, and writes each sample
   as `(short)(sample * Short.MAX_VALUE)` little-endian.
3. `cargo run -p trunkd-mbe --example oracle_diff -- <name>.hex <reference.f32>` compares against the
   raw float output if you also keep it, and localises any mismatch to a frame and sample.

Muted frames are filled by JMBE from an unseeded `java.util.Random`, so they never match; the test
skips their samples.

## Why the clear pair is optional

The clear pair is 5.4 s of real dispatch speech. It cannot be synthesized (there is no IMBE encoder
in this project or in JMBE), so it is kept out of the public tree unless its content has been
checked and cleared for publication; it is excluded from the public source tree.

Without it, the four clear-call tests in `tests/jmbe_oracle.rs` are reported **ignored** with the
reason, and everything else, including the full encrypted-call oracle comparison, still runs. To run
them, put both clear files in this directory, or in any directory named by `TRUNKD_MBE_VECTORS`,
and rebuild: `crates/trunkd-mbe/build.rs` looks here first, then in `$TRUNKD_MBE_VECTORS`.

```sh
TRUNKD_MBE_VECTORS=/path/to/vectors cargo test -p trunkd-mbe --test jmbe_oracle
```
