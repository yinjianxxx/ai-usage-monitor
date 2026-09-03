# v2.5.3 release verification

- Date: 2026-09-03
- Candidate: `claude/v2.5.3-update-notice`, branched from `main` / `a855962`,
  which is also the `v2.5.2` tag. The release-tag ancestry requirement in
  docs/INVARIANTS.md is satisfied: `v2.5.2` is an ancestor of this branch.
  Four commits. Not tagged, not pushed, no PR.
  `Cargo.toml` and `Cargo.lock` read `2.5.3`.
- Scope: one behaviour, in three parts. The daily update check already found
  new versions; it announced them only by rewriting one entry inside the
  Settings submenu. It now raises a silent balloon (once per version), that
  balloon can be clicked to reach the same confirmation the manual check
  shows, the detail popup footer carries `v2.5.2 → 2.5.3` for as long as an
  update is outstanding, and that footer line is itself clickable. No change to
  how updates are downloaded, verified, staged, applied, or rolled back. No change to the executable,
  settings-directory, cache, or update identities.
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 Pro 26200 on 2026-09-03, all against the final tree
at `2.5.3`. Exit codes were read back, not inferred from output.

- `cargo fmt --all -- --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - exit 0, and again with
  `--release` - exit 0
- `cargo test --locked` - 381 passed, 0 failed (373 at v2.5.2; the eight new
  tests are listed under *Targeted evidence*)
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
`targetelease` build, was repointed at the dogfood copy.

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

The footer link landed after this run, so it has not been exercised live.

### The footer, rendered

`--dump-detail-popup` was temporarily pointed at an outstanding update and the
result was looked at, in Chinese and English, light theme: the footer reads
`v2.5.3 → 2.5.4` with the status line shortened by exactly the extra width and
no overlap, and the arrow renders in Segoe UI. That temporary change was
reverted; the committed dump path reports no outstanding update, because a
README image implying an update exists would be a lie.

## Not verified

These are gaps in this document, not passed rows.

- **The footer marker was never confirmed in a live window.** What is
  verified: the status-to-footer mapping by unit test including the remembered
  case, and the drawing itself by a real render. What is not: the one
  expression that feeds the live snapshot from `update_status`. The owner did
  open the popup seconds after clicking the balloon, but what it showed was not
  reported and is not claimed here. An earlier attempt to capture a sandboxed
  instance's popup was abandoned after a screen-region capture picked up the
  owner's own desktop contents; that image was deleted immediately and the
  approach dropped rather than retried.
- **Whether the balloon survives into the notification centre** after timing
  out was not observed. That it is shown and can be clicked is now evidenced
  on the owner's machine - see *On the owner's own machine* - but nobody
  waited for one to time out and then looked for it.
- **The footer link has never been clicked.** Its geometry is unit tested and
  its hover appearance was rendered and looked at, but no live run has gone
  from pointer to dialog through it.
- **The manual Windows smoke test was run only for the rows this round
  touches** (launch, balloon, balloon click, update path). The consent,
  provider-detection, credential-watch and surface-interaction rows are
  inherited from v2.5.1 and v2.5.2.
- **The independent review has not been run.** It is the one open item; see
  *Open*.

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

## Open

- The independent review agreed for this round has not been run. Its scope is
  the whole update transaction - download, verification, staging, replacement,
  rollback, restart - plus the balloon click this branch adds as a new entry
  point into it. This carries forward the item left open at v2.5.2, where the
  update-target identity check shipped having been written and reviewed by the
  same author.
- No tag, push, PR, or WinGet submission has been made. All of those need
  explicit owner approval.
