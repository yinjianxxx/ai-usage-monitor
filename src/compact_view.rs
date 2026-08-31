//! Shared view model for the taskbar widget and floating monitor.
//!
//! This module contains product semantics only. It deliberately has no HWND,
//! HDC, DPI, or drawing dependencies so compact-surface state can be tested
//! without constructing Windows UI objects.

use crate::localization::Strings;
use crate::models::{AppUsageData, ProviderStatus, UsageData, UsageWindow};
use crate::poller;
use crate::tray_icon::TrayIconKind;

pub(crate) const WARN_THRESHOLD_PERCENT: i32 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Normal,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Attention {
    Normal,
    Warn,
    Stale,
    ActionRequired,
}

#[derive(Clone, Debug)]
pub(crate) struct WindowView {
    pub(crate) label: String,
    pub(crate) percent: Option<f64>,
    pub(crate) display_percent: i32,
    pub(crate) percent_text: String,
    pub(crate) countdown: String,
    pub(crate) duration_seconds: Option<u64>,
    pub(crate) severity: Severity,
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderView {
    pub(crate) kind: TrayIconKind,
    /// Canonical headline window shared by every compact surface.
    pub(crate) badge: Option<WindowView>,
    /// Headline first, then the most urgent remaining window (at most two).
    pub(crate) windows: Vec<WindowView>,
    pub(crate) placeholder: Option<String>,
    pub(crate) attention: Attention,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct CompactViewModel {
    pub(crate) providers: Vec<ProviderView>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    data: Option<&AppUsageData>,
    strings: Strings,
    order: &[TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> CompactViewModel {
    let providers = order
        .iter()
        .filter(|kind| match kind {
            TrayIconKind::Claude => show_claude_code,
            TrayIconKind::Codex => show_codex,
            TrayIconKind::Antigravity => show_antigravity,
            TrayIconKind::Grok => show_grok,
        })
        .map(|kind| {
            provider_view(
                *kind,
                data.and_then(|data| data.usage(*kind)),
                data.and_then(|data| data.error(*kind)),
                strings,
            )
        })
        .collect();
    CompactViewModel { providers }
}

pub(crate) fn placeholder_model(
    text: &str,
    order: &[TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> CompactViewModel {
    let providers = order
        .iter()
        .filter_map(|kind| {
            let visible = match kind {
                TrayIconKind::Claude => show_claude_code,
                TrayIconKind::Codex => show_codex,
                TrayIconKind::Antigravity => show_antigravity,
                TrayIconKind::Grok => show_grok,
            };
            visible.then(|| ProviderView {
                kind: *kind,
                badge: None,
                windows: Vec::new(),
                placeholder: Some(text.to_string()),
                attention: Attention::Normal,
            })
        })
        .collect();
    CompactViewModel { providers }
}

/// Keep the cached compact surfaces in the same provider order as the tray.
/// Sorting the existing rows preserves their current values and attention
/// states even while provider polling is paused or failing.
pub(crate) fn reorder_providers(model: &mut CompactViewModel, order: &[TrayIconKind]) {
    model.providers.sort_by_key(|provider| {
        order
            .iter()
            .position(|kind| *kind == provider.kind)
            .unwrap_or(order.len())
    });
}

pub(crate) fn worst_window(provider: &ProviderView) -> Option<&WindowView> {
    provider.windows.iter().reduce(|best, candidate| {
        if candidate.display_percent > best.display_percent
            || (candidate.display_percent == best.display_percent
                && duration_rank(candidate.duration_seconds) < duration_rank(best.duration_seconds))
        {
            candidate
        } else {
            best
        }
    })
}

/// Window shown by the taskbar badge.
///
/// Keep the short-window value stable during normal operation. A different
/// window only takes over after it reaches the warning threshold, so weekly
/// exhaustion is never hidden even though sub-threshold long-window drift is
/// intentionally left to the tooltip and detail surfaces.
pub(crate) fn badge_window(provider: &ProviderView) -> Option<&WindowView> {
    if provider.badge.is_some() {
        return provider.badge.as_ref();
    }
    if let Some(warned) = worst_window(provider).filter(|window| window.severity == Severity::Warn)
    {
        return Some(warned);
    }

    provider
        .windows
        .iter()
        .find(|window| {
            window
                .duration_seconds
                .is_some_and(|seconds| approximately(seconds, 5 * 60 * 60))
        })
        .or_else(|| {
            provider
                .windows
                .iter()
                .min_by_key(|window| duration_rank(window.duration_seconds))
        })
}

fn duration_rank(duration_seconds: Option<u64>) -> u64 {
    duration_seconds.unwrap_or(u64::MAX)
}

fn provider_view(
    kind: TrayIconKind,
    usage: Option<&UsageData>,
    error: Option<ProviderStatus>,
    strings: Strings,
) -> ProviderView {
    let badge = usage
        .filter(|usage| !usage.is_empty())
        .and_then(|usage| badge_usage_window(usage, strings));
    let windows = usage
        .filter(|usage| !usage.is_empty())
        .map(|usage| {
            compact_usage_windows(usage)
                .into_iter()
                .map(|window| window_view(window, strings))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let placeholder = windows.is_empty().then(|| "--".to_string());
    let quota_attention = if windows
        .iter()
        .any(|window| window.severity == Severity::Warn)
    {
        Attention::Warn
    } else {
        Attention::Normal
    };
    let attention = match error {
        Some(ProviderStatus::AuthenticationFailed) => Attention::ActionRequired,
        // Never signed in is a resting state, not a failure: the user may
        // simply not use this provider. Showing the "not detected" badge is
        // enough - raising the alarm colour would demand action that isn't due.
        Some(ProviderStatus::NotSignedIn) => quota_attention,
        // A single rate limit or request failure is handled by automatic retry.
        // The application layer promotes it to `Stale` only after the value's
        // freshness or the consecutive-failure count crosses the UI threshold.
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        )
        | None => quota_attention,
    };
    ProviderView {
        kind,
        badge,
        windows,
        placeholder,
        attention,
    }
}

fn badge_usage_window(usage: &UsageData, strings: Strings) -> Option<WindowView> {
    headline_usage_window(usage).map(|window| window_view(window, strings))
}

/// One canonical primary value for the taskbar badge, tray icon number,
/// widget, and floating monitor. A warned window takes priority; otherwise the
/// five-hour window stays stable, falling back to the shortest known window.
pub(crate) fn headline_usage_window(usage: &UsageData) -> Option<&UsageWindow> {
    let worst = usage.windows.iter().reduce(|best, candidate| {
        let best_percent = display_percent(best.percentage);
        let candidate_percent = display_percent(candidate.percentage);
        if candidate_percent > best_percent
            || (candidate_percent == best_percent
                && duration_rank(candidate.duration_seconds) < duration_rank(best.duration_seconds))
        {
            candidate
        } else {
            best
        }
    });
    if let Some(warned) = worst.filter(|window| display_percent_warns(window.percentage)) {
        return Some(warned);
    }

    usage
        .windows
        .iter()
        .find(|window| {
            window
                .duration_seconds
                .is_some_and(|seconds| approximately(seconds, 5 * 60 * 60))
        })
        .or_else(|| {
            usage
                .windows
                .iter()
                .min_by_key(|window| duration_rank(window.duration_seconds))
        })
}

fn window_view(window: &UsageWindow, strings: Strings) -> WindowView {
    let percent = window.percentage.clamp(0.0, 100.0);
    let shown = display_percent(percent);
    let countdown = poller::format_countdown(window.resets_at);
    let countdown = if countdown.is_empty() {
        String::new()
    } else {
        format!("\u{00b7}{countdown}")
    };
    WindowView {
        label: compact_usage_window_label(window, strings),
        percent: Some(percent),
        display_percent: shown,
        percent_text: display_percent_text(percent),
        countdown,
        duration_seconds: window.duration_seconds,
        severity: if display_percent_warns(percent) {
            Severity::Warn
        } else {
            Severity::Normal
        },
    }
}

pub(crate) fn display_percent(percent: f64) -> i32 {
    if percent.is_finite() {
        percent.clamp(0.0, 100.0).round() as i32
    } else {
        0
    }
}

pub(crate) fn display_percent_text(percent: f64) -> String {
    format!("{}%", display_percent(percent))
}

pub(crate) fn display_percent_warns(percent: f64) -> bool {
    display_percent(percent) >= WARN_THRESHOLD_PERCENT
}

pub(crate) fn approximately(actual: u64, expected: u64) -> bool {
    actual >= expected.saturating_mul(95) / 100 && actual <= expected.saturating_mul(105) / 100
}

pub(crate) fn compact_usage_window_label(window: &UsageWindow, strings: Strings) -> String {
    if let Some(seconds) = window.duration_seconds.filter(|seconds| *seconds > 0) {
        if approximately(seconds, 5 * 60 * 60) {
            return "5h".to_string();
        }
        if approximately(seconds, 7 * 24 * 60 * 60) {
            return "7d".to_string();
        }
        if seconds % (24 * 60 * 60) == 0 {
            return format!("{}d", seconds / (24 * 60 * 60));
        }
        if seconds % (60 * 60) == 0 {
            return format!("{}h", seconds / (60 * 60));
        }
        if seconds % 60 == 0 {
            return format!("{}m", seconds / 60);
        }
        return format!("{seconds}s");
    }

    window
        .source_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(8).collect())
        .unwrap_or_else(|| strings.quota_window.to_string())
}

pub(crate) fn compact_usage_windows(usage: &UsageData) -> Vec<&UsageWindow> {
    let Some(headline) = headline_usage_window(usage) else {
        return Vec::new();
    };

    let mut selected = vec![headline];
    let secondary = usage
        .windows
        .iter()
        .filter(|window| !std::ptr::eq(*window, headline))
        .reduce(|best, candidate| {
            let best_percent = display_percent(best.percentage);
            let candidate_percent = display_percent(candidate.percentage);
            if candidate_percent > best_percent
                || (candidate_percent == best_percent
                    && duration_rank(candidate.duration_seconds)
                        < duration_rank(best.duration_seconds))
            {
                candidate
            } else {
                best
            }
        });
    if let Some(secondary) = secondary {
        selected.push(secondary);
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localization::LanguageId;
    use crate::models::{
        AppUsageData, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_WEEK_SECONDS,
    };
    use std::time::{Duration, SystemTime};

    const ORDER: [TrayIconKind; 3] = [
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ];

    fn usage(windows: Vec<UsageWindow>) -> UsageData {
        UsageData::from_windows(windows)
    }

    fn data_with_claude(claude: UsageData) -> AppUsageData {
        AppUsageData::default().with_usage(TrayIconKind::Claude, claude)
    }

    #[test]
    fn zero_percent_keeps_a_known_reset_countdown() {
        let strings = LanguageId::English.strings();
        let resets = SystemTime::now().checked_add(Duration::from_secs(4 * 86_400));
        let data = data_with_claude(usage(vec![
            UsageWindow::new(0.0, resets, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(92.0, resets, Some(ONE_WEEK_SECONDS)),
        ]));
        let vm = build(Some(&data), strings, &ORDER, true, false, false, false);
        assert_eq!(vm.providers[0].attention, Attention::Warn);
        assert_eq!(vm.providers[0].windows[0].severity, Severity::Warn);
        assert_eq!(vm.providers[0].windows[0].label, "7d");
        assert_eq!(
            vm.providers[0]
                .windows
                .iter()
                .find(|window| window.label == "5h")
                .unwrap()
                .countdown,
            "\u{00b7}4d"
        );
    }

    #[test]
    fn displayed_percentage_is_the_single_text_and_warning_boundary() {
        for (raw, shown, text, warns) in [
            (f64::NAN, 0, "0%", false),
            (-1.0, 0, "0%", false),
            (89.4, 89, "89%", false),
            (89.6, 90, "90%", true),
            (90.5, 91, "91%", true),
            (100.4, 100, "100%", true),
        ] {
            assert_eq!(display_percent(raw), shown);
            assert_eq!(display_percent_text(raw), text);
            assert_eq!(display_percent_warns(raw), warns);
        }
    }

    #[test]
    fn transient_provider_errors_stay_quiet_until_the_application_promotes_them() {
        let strings = LanguageId::English.strings();
        let cached = AppUsageData::default()
            .with_usage(
                TrayIconKind::Codex,
                usage(vec![UsageWindow::new(51.0, None, Some(ONE_WEEK_SECONDS))]),
            )
            .with_error(TrayIconKind::Codex, ProviderStatus::RateLimited);
        let vm = build(Some(&cached), strings, &ORDER, false, true, false, false);
        assert_eq!(vm.providers[0].attention, Attention::Normal);
        assert_eq!(vm.providers[0].windows[0].percent_text, "51%");

        let transient =
            AppUsageData::default().with_error(TrayIconKind::Codex, ProviderStatus::RequestFailed);
        let vm = build(Some(&transient), strings, &ORDER, false, true, false, false);
        assert_eq!(vm.providers[0].attention, Attention::Normal);
        assert_eq!(vm.providers[0].placeholder.as_deref(), Some("--"));

        let warned_transient = AppUsageData::default()
            .with_usage(
                TrayIconKind::Codex,
                usage(vec![UsageWindow::new(92.0, None, Some(ONE_WEEK_SECONDS))]),
            )
            .with_error(TrayIconKind::Codex, ProviderStatus::RequestFailed);
        let vm = build(
            Some(&warned_transient),
            strings,
            &ORDER,
            false,
            true,
            false,
            false,
        );
        assert_eq!(vm.providers[0].attention, Attention::Warn);

        let unavailable = AppUsageData::default()
            .with_error(TrayIconKind::Codex, ProviderStatus::AuthenticationFailed);
        let vm = build(
            Some(&unavailable),
            strings,
            &ORDER,
            false,
            true,
            false,
            false,
        );
        assert_eq!(vm.providers[0].attention, Attention::ActionRequired);
        assert_eq!(vm.providers[0].placeholder.as_deref(), Some("--"));
    }

    #[test]
    fn worst_window_ties_use_duration_not_input_order() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Claude,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(50.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(50.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(worst_window(&provider).unwrap().label, "5h");
    }

    #[test]
    fn badge_window_pins_five_hour_usage_until_another_window_warns() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Claude,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(85.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(badge_window(&provider).unwrap().label, "5h");

        let warned = ProviderView {
            windows: vec![
                window_view(
                    &UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
                    strings,
                ),
                window_view(
                    &UsageWindow::new(92.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
            ],
            attention: Attention::Warn,
            ..provider
        };
        let selected = badge_window(&warned).unwrap();
        assert_eq!(selected.label, "7d");
        assert_eq!(selected.display_percent, 92);
    }

    #[test]
    fn badge_window_uses_shortest_available_window_when_five_hour_is_absent() {
        let strings = LanguageId::English.strings();
        let provider = ProviderView {
            kind: TrayIconKind::Codex,
            badge: None,
            windows: vec![
                window_view(
                    &UsageWindow::new(20.0, None, Some(ONE_WEEK_SECONDS)),
                    strings,
                ),
                window_view(&UsageWindow::new(40.0, None, Some(24 * 60 * 60)), strings),
            ],
            placeholder: None,
            attention: Attention::Normal,
        };
        assert_eq!(badge_window(&provider).unwrap().label, "1d");
    }

    #[test]
    fn compact_windows_keep_the_badge_window_as_the_primary_row() {
        let strings = LanguageId::English.strings();
        let data = data_with_claude(usage(vec![
            UsageWindow::new(10.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(70.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(80.0, None, Some(ONE_WEEK_SECONDS)),
        ]));
        let vm = build(Some(&data), strings, &ORDER, true, false, false, false);
        let provider = &vm.providers[0];

        assert_eq!(provider.windows.len(), 2);
        assert_eq!(provider.windows[0].label, "5h");
        assert_eq!(provider.windows[1].label, "7d");
        assert_eq!(badge_window(provider).unwrap().label, "5h");
    }

    #[test]
    fn warned_headline_window_is_first_on_every_compact_surface() {
        let strings = LanguageId::English.strings();
        let data = data_with_claude(usage(vec![
            UsageWindow::new(10.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(70.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(92.0, None, Some(ONE_WEEK_SECONDS)),
        ]));
        let vm = build(Some(&data), strings, &ORDER, true, false, false, false);
        let provider = &vm.providers[0];

        assert_eq!(provider.windows.len(), 2);
        assert_eq!(provider.windows[0].label, "7d");
        assert_eq!(provider.windows[1].label, "1d");
        assert_eq!(badge_window(provider).unwrap().label, "7d");
    }

    #[test]
    fn labels_and_provider_order_are_language_independent() {
        let strings = LanguageId::Korean.strings();
        let data = AppUsageData::default()
            .with_usage(
                TrayIconKind::Claude,
                usage(vec![UsageWindow::new(10.0, None, Some(30 * 60))]),
            )
            .with_usage(
                TrayIconKind::Antigravity,
                usage(vec![UsageWindow::new(1.0, None, Some(365 * 24 * 60 * 60))]),
            );
        let vm = build(Some(&data), strings, &ORDER, true, false, true, false);
        assert_eq!(vm.providers[0].kind, TrayIconKind::Claude);
        assert_eq!(vm.providers[0].windows[0].label, "30m");
        assert_eq!(vm.providers[1].windows[0].label, "365d");
    }

    #[test]
    fn placeholder_model_respects_visibility() {
        let vm = placeholder_model("--", &ORDER, true, false, true, false);
        assert_eq!(vm.providers.len(), 2);
        assert!(vm
            .providers
            .iter()
            .all(|provider| provider.placeholder.as_deref() == Some("--")));
    }

    #[test]
    fn provider_reorder_preserves_cached_payload_and_attention() {
        let mut vm = placeholder_model("--", &ORDER, true, true, true, false);
        let codex = vm
            .providers
            .iter_mut()
            .find(|provider| provider.kind == TrayIconKind::Codex)
            .unwrap();
        codex.placeholder = Some("cached Codex".to_string());
        codex.attention = Attention::ActionRequired;

        reorder_providers(
            &mut vm,
            &[
                TrayIconKind::Antigravity,
                TrayIconKind::Codex,
                TrayIconKind::Claude,
            ],
        );

        assert_eq!(
            vm.providers
                .iter()
                .map(|provider| provider.kind)
                .collect::<Vec<_>>(),
            vec![
                TrayIconKind::Antigravity,
                TrayIconKind::Codex,
                TrayIconKind::Claude,
            ]
        );
        assert_eq!(vm.providers[1].placeholder.as_deref(), Some("cached Codex"));
        assert_eq!(vm.providers[1].attention, Attention::ActionRequired);
    }
}
