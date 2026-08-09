# v2.4.2 release verification

- Date: 2026-08-09
- Candidate: `v2.4.2`, based on `v2.4.1` / `159fecf`
- Scope: reliability hardening, persistence, diagnostics, bounded JSON, and
  release governance; no UI layout, credential source, or identity migration
- Signing: not in scope (owner has no signing certificate)

## Automated gates

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo clippy --all-targets --locked -- -D warnings` | Pass |
| `cargo test --locked` | Pass: 299 passed, 0 failed, 0 ignored |
| RustSec (`cargo-audit 0.22.1`, 1190 advisories, 122 dependencies) | Pass |
| `tools/check-retired-identity.ps1` | Pass |
| current updater inbound readiness E2E | Pass |
| updater helper E2E: `Success` | Pass |
| updater helper E2E: `ChildExit` | Pass |
| `cargo build --release --locked` | Pass |
| `tools/check-portable-runtime.ps1` | Pass |
| previous-release ancestry gate, simulated for `v2.4.2` | Pass: `v2.4.1` is an ancestor |

The final local EXE reports FileVersion/ProductVersion `2.4.2`, ProductName
`Gengchou`, CompanyName `ynjmxn`, and OriginalFilename `gengchou.exe`. Its local
pre-release SHA-256 is
`bf6e4fd1381e27829d32326c04f2fc5df018547783462f57b0fc7dcc24f15a05`.
The GitHub workflow rebuilds independently, so the public asset hash is expected
to differ and remains the only hash valid for WinGet.

## Targeted regression evidence

- WSL probe spawn failure and timeout remain distinguishable, and
  `ProbeUnavailable` maps to `RequestFailed`, never to an authentication alert.
  True unreadable/malformed/expired/rejected credential states remain
  `AuthenticationFailed`.
- Settings and usage-cache writers have independent revision streams. A
  `Barrier` test forces the newer snapshot to write first and proves the older
  late arrival is discarded.
- Diagnostic tests cover runtime rotation, following an externally replaced
  current pathname, and retaining only current plus one `.old` generation.
- Bounded-response tests cover exactly 4 MiB, one byte over the limit, and an
  oversized declared `Content-Length`; every production JSON response entry
  uses the bounded reader.
- Invalid, short, and non-ASCII theme colors fall back without a panic.

## Packaging and deterministic visual checks

- A workflow-equivalent local package contains six attachments, five hashed
  payloads, and exactly seven ZIP entries. The executable inside the ZIP has
  the same hash as the standalone local asset.
- `tools/render-readme-images.ps1` was rerun from the v2.4.2 build. Four detail
  images changed only because the displayed version advanced; English/dark and
  Simplified-Chinese/light outputs were inspected and render correctly.
- The pre-existing v2.4.0 resident process was closed through Gengchou's normal
  `WM_APP_REQUEST_QUIT` path before the final release build; no force-kill was
  used.

## Manual rows unavailable or deferred

- **N/A — physical Shell notification glyph and sound:** the current automation
  cannot observe Windows notification audio or force the app-icon load failure
  path. Flag-level unit coverage passes; this release does not alter balloon
  rendering itself.
- **N/A — disposable WSL credential matrix:** no isolated WSL distro/account was
  available for safe exit-45 and unexpected-exit manipulation. The command
  runner and classification boundaries are covered directly by tests.
- **N/A — multi-DPI, vertical/multi-row taskbar, multi-monitor, RDP, Narrator,
  and High Contrast hardware sweeps:** those surfaces are unchanged in v2.4.2;
  deterministic layout, localization, accessibility, and High Contrast tests
  pass, and the README fixtures were regenerated.
- **Deferred until the tag workflow — public asset re-download, exact attachment
  set, SHA256SUMS, and provenance attestation:** these are enforced before the
  release workflow changes the draft to public.
- **Deferred until the public release — clean WinGet install/update/uninstall:**
  the manifest must be generated from the public ZIP and can only be exercised
  after the WinGet PR is merged.

No pre-tag blocker remains within the verified scope. Deferred rows are ordered
release/post-release gates, not claims of completion.
