# Changelog

## v0.2.0

### JBL Bass Mode
- **Auto level** (on by default): the effect follows input loudness, so distortion, pump and bass boom sound the same at any volume, and output stays just under the original level. Turn off for the legacy fixed-drive behaviour.
- **Level guard** (off by default): optional safety net that keeps the output from exceeding the original level (plus any bass boost).
- **Mids/highs pass-through** with an adjustable crossover (default 15 % at 400 Hz), so vocals and highs can stay clean while the bass distorts.
- **Ducking floor**: limits how far the level dips on a bass hit (default -9 dB; -24 = uncapped).
- **Bass boost**: clean low-shelf gain applied after the distortion, with its own corner frequency.
- **Presets**: new preset picker in the Fun panel; editing any value switches it to "Custom".
- Retuned defaults: stronger drive and ducking, rattle LFO on, higher low-pass cutoffs, default output trim 1.1 dB. Trim range widened to -12..+12 dB.
- Older saved `[jbl]` config tables load unchanged: new keys default to values that preserve the old sound.

### Other
- Expanded JBL audit tests and updated `docs/configuration.md`.
- CI: builds and tests now run again after the GitHub billing lock was cleared.

## v0.1.0

- Initial release: EQ ASIO router with JBL Bass Mode, AutoEQ enable toggle, configurable JBL parameters.
