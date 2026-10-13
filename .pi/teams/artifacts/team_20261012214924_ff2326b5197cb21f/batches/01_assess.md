=== Task 1: 01_assess (planner) ===
Status: COMPLETED
Role: planner
Result artifact: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_ff2326b5197cb21f\results\01_assess.txt
Log artifact: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_ff2326b5197cb21f\logs\01_assess.log
Transcript: C:\Projects\eq_asio_router\.pi\teams\artifacts\team_20261012214924_ff2326b5197cb21f\transcripts\01_assess.attempt-0.jsonl
Usage: {"input":598,"output":11749,"cacheRead":48000,"cacheWrite":0}
## Rationale

I inspected the repo to size the work: the main docs (`README.md`, `docs/architecture.md`, `docs/configuration.md` 43 KB, `docs/latency.md` 49 KB, `setup-guide.md`, `troubleshooting.md`, `development.md`) still contain **zero** mentions of VBMatrix/bridge and are saturated with removed terms (`--source`, `capture_device`, `frozen v1`, Hi-Fi/VAC wording), while the code side is already fully cut over (`REMOVED_CONFIG_KEYS` in `eq-core/src/config.rs:33`, status v3/192 bytes in `eq-core/src/status.rs:11`, rejected-flag test in `router-cli/src/main.rs:621`). The work is a **file-disjoint parallel edit job**, so the efficient crew is: one code-facts explorer (so 7 writers don't each re-derive thread names/status fields/deprecation wording and drift), 7 parallel executors with strict per-file ownership, one post-edit grep sweep (must be sequential — it depends on all edits), then gates + two independent reviewers. Display-string edits in `eq-asio-app` are delicate (tests assert exact tooltip text at `tray.rs:638`, and several `"EQ ASIO Router"` occurrences are the frozen device/registry name that must NOT change: `autostart.rs:24`, `app.rs:197`, `panels/bridge.rs:44`), so they get a dedicated executor. Scripts (`install-driver.ps1` currently has the bitness default **inverted** vs. the spec) fold into the plan-doc owner since `vbmatrix-bridge.md:119` documents that exact behavior. Total: 12 subagents, 4 phases.

```ADAPTIVE_PLAN_JSON_START
{
  "phases": [
    {
      "name": "code-facts-extraction",
      "tasks": [
        {
          "role": "explorer",
          "title": "Extract bridge-era code facts for doc writers",
          "task": "READ-ONLY. In C:/Projects/eq_asio_router, grep-then-read (never whole files) and produce a compact FACT SHEET the doc writers will rely on. Cover: (1) router-engine::bridge + eq-asio-driver v2 thread/actor inventory: exact names and jobs of the bridge status tick, loopback feeder, and driver pacing thread (grep crates/router-engine/src/bridge.rs, crates/eq-asio-driver/src/); (2) status block v3: STATUS_VERSION=3, 192 bytes, every bridge field name from bridge_attached..b_dropped (crates/eq-core/src/status.rs); (3) [bridge] config: ring_target_ms (default 0.0, clamp 0..200) and loopback_enabled (default true) semantics + which changes are live vs restart (crates/eq-core/src/config.rs, router-engine/src/logic.rs); (4) REMOVED_CONFIG_KEYS list and the exact one-shot deprecation notice wording incl. 'VBMatrix' mention; (5) current router-cli flag surface after the cutover (run/latency/impulses help text in crates/router-cli/src/main.rs) — which flags exist now, which are rejected; (6) latency/impulses behavior: VBMatrix In allowed sinks, BRIDGE_SOURCE_KEY / 'source=vbmatrix' key in eq-core/src/measured.rs, stage-1 ring-A tap; (7) current crate list incl. bridge-abi and eq-asio-driver v2 role, and workspace test suite count; (8) eq-asio-app DISPLAY-STRING inventory: classify every 'EQ ASIO Router' occurrence in crates/eq-asio-app/src as (A) project display string that the Headroom rebrand may change [main.rs:69 window title, main.rs:82 run_native name if purely display, app.rs:471 heading, tray.rs:154 tooltip base, cli.rs:4 usage text, tray tooltip tests tray.rs:638-640], vs (B) frozen identifiers that MUST NOT change [autostart.rs registry VALUE_NAME, app.rs:197 device filter, panels/bridge.rs find_driver/registered text, tray window class name]. Also list which tests assert display strings. Output as terse bullet facts with file:line anchors; include the exact recommended Headroom strings (window title 'Headroom', tooltip 'Headroom — <label>', tagline from docs/plans/naming-decision.md). Do not edit anything."
        }
      ]
    },
    {
      "name": "parallel-file-owners",
      "tasks": [
        {
          "role": "writer",
          "title": "README + docs index rebrand and rewrite",
          "task": "Own ONLY README.md and docs/README.md. Obey docs/plans/naming-decision.md: display brand is 'Headroom', tagline 'System-wide parametric EQ at the lowest latency your hardware allows.'; the out-of-scope list (binaries, crate names, config paths, the 'EQ ASIO Router' ASIO device name, status mapping) must stay unchanged — 'EQ ASIO Router' remains wherever it names the registered ASIO device. In README.md: retitle to '# Headroom' with tagline; add <img src=\"docs/assets/headroom-icon.svg\" width=\"96\" alt=\"Headroom\"> near the title; rewrite intro (VBMatrix In WDM source, not Hi-Fi Cable); replace the architecture diagram with: apps -> VBMatrix In WDM -> VBMatrix matrix -> ASIO slot \"EQ ASIO Router\" DLL <= shm rings => engine -> Yamaha; Features: add VBMatrix source + post-EQ loopback, remove VAC/Hi-Fi Cable/arrival-model/render-keepalive/cable-rate-matching wording; Requirements: VBMatrix replaces Hi-Fi Cable + driver registration step; Quick start: scripts/install-driver.ps1, pick 'EQ ASIO Router' in a VBMatrix ASIO slot, Device Master/Internal Clock note; Crates list: add bridge-abi, describe eq-asio-driver as the v2 bridge driver (drop 'frozen v1'). In docs/README.md: update index rows so descriptions match the post-bridge docs (remove VAC Line 1 / Hi-Fi Cable marketing, fix any links you break). Process: grep -n first, targeted offset/limit reads, surgical edits with tight anchors — no wholesale rewrites. Do NOT touch docs/plans/cross-platform.md or any other file."
        },
        {
          "role": "writer",
          "title": "docs/architecture.md bridge rewrite",
          "task": "Own ONLY docs/architecture.md. Use the phase-1 fact sheet if provided; otherwise grep crates/router-engine/src/bridge.rs, crates/bridge-abi/src, crates/eq-asio-driver/src, crates/eq-core/src/status.rs first for exact names. Surgical section edits: §1 data-flow diagram -> bridge chain (apps -> VBMatrix In -> matrix -> ASIO slot DLL <=> shm rings -> rate matcher/EQ -> Yamaha); §2 threads table: drop Capture thread, Cable-format helper, Render keep-alive, Source-readiness probe; add bridge status tick, loopback feeder, driver pacing thread (exact owners/jobs from code, not invented); §3 RT rules: ring A/B atomic SPSC rules, no WASAPI on the audio path; §5a: remove the cable-rate/fixed-period bullets (the whole subsection if it becomes empty); §6 crate table: drop cable.rs/source.rs responsibilities, add router-engine::bridge and the bridge-abi crate, mark eq-asio-driver as the v2 bridge driver and wasapi-capture as capture-probe only; §8: 'frozen v1 driver' -> v2 bridge driver description (same registered name 'EQ ASIO Router' and CLSID, pacing thread, shm rings); status block: v3 / 192 bytes with the bridge field group. Keep headings/anchors stable where other docs link to them (grep for incoming '#'-anchors before renaming any heading). grep -n first, tight anchors, no wholesale rewrite. Do NOT touch other files or docs/plans/cross-platform.md."
        },
        {
          "role": "writer",
          "title": "docs/configuration.md cutover update",
          "task": "Own ONLY docs/configuration.md (43 KB — surgical, never rewrite wholesale). (a) REMOVE rows/sections for capture_device, capture_mode, capture_period_ms, render_keepalive, match_cable_rate, source_preset, jitter_margin_by_device and other per-device jitter-margin docs (keep the single global jitter_margin_ms if the code still reads it — verify in crates/eq-core/src/config.rs first); remove CLI docs for the rejected flags --source/--capture/--mode/--period-ms/--keepalive/--no-match-cable from run/latency usage blocks and flag tables (confirm rejection list at crates/router-cli/src/main.rs ~line 621). (b) ADD [bridge] section: ring_target_ms (default 0.0, clamped 0..=200 ms, applied live) and loopback_enabled (default true, restart-class) — verify against eq-core/src/config.rs:134-151 and router-engine/src/logic.rs:57. (c) Document the one-shot deprecation notice for removed keys (take_deprecation_notice, mentions the keys + VBMatrix, once per process). (d) Document status block v3 / 192 bytes and its bridge fields (bridge_attached, driver_running, negotiated rate/buffer, ring A/B fills, drop counters — exact names from eq-core/src/status.rs). (e) Update latency/impulses behavior text: VBMatrix In endpoints allowed as sinks, source=vbmatrix key, stage-1 ring-A tap. grep every term before editing; keep historical measurement tables if clearly labelled. Do NOT touch other files or docs/plans/cross-platform.md."
        },
        {
          "role": "writer",
          "title": "docs/latency.md + latency subdocs bridge wording",
          "task": "Own ONLY docs/latency.md and docs/latency/vac_lite.md, docs/latency/capture_apis.md, docs/latency/apo_tap.md. In latency.md: change stage-1 wording to 'ring-A source tap'; strip removed flags from every command example (--source, --capture, --period-ms, --keepalive, --no-match-cable) keeping only flags that exist today (verify against crates/router-cli/src/main.rs); budget wording: source=bridge / VBMatrix chain; mark VAC and Hi-Fi Cable sections clearly 'Historical (pre-bridge)' — label only, do NOT rewrite or delete the measurements. In the three subdocs add ONLY a one-line historical banner under the title (e.g. '> Historical (pre-bridge): written for the WASAPI cable source; kept for reference.'). Preserve all existing heading anchors (#5a-..., #5b-..., #7-first-run-validation-checklist-vac-line-1) because README.md, setup-guide.md, development.md and configuration.md link to them — if you must rename a heading, grep the workspace for the old anchor and report it for the sweep phase. grep -n first, surgical edits. Do NOT touch other files or docs/plans/cross-platform.md."
        },
        {
          "role": "writer",
          "title": "setup-guide + troubleshooting bridge equivalents",
          "task": "Own ONLY docs/setup-guide.md and docs/troubleshooting.md. setup-guide.md: replace all --source/--capture/--keepalive/--period-ms examples with current flags; rewrite the active setup path per docs/plans/vbmatrix-bridge.md (~line 159): install VBMatrix, Windows default playback = VBMatrix In, master clock = Internal Clock, install/register the driver via scripts/install-driver.ps1 (64-bit default, -Both for 32-bit hosts), pick 'EQ ASIO Router' in a VBMatrix ASIO slot; demote the VAC Line 1 quick start / Hi-Fi Cable comparison to 'Historical (pre-bridge)' labels (keep content, no measurement rewrites) or remove instructions that reference deleted UI/presets; update the rate-match / capture_device explanation rows (capture_device no longer exists). troubleshooting.md: convert capture-failure rows into bridge equivalents — VBMatrix not running, driver not registered (re-run scripts/install-driver.ps1), slot not started, Device Master/clock note, mapping not found, ring fill/underrun counters, status v3 — and remove/label rows for capture_mode, capture_period_ms fallback banners, VAC readiness notes, 'frozen v1' log row (the %TEMP% driver log row must reflect the v2 bridge driver). Keep the table format; grep -n first; tight anchors. Do NOT touch other files or docs/plans/cross-platform.md."
        },
        {
          "role": "writer",
          "title": "development.md + plan milestones + install scripts",
          "task": "Own ONLY docs/development.md, docs/plans/vbmatrix-bridge.md, scripts/install-driver.ps1, scripts/uninstall-driver.ps1. (1) development.md: update crate map (add bridge-abi, eq-asio-driver v2 bridge, router-engine::bridge; wasapi-capture = capture-probe only), test-strategy list (workspace suites incl. bridge cross-process tests), both driver build targets (x86_64 default and i686-pc-windows-msvc), registration steps reflecting the updated scripts, and remove/replace --source/--capture examples and VAC-era 'known limitations' items. (2) vbmatrix-bridge.md: mark milestones M0, M1, M2, M3 (and M3a/M3b split as written) COMPLETE with one evidence line each — asio-probe run showed 937/938 callbacks in 5 s, gates green (cargo build --workspace --all-targets 0 warnings, cargo test --workspace 25 suites), code committed at 0728f2c; leave M4 (hardware checklist) OPEN; add completion ticks to Component 4 items (use '- [x]' markers or a leading 'Done:' prefix — keep the bullets). Do not alter technical content of the plan. (3) scripts: INVERT the bitness default — default registers the 64-bit DLL (VBMatrix is x64), -Both additionally registers the 32-bit DLL (currently the opposite: install-driver.ps1 lines 11-13, 38-51); mirror in uninstall-driver.ps1; replace 'VB-Audio ASIO Bridge' wording with VBMatrix ('32-bit DLL for 32-bit ASIO hosts'); final success message must say to pick 'EQ ASIO Router' in a VBMatrix ASIO slot; KEEP self-elevation exactly as is; keep registry key names 'SOFTWARE\\ASIO\\EQ ASIO Router' untouched. If pwsh is available, syntax-check both scripts yourself (e.g. pwsh -NoProfile -Command \"[void][System.Management.Automation.Language.Parser]::ParseFile(...)\"). Do NOT touch any other file or docs/plans/cross-platform.md."
        },
        {
          "role": "executor",
          "title": "eq-asio-app Headroom display strings only",
          "task": "Own ONLY crates/eq-asio-app/src. Change display strings to the Headroom brand, nothing behavioral: main.rs:69 window title 'EQ ASIO Router' -> 'Headroom'; main.rs:82 eframe::run_native name -> 'Headroom' ONLY if it is purely a display/app-id string (check first whether anything behavioral depends on it; if in doubt leave it and report); app.rs:471 ui.heading -> 'Headroom'; tray.rs:154 tooltip base 'EQ ASIO Router — {label}' -> 'Headroom — {label}'; cli.rs:4 usage banner 'EQ ASIO Router (GUI)' -> 'Headroom (GUI)'. MUST NOT change: autostart.rs VALUE_NAME and its test (registry), app.rs:197 device-name filter, panels/bridge.rs find_driver('EQ ASIO Router') and the '\"EQ ASIO Router\" registered' text (registered device name), tray window class name (internal identifier), router-cli or any other crate. Update the tray tooltip assertions in tray.rs tests (~lines 636-641) to the new strings — they assert display text, so this is in scope; grep for any other test asserting a changed string. Then run cargo build -p eq-asio-app --all-targets and cargo test -p eq-asio-app and iterate until green with 0 warnings. Do not modify docs, scripts, or any crate outside eq-asio-app."
        }
      ]
    },
    {
      "name": "sweep",
      "tasks": [
        {
          "role": "executor",
          "title": "Post-edit grep sweep for dead terms",
          "task": "Runs AFTER all writers finish. grep -rn across README.md, docs/ (excluding docs/plans/cross-platform.md — DO NOT EDIT IT), scripts/ for: '--source', '--capture', '--period-ms', '--keepalive', '--no-match-cable', 'capture_device', 'capture_mode', 'capture_period_ms', 'render_keepalive', 'match_cable_rate', 'source_preset', 'jitter_margin_by_device', 'frozen v1', 'ASIO Bridge', 'Hi-Fi Cable', 'VAC Line 1', 'arrival model', 'EQ ASIO Router' (verify each is either the legitimate registered-device name or a historical label). Fix live-section stragglers with surgical edits; EXEMPT historical/log sections (Historical (pre-bridge) labels, measure/ logs, reverse/ docs, plans' historical narration) — instead list them in your report as 'intentional historical mentions'. Also verify: no dangling '#anchor' links after heading changes (grep each changed anchor workspace-wide), the Headroom rebrand is consistent (title/logo/tagline present in README), and git diff shows NO changes to docs/plans/cross-platform.md, crates/eq-asio-driver, crates/bridge-abi. Report files changed, leftovers kept and why."
        }
      ]
    },
    {
      "name": "gates-and-review",
      "tasks": [
        {
          "role": "test-engineer",
          "title": "Run M3b gates",
          "task": "Run and report exact results: (1) cargo build --workspace --all-targets — must be 0 warnings, 0 errors; (2) cargo test --workspace — all suites green (expected 25); (3) if pwsh is available, syntax-check scripts/install-driver.ps1 and scripts/uninstall-driver.ps1 (PowerShell Parser::ParseFile, no execution, no elevation); (4) confirm the default/ -Both bitness logic in both scripts is consistent (default=64-bit, -Both adds 32-bit) by reading them. Read-only apart from build/test side effects. Report pass/fail with command output excerpts; if a gate fails, report the exact failure and which phase-2 owner's files are implicated — do not fix docs yourself unless the failure is a test assertion in crates/eq-asio-app/src (then report, do not edit)."
        },
        {
          "role": "reviewer",
          "title": "Scope and rebrand compliance audit",
          "task": "READ-ONLY audit of the full diff (git diff / git status). Verify: (1) no code-behavior changes — the only Rust edits are display strings + their assertions in crates/eq-asio-app/src; (2) crates/eq-asio-driver and crates/bridge-abi untouched; (3) docs/plans/cross-platform.md untouched (parallel crew owns it); (4) naming-decision.md out-of-scope list obeyed: binary names, crate/module names, config file paths, the 'EQ ASIO Router' ASIO device name, CLSID/registry, status mapping name 'Local\\eq_asio_router_status', log file names all unchanged; (5) Headroom rebrand strings are consistent across README/docs (title, tagline, logo tag exactly <img src=\"docs/assets/headroom-icon.svg\" width=\"96\" alt=\"Headroom\">, window title/tray tooltip); (6) install scripts keep self-elevation and registry key names. Produce a PASS/FAIL checklist with file:line evidence for every failure and exact recommended fixes. Do not edit anything."
        },
        {
          "role": "reviewer",
          "title": "Doc consistency and cross-reference audit",
          "task": "READ-ONLY audit of README.md and docs/ for internal consistency after the update: (1) every markdown link and '#anchor' resolves (especially latency.md #5a/#5b/#7 anchors referenced from README, setup-guide, development, configuration); (2) no live section still documents removed config keys or rejected CLI flags (historical-labelled sections exempt); (3) architecture.md threads table / crate table / status v3 192-byte description agree with the code (spot-check crates/eq-core/src/status.rs, router-engine/src/bridge.rs); (4) configuration.md [bridge] defaults match eq-core/src/config.rs (ring_target_ms 0.0, loopback_enabled true); (5) troubleshooting rows reference real current behaviors (VBMatrix not running, driver not registered, slot not started, Device Master); (6) vbmatrix-bridge.md shows M0-M3a complete with evidence and M4 still open. Output PASS/FAIL per item with file:line evidence; do not edit anything."
        }
      ]
    }
  ]
}
ADAPTIVE_PLAN_JSON_END
```

---

```
PLAN: M3b docs/rebrand/scripts update for the VBMatrix bridge cutover via 1 facts explorer + 7 file-disjoint writers + 1 sweep + gates/2 audits (12 subagents, 4 phases).
PHASES:
  - id: P1, title: code-facts extraction, owner_role: explorer, depends_on: [], scope: Read-only fact sheet (threads, status v3 fields, [bridge] config, removed keys/flags, display-vs-identifier string classification) feeding all writers.
  - id: P2, title: parallel file-owner edits, owner_role: writer|executor, depends_on: [P1], scope: Seven disjoint owners edit README+index, architecture, configuration, latency+subdocs, setup-guide+troubleshooting, development+plan+scripts, eq-asio-app display strings.
  - id: P3, title: grep sweep, owner_role: executor, depends_on: [P2], scope: Workspace sweep for removed flags/dead terms and broken anchors, fixing live stragglers, exempting historical sections and cross-platform.md.
  - id: P4, title: gates and audits, owner_role: test-engineer|reviewer, depends_on: [P3], scope: cargo build/test gates + pwsh syntax check, scope/rebrand compliance audit, doc consistency/cross-reference audit.
DEPENDENCIES:
  - crates/eq-core/src/status.rs, config.rs; crates/router-engine/src/bridge.rs; crates/router-cli/src/main.rs: phase P1, reason source of truth for status v3/[bridge]/flag facts writers must document.
  - docs/plans/naming-decision.md: phase P2, reason in-scope/out-of-scope rebrand rules every writer obeys.
  - docs/plans/vbmatrix-bridge.md: phase P1 (read), P2 (edit by plan owner), reason approved plan + milestone evidence source.
  - All P2 outputs: phase P3, reason sweep must run after every edit to catch stragglers and validate anchors.
  - P3 sweep output: phase P4, reason gates and audits must judge final state.
  - pwsh (optional external): phase P4, reason script syntax check only if available.
OWNERSHIP:
  - README.md, docs/README.md: P2, no_conflict_with: [P3, P4]
  - docs/architecture.md: P2, no_conflict_with: [P3, P4]
  - docs/configuration.md: P2, no_conflict_with: [P3, P4]
  - docs/latency.md, docs/latency/*.md: P2, no_conflict_with: [P3, P4]
  - docs/setup-guide.md, docs/troubleshooting.md: P2, no_conflict_with: [P3, P4]
  - docs/development.md, docs/plans/vbmatrix-bridge.md, scripts/*.ps1: P2, no_conflict_with: [P3, P4]
  - crates/eq-asio-app/src: P2, no_conflict_with: [P3, P4]
  - FROZEN: crates/eq-asio-driver, crates/bridge-abi, docs/plans/cross-platform.md (parallel crew): no phase owns them; P4 audits they are untouched.
VERIFICATION_GATES:
  - after_phase: P1, gate: verifier, success_criteria: fact sheet contains all 8 requested sections with file:line anchors, incl. display-vs-identifier classification of every eq-asio-app "EQ ASIO Router" occurrence.
  - after_phase: P2, gate: reviewer, success_criteria: each owner's file greps clean of removed flags/keys in live sections; no file edited outside its owner's list (git status).
  - after_phase: P3, gate: test-engineer, success_criteria: grep for --source|capture_device|frozen v1|ASIO Bridge etc. returns only entries classified as historical/log; all '#anchor' links resolve.
  - after_phase: P4, gate: test-engineer, success_criteria: cargo build --workspace --all-targets = 0 warnings/errors; cargo test --workspace all suites green (25); pwsh syntax check passes if available; both audits PASS with file:line evidence.
ROLLBACK_PLAN: No commits are made (commitPolicy: do not commit unless requested), so rollback is `git checkout -- <changed files>` / `git restore .` for the whole working tree; the sweep and audits read git status, so any unintended file (cross-platform.md, frozen crates) is detected and reverted first. Worst case, the pre-existing green state at 0728f2c is untouched in history.
ASSUMPTIONS_NOTED:
  - Code state is committed at 0728f2c and gates were green (0 warnings, 25 test suites) before M3b; docs/scripts carry the entire delta.
  - Only display strings in crates/eq-asio-app may change in code; tests asserting those display strings may be updated to match; no other Rust behavior changes.
  - docs/plans/cross-platform.md is owned by a parallel read-only research crew and must not be touched; crates/eq-asio-driver and crates/bridge-abi are frozen.
  - Historical/log sections (Historical (pre-bridge) labels, measure/ logs, reverse/ docs, plans narration) are exempt from the dead-term sweep and keep their measurements unrewritten.
  - pwsh may be unavailable; script syntax check is best-effort and reported as skipped if so.
  - "EQ ASIO Router" remains the registered ASIO device name, CLSID/registry keys, autostart registry value, status mapping name, binary and crate names, and config paths throughout.
```