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
- Read provider credentials only after explicit consent and only for providers
  whose access is enabled. Send a credential only to the provider that issued
  it.
- A missing credential is **Not detected**. A concrete credential source that
  is unreadable, malformed, expired, rejected, or otherwise unusable is grouped
  as **Authentication failed**, because the user recovery is to sign in again.
  Failure to start or complete a WSL probe is a transient request failure, not
  proof of an authentication failure.
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
  current log plus one `diagnose.log.old` generation.

## Network and updates

- Every HTTP JSON response is bounded to 4 MiB before deserialization.
- Update payloads remain hash-verified, staged, atomically replaced, and rolled
  back unless the new process reports ready. Never replace a still-running
  executable or restart an unverified path.

## Release history

- Never rewrite published history or move/delete a published version tag.
- Every new release commit must descend from the immediately preceding official
  release tag. See [RELEASE_HISTORY.md](RELEASE_HISTORY.md) for the one recorded
  historical discontinuity and the correct comparison anchors.
- Do not tag, publish, push, or submit WinGet manifests without explicit owner
  approval. Prepare WinGet only from a public, re-downloaded GitHub release.
