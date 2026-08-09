# Release history and comparison anchors

Published tags are immutable. During the v2.3 identity migration, the main
history was reconstructed without moving the already published tags. As a
result, tags v2.3.2 through v2.4.0 are not ancestors of current `main`, even
though each published tree has an identical main-line counterpart.

| Published tag | Tag commit | Identical tree on current main |
| --- | --- | --- |
| `v2.3.2` | `f2484a2` | `bfaf353` |
| `v2.3.3` | `e8992ce` | `b39e0c7` |
| `v2.3.4` | `576d94a` | `d5fb42a` |
| `v2.4.0` | `e43c80e` | `4a90d60` |
| `v2.4.1` | `159fecf` | `159fecf` (normal ancestry restored) |

The mappings above were verified by comparing Git tree object IDs, not by
assuming similar commit messages. No released content was lost; the break
affects ancestry-based history and release-delta commands.

For a v2.4.0-to-v2.4.1 source comparison, use the main-line anchor:

```powershell
git diff 4a90d60..v2.4.1
```

Do not use `git log v2.4.0..v2.4.1` as the release commit list: it crosses the
recorded discontinuity and includes reconstructed copies of older work.

From v2.4.1 onward, every release must preserve normal ancestry. The release
workflow verifies that the immediately preceding semantic-version tag is an
ancestor of the new tagged commit. If the gate fails, fix the branch topology;
never move an old tag or rewrite the published history to make the check pass.

Exact published release text and assets live on GitHub Releases. Files under
`.github/release-notes/` are the repository copies used by automation; a file
may contain an explicit post-publication correction note, but that does not
change the already published release body.
