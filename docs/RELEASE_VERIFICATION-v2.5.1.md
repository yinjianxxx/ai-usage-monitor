# v2.5.1 release verification

- Date: 2026-09-01
- Candidate: in progress on `claude/v2.5.1-fixes`, based on `v2.5.0` /
  `0b478f1`. Not tagged, not pushed; `Cargo.toml` still reads `2.5.0`.
- Scope: two defects that had shipped silently since v2.4.1 (informational tray
  balloons never displayed, periodic provider detection never ran), one
  behaviour change (detection sweep also runs at startup on an existing
  install), and three interface fixes; no change to the executable, settings
  directory, tray, or update identities
- Signing: not in scope (owner has no signing certificate)

## Automated gates

Run locally on Windows 11 26200:

- `cargo fmt --check` - clean
- `cargo clippy --all-targets --locked -- -D warnings` - clean
- `cargo test --locked` - 316 passed, 0 failed

Each of the six commits was type-checked in isolation before being recorded, so
the branch is bisectable.

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

Two unit tests were added and both were mutation-checked: reverting the
behaviour each guards makes the corresponding test fail.

### Interface fixes

Confirmed on the machine by the owner: the application icon in the notification
area while no provider is authorised, the Gengchou icon in the credential
dialog's title bar, and the hover tooltip no longer reappearing beside an open
detail popup.

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

- `Cargo.toml` still reads `2.5.0`; bump before tagging.
