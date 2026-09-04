# v2.5.3 release verification

- Date: 2026-09-03, continued 2026-09-04 (code follow-up later the same day)
  and 2026-09-05 (dogfood latch walk)
- Candidate: `claude/v2.5.3-update-notice`, branched from `main` / `a855962`,
  which is also the `v2.5.2` tag. The release-tag ancestry requirement in
  docs/INVARIANTS.md is satisfied: `v2.5.2` is an ancestor of this branch.
  Merged as PR #8. Tagged `v2.5.3` at `f0fd239`.
  `Cargo.toml` and `Cargo.lock` read `2.5.3`.
- Scope: one behaviour. The daily update check already found new versions; it
  announced them only by rewriting one entry inside the Settings submenu. It
  now raises a silent balloon (once per version), that balloon can be clicked
  to reach the same confirmation the manual check shows, the detail popup
  footer carries `v2.5.2 → 2.5.3` for as long as an update is outstanding, and
  that footer line is itself clickable. A remembered offer that is not newer
  than the running build is treated as up to date; a failed automatic check
  does not clear the last successful offer. No change to how updates are
  downloaded, verified, staged, applied, or rolled back. No change to the
  executable, settings-directory, cache, or update identities.
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 Pro 26200 on 2026-09-03, all against the final tree
at `2.5.3`. Exit codes were read back, not inferred from output.

- `cargo fmt --all -- --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - exit 0, and again with
  `--release` - exit 0
- `cargo test --locked` - 381 passed, 0 failed on 2026-09-03 (373 at v2.5.2;
  the eight new tests are listed under *Targeted evidence*). Re-run on
  2026-09-04 after the follow-up below: 386 passed, 0 failed. `cargo fmt
  --all -- --check` clean and `cargo clippy --all-targets --locked -- -D
  warnings` exit 0 on that same tree. `clippy --release` and
  `cargo build --release --locked` were not re-run for the follow-up.
- `cargo build --release --locked` - exit 0
- PE properties of that build: ProductName `Gengchou`, ProductVersion and
  FileVersion `2.5.3`, OriginalFilename `gengchou.exe`, upstream copyright and
  `Comments` retained
- `tools\check-portable-runtime.ps1` - passed, no external MSVC/UCRT imports
- `tools\check-retired-identity.ps1` - passed, 13 historical lines
- `cargo audit` - 1239 advisories loaded, 122 crate dependencies scanned,
  exit 0, no advisories. cargo-audit is now 0.22.2, the current release,
  installed under the `stable` toolchain (rustc 1.97.0) rather than the pinned
  1.86.0 build toolchain: cargo-audit parses `Cargo.lock` and does not compile
  this project, so the two never needed to be the same. `rust-toolchain.toml`
  is untouched. This retires the local/CI divergence recorded at v2.5.2.
- `tests/update_ready_inbound_e2e.ps1` - passed
- `tests/updater_e2e.ps1 -Scenario Success` - passed
- `tests/updater_e2e.ps1 -Scenario ChildExit` - passed
- compact-surface debug gate: `--dump-widget` - 37 fixtures generated, exit 0.
  This round they were also composed into one contact sheet and looked at,
  which closes a gap carried since v2.5.2 - see *A correction* below for why
  that gap was smaller than it was described as.
- README previews re-rendered with `tools\render-readme-images.ps1`. The four
  `detail-popup-*.png` were kept: their only visible change is the footer
  version, `v2.5.2` to `v2.5.3`, read off the rendered image. The five widget
  and floating images the script also rewrote were reverted - see *README
  renders are not bit-reproducible*.

## Targeted evidence

Eight tests were added. Seven mutation checks were run: the behaviour a test
names was reverted in the source, the mutation was confirmed to be on disk,
and the test was observed to fail. All seven were caught, and all seven source
mutations were reverted.

- `update_balloon_due` with the version comparison removed - caught by
  `an_update_is_announced_once_per_version`.
- The German balloon body with `{version}` dropped - caught by
  `the_update_balloon_body_names_the_version_and_the_menu`, which checks every
  language for both placeholders and for leftover braces.
- `balloon_offer_is_current` with the busy guard removed - caught by
  `a_balloon_click_only_acts_on_the_version_it_offered`.
- The `NIN_BALLOONUSERCLICK` arm deleted from the tray decoder - caught by
  `a_balloon_click_is_recognised_on_any_icon`.
- `take_balloon_click` changed to clone instead of take - caught by
  `only_the_balloon_on_screen_carries_an_offer`.
- `point_in_detail_update_link` treating a missing rect as a hit, and again
  with the right edge made inclusive - both caught by
  `the_footer_line_answers_only_inside_itself`.

`the_footer_says_where_the_running_version_can_go` and
`a_remembered_update_reaches_the_footer` were not mutation-checked; both assert
a literal mapping with no branch to invert beyond the assertion itself.

### Follow-up, 2026-09-04

Five tests were added after the second independent review. They pin helpers
the call sites now use. They were not mutation-checked by reverting source
and watching a test go red.

- `a_failed_check_keeps_the_last_successful_offer` - the failed-check arm
  restores through `retain_update_after_failed_check`. Reverting that helper
  to Idle + `None` fails this test. Reverting only the call site to Idle +
  `None` while leaving the helper still would not.
- `a_balloon_that_never_appeared_carries_no_offer` - `settle_balloon_click`
  drops the offer when delivery failed, including a previous offer.
- `a_balloon_click_without_an_offer_does_nothing` - an empty latch is a
  no-op, not a recheck. Quota-reset and provider-detection balloons must
  not start an update check. (An earlier follow-up treated this as a
  recheck; that was reverted so news balloons stay news.)
- `the_footer_snapshot_carries_an_outstanding_update` - live snapshot footer
  fields come from `detail_footer_versions`. Dump fixtures still hardcode
  `None`.
- `destroying_the_detail_popup_clears_the_footer_target` -
  `set_detail_update_link_rect(None)` forgets the painted hit rect and hover.
  The `WM_DESTROY` arm now calls that; this test does not compile-fail if
  only that one line is deleted.

`a_remembered_update_reaches_the_footer` now also asserts `Available` (fresh)
and the busy states. Balloon/footer confirmation occupies `Prompting` via
`prompt_then_apply_update`, shared with the interactive check arm. That wrap
has no extra unit test beyond `update_prompt_is_a_busy_state_until_the_modal_choice_returns`.

### The balloon, live

Run in a sandboxed data directory (`APPDATA`/`LOCALAPPDATA` overridden) with
consent pre-set to declined, against a debug build whose `Cargo.toml` version
was temporarily lowered to `2.5.1` so the real `v2.5.2` release would read as
newer. Every launch logged `poll skipped; no shown provider has credential
access`: no credential was read and no provider was contacted. Only GitHub was
reached, by the update check itself.

- First launch: `update available, announced version 2.5.2`, and no
  `balloon not delivered` line, so `Shell_NotifyIconW` accepted the balloon.
- Third launch, after `last_update_check_unix` was aged to force a re-check:
  the check ran (the stored timestamp moved to that launch, and the outcome is
  still `available 2.5.2`) and **no** announcement line was written. One
  announcement per version, end to end.
- `WM_APP_TRAY` with `NIN_BALLOONUSERCLICK` posted to the main window produced
  `update balloon clicked for 2.5.2`, so the whole chain runs: shell message,
  decode, offer taken, confirmation path entered. Throughout, the log contains
  no apply, staging, download or replacement activity - an unanswered
  confirmation never starts replacing the executable.

A second identical post produced no second line, which is consistent with the
offer being answered once, but is not proof: the confirmation dialog was open
by then and the message may simply not have been dispatched.
`only_the_balloon_on_screen_carries_an_offer` covers that property
deterministically.

Sandbox side effects: the temporary version lowering was reverted and the
lockfile rebuilt at `2.5.3`; the sandboxed data directories under `%TEMP%`
were removed; the sandboxed process was stopped.

### On the owner's own machine

The branch was installed as a dogfood build on 2026-09-03: the same code,
stamped `2.5.1` so the published `v2.5.2` would read as newer, copied to
`%LOCALAPPDATA%\Programs\Gengchou-dogfood\` and started in place of the
WinGet-installed instance, which was left untouched. Settings were backed up
first. The startup entry, which had been pointing at the repository's
`target\release` build, was repointed at the dogfood copy.

The owner then produced the evidence this document had listed as missing:

```
16:58:45  update available, announced version 2.5.2
16:58:47  update balloon clicked for 2.5.2
16:58:56  detail popup: open requested
```

Nothing posted that click - the balloon was shown by Windows, seen, and
clicked. The confirmation was declined: no apply, staging, download or
replacement appears in the log, and the executable is byte-for-byte the one
copied in. So "an unanswered confirmation never replaces the executable" holds
on a real profile, not only in a sandbox.

The footer link landed after this run, so it was not exercised in that
dogfood session. It was exercised on 2026-09-04; see *Footer and notification
history, 2026-09-04*.

### The footer, rendered

`--dump-detail-popup` was temporarily pointed at an outstanding update and the
result was looked at, in Chinese and English, light theme: the footer reads
`v2.5.3 → 2.5.4` with the status line shortened by exactly the extra width and
no overlap, and the arrow renders in Segoe UI. That temporary change was
reverted; the committed dump path reports no outstanding update, because a
README image implying an update exists would be a lie.

### Footer and notification history, 2026-09-04

Isolated debug build of `95018c9`, ProductVersion stamped `2.5.1` so the
published `v2.5.2` read as newer. `APPDATA`/`LOCALAPPDATA` under
`%TEMP%\gengchou-v253-live-rtpyfbeo`, consent declined. The owner's
WinGet-installed instance and dogfood PID 63424 were left untouched.
`GENGCHOU_RELAUNCH=1` so the sandbox would not hand off through the
desktop-global `GengchouBroadcast` window.

Live PID 111896 then 79876 (re-announce). Log lines, read back:

```
22:31:28  poll skipped; no shown provider has credential access
22:31:28  update available, announced version 2.5.2
22:37:52  detail popup: update link clicked for 2.5.2
22:52:32  update available, announced version 2.5.2
```

No apply, staging, download, or replacement. The owner reported: the live
footer read as an outstanding update; clicking it opened the confirmation;
No was chosen. After the banner timed out, the taskbar notification-centre
flyout did **not** keep the toast. Settings → System → Notifications **did**.
The second announcement was a harness reset of `last_update_outcome`, not a
product re-announce, so the owner could watch without clicking.

The independent review (`tmp/v253-independent-review.md`, not committed)
returned **conditional**. Finding 1 (remembered `Available` of the running
version still shown as outstanding) and finding 2 (a failed auto-check
cleared that offer) are fixed in `2098e0f`. Finding 3 (default the
confirmation to No) was declined by the owner: the dialog stays default Yes.
Findings 4–5 are notes, not acted on.

### Latch mapping, live, 2026-09-05

Dogfood was swapped to `3ad4f82` (SHA256
`3EDC4543DF1724F2CB6DB503341481D38473F777BBE6ECDF5ED0694F97BAECED`),
ProductVersion `2.5.3`. The WinGet-installed executable was left untouched.
PID 129832 logged `diagnostic logging started v2.5.3`. At swap,
`settings.json` still had `last_update_outcome` `available 2.5.2`.

The owner then walked three footer states on that build, on the real
profile, and reported all three as expected. The screens were not seen by
anyone else.

- Disk `available 2.5.2` (older than the running build): footer `v2.5.3`,
  not `v2.5.3 → 2.5.2`.
- Disk `available 2.5.3` (equal to the running build): footer `v2.5.3`,
  not `v2.5.3 → 2.5.3`.
- Disk `available 9.9.9` (newer): footer `v2.5.3 → 9.9.9`.

That is Finding 1's mapping on a live window. It is not a successful apply:
the new process after replacing the executable has still not been watched.

## Not verified

These are gaps in this document, not passed rows.

- **The manual Windows smoke test was run only for the rows this round
  touches** (launch, balloon, balloon click, footer, update path). The
  consent, provider-detection, credential-watch and surface-interaction rows
  are inherited from v2.5.1 and v2.5.2.
- **Whether a click from Settings → System → Notifications delivers
  `NIN_BALLOONUSERCLICK`** was not tried. The owner looked and did not click.
  While this process still holds the offer, that click is the same as the
  banner. After a restart or after the offer was taken, it is a no-op; the
  footer is the recovery. Quota-reset balloons do not start a check.
- **The new process after a successful apply has not been watched** for
  `vX → X`. Finding 1's mapping was walked live on 2026-09-05 by writing
  `last_update_outcome` and restarting; that is the same function apply
  relies on, not the apply itself. A 2.5.2 → 2.5.3 apply can only be
  watched after this version is published.

## A correction

The plan for this round said the 37 compact-surface fixtures had to be
inspected because "the footer changed". That was wrong: the footer belongs to
the detail popup, and nothing in the compact surfaces (widget, floating, tray)
was touched this round. The fixtures were generated and inspected anyway,
which closes the standing gap, but inspecting them was not evidence about this
round's changes.

## README renders are not bit-reproducible

The v2.5.2 record leaned on the widget, floating and tray strips coming back
byte-identical after a re-render, treating that as evidence the surfaces had
not changed. Re-rendering on this branch changed five of those images. A
pixel-level comparison against the committed versions found the same glyphs at
the same positions, differing only in subpixel antialiasing values - invisible
at 1:1 and still invisible magnified 8x. Two consecutive renders in this
session were identical to each other, so the output is stable within a session
but not across them.

The five images were reverted rather than committed: nothing about those
surfaces changed this round, and the diff would have been pure noise. The
consequence for future rounds is that "byte-identical" cannot be used as
evidence of an unchanged surface; a pixel comparison with a tolerance, or a
visual check, is what the claim actually needs.

## Post-release

Recorded on 2026-09-05, read back from GitHub rather than from memory of the
release run.

- PR #8 merged as `f0fd239`; annotated tag `v2.5.3` points at that commit.
  `v2.5.2` (`a855962`) is an ancestor, so the ancestry gate held.
- Release published 2026-09-04T16:55:29Z, not a draft and not a pre-release.
  `GET /repos/ynjmxn/gengchou/releases/latest` returns `v2.5.3`.
- Six assets, including the two the updater requires by name: `gengchou.exe`
  and `SHA256SUMS`. Re-downloaded and checked against `SHA256SUMS`:
  zip `53a0a43b8f6efd4fa8fa987a70677f6c73b07118dedd99767f3ccbf51a81112c`,
  exe `b9d137cb45493127d35b4080e43a1bb656f7f5d97dd1a7cac4e9d4115b61a0f9`.
  Both carry GitHub attestations (`gh attestation verify` exit 0).
- WinGet: `microsoft/winget-pkgs#429617` opened; not yet merged. Installing
  the merged package on a clean Windows profile remains out of scope.

## Open

- WinGet PR `microsoft/winget-pkgs#429617` waiting on the upstream pipeline.
