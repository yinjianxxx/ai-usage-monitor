# v2.5.1 release verification

- Date: 2026-09-01
- Candidate: `claude/v2.5.1-fixes`, based on `v2.5.0` / `0b478f1`, at the
  release-prep commit that follows `7bbdd96`. Not tagged, not pushed.
  `Cargo.toml` and `Cargo.lock` read `2.5.1`.
- Scope: three defects that had shipped silently since v2.4.1 or earlier
  (informational tray balloons never displayed, periodic provider detection
  never ran, detection ignoring a revoked provider's per-provider switch), a
  Grok-only profile never polling, two silent-failure repairs (poll-timer arm,
  poll-controller self-deadlock), a credential-state classification fix for
  Codex/Antigravity/Grok, one behaviour change (detection sweep also runs at
  startup on an existing install), and three interface fixes; no change to the
  executable, settings directory, tray, or update identities. Consent schema
  3 adds per-provider LegacyNeedsReview (`*_credential_access_pending`). Older
  `allow=false` values become pending rather than guessed revocations; they
  are not read until the user allows access or keeps the provider closed.
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 26200 on 2026-09-01, all against the same tree:
`7bbdd96` with the version bumped to `2.5.1`. An earlier run at `cb0ceac`
reported 324 tests; the 20 added by `7bbdd96` are the difference, so that run
is superseded rather than repeated here.

- `cargo fmt --all -- --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - clean
- `cargo test --locked` - 344 passed, 0 failed
- `cargo build --release --locked` - clean. Built with `CARGO_TARGET_DIR`
  pointed at `target\release-gate`, because an earlier branch build (PE version
  `2.5.0`) was running from `target\release\gengchou.exe` and the linker cannot
  replace a running image. Nothing in the build depends on the target directory.
- `tools\check-portable-runtime.ps1 -ExecutablePath
  target\release-gate\release\gengchou.exe` - passed, no external MSVC/UCRT
  imports
- PE properties of that build: ProductName `Gengchou`, ProductVersion and
  FileVersion `2.5.1`, OriginalFilename `gengchou.exe`, upstream
  copyright and `Comments` retained
- `tools\check-retired-identity.ps1` - passed, 13 historical lines
- compact-surface debug gate: `--dump-widget tmp\compact-release-check` - 37
  fixtures generated (badges, floating rows, tooltips; dark, light and High
  Contrast; normal, warning, error, stale, auth and no-data states, plus the
  mixed-digit alignment pair). All 37 were inspected as images; no clipping,
  no misalignment, no illegible High Contrast text.
- README previews re-rendered from this build with
  `tools\render-readme-images.ps1`. Only the four `detail-popup-*.png` changed,
  and only in the footer version (`v2.5.0` to `v2.5.1`); the widget, floating
  and tray strips are byte-identical, which is the expected deterministic
  result.
- `cargo audit` against the committed `Cargo.lock` - 122 dependencies scanned,
  no advisories, exit 0. `.cargo/audit.toml` denies warnings, so that is clean
  too. Run with a locally installed cargo-audit 0.22.1; the newest release
  needs rustc 1.88 and this machine is on 1.86. CI pins
  `rustsec/audit-check@v2.0.0`, which carries its own cargo-audit, so a future
  advisory could still surface there first.
- `tests/update_ready_inbound_e2e.ps1` - passed: active readiness and locked
  confirmed-backup cleanup
- `tests/updater_e2e.ps1 -Scenario Success` - passed: old process exited, new
  one alive, target hash, ready hand-off and transaction cleanup verified
- `tests/updater_e2e.ps1 -Scenario ChildExit` - passed: helper rejected the
  child, restored the old hash, relaunched, and retained the verified `.old`

Each commit that touches `src/` was type-checked in isolation when it was
recorded, so the branch is bisectable: the first six, the four added after the
first independent review, and every commit from the second and third rounds
were each checked the same way as they were made. The gate results above are
the re-run at the final tree, not inherited from an earlier one.

## Targeted regression evidence

### Informational balloons

The failure is invisible from the call site: `Shell_NotifyIconW` returns
success and nothing is drawn. It was isolated on the machine with a temporary
probe rather than by reasoning, in three steps:

1. Log the outcome of each delivery attempt. Every candidate icon reported
   `ok=false` except the one shown provider's, which reported `ok=true` with a
   visible anchor rectangle - so neither the anchor nor the flags were at
   fault.
2. Send the same balloon with `NIIF_WARNING` and no custom image. It appeared,
   with sound. That isolated the fault to the custom-image path.
3. Keep the custom image but skip `DestroyIcon`. It appeared. That identified
   premature destruction as the cause.

The shipped fix caches one handle per DPI instead of leaking, because
`load_embedded_app_icon_of_size` can fall back to `ExtractIconExW`, whose
handles the process owns. Verified on the machine afterwards with no probe in
the binary.

Scope confirmed by reading all five call sites: the three `Info` balloons were
affected; `ActionRequired` passes `HICON::default()` and so never reached the
destroy call.

### Periodic detection sweep

`git log -S "TIMER_PROVIDER_DETECT"` dates the defect to `693c124`
(2026-08-01); `git tag --contains` reports v2.4.1, v2.4.2, and v2.5.0.

Ruled out before concluding: timer ID collision (1-5 and 10-15 are all
distinct), a stray `KillTimer`, window recreation (the main window was created
once and never rebuilt), and consent being off in memory.

Verified by temporarily setting the interval to 60 seconds: three sweeps at
+0s, +60s, and +120s, and exactly one balloon. Before the fix the timer never
fired at all. The interval was restored and the release binary rebuilt from the
restored source.

### Startup sweep

A balloon was raised 2 seconds after launch on a profile shaped like an
upgraded install (`allow_grok_credentials` true, `show_grok` false,
`grok_credential_access_decided` false - the exact output of the v1-to-v2
consent migration). `show_grok` remained false afterwards, so the sweep
notified without changing what was on screen.

Two unit tests were added, both mutation-checked: reverting the behaviour each
guards makes the corresponding test fail. What they guard is the settings-level
outcome - a migrated install still owes a balloon, and a sweep immediately after
first-run detection is silent. Neither reaches `schedule_provider_detection`,
so neither would catch the startup call being passed the wrong flag; that part
rests on the run above, not on a test.

### Interface fixes

Confirmed on the machine by the owner: the application icon in the notification
area while no provider is authorised, the Gengchou icon in the credential
dialog's title bar, and the hover tooltip no longer reappearing beside an open
detail popup.

## Independent review

### First round

A Codex agent in the same workspace reviewed the branch read-only and returned
a verdict of "does not pass". It independently re-ran fmt, clippy, the test
suite, a release build, the portable-runtime and retired-identity gates, and
confirmed each of the six code commits compiles in isolation. Its findings and
their disposition:

- **Accepted, release blocker.** The poll worker gate read the four-provider
  selection as `.0 || .1 || .2`, so a profile whose only shown and authorized
  provider was Grok never started a poll at all - including a fresh install
  that detects Grok alone. Present in v2.5.0 as shipped; `git log -S` dates it
  to `b39e0c7`, not to this branch. This is the same "parallel expression"
  shape that a review caught in v2.5.0, so the selection is now an array
  indexed by `TrayIconKind`, which cannot be consumed three-quarters of the way
  by accident, and a regression test asserts the gate itself rather than the
  selection.
- **Accepted.** The balloon image cache held one slot keyed by DPI and
  destroyed the previous handle on a DPI change, which could free a handle the
  shell was still about to draw. It now keeps one per DPI and evicts nothing.
- **Accepted.** `icon_guids_are_stable_and_unique` and
  `bundled_provider_tiles_use_exact_png_and_hicon_sizes` both spelled out three
  providers, so neither covered Grok. Both now iterate `TrayIconKind::ALL`.
- **Accepted.** The consent dialog callback extracted a fresh icon pair on
  every open and never released it. The pair is now cached for the process.
- **Accepted in part.** Eleven `SetTimer` call sites discarded the return
  value; a timer that never armed is exactly the failure shape this release
  fixes, so they now report it. The review also asked for the same treatment
  on `Shell_NotifyIconW`. Declined for `NIM_DELETE`: `sync` calls it for every
  provider that is not shown, most of which were never registered, so failure
  there is the normal path and logging it would bury real diagnostics. The
  reasoning is recorded at those call sites so it is not re-litigated.
- **Accepted.** Both READMEs still described the application icon as appearing
  only when provider tray icons are disabled, and described detection as
  periodic only.
- **Rejected in part.** The review reported that the two new settings tests are
  not mutation-sensitive, having mutated the argument passed to
  `schedule_provider_detection` and seen both still pass. That argument is not
  what those tests claim to guard; mutating what they do guard does fail them.
  The valid half - that they do not cover the scheduling entry point, while
  this document implied they did - is corrected above.
- **Accepted, now closed.** The release gate set beyond fmt/clippy/test had not
  been run for this candidate. RustSec and all three updater E2E scenarios have
  since been run and are recorded above. `Cargo.toml` is still `2.5.0`.

### Second round

The whole repository, not just this branch, was reviewed again before tagging:
once by a Codex agent (gpt-5.6-sol, xhigh) and once by a Claude agent (Opus 5),
independently and read-only. They disagreed on the verdict - "blocked" and
"can ship" respectively - and their findings barely overlapped: each missed the
most severe thing the other found. Only the poll-timer arm was reported by
both. Every non-overlapping finding was re-checked against the source here
before being accepted.

- **Accepted, release blocker.** Detection read every provider's credentials
  regardless of the per-provider access switch, so a provider the user revoked
  under Provider access was still being read at every start and every sweep -
  a breach of the invariant in `docs/INVARIANTS.md`, of the README's
  per-provider revocation promise, and of the release-checklist row that says
  reads must stop. The underlying full scan is old, but the two sweep fixes in
  this release are what turned it from a dormant path into steady behaviour.
  `detect_signed_in_providers` now takes a `DetectionScope` that guards every
  probe.

  The review phrased this as "only providers whose access is enabled may be
  read", which overshoots: `allow_*_credentials` defaults to `false`, so read
  literally it would forbid detection entirely, and detection is the only way
  an unenabled provider is ever noticed. Worse, the schema-1 migration sets
  `*_credential_access_decided = true` for every pre-existing provider while
  carrying an old `false` forward - so on real upgraded installs "not allowed"
  cannot be distinguished from "never asked", and keying off it would have
  permanently blinded those installs to a provider added later. That migration
  already ran on shipped installs, so it cannot be repaired retroactively. Four
  new `*_credential_access_revoked` fields carry the distinction instead; only
  the Provider access menu sets them.
- **Accepted, narrowed.** Codex, Antigravity and Grok mapped every failed
  credential read to `NoCredentials`, so a malformed `auth.json` displayed as
  **Not detected** - pointing the user at installing a CLI they already had.
  The review asked for a four-state result type across every source. Narrowed
  to the Windows-local reads: a new `PollError::CredentialUnusable` displays as
  **Authentication failed**, which adds no new user-visible state, since
  `ProviderStatus` already had it and Claude already used it.

  Deliberately not extended to WSL. A WSL probe cannot separate "no credential
  in this distro" from "the probe did not answer", so classifying it would
  raise sign-in warnings for a distro that was merely slow. `INVARIANTS.md`
  and both READMEs are narrowed to match, rather than leaving a promise the
  implementation does not keep. `CredentialUnusable` is also deliberately not
  one of the rejection errors: nothing was rejected remotely, so it must not
  set `remote_auth_rejection` or arm the bounded service recheck. The
  credential watch is the correct recovery and fires when the CLI rewrites the
  file.
- **Accepted, release blocker.** `poll_controller_hwnd` takes the state lock on
  its fallback path, and three call sites passed it as an argument while
  already holding that lock. `STATE` is a plain `Mutex`, so if the
  process-level helper window could not be created, the UI thread would block
  on a lock it holds itself - with the widget drawn and the tray icons
  registered, and nothing in the log. Four other call sites already resolved
  the window first; those three now do the same, and the helper documents the
  constraint.
- **Accepted.** `arm_poll_timer` was the one `SetTimer` left out of the
  silent-failure sweep, and it drives the whole monitor: nothing else re-arms
  `TIMER_POLL`. Reported by both reviewers. It now goes through `arm_timer`.
  A bounded retry was suggested and not added - every other timer here only
  logs, and a new recovery mechanism is not something to introduce untested on
  the eve of a release.
- **Accepted.** Two doc comments had been left attached to the wrong item when
  new symbols were inserted above their functions.
- **Accepted.** `PROVENANCE.md` still described the old
  `taskbar_index + tray_offset` position model, and that file ships inside the
  release ZIP. The release-checklist rows for the tray icon fallback and for
  the startup sweep were also never updated.
- **Accepted.** This document claimed "six commits" after four more had been
  added, so its bisectability claim covered less than it appeared to. Both
  reviewers reported this independently.
- **Not acted on.** `TIMER_TRAY_ORDER` is not re-armed after the widget window
  is recreated, so the fallback tray-order sampling stops after an Explorer
  restart. The reviewer that found it recommended leaving it, and its own
  analysis shows the user-visible path survives: `attach_to_taskbar` reinstalls
  the WinEvent hook, so a drag is still caught. Out of scope for a fix release.
- **Not acted on.** `updater.rs` calls `std::env::remove_var` after worker
  threads that read the environment have started. Sound on Windows today, a
  hard error whenever the crate moves to edition 2024. Recorded, not changed.
- **Not acted on.** `diagnose.rs` uses a `Global\` mutex where the log path is
  per-user, so a second user's session could fail to open it and silently lose
  diagnostics. Pre-existing, no evidence of it occurring.

### Third round

Both reviewers were sent the fixed branch and reviewed it again, still
read-only and still independently. Their verdicts diverged the same way as
before - "can ship, one wording item must be settled" and "still blocked" - and
so did their findings: of the six distinct items between them, exactly one was
reported by both. Each was re-checked against the source before being accepted.

- **Accepted, both reviewers.** The credential watch read all four providers'
  credentials whenever more than one provider qualified, because
  `CredentialWatchMode::AllProviders` named no set. That is a wider hole than
  the detection sweep it accompanies: the watch samples on every poll pass and
  every 15 seconds while polling is parked, against the sweep's half hour. The
  variant now carries `[bool; TrayIconKind::COUNT]`, built from the same
  shown-and-allowed selection the poll gate uses, and the snapshot probes only
  the sources in it.

  One reviewer's stated scenario for this was wrong - it assumed the mode was
  derived from raw `show_*`, when `PollPassPlan` already passes the gated
  selection - but the defect is real for a different reason: with two providers
  selected, `AllProviders` still read the other two.
- **Accepted.** Re-showing a provider from the Providers menu restored
  `allow_*` without clearing the new revoked flag, so access and detection
  could disagree permanently: polled and read, yet invisible to every sweep,
  with nothing on screen to explain it. Both grant paths now clear the flag.
- **Accepted.** A revocation landing while a detection pass was in flight could
  be undone by the stale result. The scope is sampled before the worker starts
  and the result was only re-checked against the global consent; manual
  detection assigns `allow_*` from what it found, so the provider came back and
  its token went out on the next poll. The result is now masked with the
  current scope inside the state lock, with a regression test.
- **Accepted.** The Credential Manager path still collapsed to "missing": an
  entry that exists with an empty blob, non-UTF-8 contents, or a `CredReadW`
  failure that is not `ERROR_NOT_FOUND` read as **Not detected**. That left the
  wording this release added - "a Windows file *or Credential Manager entry*
  that is unreadable or malformed" - false for Antigravity, which has no other
  Windows source. The read is now classified like the file path.
- **Accepted.** The WSL wording added in the previous round overshot. Claude's
  WSL probe *does* separate an absent credential (exit 44) from an unreadable
  one (exit 45) and from a probe that never answered; only the Codex,
  Antigravity and Grok probes collapse everything into "move on". The
  invariant, both READMEs and the release checklist now state the difference
  per provider instead of contradicting each other.

  The other reviewer proposed resolving the same contradiction by deleting the
  exit-45 requirement from the checklist. Rejected: it read only
  `read_wsl_script_output`, which serves the other three providers; Claude's
  probe goes through `read_wsl_credential_bytes` and the requirement is
  correct there.
- **Accepted.** Two comments still described `ActionRequired` and the
  credential balloon as "rejected by the provider" only, which stopped being
  the whole story once a locally unusable credential could raise one.
- **Closed by schema 3.** Older `allow=false` is no longer guessed. Consent
  schema 3 classifies it as LegacyNeedsReview (`pending`) after the existing
  schema-1/2 migrations. Explicit `revoked=true` from this unreleased branch
  is kept. Grok on a consent-granted upgrade stays `allow=true` and unseen
  (`show=false`, announced=false) so Rescan can still find it. Isolated
  diagnose.log scope, a credentials-focused review of the five silent read
  paths, and isolated pending-review UI (Keep closed, cancel, Providers
  visibility, Detect providers again, Allow access) have passed. An upgraded
  `allow=false` is pending until that choice; it is not a guessed revocation.
- **Not acted on.** Claude's locally unusable credentials still arm the bounded
  service recheck, because they map to `AuthRequired`. Asymmetric with
  `CredentialUnusable`, but the recheck fails locally without a request; only
  the cadence differs. Pre-existing.

## Deferred v2.5.0 smoke items, now closed

These were recorded as not verified for v2.5.0 and were completed on
2026-09-01 against the v2.5.0 code plus these fixes:

- Fresh-profile first run, consent path - all four providers detected.
- Fresh-profile first run, decline path - no credential read, no request, no
  warning balloon.
- **Provider access -> Detect providers again** - Grok appeared immediately,
  with no balloon, which is correct: the menu action is the user's own request.
- The one-time detection notification for Grok on an upgraded profile.
- High Contrast rendering of the tray placeholder letter `X` - checked by
  rendering rather than on a High Contrast desktop.

### WSL availability

`wsl --terminate` was not used: it would have killed the owner's running
processes. Instead a `wsl.exe` that exits non-zero was placed next to
`gengchou.exe`, which wins the `CreateProcess` search order (prepending `PATH`
does not - System32 is searched first).

`list_running_wsl_distros` funnels a failed command and an empty distribution
list into the same empty vector, so everything downstream is identical. The app
reported `running WSL distros: 0`, attempted no WSL credential read, did not
hang, and fell back to the Windows-side credentials correctly. Restoring the
shim-free binary restored `running WSL distros: 1`.

## Not verified

- `wsl -l -q --running` succeeding with an empty list. Only the command-failure
  branch was exercised; the two converge four lines later.
- A provider whose credentials exist only inside WSL while WSL is stopped. That
  would require moving the owner's real credential file.
- The revocation scope and the unusable-credential classification are covered
  by unit tests and by reading the call sites, not by a live end-to-end run on
  this machine: both would require revoking the owner's own providers and
  corrupting a real `auth.json`. The release-checklist rows for them are
  written, and remain to be walked.
- High Contrast on an actual High Contrast desktop.
- Any balloon on a light system theme.

## Known limitation: the toast attribution icon is inverted

Windows renders the small attribution icon in the top-left of a toast as a
colour inversion of the tray icon it was anchored to. Established from a
screenshot rather than assumed: the application icon's `#202124` tile inverts
to exactly `(223, 222, 219)`, which is the value sampled from the toast, and
the same inversion turns the orange bar teal and the blue bar orange - which
is what made the icon look like a light-theme variant with its bars reordered.

This is not a theme-selection defect. The application icon is a single fixed
dark design (`src/icons/icon.ico`, one design at seven sizes, all within 2
units of the same mean luminance), no code path selects a light variant for
it, and the icon resource in the WinGet-installed v2.4.2 executable is
identical. Only the provider tiles have dark and light variants, and only for
Claude and Antigravity.

Others have reported the same behaviour: Windows applies an automatic contrast
adjustment to the app icon slot in the notification center, and it is reported
not to affect the taskbar or the tray icon - which is what we observe with the
same `HICON`. Reporters found it driven by the icon's own tone rather than by
the system theme alone, and found no way to opt out or to supply a
theme-specific icon; the toolkit maintainers only backlogged it. That is a
community report, not a documented rule, and it does not fully match what we
see: it claims a multi-hue icon is not inverted, while ours has four hues and
is. Its neutral dark tile covers most of the area, so a dominant-tone heuristic
would explain it - but that is inference, not established.

Nothing can be done about it from here: `Shell_NotifyIcon` has no parameter for
the attribution icon. The three other places the application icon appears - the
notification area, the main window and detail popup title bars, and the
credential dialog title bar - are unaffected.

The balloon image that Gengchou does supply is rendered faithfully. Measured on
an informational balloon: top bar `(235, 102, 68)`, middle `(244, 243, 243)`,
bottom `(57, 137, 248)`, tile `(33, 33, 38)` - the source asset, not inverted.
The owner's decision to keep the Gengchou mark in that slot therefore stands.

## Open

- `7bbdd96` - consent schema 3 and the pending-review strings in all eleven
  languages - landed after the third review round and has had no independent
  review. Every earlier round returned at least one accepted blocker.
- The manual smoke rows this release added or changed have not been walked on
  the machine: revocation scope, the pending/needs-review choices, the
  `CredentialUnusable` classification, an informational balloon on a light
  system theme, and High Contrast on a real High Contrast desktop. See
  "Not verified" above for the rest.
- Tagging order: merge to `main`, then tag. `git merge-base --is-ancestor
  v2.5.0 HEAD` already passes, so the workflow's ancestry gate is satisfied,
  and `.github/workflows/release.yml` additionally requires the tag to equal
  the `Cargo.toml` version, which the bump above now satisfies.
