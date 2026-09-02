# Repository invariants

These are release-blocking constraints, not implementation suggestions. Read
them before changing runtime behavior, persistence, provider integration,
identity, or release automation.

## Stable identity

- Keep `gengchou` as the executable and Cargo package name.
- Keep `Gengchou` as the data-directory, updater, single-instance, and WinGet
  identity. A display-name change must not silently create a second install or
  data location.
- Preserve the retained upstream attribution and current publisher metadata.

## Credentials and privacy

- Never log, persist, commit, or include in diagnostics any token, cookie,
  account identifier, or raw credential/CLI response.
- Read provider credentials only after explicit consent, and never for a
  provider the user has turned off under Provider access or that is waiting
  for a LegacyNeedsReview choice - detection and the
  credential watch included, at every start, on every sweep, and on every poll
  pass. A provider that is not pending and not revoked stays
  in scope even when it is not currently enabled: detection is the only way a
  newly installed provider is ever noticed. An upgraded `allow=false` whose
  reason cannot be recovered is pending until the user allows access or keeps
  it closed; it is not guessed to be a revocation. The review dialog must keep
  a visible third answer that changes nothing, and default to it: dismissing a
  prompt is not a decision either way. Send a credential only to
  the provider that issued it.
- A missing credential is **Not detected**. A Windows-local credential source
  that is unreadable or malformed, and any credential the provider expires or
  rejects, is grouped as **Authentication failed**, because the user recovery
  is to sign in again. Failure to start or complete a WSL probe is never proof
  of an authentication failure and takes the transient request-failure path.
- How far that reaches into WSL differs per provider, deliberately. Claude's
  probe reports its own outcome, so an absent credential, a present but
  unreadable one, and a probe that did not answer stay distinct there. The
  Codex, Antigravity and Grok probes report only success or nothing, so they
  cannot separate a distribution that has no credential from one that failed to
  answer; they move on to the next source instead of guessing, and a credential
  that lives solely in an unreachable distribution reads as **Not detected**
  for those three. Do not classify a bare WSL failure as unusable for them: it
  would raise a sign-in warning for a probe that was merely slow.
- Routine notifications use the Gengchou icon and are silent. Only a current
  credential problem requiring user action uses the Windows warning glyph and
  notification sound.

## Persistence and diagnostics

- Settings and the usage cache contain no credentials. Each has an independent
  monotonic revision and serialized writer; an older snapshot must never
  overwrite a newer snapshot.
- Atomic persistence writes the temporary file completely, flushes and syncs
  it, then replaces the destination in the same directory.
- Diagnostic logging rejects reparse-point paths, reopens the current pathname
  for every write, serializes cooperating processes, and keeps exactly the
  current log plus one `diagnose.log.old` generation. Its cross-process lock is
  derived from the log path, so accounts that write different files do not wait
  on each other.
- A settings file that cannot be parsed is preserved as `settings.json.corrupt`
  and reported to the user before defaults are loaded. Loading defaults is not
  a read-only outcome: the first save that follows replaces the user's layout,
  language, provider selection and access decisions. A value a newer build
  wrote costs at most the one field that cannot be represented, never the file.

## Network and updates

- Every HTTP JSON response is bounded to 4 MiB before deserialization.
- Update payloads remain hash-verified, staged, atomically replaced, and rolled
  back unless the new process reports ready. Never replace a still-running
  executable or restart an unverified path.
- The new process confirms readiness before any startup step that can wait on
  the user. The helper's timeout is fixed, so a dialog placed ahead of that
  confirmation rolls a healthy update back while the user is still reading it.

## Release history

- Never rewrite published history or move/delete a published version tag.
- Every new release commit must descend from the immediately preceding official
  release tag. See [RELEASE_HISTORY.md](RELEASE_HISTORY.md) for the one recorded
  historical discontinuity and the correct comparison anchors.
- Do not tag, publish, push, or submit WinGet manifests without explicit owner
  approval. Prepare WinGet only from a public, re-downloaded GitHub release.
