# Release checklist

Use this checklist before creating a version tag. Automated checks remain the
release gate; unavailable hardware-specific rows may be marked not applicable
with a short note in the release runbook.

Read [`INVARIANTS.md`](INVARIANTS.md) before changing a release candidate. If a
release comparison crosses v2.3.2-v2.4.0, use the anchors in
[`RELEASE_HISTORY.md`](RELEASE_HISTORY.md) instead of a raw tag range.

## Automated

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- The immediately preceding official version tag is an ancestor of the release
  commit; the release workflow enforces this and published tags are never moved
  to repair a failure.
- RustSec `audit-check` passes against the committed `Cargo.lock`, with informational
  warnings denied by `.cargo/audit.toml`
- `tools\check-retired-identity.ps1`
- current updater inbound readiness E2E
- updater helper E2E: `Success` and `ChildExit`
- `cargo build --release --locked`
- `tools\check-portable-runtime.ps1` rejects external MSVC/UCRT runtime DLLs
  so the portable executable starts without a separate redistributable.
- Confirm tests cover settings that predate provider permissions, the
  default-deny permission state, the hard poll gate that requires both
  visibility and explicit provider permission, schema-3 LegacyNeedsReview
  pending classification, and zero credential reads for pending/revoked
  providers on FirstRun, Rescan, Manual, poll, and the credential watch.
- Confirm tests cover the 4 MiB JSON response ceiling, settings/cache stale
  snapshot rejection, and diagnostic-log runtime/external rotation.
- Two updater behaviours are covered by unit tests only, deliberately, and do
  not need a live run recorded each release: the staging-directory reparse
  check (creating a real junction needs privileges this suite does not assume)
  and the recycled-pid case of the update-target check (a live run cannot
  arrange a recycled pid). Design limits that are accepted rather than open are
  listed under *Known limits* in [`INVARIANTS.md`](INVARIANTS.md).
- When a release adds a provider, state the downgrade consequence in the
  release notes. `provider_order` stores provider names, and a build that does
  not know one of them cannot use the file: v2.5.0 and later drop the unknown
  entry and keep the rest, but **v2.4.2 and earlier reject the whole settings
  file**, fall back to defaults, and overwrite the user's layout, language, and
  provider selection on the next save. Downgrading across that boundary is a
  settings reset, not a rollback.
- The same applies to any new `#[serde(tag = ...)]` variant, not just provider
  names. `widget_placement`, `floating_placement` and `last_update_outcome`
  became tolerant of an unknown variant in v2.5.2 - an older build there loses
  that one field and keeps the file - but **v2.5.1 and earlier reject the whole
  settings file** the same way. Adding a variant is therefore a downgrade
  consequence to state in the release notes, exactly like adding a provider.
- An unreadable settings file is kept as `settings.json.corrupt` and reported
  once; only one generation is kept. Returning defaults is not a read-only
  outcome, so any change that makes the parse stricter has to keep that rescue
  ahead of the first save.
- Before each Gengchou minor release (`vX.Y.0`), review the pinned Rust toolchain
  and principal dependencies and record whether an upgrade is justified.
  Between minor releases, review them immediately only for a security advisory,
  upstream end-of-support notice, or a reproduced compatibility problem; do
  not destabilize a patch release solely to follow the newest version.
- Confirm the built file is `target/release/gengchou.exe`; inspect PE properties
  for ProductName `Gengchou`, version/tag agreement, retained upstream
  copyright/Comments, and the unchanged v2.1.0 application icon.
- Debug compact-surface gate: `cargo run --locked -- --dump-widget
  tmp/compact-release-check`; inspect every generated theme, warning/error,
  High Contrast, tooltip, and mixed-digit alignment fixture.
- Every regression test added for this release asserts on the production path,
  not on a helper or a copy of the value under test, and each one is
  mutation-checked: revert the behaviour it names and watch it fail. Two v2.5.1
  tests were written against a field-for-field copy of the detection scope and
  against an unchanged intersection helper, and stayed green under the exact
  revert they claimed to guard.
- An independent agent reviews the release candidate read-only before the tag,
  with the already-dispositioned findings listed so it does not repeat them.
  The v2.5.1 final review returned two accepted blockers after four earlier
  rounds, one of them a regression introduced by the round before it.

## Manual Windows smoke test

- Start one instance, launch the EXE again, and confirm the existing detail
  popup opens without a second resident process.
- With Gengchou running, sign in as a second Windows account and start it
  there. It must come up with its own widget, tray icons and settings; the two
  must not see each other. Then, from the first account, use Fast User
  Switching to open a second session of that same account and launch it: this
  one must refuse and say so, because both sessions write one `settings.json`.
  A refusal that closes without a dialog is a failure. If a second account is
  not available, record that this row was skipped rather than marking it
  passed.
- Confirm `%APPDATA%\Gengchou\instance.lock` exists while the app runs, stays
  empty, and cannot be opened by anything else until the app exits. Kill the
  process rather than exiting it and confirm the next launch still starts:
  Windows releases the handle, so there is no stale lock to clear, and any
  future change that adds one is a regression.
- Confirm taskbar widget, all enabled tray icons, detail popup, context menu,
  manual refresh, and clean Exit.
- On a fresh profile, confirm exactly one permission prompt appears, covering
  every provider, before any credential read, credential watch, or quota
  request. It must explain that the access is used only to query usage,
  consumes no model allowance, and stores no sign-in information; appear only
  after an app surface is visible; default to No; minimize and restore without
  granting access; and persist either decision without prompting again at the
  next launch. Repeat with Common Controls v6 unavailable and confirm the
  standard Yes/No fallback dialog appears instead of a failed start.
- After granting on a fresh profile, confirm Gengchou detects the signed-in
  providers and shows exactly those. On a machine where nothing is signed in,
  confirm one polled placeholder row remains so the credential watch still
  notices the first sign-in. After declining, confirm nothing is read or
  polled.
- On an upgrade from settings that predate the one-time prompt, confirm no
  prompt appears and previously allowed providers stay allowed. Providers whose
  older file only has `allow=false` appear under Provider access as **needs
  review** and are not read until **Allow access** or **Keep closed** is
  chosen. Confirm the review dialog offers a third choice, **Decide later**,
  that it is the focused default, and that it, Esc and the title-bar close
  button all leave the provider pending with nothing read - a stray Enter must
  never record a revocation. Repeat from settings that had declined every
  provider: they stay declined and pending, not silently re-enabled.
- Sign in to a provider that was not previously detected and confirm
  **Provider access → Detect providers again** picks it up immediately when it
  is already allowed. Pending providers must be confirmed first; the prompt
  says local credentials will be read for ongoing monitoring. Confirm
  the periodic sweep notifies once per provider and never changes what is
  displayed on its own, and stays quiet about providers the user turned off
  or left pending. Confirm the sweep also runs shortly after start on an
  install that has already answered the access prompt, so a provider added by
  an upgrade that is not pending or revoked does not wait out a full interval.
  Revoke one provider, then confirm neither the startup sweep, the periodic
  sweep, nor **Detect providers again** reads its credentials at all - the
  diagnostic log records the scope of every pass. Showing a provider under
  **Providers** must not grant access. Confirm an upgraded `allow=false` is
  pending rather than a guessed revocation, and that startup Rescan, poll, and
  Detect providers again stay out of scope until **Allow access** or **Keep
  closed**.
- With Codex, Antigravity, and Grok credentials only inside WSL, confirm they
  are read from a **running** distribution (`$CODEX_HOME/auth.json`,
  `$HOME/.gemini/antigravity-cli/antigravity-oauth-token`, and
  `$GROK_HOME/auth.json`) and that a stopped
  distribution is never started by the scheduled check. Actually place a
  credential inside a distribution and watch it appear: this row failed
  silently for every provider until v2.5.0, because `wsl.exe --` re-parsed the
  probe script in the login shell and dropped its quoting, and a WSL-only
  credential is indistinguishable from an absent one on the surfaces.
- Force a WSL credential probe spawn failure, timeout, and unexpected exit;
  each must follow the transient request-failure path and must not show an
  authentication warning. A concrete credential file that is present but
  unreadable, malformed, expired, or rejected must still use **Authentication
  failed** and the sign-in recovery. Inside WSL that distinction is Claude's
  only: its probe reports exit 45 for a present-but-unreadable file, while the
  Codex, Antigravity and Grok probes report only success or nothing and must
  fall through quietly to the next source. Check the Windows-side paths for
  those three instead - a malformed `auth.json` or an unparsable Credential
  Manager entry must show **Authentication failed**, not **Not detected**.
- Confirm a provider with no credential shows **Not detected** with the
  automatic-recognition note and raises no notification, while a concrete but
  unusable credential shows **Authentication failed**. Both credential states
  must park polling and recover automatically once a credential appears.
- Grant access, then confirm only the shown providers poll. Rotate a provider's
  original file or Credential Manager entry and confirm the new credential is
  picked up without Gengchou storing a token. Revoke that one provider under
  Provider access, confirm the others keep polling, pending results are
  discarded and future reads/polls stop, then confirm settings, usage cache,
  and diagnostics contain no token.
- Drive diagnostics past 1 MB without restarting. Confirm rotation happens
  during the run, later lines continue in `diagnose.log`, and only that file
  plus `diagnose.log.old` remain. Rename the current file externally, create a
  replacement, and confirm the next line follows the replacement rather than
  the renamed file.
- Corrupt `settings.json` - a UTF-8 BOM is enough, and PowerShell 5.1's
  `Set-Content -Encoding utf8` writes one - then start. Confirm the original
  bytes are kept at `settings.json.corrupt`, that one warning names that path,
  and that the run then behaves like a fresh profile. Repeat once more and
  confirm only the newer original is kept. Do this while an update is being
  applied too: the readiness confirmation must complete before that warning,
  otherwise the helper's 30-second budget expires behind the dialog and rolls
  the update back.
- Confirm Refresh is one submenu whose first item is Refresh now, followed by a
  separator and the six checked polling intervals (1, 2, 5, 10, 15, and 30
  minutes); exercise Refresh now and each interval once. While a slow manual
  refresh runs, the previous valid values must remain visible, the detail
  footer must say it is refreshing, and repeated clicks must coalesce.
- Keep the detail popup open across a poll boundary and confirm Last updated /
  next-in advances once per second without extra provider requests. Exercise a
  rate-limit retry, transient backoff, and Claude cooldown alignment and verify
  next-in follows the actual armed timer rather than the configured interval.
- Verify the detail header exposes four separate buttons in this order: refresh,
  keep open, lock position, close. Each target must be at least 32 x 32 DP, use
  the Segoe MDL2 icon family at a consistent optical size, show current state in
  the icon, expose a localized accessible name, and expose a native button role
  to UIA/Narrator. Only hover and press may show the rounded button background.
  Tab and Shift+Tab must traverse all four; Enter and Space must
  activate them; Esc must close from every focus position. Mouse clicks must
  not leave a dotted focus rectangle, while keyboard focus remains visible in
  light, dark, and High Contrast themes.
- Drag the detail popup in its default movable state, lock its position and
  confirm it no longer moves. Pin it, move focus to another app, and confirm it
  stays open; unpin and confirm normal focus-loss dismissal returns. Close and
  reopen it: the pin preference must be retained, while the position lock must
  reset and the moved position must not be restored. Restart the app and confirm
  the pin preference is retained again.
- Open every interactive update result (available, up to date, and failure)
  and the startup readiness error. While each modal dialog is open, verify the
  Start button, taskbar app buttons, and notification area remain enabled and
  a second update action cannot start behind the prompt.
- After a check reports up to date, restart and confirm the version menu entry
  still states it instead of resetting to Check for updates. Repeat with an
  available update: the restarted entry must name the version and, when acted
  on, re-check before offering the download. Confirm a failed check clears the
  remembered answer rather than leaving a stale claim on screen.
- Restart Explorer and confirm the widget and tray icons recover once, without
  duplicate icons or processes.
- Lock/unlock Windows and, when available, disconnect/reconnect RDP; confirm no
  extra wake-time poll occurs and the normal configured polling cadence
  continues while inactive. The widget must stay hidden rather than appearing
  as a desktop popup, then re-embed from cached state as soon as the taskbar
  returns.
- Walk every entry point of a behaviour that has more than one, not just the
  one that is easiest to reach. Granting a pending provider is reachable from
  **Provider access** and from the review inside **Detect providers again**;
  the v2.5.1 walk-through used the first and shipped a defect that only the
  second exposed.
- Confirm Provider tray icons, Widget, and Floating Window appear in that
  order before Settings as direct checked toggles. Confirm the taskbar and
  floating-window position resets remain under Settings and no floating-window
  lock item is present. In Providers, the final enabled provider must be
  visibly disabled instead of accepting a no-op click.
- Hide/show the taskbar widget and restore its default position; confirm it
  returns next to the notification area on the primary taskbar.
- Show the floating window, drag it from several points across the whole
  compact surface, restart the app and confirm the position is remembered,
  then restore it to the primary work area's bottom-right. Confirm it remains
  draggable after reopening, a short click still opens details, and it never
  appears automatically as a taskbar fallback. At every work-area edge verify
  an 8-logical-pixel safety margin. Confirm the taskbar-only left divider is
  absent and the floating window remains above normal windows after dragging,
  changing display configuration, and restoring a remote session.
- Switch the UI through at least English, Simplified Chinese, and one other
  language; confirm taskbar and floating duration labels and countdowns still
  use only `d`/`h`/`m`/`s`/`now`, while detail-popup prose remains localized.
  Sweep all 11 languages for English reset-notification remnants, truncated
  authentication badges, provider/menu terminology, and locale-correct dates;
  Chinese polling text must not contain an unintended space after “每”. Confirm
  the in-app brand reads 更筹 in Simplified Chinese and 更籌 in Traditional
  Chinese, and Gengchou everywhere else, while quota surfaces say Claude and
  only CLI-specific actions say Claude Code.
- Drag the detailed tray icons into a different order and confirm the taskbar
  widget and floating window change together after the short stability delay
  (normally about 120ms), without showing an intermediate order or waiting for
  the next countdown refresh. Reopen details and trigger a multi-provider auth
  error: card order and the first credential title must use the same order.
- Force transient provider request failures and verify the first two stay
  visually quiet. On the third consecutive failure, verify the card becomes
  **Refresh failed**, keeps and mutes the last valid data, shows its age, and
  gives a localized connection action plus an automatic-retry outcome.
- Disable Provider tray icons and confirm the provider icons are replaced
  by one app icon matching the executable; re-enable it and confirm all enabled
  provider icons return without duplicates. Confirm the same app icon stands in
  while no provider has access at all, without rewriting the user's Provider
  tray icons preference. At each tested DPI, confirm the app
  icon fills the Shell slot without clipping. Exercise this notification
  matrix in both tray-icon modes:
  - provider detection, quota reset, and Claude Code update: Gengchou app icon,
    silent, no Windows warning glyph;
  - current unusable/rejected credential: Windows warning glyph and sound;
  - simulated app-icon load failure for a routine event: silent with an empty
    icon slot, never the percentage tray icon and never a warning glyph.
- Hover each provider icon and confirm its title and quota windows use separate
  lines with reset timing in parentheses. Disable Provider tray icons and
  confirm the app icon uses one compact line per provider without mid-line
  truncation.
- Hover each taskbar badge and confirm the custom theme-aware hover card appears
  after the delay, lists every reported window with reset timing, stays within
  the work area, and disappears on pointer leave, click, display change, or
  Explorer rebuild.
- Exercise 100%, 125%, 150%, 175%, and 200% DPI where available. On mixed-DPI
  monitors, move the detail and floating windows across monitor boundaries and
  confirm their suggested position, size, hit targets, and remembered floating
  position remain correct while the taskbar widget keeps its own scale.
- At each tested DPI, inspect every repeated 10-DP detail progress segment at
  200% zoom. The 2-DP corners must have coverage-antialiased edges without a
  one-pixel GDI stair-step; height, radius, proportions, gaps, and fill values
  must remain unchanged.
- Exercise horizontal, vertical/third-party, and multi-row taskbars where
  available; failed embedding must keep the widget hidden and recovery armed.
- On a multi-monitor system, switch the primary display and drag the widget
  between taskbars; verify saved position and tray-driven provider order.
  During the display transition, confirm a still-valid embedded widget is not
  detached merely because Windows briefly enumerates only one taskbar, and
  confirm tray and floating-window context menus remain responsive.
- Test both a dark and a light Windows High Contrast theme. Confirm widget,
  tray icons, popup, compact floating window, tracks, and focus cues remain
  legible; warning row text must remain visible on the window canvas and every
  character inside warning/error pills must contrast with the highlight fill.
- Re-render the README previews from the final build with
  `tools\render-readme-images.ps1` and commit any changed `.github/readme/*.png`;
  verify the README text, alt text, provider marks, compact layout, and the
  version shown in the detail-popup images match the release.
- With Codex Desktop signed in and the CLI absent or unavailable, confirm Codex
  usage still loads from a supported local session.

## Update and release hand-off

- Verify a portable update releases the old PID and the single-instance lock,
  replaces the target, starts one new PID, and preserves the rollback backup
  until the new process reports ready.
- Run the updater helper directly with a target that is not this install -
  `--apply-update <some other file> <payload> <a dead pid> <payload sha256>` -
  and confirm it exits non-zero, leaves that file byte-identical, and creates
  no `.old` beside it. Repeat with a live but unrelated parent PID. Both must
  refuse before anything is replaced or launched.
- Point the release's own download at a build whose `ProductVersion` differs
  from the announced tag and confirm the portable update refuses it after the
  hash check rather than installing it.
- Hold the confirmed `gengchou.exe.old` backup open without delete sharing and
  launch normally: the app must still reach its UI, log the exact deferred
  cleanup path, then remove the backup on the next launch after the handle is
  released. An active inbound readiness transaction must remain fatal on
  marker failure so the helper can roll back.
- With Claude credentials present in Windows and multiple WSL distributions,
  expire or refresh a non-default source while polling is paused. The watch
  must cover every known source and recover within its short watch cadence;
  local-only failures must still get a service retry at the configured poll
  interval even when no file signature changes.
- Repeat with Windows and per-distro `CLAUDE_CONFIG_DIR` overrides. Read,
  watch, and recovery must resolve the same `.credentials.json`, and an
  invalid earlier source must not mask a later usable one.
- Confirm a repeated OAuth 401 (and a direct 403) from a Windows credential
  starts one hidden `claude update`, times out after 60 seconds, requires a
  changed credential, and retries the usage endpoint before declaring success.
  Repeat with `DISABLE_UPDATES=1`; there must be no authentication-recovery
  item in Settings.
- Without a Claude CLI credential, confirm both Desktop cache generations and
  the MSIX path can supply a read-only usage request. Desktop `config.json` and
  `Local State` hashes and write times must remain unchanged, and neither access
  nor refresh tokens may appear in diagnostics. Repeat with
  `GENGCHOU_DISABLE_CLAUDE_DESKTOP_AUTH=1` and require **Authentication failed**.
- With CLI and Desktop sessions representing different test accounts, confirm
  usable CLI credentials take precedence and Desktop is tried only after no
  usable CLI credential remains. Network failures and 429 responses must not
  switch sources.
- Confirm WSL credentials never invoke the Windows CLI. Remove or expire the
  refresh token, then verify every unrecoverable credential path displays
  **Authentication failed** and directs the user to sign in to Claude again.
  Verify the README recovery paths separately: send a message in Claude
  Desktop first and sign out/in only if needed; use `claude auth login` for the
  standalone CLI.
- Verify credential-only recovery is silent. When the recovery also changes the
  CLI version, exactly one notification always appears; confirm no setting can
  suppress it and that no such item remains in the menu. Repeat with
  `DISABLE_UPDATES=1` and confirm neither the update nor the notification runs.
- Confirm initial loading, cached data, automatic recovery, and fresh 429/5xx
  or transient request failures show no status badge. A persistent transport
  or request failure (three consecutive failures or stale for the configured
  age) must show **Refresh failed**. A 429 must cool down only its provider and
  become **Refresh failed** only when that provider's data is stale. Verify old
  values are muted with **Last updated ... ago**, while empty rows distinguish
  waiting, authentication failure, and temporary unavailability. Quota states
  remain **Near limit** and **Limit reached** when no higher-priority state
  applies. Inspect diagnostics to confirm no credential values were logged.
- Remove `%APPDATA%` in a disposable test profile and confirm settings plus the
  usage cache use the Windows configuration or `%LOCALAPPDATA%` fallback.
  Make every fallback unwritable and confirm exactly one localized storage
  warning appears after the UI is ready.
- Exercise Start with Windows from a Unicode installation path. The registry
  value must be one fully quoted `REG_SZ` command with no arguments, case-only
  path differences must compare equal under Windows Unicode rules, and a
  registry write failure must be reported once without changing the checkmark.
- For the v2.3.0 tag only, confirm every supported older installation has run
  v2.2.4 twice and passed the official migration verifier. Record the result
  outside the repository before creating the tag.
- Confirm the draft release has exactly six attachments: `gengchou.exe`,
  `gengchou-windows-x64.zip`, three compliance files, and `SHA256SUMS`. The
  manifest must cover all five payload assets, and the EXE and ZIP must both
  have build provenance attestations.
- Verify the WinGet path with `ynjmxn.Gengchou` on an installed build when
  the new package update exists; do not test the unpublished former ID. Keep
  the original PID alive past the 30-second wait and confirm WinGet never runs.
  On a normal update, confirm only the originally verified executable is
  restarted and its ProductVersion exactly matches the expected release.
  Simulate WinGet and version-mismatch failures and confirm the same target
  restarts with one failure marker, producing exactly one in-app notice.
- Confirm the draft release re-download passes `SHA256SUMS` and GitHub
  attestation verification before the workflow publishes it.
- Confirm release notes describe user-visible changes and any required upgrade
  order without reintroducing retired asset names.
- Confirm both READMEs disclose credential/session access and provider-only
  HTTPS transmission next to installation, with a working link to the full
  data-and-privacy section.
- Confirm the GitHub About description and topics contain only the current
  product identity. Preserve the renamed repository redirect, historical
  releases, tags, and git history.

## Post-release WinGet hand-off

- Publish and re-download the final GitHub release before preparing any WinGet
  manifest; use the public `gengchou-windows-x64.zip` URL and its released
  SHA-256, never a draft or local build.
- Generate the update with `wingetcreate update ynjmxn.Gengchou --version
  <version> --urls <public-zip-url> --out manifests`; do not use
  `update --submit`. In the generated locale manifest set `Author` exactly to
  `ynjmxn, lvfeinan and contributors`, keep Publisher and Copyright unchanged,
  run manifest validation, and inspect the complete diff before submitting.
- Submit `ynjmxn.Gengchou` only after the matching GitHub release is public,
  then wait for the WinGet validation pipeline and review.
- After the WinGet pull request is merged, confirm the version directory exists
  under `manifests/y/ynjmxn/Gengchou/` in the public registry.

Installing the merged package on a clean Windows profile is **permanently out
of scope**, at the owner's instruction, along with the real-machine upgrade
matrix. This is a single-machine, single-user project with no clean profile to
install into, and a row that is never run is worse than no row: it makes every
release record either dishonest or cluttered with the same exception. What
stands in for it: the WinGet validation pipeline installs and uninstalls the
package before merge, the submitted manifest is generated from the public
release ZIP and its released SHA-256, and the release assets are verified
against `SHA256SUMS` and GitHub attestations before publication.
