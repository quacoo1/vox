# vox-audio test fixtures

| File | What | Provenance and licence |
|---|---|---|
| `tone-44k-mono.mp3` | 4 290 bytes: an ID3v2.4 tag, a CBR `Info` frame with a `Lavf` LAME extension (encoder delay 576, padding 1000 — the layout ElevenLabs uses), then 40 MPEG-1 Layer III frames (44.1 kHz, mono, 32 kbps) whose granules each carry one spectral line, decoding to a steady ≈ 210 Hz tone at about −12 dBFS for 44 504 frames (≈ 1.009 s) after gapless trimming. | Generated procedurally by vox-audio's own test code (`decode::tests::mp3gen`); no third-party audio or encoder was involved. Dedicated to the public domain under CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/). Regenerate with `cargo test -p vox-audio -- --ignored regenerate_mp3_fixture`; the `fixture_matches_its_generator` test keeps the file and its generator in sync. |

No real ElevenLabs audio is committed: the gapless behaviour of real files was verified locally against an Electron
profile's audio cache (see the `decode` module docs) and is reproduced here by the generated tag.
