# Naming decision: "Headroom"

Status: confirmed by the user (2026-10-12). Applies from milestone M3b onward,
on top of `vbmatrix-bridge.md` and alongside `cross-platform.md`.

- **Display / brand name: "Headroom"** — tagline: *"System-wide parametric EQ at
  the lowest latency your hardware allows."*
- Chosen over Biquad / Flatline / Roundabout for pro-audio tone and the tie-in to
  the auto-headroom safety feature.

## In scope for M3b (documentation rebrand)

- README title, docs titles/intro lines, tray tooltip and GUI window title strings,
  status/tray wording ("Headroom is running …"), plan doc headers where they name
  the project.

## Explicitly out of scope (keep stable for compatibility)

- Binary names: `eq-asio-app.exe`, `router-cli.exe`, `asio-probe.exe`, …
- Crate names (`eq-core`, `router-*`, `eq-asio-driver`, …) and module names.
- Registered ASIO device name **"EQ ASIO Router"** (VBMatrix shows it; CLSID and
  registry stay as-is), status mapping `Local\eq_asio_router_status`, log/config
  file names and paths (`%APPDATA%`, `config.toml` locations).
- A physical rename of any of the above may happen later with a compatibility
  shim; do not do it in this plan.
