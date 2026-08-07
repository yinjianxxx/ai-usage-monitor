# Adding a provider

Working notes for the cost of adding a fourth quota provider, plus the
OpenCode Go research that prompted them.

> **This document is a snapshot taken on 2026-08-06. Re-verify before acting
> on any of it.**
>
> Two kinds of claims appear below, and they decay at different speeds:
>
> - **In-repo facts** (file names, symbol names, reference counts, line
>   numbers). Verifiable with the commands in *Re-measuring* at the end of this
>   document. Line numbers drift with every commit; anchor on the symbol name,
>   not the number.
> - **External facts** (upstream endpoints, third-party repositories, issue and
>   pull-request state, another product's file layout). These can be wrong at
>   any time without anything in this repository changing, and several of them
>   were already unstable when they were written down. Treat every one as a
>   lead to re-check, never as a settled fact.

## 1. What a fourth provider costs today

Providers are not modelled as a collection. Claude Code, Codex, and Antigravity
each exist as a set of parallel fields and match arms, so a fourth provider is a
diff across the whole tree rather than a new table row.

As of this snapshot, `antigravity` — the most recently added provider, and so
the best proxy for what a fourth one touches — appears on **600 lines** across
`src/`:

| File | Lines | What lives there |
| --- | --- | --- |
| `src/window.rs` | 283 | refresh state, widget data, usage cache, menus, detail rows |
| `src/poller.rs` | 187 | credential read, endpoint, parse, backoff, detection |
| `src/settings.rs` | 53 | visibility, permission, announcement, order, migration |
| `src/provider_tile.rs` | 25 | brand tiles per DPI bucket and theme |
| `src/tray_icon.rs` | 22 | kind, brand mapping, colour, placeholder letter, add/remove |
| `src/compact_view.rs` | 10 | view-model assembly for widget and floating window |
| `src/compact_layout.rs` | 4 | badge geometry and wrapping |
| `src/models.rs` | 4 | `AppUsageData` parallel fields |
| `src/localization/*.rs` | 12 | one display string per language, plus the struct field |

### 1.1 Code touchpoints

- `models::AppUsageData` — four parallel fields per provider (`usage`,
  `updated_unix`, `error`, `retry_after_ms`).
- `tray_icon::TrayIconKind` — the de facto provider identity; already the
  serialized key for `provider_order`. Adding a variant is the smallest part.
- `tray_icon::provider_color` and `tray_icon::placeholder_letter` — the letters
  in use are `A` (Claude), `O` (Codex), `G` (Antigravity). **A fourth provider
  needs a letter that is not already taken, or a logo-only tray fallback.**
  Decide this before drawing anything.
- `poller::poll_with` — three near-identical error-handling blocks, one per
  provider, plus a thread per provider in the scope.
- `poller::DetectedProviders` / `detect_from` — first-run and re-detection.
- `settings::SettingsFile` — `show_*`, `allow_*_credentials`, and
  `*_credential_access_decided` triples. `provider_order` needs no migration:
  `settings::normalize_provider_order` already appends any kind missing from a
  stored order and de-duplicates it.
- `compact_view::build` and `compact_view::placeholder_model` — match arms per
  kind.
- `window.rs` — by far the largest surface; refresh states, cached widget data,
  the usage cache file, menu construction, and detail-popup rows all enumerate
  providers explicitly.

### 1.2 Everything a refactor would *not* help with

This is the part worth reading before assuming an abstraction pays for itself.
Roughly half the work of adding a provider is untouched by any amount of
restructuring:

- **Brand tiles.** Add `src/icons/providers/source/<name>.svg`, then regenerate
  with `tools/generate_provider_logos.mjs`: 3 size classes x 10 DPI buckets,
  doubled for brands that need separate dark and light art (Claude and
  Antigravity do; the OpenAI mark does not) — about 60 PNGs for a dark/light
  brand. Confirm the mark may be redistributed before committing it.
- **Localization.** One display string in each of the 11 language files plus the
  field in `localization/mod.rs`, and any menu or status prose that names
  providers.
- **Compact layout.** `compact_layout.rs` carries the widget and floating-window
  geometry. Four badges change width, wrapping, and tray icon count; add
  four-provider cases to its tests rather than assuming three-provider cases
  generalise.
- **Documentation and release.** `README.md` and `README.zh-CN.md` both describe
  the provider set and its credential requirements and must stay synchronized;
  regenerate `.github/readme/*.png` with `tools/render-readme-images.ps1`; add
  release notes; walk `docs/RELEASE_CHECKLIST.md`, which has several
  provider-specific manual rows.
- **Consent and detection copy.** The one-time permission prompt covers every
  provider at once while permissions stay per-provider.

## 2. The optional refactor, and why it was deferred

A registry-shaped refactor was drafted on 2026-08-06 and **deliberately not
done**:

1. `AppUsageData`'s parallel fields become slots indexed by provider identity,
   with an accessor so call sites can migrate gradually.
2. `TrayIconKind` becomes the single provider identity with an `ALL` constant.
3. `poll_with` takes a table of adapters instead of three closures.
4. Settings' three per-provider triples become maps keyed by provider, with
   serde compatibility for the existing flat keys.
5. UI match arms become table lookups.

Reasons for deferring:

- Its only benefit is making a fourth provider cheap, and the fourth provider is
  indefinitely postponed (section 3).
- It addresses the left column of section 1 but none of section 1.2, so it saves
  well under half the real cost.
- Steps 4 and 5 carry the risk: `window.rs` is large and largely UI, and a
  settings migration bug loses a user's layout and provider selection. Neither
  has any user-visible upside on its own.

If some of it is ever wanted for its own sake, steps 1 and 3 stand alone as a
simplification of existing duplication, have unit-test coverage already, and
touch neither the UI nor persisted state. Stop there unless a fourth provider is
actually landing.

### 2.1 If a balance-style measure is ever added

A prepaid balance (for example a Zen credit balance) is not a quota window: it
is unbounded, has no reset, inverts polarity, and is denominated in money.
Do **not** flip existing surfaces from usage to remaining to accommodate it —
that inverts the meaning of every existing number, screenshot, and warning
threshold, and a balance still has no denominator afterwards. Model it as a
second kind of measure alongside `UsageWindow`, and note that
`display_percent`'s rounding direction is currently chosen for *usage* (floor is
conservative); the same rounding on a *remaining* value is optimistic and would
delay warnings.

## 3. OpenCode Go — external snapshot, 2026-08-06

**Everything in this section is about software this repository does not control,
and was already in motion when it was recorded. Re-check all of it.**

### 3.1 There is no usable usage API today

- Upstream issue [anomalyco/opencode#16017][i16017] requests exactly this
  endpoint. Open since 2026-03-04, ~32 comments, no maintainer reply as of this
  snapshot. Issue #31084 is the same request, closed by its author as a
  duplicate.
- Pull request [anomalyco/opencode#16513][p16513] implements
  `GET /zen/go/v1/usage`, authenticated by `Authorization: Bearer <Go API key>`,
  returning the console's own rolling / weekly / monthly analysis. Open since
  2026-03-07, no code review. A second independent implementation of the same
  route exists in PR #32913, also untouched.
- The upstream `CONTRIBUTING.md` requires core-team design review before
  implementing a product feature, which is the most likely reason both pull
  requests are stalled. The repository had roughly 1,150 open pull requests at
  the time of writing.
- The Zen inference route (`/zen/go/v1/chat/completions`) scrubs response
  headers down to `content-type` and `cache-control`, so no quota information
  leaks through normal API traffic.

### 3.2 The three acquisition paths, and their constraints

**Web (server truth, fragile).** `GET https://opencode.ai/workspace/{wrk_id}/go`
with a console session cookie. Usage values are embedded in the page's
server-rendered hydration payload (`rollingUsage` / `weeklyUsage` /
`monthlyUsage`, each with `usagePercent`, `resetInSec`, `status`); a newer
`data-slot` HTML variant also exists, so any parser needs both shapes. The
workspace id can be discovered rather than asked for. A signed-out response is
HTTP 200 HTML, so detect it by content, not status code.
*Constraints:* requires a session cookie, which this app does not read or store
today; breaks whenever the page changes.

**Local (no credentials, undercounts).** The OpenCode CLI keeps a SQLite
database next to its `auth.json` — under the XDG data directory, which resolves
to `%USERPROFILE%\.local\share\opencode\` on Windows unless `XDG_DATA_HOME`
overrides it. Assistant messages carry `providerID` and a numeric `$.cost`, so
usage can be summed against the published Go limits.
*Constraints, and they are serious:* it only sees usage that flowed through an
OpenCode client writing to that one database file. OpenCode Go is explicitly
usable from any standard-API client, and a second machine keeps its own
database, so the result is a **lower bound** on real usage — the error runs in
the dangerous direction for a tool whose job is warning before a limit is hit.
The percentages are also derived from hard-coded limits rather than reported by
the provider, which conflicts with the README's claim that windows and reset
times are never guessed or extrapolated.

**Verified on one machine on 2026-08-06:** the database exists at the path
above and contains assistant rows with `providerID = 'opencode-go'` carrying a
numeric `$.cost`. Not verified on any other configuration.

### 3.3 Reference implementation

[`nesszer/Win-CodexBar`][wcb] (Rust, MIT) implements both paths in
`rust/src/providers/opencodego/`. Worth reading before writing anything, in
particular:

- Opening the SQLite database read-only **without creating `-wal` / `-shm`
  sidecars** when they are absent, via an `immutable=1` URI, including stripping
  the `\\?\` prefix that Windows path canonicalisation adds. A scheduled poller
  must not leave files in another program's data directory.
- Summing `step-finish` part costs in preference to message costs, falling back
  to the message cost only when a message has no such part.
- Window boundaries: ISO week starting Monday 00:00 UTC; a monthly window
  anchored on the subscription day-of-month rather than the calendar month.
- Source-mode selection with an explicit list of which error kinds may fall back
  to the other path.

Its automatic browser-cookie extraction (DPAPI decryption of Chromium cookie
databases) is deliberately **not** recommended here: it is a much larger step
than reading an AI CLI's own credential file, and conflicts with both this
repository's stated credential policy and `AGENTS.md`'s rule against persisting
cookies or session data.

### 3.4 Decision recorded on 2026-08-06

No work started. Re-evaluate only when **both** of these hold:

1. An upstream usage endpoint is merged and deployed, **and**
2. it authenticates with the API key already stored in the local
   `auth.json`, not with a console session cookie.

That combination reduces the whole feature to one credential read plus one
HTTPS GET, matching how the three existing providers already work. Watching
PR #16513 is a better signal than watching the issue. Anything short of that
means choosing between a fragile scrape and a knowingly low estimate, and
neither was judged worth the cost.

## 4. Re-measuring

The counts in section 1 were produced with the command below. `Select-String`
is case-insensitive by default, which is what is wanted here: the identifier
appears as both `antigravity` and `Antigravity`, and a case-sensitive search
misses `tray_icon.rs` entirely.

```powershell
$m = Get-ChildItem -Recurse -Filter *.rs src | Select-String -Pattern 'antigravity'
"total lines: $(($m | Select-Object Path,LineNumber -Unique).Count)"
$m | Group-Object Filename | Sort-Object Count -Descending | Select-Object Count,Name
```

Run it before trusting any number in section 1. Verify the code touchpoints by
symbol name; the surrounding line numbers will have moved.

[i16017]: https://github.com/anomalyco/opencode/issues/16017
[p16513]: https://github.com/anomalyco/opencode/pull/16513
[wcb]: https://github.com/nesszer/Win-CodexBar
