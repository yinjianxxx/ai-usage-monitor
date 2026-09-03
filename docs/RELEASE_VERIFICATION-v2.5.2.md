# v2.5.2 release verification

- Date: 2026-09-03
- Candidate: `claude/v2.5.2-followups`, 16 commits ahead of `main` /
  `3c123f2`, which is also the `v2.5.1` tag. The release-tag ancestry
  requirement in docs/INVARIANTS.md is satisfied: `v2.5.1` is an ancestor of
  this branch. Not tagged, not pushed, no PR opened.
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
- One internal name changed: the single-instance mutex is now
  `Global\Gengchou-Instance-v2-<data directory hash>` with a `Local\` fallback,
  replacing the machine-wide `Global\Gengchou`. Recorded in PROVENANCE.md.
  Nothing outside the app reads it.
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 Pro 26200 on 2026-09-03, all against the final tree
at `2.5.2`.

- `cargo fmt --all -- --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - clean. Also run with
  `--release`, because the debug-only `GENGCHOU_TEST_MUTEX_SUFFIX` block makes
  one `let mut` unused in a release build; that path is clean too, so CI's
  debug-profile clippy is not hiding a release warning.
- `cargo test --locked` - 371 passed, 0 failed (350 at v2.5.1; 361 after the
  first four batches on this branch)
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
was actually on disk, and the test was observed to fail. Ten mutations in
total, all caught.

### Single-instance scope

The mutex name is derived from the data directory. Verified live: the debug
build was started with `APPDATA`/`LOCALAPPDATA` pointed at a temporary
directory and consent pre-set to declined, and PowerShell
`[Threading.Mutex]::OpenExisting` confirmed
`Global\Gengchou-Instance-v2-89aeb764917b26d0` - the name computed
independently from that directory path - existed while it ran and was released
when it exited. The owner's real WinGet-installed v2.5.1 was running
throughout and still held `Global\Gengchou`; under the old code those two
processes could not have coexisted. The sandboxed run logged `poll skipped; no
shown provider has credential access`, so no credential was read and no
provider was contacted.

Mutations caught: dropping the `to_lowercase` before hashing, collapsing the
unresolved-directory fallback onto the unhashed one, and making a handed-off
launch raise a dialog.

### Child-process runner

`native_interop::run_with_timeout` drains stdout and stderr while the child
runs and terminates a job object on timeout.

- Removing the concurrent drain turned a 300 KB stdout from a 5-second pass
  into a 30-second timeout - the exact reported failure, an ordinary large
  answer read as an unresponsive probe.
- Replacing the job-object kill with `Child::kill` made the tree test report
  `grandchild 122192 outlived the timeout that killed its parent`, and left a
  real orphaned `powershell.exe` behind, which was then cleaned up.

Cost: the tree-kill test waits out a real 5-second timeout, so `cargo test`
went from about 0.2s to about 5.1s. Accepted deliberately - it is the only
evidence that the job object does what its doc comment claims.

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

## Not verified

These are gaps in this document, not passed rows.

- **The full manual Windows smoke test in docs/RELEASE_CHECKLIST.md was not
  run this round.** What was run is the targeted live evidence above plus the
  automated gates. The consent, provider-detection, credential-watch and
  surface-interaction rows are inherited from v2.5.1 on the reasoning that this
  branch touches none of that code; that reasoning is not a substitute for
  running them.
- **Two accounts, and one account in two sessions.** Skipped at the owner's
  instruction. This is the core scenario of the single-instance change and the
  only part of it not backed by live evidence. What is proved: the name is
  derived per data directory, it is created and released correctly, and the
  branch selection between handing off, reporting "already running elsewhere",
  reporting an unusable guard, and staying silent on the watchdog path is unit
  tested. What is not proved: what a second signed-in account actually sees,
  and whether a real second user token can create its own `Global\` name -
  the `ACCESS_DENIED` branch that was one of the two original silent exits.
  Windows 11 Pro is single-session, so this cannot be done without taking over
  the console session.
- **The 37 compact-surface fixtures were generated but not individually
  inspected.** No rendering code changed on this branch, and the re-rendered
  README widget, floating and tray strips came back byte-identical, which is
  stronger evidence of unchanged output than eyeballing would be - but it is
  different evidence than the row asks for.
- **The staging-directory reparse check was not exercised against a real
  junction.** The unit test covers an ordinary directory and a non-directory;
  creating a reparse point in a test needs privileges this suite does not
  assume, which is why the existing tests avoid it too.

## Side effects of verification

- `%LOCALAPPDATA%\Gengchou\updates` was created, empty, by the one E2E run that
  used the release helper before the harness/binary mismatch above was
  understood. It was left in place: the app creates that directory itself
  before every self-update, so its presence changes nothing.
- One `powershell.exe` orphaned by the job-object mutation run was located by
  command line and stopped.
- Temporary sandboxes under `%TEMP%` (`gcmutex`, `gcshared`, `gc-apply-*`,
  `gcdump`, tree-kill markers and shims) were removed.

## Open

- `Cargo.toml` is at `2.5.2` and no tag, push, PR, or WinGet submission has
  been made. All of those need explicit owner approval.
- The update-target identity check is the one change on this branch with a
  security character, and it was written and reviewed by the same author. A
  second independent pass over it is worth having before publishing.
