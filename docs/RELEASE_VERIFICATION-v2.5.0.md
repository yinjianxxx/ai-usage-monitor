# v2.5.0 release verification

- Date: 2026-08-31
- Candidate: `v2.5.0`, based on `v2.4.2` / `badb412`
- Scope: a fourth quota provider (Grok), a WSL credential-probe fix affecting
  every provider, usage-cache coverage for the new provider, and tolerant
  parsing of unknown provider names; no change to the executable, settings
  directory, tray, or update identities
- Signing: not in scope (owner has no signing certificate)

## Automated gates

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo clippy --all-targets --locked -- -D warnings` | Pass |
| `cargo test --locked` | Pass: 314 passed, 0 failed, 0 ignored |
| RustSec (`cargo-audit`) | Pass in CI: 1233 advisories, 122 dependencies, 0 vulnerabilities, 0 warnings. Not run locally; not installed here |
| `tools/check-retired-identity.ps1` | Pass (13 historical lines) |
| current updater inbound readiness E2E | Pass |
| updater helper E2E: `Success` | Pass |
| updater helper E2E: `ChildExit` | Pass |
| `cargo build --release --locked` | Pass |
| `tools/check-portable-runtime.ps1` | Pass |
| preceding tag ancestry (`v2.4.2` is an ancestor of the candidate) | Pass |
| GitHub `verify` workflow on the release candidate | Pass (run 33398398858) |

The final local EXE reports FileVersion/ProductVersion `2.5.0`, ProductName
`Gengchou`, and CompanyName `ynjmxn`. Its local pre-release SHA-256 is
`53c2b9c6d81a961036e812dfa4da84e288f5239866cd96a1428fdb20b8b50eb2`.
The GitHub workflow rebuilds independently, so the public asset hash is expected
to differ and remains the only hash valid for WinGet.

## Targeted regression evidence

### Grok, against the live endpoint

- `GET /v1/billing?format=credits` returned HTTP 200 with
  `creditUsagePercent`, and a `currentPeriod` whose `start` and `end` carry
  fractional seconds and a numeric UTC offset rather than `Z`. A fixture of
  that exact shape is now a unit test, because the earlier fixture used whole
  seconds and would not have caught a parser that mishandles either form.
- The running application polled Grok and wrote it to the usage cache with
  `percent=6.0`, `duration_seconds=604800`, and
  `resets=2026-09-06T18:29:39Z` - the same instant the endpoint reported as
  `currentPeriod.end`.
- Window length is derived from `end - start`. A February fixture yields 28
  days rather than a rounded 30, which is what a period-type constant would
  have produced.
- Entry selection rejects an issuer outside `x.ai`, including a hostname that
  merely ends in the same text, and prefers the xAI OAuth scope when several
  entries are usable.

### WSL credential probes

Reproduced and fixed on Windows 11 26200 with WSL Ubuntu. With `--`, the probe
script runs in the distribution's login shell: `$0` reports `/bin/bash`, and a
two-statement script loses the variable its first statement assigned. Switching
to `-e` restored all four providers in the same process:

| Probe | Before | After |
| --- | --- | --- |
| Grok `auth.json` | not found | 1751 bytes, token selected |
| Codex `auth.json` | not found | 4441 bytes |
| Claude credential path resolution | failed | `/home/<user>/.claude/.credentials.json` |
| Antigravity token file | not found | not found - the distribution genuinely has none |

The Codex row is the evidence that this was a shipped, user-affecting defect
rather than a theoretical one: a real Codex session existed in that
distribution and was never read.

### Settings migration, on a real pre-existing profile

An installation with `consent_schema_version = 1` and consent already granted
migrated to version 2 and gained `allow_grok_credentials = true`, while
`show_grok` stayed `false` and `grok_credential_access_decided` stayed `false`,
so the detector still owes the user one notification before Grok can appear.
`provider_order` gained `grok` at the end.

### Usage cache

`UsageCacheFile` initially had no Grok section, which is invisible at runtime -
the provider polls and renders normally and only loses its value across a
restart. The provider-to-field mapping is now a fixed-size array indexed by
provider count, so omitting a provider is a compile error, and a test
round-trips every provider through the serialized form.

### Unknown provider names

A settings file naming a provider this build does not know now loads with that
entry dropped and every other field intact; normalization then re-appends the
providers this build does know.

### Independent pre-release review

A second agent reviewed `v2.4.2..HEAD` in the same working copy and initially
returned a no-go. Its three findings were reproduced here before being fixed:

- **Release blocker.** Grok reached the poll pass but not the failure-recovery
  state machine: `update_provider_refresh_states`, the credential-watch
  predicates, the all-failed transient check, and the stale-transition timing
  each spelled out three providers in parallel expressions rather than matching
  on the provider, so the compiler could not catch the omission. Grok therefore
  recorded no rate-limit cooldown, counted no consecutive failures, never went
  stale, and never raised the one authentication balloon it owes the user. All
  five now iterate `TrayIconKind::ALL`, and
  `every_provider_reaches_the_refresh_state_machine` fails if a provider is
  dropped from that loop - verified by temporarily restricting it to three.
- **Medium.** Loading the usage cache at startup cleared unauthorized Claude,
  Codex, and Antigravity values but not Grok, so a revoked Grok could be
  restored from disk. The filter now iterates every provider and uses
  `provider_has_credential_access`, the same predicate that gates polling.
- **Low.** Both READMEs rendered the Windows path as `%GROK_HOME%uth.json`
  with the backslash written as U+0007, and the network-traffic list did not
  name xAI. Both are corrected.

## Not verified

- Fresh-profile first run, both consent and decline paths.
- The one-time detection notification for Grok on an upgraded profile. The
  periodic sweep runs every 30 minutes, and the run was verified by enabling
  Grok directly instead of waiting for it.
- **Provider access -> Detect providers again** from the menu.
- Stopped versus running WSL distribution behaviour for the Grok probe.
- High Contrast rendering of the new tray placeholder letter `X`.
