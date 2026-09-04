# v2.5.2 release verification

- Date: 2026-09-03
- Candidate: `claude/v2.5.2-followups`, branched from `main` / `3c123f2`,
  which is also the `v2.5.1` tag. The release-tag ancestry requirement in
  docs/INVARIANTS.md is satisfied: `v2.5.1` is an ancestor of this branch.
  Open as PR #7. Not tagged, not merged.
  `Cargo.toml` and `Cargo.lock` read `2.5.2`.
- Scope: fifteen changes. Most are a case of the app stopping without saying
  so - a background-thread panic freezing the usage for the life of the
  process, an unreadable settings file being replaced by defaults within
  seconds, an empty quota response recorded as a successful refresh, an
  Explorer restart permanently disarming the timers in the degraded path, a
  second Windows account's launch ending with no window at all, a credential
  probe deadlocking on a full pipe and being reported as unresponsive, a
  timeout that stopped only a `.cmd` shim, and a diagnostic-logging failure
  leaving neither a log nor a reason. Three harden the update transaction:
  target identity, staging-directory validation, and a version check on the
  download. The rest are the settings-tolerance, log-lock scoping,
  absolute-path launch, modal-ordering and lock-scope repairs listed in
  `.github/release-notes/v2.5.2.md`. No change to the executable,
  settings-directory, cache, or update identities.
- One internal mechanism changed: the single-instance guard is an exclusive
  handle on `instance.lock` inside the data directory, replacing the
  machine-wide `Global\Gengchou` mutex. Recorded in PROVENANCE.md and in both
  READMEs' data tables. Nothing outside the app depends on either form.
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 Pro 26200 on 2026-09-03, all against the final tree
at `2.5.2`.

- `cargo fmt --all -- --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - clean, in the debug
  profile CI uses and again with `--release`, so a release-only warning cannot
  hide behind CI's debug run.
- `cargo test --locked` - 373 passed, 0 failed (350 at v2.5.1; 361 after the
  first four batches on this branch; 371 before the independent review round)
- `cargo build --release --locked` - clean, at `target\release\gengchou.exe`
- PE properties of that build: ProductName `Gengchou`, ProductVersion and
  FileVersion `2.5.2`, OriginalFilename `gengchou.exe`, upstream copyright and
  `Comments` retained
- `tools\check-portable-runtime.ps1` - passed, no external MSVC/UCRT imports
- `tools\check-retired-identity.ps1` - passed, 13 historical lines
- `cargo audit` against the committed `Cargo.lock` - 122 dependencies, no
  advisories, exit 0. Locally installed cargo-audit 0.22.1; the newest release
  needs rustc 1.88 and this machine is on 1.86, so CI's pinned
  `rustsec/audit-check@v2.0.0` could still surface a future advisory first.
- `tests/update_ready_inbound_e2e.ps1` - passed
- `tests/updater_e2e.ps1 -Scenario Success` - passed
- `tests/updater_e2e.ps1 -Scenario ChildExit` - passed
- compact-surface debug gate: `--dump-widget` - 37 fixtures generated, exit 0
- README previews re-rendered with `tools\render-readme-images.ps1`. Only the
  four `detail-popup-*.png` changed, and only in the footer version (`v2.5.1`
  to `v2.5.2`), which was read off the rendered image. The widget, floating and
  tray strips came back byte-identical - see the note under *Not verified*.

The three E2E scripts must be run against the **debug** build. `ready_markers_dir`
honours `GENGCHOU_UPDATE_TEST_READY_DIR` only under `cfg(debug_assertions)`, so
a release helper silently writes its readiness marker to the real
`%LOCALAPPDATA%\Gengchou\updates` and the harness fails with "Ready marker
escaped the unique test directory". That happened once here before the runs
above; it is a harness/binary mismatch, not a product defect. CI already builds
the debug binary for this purpose.

## Targeted evidence

Every test added on this branch was mutation-checked: the behaviour the test
names was reverted in the source, `git diff --numstat` confirmed the mutation
was actually on disk, and the test was observed to fail. Fourteen mutations in
total, all caught.

### Single-instance guard

Verified live with two sandboxed data directories: both instances ran at the
same time, each `instance.lock` refused a second exclusive open while its
holder was alive, and both were free immediately after the holders exited. An
earlier run of the same shape against the hash-named mutex confirmed the
scoping half of this - that build started while the owner's real WinGet-
installed v2.5.1 still held `Global\Gengchou`, which the old machine-wide name
made impossible - and both runs logged `poll skipped; no shown provider has
credential access`, so no credential was read and no provider was contacted.

Mutations caught: giving the lock file a permissive share mode, and - against
the mutex the file replaced - dropping the `to_lowercase` before hashing,
collapsing the unresolved-directory fallback, and making a handed-off launch
raise a dialog.

### Child-process runner

`native_interop::run_with_timeout` drains stdout and stderr while the child
runs and terminates a job object on timeout.

- Removing the concurrent drain turned a 300 KB stdout from a 5-second pass
  into a 30-second timeout - the exact reported failure, an ordinary large
  answer read as an unresponsive probe.
- Replacing the job-object kill with `Child::kill` made the tree test report
  that the grandchild outlived the end of the run, and left a real orphaned
  `powershell.exe` behind, which was then cleaned up.

The tree test was rewritten once, after CI caught it being flaky. The first
version drove a real 5-second timeout and then read the process id the
grandchild was supposed to have written; on a GitHub runner PowerShell had not
finished starting inside that window, so the marker did not exist and the test
failed with `NotFound`. It passed on the first CI run and failed on the second
with no relevant change between them - a race, not a regression. It now drives
`end_process_tree` directly, which is the function both failure paths route
through, and waits for the grandchild to exist before ending it. The deadline
there bounds a hang rather than timing the interpreter, so a slow machine costs
seconds instead of a false failure. The suite is 0.7-0.9s across five
consecutive runs, and the job-object mutation is still caught.

What the rewrite gives up: a mutation at the call site - replacing
`end_process_tree(&job, &mut child)` inside the timeout branch with a bare
`child.kill()` - would no longer be caught, because the test no longer goes
through a real timeout. `run_with_timeout` returning `TimedOut` is still
covered by `command_runner_distinguishes_spawn_failure_from_timeout` in
`poller.rs`, which does not depend on the child reaching any state and so
cannot race.

### Update target identity

`--apply-update` accepted any target path. With the new check temporarily
removed and the helper rebuilt, calling it with a dead PID and an unrelated
file **replaced that file**, found the "new process" unhealthy, rolled back and
**launched** the restored path, leaving `someone-elses-file.exe.old` beside it.
So the pre-fix behaviour is not merely a replacement primitive; it also
executes a path chosen by whoever wrote the arguments.

With the check in place, both refusal paths were verified live against the
debug build: a dead PID (`4294967294`) and a live but unrelated parent
(`powershell.exe`). Both exited 1, the decoy file was byte-identical
afterwards, and no `.old` was created.

Mutations caught: ignoring the parent image path, accepting any helper hash,
making the version comparison always true, removing the trailing-zero folding
in that comparison, and skipping the pre-create ancestor check on the staging
directory.

### A claim that was disproved and corrected

An earlier draft of the release notes said that in a shared install directory a
self-update would fail and roll back because "Windows will not replace a loaded
image". That was tested and is false: a running `gengchou.exe` was renamed
aside and a new file written in its place while the process kept running, which
is exactly how `ReplaceFileW` performs the replacement. The note was rewritten
to the measured outcome - the update succeeds and the other account keeps
running the previous build from the renamed file until it restarts. Commit
`0f064da`.

### Automated review round on PR #7

GitHub's Copilot reviewer raised five comments - three distinct findings, one
of them repeated once per affected language file. All three were checked
against the source, all three held, and all three are fixed in `7aa5172`.

- `run_with_timeout` returned `WaitFailed` from a `try_wait` error without
  ending the child or joining the drain threads, contradicting the cleanup its
  own doc comment promises. Both failure paths now route through
  `end_process_tree`. A `try_wait` failure cannot be induced in a test - it
  calls `GetExitCodeProcess` on a handle this process owns - so this change has
  no regression test.
- The "another Windows session" wording was wrong on the `Local\` fallback
  path. That fallback is reached only when the `Global\` name cannot be created
  at all, and a `Local\` name is per session, so `ERROR_ALREADY_EXISTS` there
  means another instance in *this* session whose broadcast window was not yet
  findable. The message now names the likely case rather than asserting it, and
  the log line reports only the mutex name, whose prefix already states the
  scope.
- French, Spanish and Brazilian Portuguese `diagnostics_unavailable` were
  written without accents. German has the same defect (`verfuegbar`,
  `Abstuerze`) and was not flagged; it is fixed too.

Two of the three landed on code written earlier in this same session and had
survived two passes by its author. What the reviewer did not raise is anything
about the design - whether the two facts the update-target check relies on are
sufficient, or that German had the same accent defect as the three languages it
did flag. It reads lines well; that is not the same as the independent review
still listed under *Open*.

### Independent review round on PR #7

A Codex agent reviewed `3c123f2..HEAD` with no shared context, told explicitly
to treat this document and the commit messages as claims to check rather than
facts. It reported six Medium and two Low findings. Every one was traced to the
source before being accepted; none was invented, and all eight are fixed.

Two of them were defects in this branch's own repairs:

- **The update-target check ran after three paths that start the target.**
  `apply_update` restarts the target through `relaunch_unchanged_target` on a
  missing source, a leftover backup, and a target that cannot be hashed - and
  the identity check sat below all three, under a comment of mine saying that
  starting an unconfirmed path is worse than doing nothing. Verified by running
  the helper: `--apply-update <decoy.cmd> <missing source> 4294967294 <hash>`
  executed the decoy, which left the file it was written to write. After the
  fix, that invocation and a live-but-unrelated parent both exit 1 with the
  decoy byte-identical and no `.old` beside it.
- **The `.corrupt` rescue did not cover read failures.** `read_settings_content`
  returned the same `None` for "no file" and "file that cannot be read", so
  invalid UTF-8, a denied ACL and a sharing violation all loaded defaults with
  no quarantine, and the first save overwrote the original. That is the same
  data loss the rescue was added to prevent, and it made the README's claim
  about unreadable files overstated.

The rest, in the order they were fixed:

- A descendant that escaped the job object could hold the output pipe open, and
  joining the reader on the timeout path then never returned - turning a
  reportable timeout into a hung poll thread, which is worse than the bug the
  drain was added to fix. The failure paths no longer join, and the success
  path waits with a bounded grace.
- A download with no `ProductVersion` was passed rather than refused, which
  contradicts the invariant written for it in this same round; and
  `unwrap_or(0)` on a version component made `2.5.2.beta` equal `2.5.2` and any
  unparseable string equal `0` - a parse failure quietly answering "match".
- `system_program` fell back to a bare program name when
  `GetSystemDirectoryW` failed, which is exactly the hole that function was
  added to close.
- The single-instance mutex was replaced outright; see below.
- `docs/INVARIANTS.md` claimed the update-target check *proves* the target
  started the helper. It does not: both facts describe shape, not provenance,
  and a caller who can start a program and run the helper can satisfy either.
  The wording now says what the check actually delivers.

### The single-instance guard is a file, not a mutex

The review found two ways the hash-named mutex still allowed two writers on one
`settings.json`, and the owner chose the redesign over documenting them:

- When the `Global\` name cannot be created, the code fell back to a `Local\`
  one. `Local\` is per session, so two sessions of the same account then
  stopped excluding each other - the exact case that shares every file.
- The name came from a hash of the data directory path, lowercased. That cannot
  see that a short (8.3) name, a `.` component and a different drive-letter
  case all name one directory.

Holding `instance.lock` open with no sharing has neither problem: it is the
same file however the path is spelled, it excludes across sessions and accounts
because the filesystem does, and Windows releases it when the process ends
however it ends, so there is no stale lock. The mutex naming code is removed
rather than kept alongside, and with it the debug-only
`GENGCHOU_TEST_MUTEX_SUFFIX` escape - overriding `%APPDATA%` is now the way to
run a second instance, because the guard follows the data directory.

Verified live: two sandboxed data directories each ran an instance
simultaneously; each `instance.lock` refused a second exclusive open while its
holder ran; both were free immediately after the holders exited.

## Manual smoke test, partial

Run against the release build at `43cd4b3`, in a sandboxed data directory with
consent pre-set to declined. All four launches logged `poll skipped; no shown
provider has credential access`: no credential was read and no provider was
contacted.

| Row | Result |
| --- | --- |
| Startup: main window, broadcast helper, taskbar selection, widget embedded and ready | passed, 4 of 4 launches |
| `instance.lock` exists and stays empty | passed, 0 bytes |
| A second exclusive open is refused while the holder runs | passed |
| The guard is free after the process is **killed**, not exited | passed - no stale lock to clear |
| A launch after that kill starts normally and retakes the guard | passed |
| A second launch on the same data directory does not become resident | passed, `exit=0`, the first stayed up |
| Exit from the menu shuts down cleanly | passed - `deliberate quit requested`, `message loop exited`, `code=0` |
| The guard is released on that clean exit | passed |
| Data directory contents | only `settings.json` and `instance.lock` |
| Diagnostic log | one `startup aborted: an instance on this desktop was asked to show details`, which is the expected result of the second launch; nothing else |

Two observations from the run that are behaviour, not defects, and are recorded
so the next person does not re-investigate them:

- `WM_CLOSE` does not end the process. That is deliberate - external
  destruction starts in-process recovery instead of terminating, which is one
  of the stability changes in PROVENANCE.md. The exit path is the menu command,
  which is what the row above exercises.
- The broadcast helper window does not handle `WM_COMMAND`; the widget and the
  detail popup do. Posting the exit command to the helper does nothing, which
  is correct.

Not exercised: any actual clicking. The rows above assert that windows exist
and that the documented exit path works, not that the detail popup, context
menu or manual refresh behave correctly under real input.

## Not verified

These are gaps in this document, not passed rows.

- **Most of the manual Windows smoke test was not run.** The rows that were
  run are listed under *Manual smoke test, partial* above. The consent,
  provider-detection, credential-watch and surface-interaction rows are
  inherited from v2.5.1 on the reasoning that this branch touches none of that
  code. That reasoning is not a substitute for running them - this round
  already produced one counter-example, where code its own author had read
  twice still had the defect an independent pass found immediately.
- **Two accounts, and one account in two sessions.** Skipped at the owner's
  instruction, permanently. Windows 11 Pro is single-session: Fast User
  Switching and RDP to localhost both take over the console, so it cannot be
  done in the background. What is proved: two data directories never exclude
  each other and one always excludes itself however its path is spelled, the
  guard is released when its holder ends, and the choice between handing off,
  reporting "already running elsewhere", reporting an unusable guard, and
  staying silent on the watchdog path is unit tested. What is not proved: what
  a second signed-in account actually sees on screen. The file lock removes the
  namespace question that a named mutex would still have left open here.
- **The 37 compact-surface fixtures were generated but not individually
  inspected.** No rendering code changed on this branch, and the re-rendered
  README widget, floating and tray strips came back byte-identical, which is
  stronger evidence of unchanged output than eyeballing would be - but it is
  different evidence than the row asks for.
- **The staging-directory reparse check was not exercised against a real
  junction.** The unit test covers an ordinary directory and a non-directory;
  creating a reparse point in a test needs privileges this suite does not
  assume, which is why the existing tests avoid it too.
- **The job-object escape window is narrowed, not closed.** A grandchild
  created between `spawn` and `AssignProcessToJobObject` is outside the job,
  because `std` offers no way to resume a suspended child. The bounded drain
  means such an escape can no longer hang a call, but it can still outlive the
  timeout that was meant to end it.
- **The update-target check has no test for a live parent whose image differs
  while the helper's bytes match.** That combination now passes deliberately -
  it is the recycled-pid case - and the unit test asserts it, but no live run
  reproduced a recycled pid.

## Side effects of verification

- `%LOCALAPPDATA%\Gengchou\updates` was created, empty, by the one E2E run that
  used the release helper before the harness/binary mismatch above was
  understood. It was left in place: the app creates that directory itself
  before every self-update, so its presence changes nothing.
- One `powershell.exe` orphaned by the job-object mutation run was located by
  command line and stopped.
- Temporary sandboxes under `%TEMP%` (`gcmutex`, `gcshared`, `gc-apply-*`,
  `gcdump`, tree-kill markers and shims) were removed.

## Post-release

Recorded on 2026-09-03, after the fact. Everything below was read back from
GitHub and the WinGet registry rather than from memory of the release run.

- PR #7 merged as `a855962`; tag `v2.5.2` points at that commit. `v2.5.1`
  (`3c123f2`) is an ancestor, so the ancestry gate held.
- Release published 2026-09-03T04:44:48Z, not a draft and not a pre-release.
  `GET /repos/ynjmxn/gengchou/releases/latest` returns `v2.5.2`, which is the
  endpoint the in-app check reads.
- Six assets, including the two the updater requires by name: `gengchou.exe`
  and `SHA256SUMS`. The `gengchou.exe` digest carries two GitHub attestations.
- WinGet: `microsoft/winget-pkgs#428603` merged;
  `manifests/y/ynjmxn/Gengchou/2.5.2` exists in the public registry.
- Installing the merged package on a clean Windows profile was **not** done,
  and is now permanently out of scope. See the note at the end of
  *Post-release WinGet hand-off* in docs/RELEASE_CHECKLIST.md for what stands
  in for it. The row was removed from the checklist rather than left as a gate
  that is never run.

## Open

- The update-target identity check is the one change on this branch with a
  security character, and it was written and reviewed by the same author. It
  shipped without a second independent pass. That pass is now folded into the
  v2.5.3 round, with the scope widened from this one function to the whole
  update transaction - download, verification, staging, replacement, rollback,
  restart - plus the balloon click that v2.5.3 adds as a new entry point to it.
