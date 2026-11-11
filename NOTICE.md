# Notices and attribution

trunkd is free software, licensed under the GNU General Public License,
version 3 or (at your option) any later version (`GPL-3.0-or-later`). The full
license text is in [`LICENSE`](LICENSE).

Copyright © 2026 Eric Holzhueter.

trunkd contains code ported from, and design derived from, the projects below.
Their authors' copyright and license notices are reproduced here as those
licenses require. The license texts of the Rust crates trunkd links against
are collected in `THIRD-PARTY.md`, which is generated at release time by
`cargo about generate about.hbs > THIRD-PARTY.md`.

## JMBE

- Project: <https://github.com/DSheirer/jmbe>
- Copyright © Dennis Sheirer
- License: GPL-3.0

`crates/trunkd-mbe` is a structure-preserving Rust port of JMBE's IMBE
decoder (the `jmbe.codec.imbe` package and the `jmbe.codec`, `jmbe.edac` and
`jmbe.binary` pieces it depends on). It is distributed under
`GPL-3.0-or-later`, which is compatible with JMBE's GPL-3.0. The test vectors
in `crates/trunkd-mbe/tests/data/` named `jmbe_*` and `jtransforms_*` were
produced by running JMBE and JTransforms themselves and are used to check the port sample for sample.

## sdrtrunk

- Project: <https://github.com/DSheirer/sdrtrunk>
- Copyright © Dennis Sheirer
- License: GPL-3.0

sdrtrunk is the reference implementation that the P25 Phase 1 receive chain
in `crates/trunkd-p25` (demodulation, frame sync, NID/BCH, trellis and
Viterbi decoding, trunking message parsing and channel management) follows
closely; much of it is a port. The porting notes in `docs/extraction/`
paraphrase how sdrtrunk implements each part; they name sdrtrunk's Java
classes and constants but do not reproduce its source.

## JTransforms

- Project: <https://github.com/wendykierp/JTransforms>
- Copyright © 2007 onward, Piotr Wendykier
- License: BSD-2-Clause

`crates/trunkd-mbe/src/fft/` ports the JTransforms 3.2 split-radix kernels
that JMBE uses for its 256-point real FFT, including the twiddle and
bit-reversal tables, so the output is bit-identical. JTransforms' license
notice follows in full:

```text
Copyright (c) 2007 onward, Piotr Wendykier
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

JTransforms' FFT is itself derived from Takuya Ooura's General Purpose FFT
Package (<https://www.kurims.kyoto-u.ac.jp/~ooura/fft.html>, Copyright ©
1996–2001 Takuya Ooura), whose notice permits use, copying and modification
for any purpose without fee.

## LAME

- Project: <https://lame.sourceforge.io/>
- Copyright © the LAME project contributors
- License: LGPL-2.0-or-later (LAME 3.100, as shipped in `mp3lame-sys`)

`crates/trunkd-call` encodes MP3 through the `mp3lame-encoder` and
`mp3lame-sys` crates (<https://github.com/DoumanAsh/mp3lame-sys>, declared
`LGPL-3.0`), which build LAME 3.100 from source and link it statically. trunkd
uses LAME unmodified. The LGPL allows this inside a GPL-3.0-or-later program;
because trunkd's complete source is available under the GPL, anyone can
relink trunkd against a modified LAME.

## IMBE patents

The IMBE vocoder used by APCO Project 25 Phase 1 was covered by US patents
held by Digital Voice Systems, Inc. (DVSI). The patents commonly listed for
IMBE are:

| US patent | Title |
|---|---|
| 5,081,681 | Method and apparatus for phase synthesis for speech processing |
| 5,216,747 | Voiced/unvoiced estimation of an acoustic signal |
| 5,226,084 | Methods for speech quantization and error correction |
| 5,226,108 | Processing a speech signal with estimated pitch |
| 5,247,579 | Methods for speech transmission |
| 5,491,772 | Methods for speech transmission |
| 5,517,511 | Digital transmission of acoustic signals over a noisy communication channel |
| 5,630,011 | Quantization of harmonic amplitudes representing speech |
| 5,664,051 | Method and apparatus for phase synthesis for speech processing |
| 5,754,974 | Spectral magnitude representation for multi-band excitation speech coders |
| 5,870,405 | Digital transmission of acoustic signals over a noisy communication channel |
| 6,199,037 | Joint quantization of speech subframe voicing metrics and fundamental frequencies |

They were filed between 1989 (5,081,681, filed 1989-11-30) and 1997
(6,199,037, filed 1997-12-04). All of them have passed their full term. The
last to expire was 6,199,037, on 2017-12-04 (Google Patents lists it as
"Expired - Lifetime"). trunkd implements only IMBE (Phase 1, full rate). It
does not implement AMBE+2 or the Phase 2 half-rate vocoder, whose patents are
separate.

This section records the project's understanding. It is not legal advice.

## Trademarks

"P25" and "Project 25" refer to the APCO/TIA standard. Broadcastify,
OpenMHz, RadioReference, HackRF, Airspy and SoapySDR are names of their
respective owners. They are used here only to identify compatible services
and hardware. trunkd is not affiliated with or endorsed by any of them.
