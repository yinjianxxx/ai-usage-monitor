use std::cell::Cell;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::{
    CompareStringOrdinal, GetDateFormatEx, CSTR_EQUAL, DATE_SHORTDATE,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::*;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, ODS_FOCUS, ODS_HOTLIGHT, ODS_NOFOCUSRECT, ODS_SELECTED, TASKDIALOGCONFIG,
    TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS, TASKDIALOG_FLAGS, TASKDIALOG_NOTIFICATIONS,
    TDCBF_NO_BUTTON, TDCBF_YES_BUTTON, TDF_ALLOW_DIALOG_CANCELLATION, TDF_CAN_BE_MINIMIZED,
    TDF_USE_COMMAND_LINKS, TDN_CREATED,
};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_NEXT, VK_PRIOR, VK_UP,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ExtractIconExW, RemoveWindowSubclass, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::compact_layout::{self, BadgeHit, ColorKey, DrawCmd, FontKey, Metrics, Scene, TileSize};
use crate::compact_view::{self, CompactViewModel};
use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{
    AppUsageData, ProviderStatus, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_WEEK_SECONDS,
};
use crate::native_interop::{
    self, Color, TIMER_AUTH_WATCH, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL,
    TIMER_UPDATE_CHECK, WM_APP_TRAY, WM_APP_USAGE_UPDATED,
};
use crate::placement::{self, PlacementRect};
use crate::poller;
use crate::provider_tile;
use crate::settings::{
    self, default_provider_order, FloatingDefaultPosition, FloatingPlacement, MonitorKey,
    SettingsFile, WidgetDefaultPosition, WidgetPlacement, PLACEMENT_SCHEMA_VERSION, POLL_10_MIN,
    POLL_15_MIN, POLL_1_MIN, POLL_2_MIN, POLL_30_MIN, POLL_5_MIN,
};
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
#[derive(Clone, Debug, Default)]
struct WidgetUsageWindow {
    percent: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct ProviderWidgetData {
    windows: Vec<WidgetUsageWindow>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProviderRefreshState {
    consecutive_failures: u8,
    unavailable_since_unix: Option<u64>,
    rate_limit_until: Option<Instant>,
    auth_failure_active: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProviderRefreshStates {
    claude_code: ProviderRefreshState,
    codex: ProviderRefreshState,
    antigravity: ProviderRefreshState,
    grok: ProviderRefreshState,
}

impl ProviderRefreshStates {
    fn for_kind(self, kind: tray_icon::TrayIconKind) -> ProviderRefreshState {
        match kind {
            tray_icon::TrayIconKind::Claude => self.claude_code,
            tray_icon::TrayIconKind::Codex => self.codex,
            tray_icon::TrayIconKind::Antigravity => self.antigravity,
            tray_icon::TrayIconKind::Grok => self.grok,
        }
    }

    fn state_mut(&mut self, kind: tray_icon::TrayIconKind) -> &mut ProviderRefreshState {
        match kind {
            tray_icon::TrayIconKind::Claude => &mut self.claude_code,
            tray_icon::TrayIconKind::Codex => &mut self.codex,
            tray_icon::TrayIconKind::Antigravity => &mut self.antigravity,
            tray_icon::TrayIconKind::Grok => &mut self.grok,
        }
    }

    fn reset_hidden(
        &mut self,
        show_claude_code: bool,
        show_codex: bool,
        show_antigravity: bool,
        show_grok: bool,
    ) {
        if !show_claude_code {
            self.claude_code = ProviderRefreshState::default();
        }
        if !show_codex {
            self.codex = ProviderRefreshState::default();
        }
        if !show_antigravity {
            self.antigravity = ProviderRefreshState::default();
        }
        if !show_grok {
            self.grok = ProviderRefreshState::default();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorIdentity {
    device: String,
    device_path: Option<String>,
    is_primary: bool,
}

impl MonitorIdentity {
    fn key(&self) -> MonitorKey {
        MonitorKey {
            device_path: self.device_path.clone(),
            gdi_device_name: self.device.clone(),
        }
    }

    fn matches_key(&self, key: &MonitorKey) -> bool {
        key.matches(self.device_path.as_deref(), &self.device)
    }

    fn matches(&self, other: &Self) -> bool {
        match (self.device_path.as_deref(), other.device_path.as_deref()) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => self.device.eq_ignore_ascii_case(&other.device),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SurfaceTopologyReset {
    taskbar: bool,
    details: bool,
}

struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    is_high_contrast: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,

    claude_widget: ProviderWidgetData,
    codex_widget: ProviderWidgetData,
    antigravity_widget: ProviderWidgetData,
    grok_widget: ProviderWidgetData,
    compact_vm: CompactViewModel,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    allow_claude_credentials: bool,
    allow_codex_credentials: bool,
    allow_antigravity_credentials: bool,
    allow_grok_credentials: bool,
    /// Answer to the one-time, all-provider access prompt. Every credential
    /// read is gated on this in addition to the per-provider switch above.
    credential_consent_granted: bool,
    credential_consent_decided: bool,
    /// Whether the user has already been told this provider exists.
    claude_credential_access_decided: bool,
    codex_credential_access_decided: bool,
    antigravity_credential_access_decided: bool,
    grok_credential_access_decided: bool,
    /// Whether the user revoked this provider from the Provider access menu.
    /// Detection skips a revoked provider; the settings field of the same name
    /// explains why the `allow_*` switch alone cannot answer this.
    claude_credential_access_revoked: bool,
    codex_credential_access_revoked: bool,
    antigravity_credential_access_revoked: bool,
    grok_credential_access_revoked: bool,
    claude_credential_access_pending: bool,
    codex_credential_access_pending: bool,
    antigravity_credential_access_pending: bool,
    grok_credential_access_pending: bool,
    provider_order: Vec<tray_icon::TrayIconKind>,
    pending_provider_order: Option<Vec<tray_icon::TrayIconKind>>,
    pending_provider_order_samples: u8,
    last_observed_tray_order: Option<Vec<tray_icon::TrayIconKind>>,

    data: Option<AppUsageData>,
    /// True while `data` came from the persisted snapshot of a previous run
    /// (shown immediately at startup); cleared by the first successful poll.
    data_is_cached: bool,
    /// A user-requested poll is running. Existing values stay visible while
    /// the detail footer supplies the progress feedback.
    manual_refresh_in_progress: bool,
    /// The error of the last completely failed poll (every enabled provider
    /// failed), for the detail popup's per-provider status badges.
    last_error: Option<poller::PollError>,
    provider_refresh_states: ProviderRefreshStates,

    poll_interval_ms: u32,
    /// Actual deadline of the currently armed provider poll timer. This can
    /// differ from the configured interval because of backoff, rate limits,
    /// Claude cooldown alignment, or paused-auth recovery.
    next_poll_deadline: Option<Instant>,
    retry_count: u32,
    auth_error_paused_polling: bool,
    /// When a remotely rejected credential should be tried against the
    /// service again even if its local token has not changed. Local-only
    /// credential failures leave this unset and recover through the watch.
    auth_recovery_recheck_deadline: Option<Instant>,
    /// Watch the credentials for a re-login. Set both while polling is
    /// paused (every provider failed) and while a single provider needs
    /// auth but others still poll fine.
    auth_watch_active: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,
    last_success_unix: Option<u64>,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    update_status: UpdateStatus,
    /// Survives restarts so the version menu entry can state what the last
    /// check found instead of resetting to a generic "check for updates".
    last_update_outcome: Option<settings::LastUpdateOutcome>,
    last_update_check_unix: Option<u64>,
    details_hwnd: Option<HWND>,
    details_monitor: Option<MonitorIdentity>,
    floating_hwnd: Option<HWND>,
    floating_monitor: Option<MonitorIdentity>,
    floating_visible: bool,
    detailed_tray_icons: bool,
    detail_pinned: bool,
    floating_x: Option<i32>,
    floating_y: Option<i32>,
    floating_default_position: FloatingDefaultPosition,
    floating_placement: FloatingPlacement,
    floating_placement_needs_migration: bool,
    widget_tooltip_hwnd: Option<SendHwnd>,

    taskbar_index: usize,
    taskbar_monitor: Option<MonitorIdentity>,
    tray_offset: i32,
    preferred_taskbar_index: usize,
    widget_default_position: WidgetDefaultPosition,
    widget_placement: WidgetPlacement,
    widget_placement_needs_migration: bool,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_client_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Prompting,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
    /// Restored from the previous run, so only the version is known.
    ///
    /// Enough to label the menu entry, not enough to install: the release's
    /// download URL was not kept because a stored one goes stale. Acting on
    /// this runs a fresh check, which then offers the update as usual.
    AvailableRemembered {
        version: String,
    },
}

/// Rebuild the display state from what the previous run's last check found.
///
/// Without this the menu entry falls back to "check for updates" on every
/// launch, as though the app had never looked - even though it checked
/// yesterday and the answer has not been forgotten, only left in memory.
fn remembered_update_status(outcome: Option<&settings::LastUpdateOutcome>) -> UpdateStatus {
    match outcome {
        Some(settings::LastUpdateOutcome::UpToDate) => UpdateStatus::UpToDate,
        Some(settings::LastUpdateOutcome::Available { version }) => {
            UpdateStatus::AvailableRemembered {
                version: version.clone(),
            }
        }
        None => UpdateStatus::Idle,
    }
}

fn update_status_is_busy(status: &UpdateStatus) -> bool {
    matches!(
        status,
        UpdateStatus::Checking | UpdateStatus::Prompting | UpdateStatus::Applying
    )
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const RATE_LIMIT_MIN_RETRY_MS: u32 = POLL_5_MIN;
const RATE_LIMIT_MAX_RETRY_MS: u32 = 60 * 60 * 1_000;
const COMPACT_STALE_MIN_AGE_SECS: u64 = 5 * 60;
const COMPACT_REQUEST_FAILURE_THRESHOLD: u8 = 3;

const IDM_REFRESH_NOW: u16 = 1;
// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_2MIN: u16 = 14;
const IDM_FREQ_10MIN: u16 = 15;
const IDM_FREQ_30MIN: u16 = 16;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_WIDGET_PRIMARY_LEFT: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_TOGGLE_FLOATING: u16 = 32;
const IDM_WIDGET_PRIMARY_RIGHT: u16 = 33;
const IDM_FLOATING_DEFAULT_BOTTOM_LEFT: u16 = 34;
const IDM_DETAILED_TRAY_ICONS: u16 = 35;
const IDM_FLOATING_DEFAULT_BOTTOM_RIGHT: u16 = 36;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_LANG_RUSSIAN: u16 = 49;
const IDM_LANG_PORTUGUESE_BRAZIL: u16 = 50;
const IDM_LANG_SIMPLIFIED_CHINESE: u16 = 51;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_MODEL_ANTIGRAVITY: u16 = 62;
const IDM_ACCESS_CLAUDE_CODE: u16 = 63;
const IDM_ACCESS_CODEX: u16 = 64;
const IDM_ACCESS_ANTIGRAVITY: u16 = 65;
const IDM_REDETECT_PROVIDERS: u16 = 66;
const IDM_MODEL_GROK: u16 = 67;
const IDM_ACCESS_GROK: u16 = 68;
const IDM_NOTIFY_SESSION_RESET: u16 = 80;
const IDM_NOTIFY_WEEKLY_RESET: u16 = 81;

const WM_DPICHANGED_MSG: u32 = 0x02E0;
/// WM_MOUSELEAVE (winuser.h), kept local to avoid pulling a control-specific
/// constant into the custom tooltip implementation.
const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
const WIDGET_TOOLTIP_DELAY_MS: u32 = 650;
const WIDGET_TOOLTIP_MIN_WIDTH: i32 = 180;
const WIDGET_TOOLTIP_MAX_WIDTH: i32 = 320;
const WIDGET_TOOLTIP_EDGE_GAP: i32 = 7;
const TIMER_WIDGET_TOOLTIP: usize = 14;
/// Timer on the broadcast helper window that coalesces setting/display
/// broadcast bursts into one refresh (trailing-edge debounce).
const TIMER_BROADCAST_DEBOUNCE: usize = 10;
const BROADCAST_DEBOUNCE_MS: u32 = 250;
/// How often to re-read the credentials while polling is paused after an auth
/// failure. This only reads local credentials (no usage requests), so it can
/// be far shorter than the poll interval - which is what makes the widget
/// recover within seconds of signing back in.
const AUTH_WATCH_INTERVAL_MS: u32 = 15_000;
const TIMER_TRAY_ORDER: usize = 11;
const TRAY_ORDER_SAMPLE_MS: u32 = 1_000;
const TIMER_TRAY_ORDER_CONFIRM: usize = 13;
const TRAY_ORDER_CONFIRM_MS: u32 = 120;
const TRAY_ORDER_STABLE_SAMPLES: u8 = 2;
const TRAY_ORDER_EVENT_THROTTLE_MS: u128 = 100;
/// The detail popup owns this timer. It only refreshes locally formatted
/// countdown text; provider requests continue to follow the configured poll
/// interval on the main window.
const TIMER_DETAIL_REFRESH: usize = 12;
const DETAIL_REFRESH_MS: u32 = 1_000;
/// Looks for providers that appeared since the last check.
///
/// Deliberately its own timer rather than a step inside the poll pass: with
/// no provider yet allowed there is no poll worker to piggyback on, so
/// detection would never run for exactly the users who need it most. The
/// interval is long because it can shell out to `wsl.exe`, and a newly
/// installed provider is not urgent.
const TIMER_PROVIDER_DETECT: usize = 15;

/// Arm a window timer, recording a failure instead of discarding it.
///
/// `SetTimer` reports failure by returning 0. Most call sites dropped that,
/// which makes a timer that never armed invisible: the feature it drives
/// simply never happens, with nothing in the log to say so. That is the same
/// shape as the provider sweep whose `WM_TIMER` went to a window procedure
/// with no branch for it, unnoticed across three releases.
unsafe fn arm_timer(hwnd: HWND, id: usize, interval_ms: u32, label: &str) -> bool {
    let armed = unsafe { SetTimer(hwnd, id, interval_ms, None) } != 0;
    if !armed {
        diagnose::log(format!(
            "failed to arm {label} timer (id={id}, interval={interval_ms}ms)"
        ));
    }
    armed
}
const PROVIDER_DETECT_INTERVAL_MS: u32 = 30 * 60 * 1_000;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
/// Thread message (msg.hwnd == null) handled directly in the message loop:
/// recreate/re-attach the widget window after it was destroyed externally.
const WM_APP_REVIVE: u32 = WM_APP + 4;
/// Thread message posted by the revival background thread once the taskbar
/// set is stable and the UI thread should recreate/re-attach the widget.
const WM_APP_REVIVE_READY: u32 = WM_APP + 5;
/// Stable process-level request for the UI thread to perform a deliberate
/// shutdown, even if the embedded main window was replaced during revival.
const WM_APP_REQUEST_QUIT: u32 = WM_APP + 6;
const WM_APP_PERSISTENCE_WARNING: u32 = WM_APP + 7;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

/// WM_WTSSESSION_CHANGE and the wparam values we care about (winuser.h).
const WM_WTSSESSION_CHANGE_MSG: u32 = 0x02B1;
const WTS_CONSOLE_CONNECT: usize = 1;
const WTS_CONSOLE_DISCONNECT: usize = 2;
const WTS_REMOTE_CONNECT: usize = 3;
const WTS_REMOTE_DISCONNECT: usize = 4;
const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

/// How often the watchdog thread polls for an explorer.exe restart (which
/// recreates the taskbar and wipes our tray-icon registration).
const TASKBAR_WATCH_INTERVAL_SECS: u64 = 2;

/// Revival tuning: how often/patiently to retry widget-window creation before
/// giving up. Taskbar availability itself is retried by shell events and the
/// watchdog without ever exposing the widget as a desktop popup.
const REVIVE_CREATE_ATTEMPTS: u32 = 12;
const REVIVE_CREATE_RETRY_DELAY: Duration = Duration::from_secs(5);
static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Set when the user picks Exit: WM_DESTROY then means a deliberate quit,
/// anything else means explorer destroyed our embedded window and we revive.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static PERSISTENCE_WARNING_SHOWN: AtomicBool = AtomicBool::new(false);

/// Guards against overlapping credential watch probes: the watch timer keeps
/// firing while a slow `wsl.exe` read is still in flight.
static CREDENTIAL_WATCH_BUSY: AtomicBool = AtomicBool::new(false);

/// Set while a revival is running so the watchdog does not interfere.
static REVIVING: AtomicBool = AtomicBool::new(false);

/// Failed window-creation attempts of the current revival; reset when a
/// revival starts or completes. Once it reaches REVIVE_CREATE_ATTEMPTS the
/// revival gives up and falls back to a process relaunch.
static REVIVE_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Unix time when the in-flight revival began; 0 when none. The watchdog
/// uses it as a backstop: if a revival's READY signal is ever lost (see
/// post_revive_ready), REVIVING would otherwise stay true forever and
/// permanently disable revival detection.
static REVIVING_SINCE: AtomicU64 = AtomicU64::new(0);

/// A revival older than this is considered stuck and its in-flight flag is
/// force-cleared so detection re-arms. Legitimate revivals stay well under:
/// the stability wait caps at 120s and 12 create retries add ~60s.
const REVIVE_STUCK_RESET_SECS: u64 = 600;

/// The broadcast helper window handle once created (0 = none). Revival
/// signals are posted here rather than as thread messages: modal message
/// loops (context menu, message boxes) pump-and-discard NULL-hwnd thread
/// messages, while window messages are dispatched correctly.
static BROADCAST_HELPER_HWND: AtomicIsize = AtomicIsize::new(0);

/// Registered shell message sent when Explorer recreates the taskbar.
/// Kept on the process-level helper so recovery still works after the
/// embedded widget window has been destroyed with its old parent.
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

/// The hidden process-level helper receives WTS notifications even when
/// Explorer destroys the embedded widget, so this can remain set for the full
/// lock/disconnect interval without stopping provider polling.
static SESSION_INACTIVE: AtomicBool = AtomicBool::new(false);

/// Serializes writes to one persisted snapshot and rejects an older snapshot
/// if a newer state revision was captured before it reached the writer.
struct PersistenceCoordinator {
    latest_revision: AtomicU64,
    writer: Mutex<()>,
}

impl PersistenceCoordinator {
    const fn new() -> Self {
        Self {
            latest_revision: AtomicU64::new(0),
            writer: Mutex::new(()),
        }
    }

    /// Call only while holding `STATE`, at the same time the value to persist
    /// is cloned. That makes revision order identical to AppState order.
    fn next_revision(&self) -> u64 {
        self.latest_revision.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn write_if_latest<E>(
        &self,
        revision: u64,
        write: impl FnOnce() -> Result<(), E>,
    ) -> Result<bool, E> {
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.latest_revision.load(Ordering::Acquire) != revision {
            return Ok(false);
        }
        write()?;
        Ok(true)
    }
}

static SETTINGS_PERSISTENCE: PersistenceCoordinator = PersistenceCoordinator::new();
static USAGE_CACHE_PERSISTENCE: PersistenceCoordinator = PersistenceCoordinator::new();

struct PollCoordinator {
    in_flight: AtomicBool,
    pending: AtomicBool,
    force_claude_refresh: AtomicBool,
    generation: AtomicU64,
}

impl PollCoordinator {
    const fn new() -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            force_claude_refresh: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    /// Register a refresh request. The caller that changes `in_flight` from
    /// false to true owns starting the single worker; every other caller is
    /// collapsed into the worker's one pending follow-up pass.
    fn request(&self, force_claude_refresh: bool) -> bool {
        if force_claude_refresh {
            self.force_claude_refresh.store(true, Ordering::Release);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending.store(true, Ordering::Release);
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn begin_pass(&self) -> (u64, bool) {
        self.pending.store(false, Ordering::Release);
        (
            self.generation.load(Ordering::Acquire),
            self.force_claude_refresh.swap(false, Ordering::AcqRel),
        )
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    fn invalidate_pending(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.pending.store(false, Ordering::Release);
        self.force_claude_refresh.store(false, Ordering::Release);
    }

    /// Release ownership after a pass unwound.
    ///
    /// The coalesced follow-up is dropped on purpose: re-running the pass that
    /// just panicked would panic again. Clearing `in_flight` last means a
    /// request racing this call can lose its refresh, which the periodic poll
    /// timer makes good on the next tick - the alternative, leaving
    /// `in_flight` set, loses every refresh for the life of the process.
    fn abandon_pass(&self) {
        self.pending.store(false, Ordering::Release);
        self.force_claude_refresh.store(false, Ordering::Release);
        self.in_flight.store(false, Ordering::Release);
    }

    /// Return true when this worker should immediately perform the one
    /// coalesced follow-up pass. The second check closes the race where a
    /// request arrives between the first pending check and releasing ownership.
    fn finish_pass(&self) -> bool {
        if self.pending.load(Ordering::Acquire) {
            return true;
        }

        self.in_flight.store(false, Ordering::Release);
        if !self.pending.load(Ordering::Acquire) {
            return false;
        }

        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

static POLL_COORDINATOR: PollCoordinator = PollCoordinator::new();

/// Releases the poll coordinator if the pass it covers unwinds.
///
/// `in_flight` is what keeps a second worker from starting, so a pass that
/// unwound past `finish_pass` left it set for the life of the process: every
/// later request was collapsed into a pending pass that nobody ran, and usage
/// froze while the app itself kept running. `Cargo.toml` deliberately keeps
/// panics unwinding rather than aborting, so the thread really does come apart
/// here.
struct PollPassGuard<'a> {
    coordinator: Option<&'a PollCoordinator>,
}

impl<'a> PollPassGuard<'a> {
    fn arm(coordinator: &'a PollCoordinator) -> Self {
        Self {
            coordinator: Some(coordinator),
        }
    }

    /// The pass returned normally; leave ownership to `finish_pass`.
    fn finished(mut self) {
        self.coordinator = None;
    }
}

impl Drop for PollPassGuard<'_> {
    fn drop(&mut self) {
        if let Some(coordinator) = self.coordinator {
            diagnose::log(
                "poll pass panicked; releasing the coordinator so a later request can start a new worker",
            );
            coordinator.abandon_pass();
        }
    }
}

fn watchdog_needs_taskbar_recovery(
    widget_exists: bool,
    binding_valid: bool,
    taskbar_available: bool,
) -> bool {
    taskbar_available && (!widget_exists || !binding_valid)
}

/// UI thread id, so the watchdog can reach the message loop once the window
/// (the usual PostMessage target) no longer exists.
static UI_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// The Win32 window class; also part of the app's identity (kept distinct
/// from the original CodeZeno app so both can run side by side).
const WINDOW_CLASS_NAME: &str = "Gengchou";
const DETAIL_WINDOW_CLASS_NAME: &str = "GengchouDetails";
const FLOATING_WINDOW_CLASS_NAME: &str = "GengchouFloating";
const WIDGET_TOOLTIP_WINDOW_CLASS_NAME: &str = "GengchouWidgetTooltip";
/// Hidden top-level helper window. Two jobs the embedded widget cannot do
/// itself: receive broadcast messages (WM_SETTINGCHANGE / WM_DISPLAYCHANGE
/// are only sent to top-level windows, and the widget is a WS_CHILD of the
/// taskbar in its normal mode - without this a dark/light theme switch was
/// not reflected until the next poll), and be findable by class name so a
/// second launched instance can ask us to show the detail popup.
const BROADCAST_WINDOW_CLASS_NAME: &str = "GengchouBroadcast";
const CURRENT_MUTEX_NAME: &str = "Global\\Gengchou";
const DETAIL_POPUP_WIDTH: i32 = 408;
/// Title area above the first provider group.
const DETAIL_HEADER_H: i32 = 52;
/// Provider identity line: icon chip + name + compact state label.
const DETAIL_GROUP_HEADER_H: i32 = 44;
/// One quota window: label/bar/percent line plus the reset line below it.
const DETAIL_WINDOW_ROW_H: i32 = 48;
const DETAIL_PRIMARY_LINE_H: i32 = 18;
const DETAIL_GROUP_GAP: i32 = 10;
const DETAIL_CARD_MARGIN: i32 = 18;
const DETAIL_GROUP_PAD_V: i32 = 6;
const DETAIL_HINT_H: i32 = 42;
const DETAIL_LOGO_CHIP_SIZE: i32 = 28;
const DETAIL_BAR_GAP: i32 = 3;
const DETAIL_CONTENT_BOTTOM_PAD: i32 = 12;
const DETAIL_FOOTER_H: i32 = 42;
const DETAIL_HEADER_BUTTON_SIZE: i32 = 32;
const DETAIL_HEADER_BUTTON_GAP: i32 = 4;
/// Refresh is an app action; the remaining three buttons are window controls.
/// A wider gap makes that grouping visible without adding another divider.
const DETAIL_HEADER_REFRESH_GROUP_GAP: i32 = 12;
const DETAIL_HEADER_BUTTON_TOP: i32 = 10;
const DETAIL_BRAND_ICON_SIZE: i32 = 20;
const DETAIL_BRAND_ICON_TEXT_GAP: i32 = 8;
/// Keep the popup inside the target monitor's work area. The margin is also
/// the minimum breathing room between the DWM shadow and the screen edge.
const DETAIL_WORK_AREA_MARGIN: i32 = 8;
/// One wheel notch advances one quota row. Keyboard arrows use a smaller
/// text-line step while Page Up/Down advance one visible viewport.
const DETAIL_SCROLL_ROW_STEP: i32 = 48;
const DETAIL_SCROLL_LINE_STEP: i32 = 16;
const DETAIL_SCROLL_GUTTER_W: i32 = 12;
const DETAIL_SCROLL_THUMB_W: i32 = 3;
const DETAIL_SCROLL_THUMB_MIN_H: i32 = 28;
const IDC_DETAIL_PIN: u16 = 1_101;
const IDC_DETAIL_MOVE: u16 = 1_102;
const IDC_DETAIL_REFRESH: u16 = 1_103;
const IDC_DETAIL_CLOSE: u16 = 1_104;
/// Visual and keyboard order, from left to right. Refresh is deliberately
/// first because it is the most frequently used command; Close stays last.
const DETAIL_HEADER_BUTTON_IDS: [u16; 4] = [
    IDC_DETAIL_REFRESH,
    IDC_DETAIL_PIN,
    IDC_DETAIL_MOVE,
    IDC_DETAIL_CLOSE,
];
/// Content height when no provider rows exist yet (waiting message).
const DETAIL_EMPTY_H: i32 = 40;
/// A popup dismissed this recently is treated as "the user clicked the tray
/// icon to close it": the click that caused the focus loss also arrives as an
/// open request, and re-opening would make the popup flicker instead of
/// toggling.
const DETAIL_REOPEN_SUPPRESS_MS: u128 = 300;
// Keep enough room for the DWM shadow without making the surface look detached
// from the display edge. This is scaled with the monitor DPI.
const FLOATING_DRAG_THRESHOLD: i32 = 3;
const FLOATING_CONTENT_LEFT_MARGIN: i32 = 8;
static FLOATING_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
static FLOATING_MOVING: AtomicBool = AtomicBool::new(false);

struct FloatingDragState {
    tracking: bool,
    moved: bool,
    start_cursor_x: i32,
    start_cursor_y: i32,
    start_window_x: i32,
    start_window_y: i32,
}

static FLOATING_DRAG_STATE: Mutex<FloatingDragState> = Mutex::new(FloatingDragState {
    tracking: false,
    moved: false,
    start_cursor_x: 0,
    start_cursor_y: 0,
    start_window_x: 0,
    start_window_y: 0,
});

fn session_is_unstable() -> bool {
    SESSION_INACTIVE.load(Ordering::Acquire)
}

thread_local! {
    /// DPI for the window currently being laid out or painted on the UI
    /// thread. Every HWND entry point installs its own value, so one window
    /// moving between monitors cannot change another window's scale.
    static ACTIVE_WINDOW_DPI: Cell<u32> = const { Cell::new(96) };
}

fn normalize_dpi(dpi: u32) -> u32 {
    if dpi == 0 {
        96
    } else {
        dpi
    }
}

fn scale_px_for_dpi(px: i32, dpi: u32) -> i32 {
    let dpi = normalize_dpi(dpi);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Scale a base pixel value (designed at 96 DPI) for the active HWND.
fn sc(px: i32) -> i32 {
    ACTIVE_WINDOW_DPI.with(|dpi| scale_px_for_dpi(px, dpi.get()))
}

fn active_window_dpi() -> u32 {
    ACTIVE_WINDOW_DPI.with(|dpi| normalize_dpi(dpi.get()))
}

struct DpiScope {
    previous: u32,
}

impl DpiScope {
    fn new(dpi: u32) -> Self {
        let dpi = normalize_dpi(dpi);
        let previous = ACTIVE_WINDOW_DPI.with(|active| {
            let previous = active.get();
            active.set(dpi);
            previous
        });
        Self { previous }
    }

    fn for_window(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        Self::new(dpi)
    }
}

impl Drop for DpiScope {
    fn drop(&mut self) {
        ACTIVE_WINDOW_DPI.with(|active| active.set(self.previous));
    }
}

fn set_default_dpi(dpi: u32) {
    ACTIVE_WINDOW_DPI.with(|active| active.set(normalize_dpi(dpi)));
}

fn dpi_from_wparam(wparam: WPARAM) -> u32 {
    normalize_dpi((wparam.0 & 0xFFFF) as u32)
}

fn suggested_dpi_rect(lparam: LPARAM) -> Option<RECT> {
    if lparam.0 == 0 {
        return None;
    }
    Some(unsafe { *(lparam.0 as *const RECT) })
}

unsafe fn apply_suggested_dpi_rect(hwnd: HWND, lparam: LPARAM, context: &str) {
    let Some(rect) = suggested_dpi_rect(lparam) else {
        diagnose::log(format!(
            "{context}: WM_DPICHANGED had no suggested rectangle"
        ));
        return;
    };
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        diagnose::log(format!(
            "{context}: ignored invalid DPI rectangle ({}, {}, {}, {})",
            rect.left, rect.top, rect.right, rect.bottom
        ));
        return;
    }
    if let Err(error) = SetWindowPos(
        hwnd,
        HWND::default(),
        rect.left,
        rect.top,
        width,
        height,
        SWP_NOACTIVATE | SWP_NOZORDER,
    ) {
        diagnose::log_error(&format!("{context}: unable to apply DPI rectangle"), error);
    }
}

/// Spacing below which two relaunches are treated as a storm (e.g. explorer.exe
/// crash-looping); when detected we back off instead of spawning in a tight loop.
const RELAUNCH_THROTTLE_SECS: u64 = 10;
const RELAUNCH_BACKOFF_SECS: u64 = 30;
/// Environment flag set on a relaunched child so it waits for the previous
/// instance's single-instance mutex instead of exiting immediately.
const ENV_RELAUNCH: &str = "GENGCHOU_RELAUNCH";
/// Unix timestamp (seconds) of the relaunch that spawned this process, passed to
/// the child so it can detect a relaunch storm.
const ENV_LAST_RELAUNCH_UNIX: &str = "GENGCHOU_LAST_RELAUNCH_UNIX";

/// Relaunch the widget as a fresh process. Last-resort recovery only: normal
/// recovery from explorer restarts and RDP session switches happens in-process
/// via `revive_after_destroy` (which keeps state and needs no process handoff).
/// This path remains for when the UI thread is unreachable or window creation
/// keeps failing. The child is flagged via `ENV_RELAUNCH` so it waits for this
/// instance's single-instance mutex to be released before taking over (see the
/// guard in `run`).
/// The command a watchdog relaunch runs.
///
/// Split out so a test can assert what it does to the environment: this is the
/// only child that inherits the whole environment, so it is where an update
/// transaction's readiness marker has to be cleared. A relaunched process that
/// inherited it would take a finished transaction for an active one and refuse
/// to start.
fn relaunch_command(exe: &std::path::Path, args: &[String], now: u64) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .env(ENV_RELAUNCH, "1")
        .env(ENV_LAST_RELAUNCH_UNIX, now.to_string())
        .env_remove(updater::UPDATE_READY_ENV);
    command
}

fn relaunch_self() {
    // Back off if we are relaunching very soon after the relaunch that spawned
    // us: that signals the shell is crash-looping, not a one-off restart.
    let now = now_unix_secs();
    let last = std::env::var(ENV_LAST_RELAUNCH_UNIX)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if last != 0 && now.saturating_sub(last) < RELAUNCH_THROTTLE_SECS {
        diagnose::log("relaunch storm detected; backing off before relaunching");
        std::thread::sleep(Duration::from_secs(RELAUNCH_BACKOFF_SECS));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            diagnose::log_error("watchdog: unable to resolve current executable", error);
            return;
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match relaunch_command(&exe, &args, now).spawn() {
        Ok(_) => {
            diagnose::log("watchdog: relaunched fresh instance, exiting old one");
            std::process::exit(0);
        }
        Err(error) => {
            diagnose::log_error("watchdog: unable to spawn relaunched instance", error);
        }
    }
}

/// Detect taskbar changes the message-based paths might miss and trigger
/// recovery. The primary recovery is in-process revival on the UI thread
/// (WM_APP_REVIVE: the message loop outlives the window); this thread is the
/// safety net that notices a changed taskbar while the app is idle, and falls
/// back to a full process relaunch only if the UI thread cannot be reached.
fn spawn_taskbar_watchdog() {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
        // Hold off while a revival is already running or the session is in
        // the middle of an RDP switch / lock screen.
        if REVIVING.load(Ordering::SeqCst) || session_is_unstable() {
            // Backstop: if a revival's READY signal was lost (message loop
            // torn down at the wrong moment, or any other one-off), REVIVING
            // would pin true and permanently disable this watchdog. After a
            // generous timeout, re-arm detection.
            let since = REVIVING_SINCE.load(Ordering::SeqCst);
            if REVIVING.load(Ordering::SeqCst)
                && since != 0
                && now_unix_secs().saturating_sub(since) > REVIVE_STUCK_RESET_SECS
            {
                diagnose::log("watchdog: revival stuck past its deadline; re-arming detection");
                clear_reviving();
            }
            continue;
        }
        let stored = {
            let state = lock_state();
            state.as_ref().map(|s| (s.hwnd.to_hwnd(), s.taskbar_hwnd))
        };
        let Some((hwnd, old)) = stored else {
            continue;
        };
        let taskbars = native_interop::find_taskbars();
        let widget_exists = unsafe { IsWindow(hwnd).as_bool() };
        let binding_valid = widget_exists
            && old.is_some_and(|taskbar| native_interop::is_embedded_in_taskbar(hwnd, taskbar));
        if watchdog_needs_taskbar_recovery(widget_exists, binding_valid, !taskbars.is_empty()) {
            let widget_missing = !widget_exists;
            if widget_missing {
                diagnose::log(format!(
                    "watchdog: widget hwnd missing hwnd={:?} -> requesting revival",
                    hwnd
                ));
            }
            if let Some(taskbar) = taskbars.first() {
                if let Some(old) = old {
                    diagnose::log(format!(
                        "watchdog: taskbar changed old={:?} new={:?} -> requesting revival",
                        old.0, taskbar.hwnd.0
                    ));
                } else {
                    diagnose::log(format!(
                        "watchdog: taskbar returned while widget hidden new={:?} -> requesting revival",
                        taskbar.hwnd.0
                    ));
                }
            }
            // Ask the UI thread to revive in-process (it also covers the case
            // where the window survived and only needs re-attaching). Only if
            // the message cannot be delivered fall back to a full relaunch.
            let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
            let posted = thread_id != 0
                && unsafe {
                    PostThreadMessageW(thread_id, WM_APP_REVIVE, WPARAM(0), LPARAM(0)).is_ok()
                };
            if posted {
                // Give the UI thread one watchdog period to run the immediate
                // in-process re-attachment before re-checking.
                std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
            } else {
                diagnose::log("watchdog: UI thread unreachable -> relaunching");
                relaunch_self();
            }
        }
    });
}

/// Recreate the widget window itself (class is already registered). Only used
/// by revival; the startup path in `run` keeps its own creation code.
unsafe fn recreate_widget_window() -> Option<HWND> {
    let hinstance = match GetModuleHandleW(PCWSTR::null()) {
        Ok(handle) => handle,
        Err(error) => {
            diagnose::log_error("revival: GetModuleHandleW failed", error);
            return None;
        }
    };
    let (title_text, model_count) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.language.strings().window_title,
                active_model_count(
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                ),
            ),
            None => return None,
        }
    };
    let class_name = native_interop::wide_str(WINDOW_CLASS_NAME);
    let title = native_interop::wide_str(title_text);
    match CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(class_name.as_ptr()),
        PCWSTR::from_raw(title.as_ptr()),
        WS_POPUP,
        0,
        0,
        total_widget_width_for(model_count),
        sc(WIDGET_HEIGHT),
        HWND::default(),
        HMENU::default(),
        hinstance,
        None,
    ) {
        Ok(hwnd) => Some(hwnd),
        Err(error) => {
            diagnose::log_error("revival: CreateWindowExW failed", error);
            None
        }
    }
}

/// First stage of revival: mark a revival as in flight and immediately ask
/// the UI thread to try the current taskbar. Shell readiness is event-driven
/// (TaskbarCreated/display/session broadcasts plus the watchdog), so delaying
/// the first attempt only makes RDP and Explorer recovery visibly slower.
fn revive_request() {
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        return;
    }
    if session_is_unstable() {
        diagnose::log("revival deferred while session is locked or disconnected");
        return;
    }
    if REVIVING.swap(true, Ordering::SeqCst) {
        return; // another revival is already in flight
    }
    REVIVING_SINCE.store(now_unix_secs(), Ordering::SeqCst);
    REVIVE_ATTEMPTS.store(0, Ordering::SeqCst);
    diagnose::log("revival: begin (immediate taskbar re-attach attempt)");
    post_revive_ready();
}

fn clear_reviving() {
    REVIVING.store(false, Ordering::SeqCst);
    REVIVING_SINCE.store(0, Ordering::SeqCst);
}

fn post_revive_ready() {
    // Prefer the broadcast helper window as the target: a NULL-hwnd thread
    // message retrieved by a modal message loop (context menu, message box)
    // is silently discarded by DispatchMessageW, which would strand
    // REVIVING=true forever; window messages survive modal loops.
    let helper = BROADCAST_HELPER_HWND.load(Ordering::SeqCst);
    if helper != 0 {
        let helper = HWND(helper as *mut _);
        let posted = unsafe {
            IsWindow(helper).as_bool()
                && PostMessageW(helper, WM_APP_REVIVE_READY, WPARAM(0), LPARAM(0)).is_ok()
        };
        if posted {
            return;
        }
    }
    // Fallback: thread message straight to the message loop.
    let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
    let posted = thread_id != 0
        && unsafe {
            PostThreadMessageW(thread_id, WM_APP_REVIVE_READY, WPARAM(0), LPARAM(0)).is_ok()
        };
    if !posted {
        // The UI thread is unreachable; clear the in-flight flag so the
        // watchdog re-detects the problem and can fall back to a relaunch.
        clear_reviving();
        diagnose::log("revival: unable to reach the UI thread with the ready signal");
    }
}

/// Ask the UI thread to perform the deliberate-quit cleanup without relying
/// on the current embedded window handle, which revival may replace while an
/// update is downloading. The hidden broadcast helper normally lives for the
/// whole process; the thread queue is the fallback if that window is gone.
fn request_process_quit() {
    let helper = BROADCAST_HELPER_HWND.load(Ordering::SeqCst);
    if helper != 0 {
        let helper = HWND(helper as *mut _);
        let posted = unsafe {
            IsWindow(helper).as_bool()
                && PostMessageW(helper, WM_APP_REQUEST_QUIT, WPARAM(0), LPARAM(0)).is_ok()
        };
        if posted {
            return;
        }
    }

    let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
    let posted = thread_id != 0
        && unsafe {
            PostThreadMessageW(thread_id, WM_APP_REQUEST_QUIT, WPARAM(0), LPARAM(0)).is_ok()
        };
    if posted {
        return;
    }

    // The helper has already been launched and is waiting for this PID. If the
    // UI thread cannot be reached at all, process termination is the only way
    // to avoid stranding the helper until its timeout.
    diagnose::log("update quit request could not reach the UI thread; exiting directly");
    std::process::exit(0);
}

/// Second stage of revival, on the UI thread with no long waits: bring the
/// widget back after Explorer destroyed our window (or moved the taskbar out
/// from under us). The taskbar widget is never shown as a desktop popup: when
/// the shell is unavailable it stays hidden until a later shell event or the
/// watchdog can verify a successful re-attachment.
/// Every fixed-interval timer a revived widget has to arm, with the window it
/// is armed on being the widget itself.
///
/// One list rather than a run of `arm_timer` calls, because this set has
/// already drifted once: a recreated widget was left without its tray-order
/// sample and the fallback ordering check stopped at the first Explorer
/// restart.
///
/// `widget_is_poll_controller` is the degraded path. `poll_controller_hwnd`
/// answers with the process-level broadcast helper when it exists and with the
/// widget otherwise, and `WM_TIMER` only ever reaches the window a timer was
/// armed on - so when `create_broadcast_helper` failed at startup, polling,
/// the credential watch and the periodic provider sweep are all armed on the
/// widget and die with it. The helper, when it exists, outlives the widget and
/// keeps them, which is why the normal path re-arms nothing but its own.
///
/// Only a recreated widget lost anything: re-arming a survivor's poll timer
/// would push the next poll out by a whole interval every time Explorer
/// restarts. `TIMER_COUNTDOWN`, `TIMER_RESET_POLL` and `TIMER_UPDATE_CHECK`
/// are deliberately absent - each is scheduled from current state by its own
/// function, which the revival path calls.
fn revive_timer_plan(
    widget_was_recreated: bool,
    widget_is_poll_controller: bool,
    poll_interval_ms: u32,
    auth_watch_active: bool,
) -> Vec<(usize, u32, &'static str)> {
    let mut plan = vec![(TIMER_TRAY_ORDER, TRAY_ORDER_SAMPLE_MS, "tray order sample")];
    if widget_was_recreated && widget_is_poll_controller {
        plan.push((TIMER_POLL, poll_interval_ms, "poll"));
        plan.push((
            TIMER_PROVIDER_DETECT,
            PROVIDER_DETECT_INTERVAL_MS,
            "provider detection",
        ));
        if auth_watch_active {
            plan.push((TIMER_AUTH_WATCH, AUTH_WATCH_INTERVAL_MS, "auth watch"));
        }
    }
    plan
}

unsafe fn revive_execute() {
    if QUIT_REQUESTED.load(Ordering::SeqCst) {
        clear_reviving();
        return;
    }

    let (
        existing_hwnd,
        preferred_taskbar_index,
        widget_placement,
        widget_placement_needs_migration,
        widget_visible,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd.to_hwnd(),
                s.preferred_taskbar_index,
                s.widget_placement.clone(),
                s.widget_placement_needs_migration,
                s.widget_visible,
            ),
            None => {
                clear_reviving();
                return;
            }
        }
    };

    let widget_was_recreated = !IsWindow(existing_hwnd).as_bool();
    let hwnd = if !widget_was_recreated {
        diagnose::log("revival: window still alive; re-attaching only");
        existing_hwnd
    } else {
        match recreate_widget_window() {
            Some(hwnd) => {
                diagnose::log(format!("revival: window recreated hwnd={:?}", hwnd));
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.hwnd = SendHwnd::from_hwnd(hwnd);
                    s.embedded = false;
                    s.taskbar_hwnd = None;
                    s.tray_notify_hwnd = None;
                }
                hwnd
            }
            None => {
                let attempt = REVIVE_ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt >= REVIVE_CREATE_ATTEMPTS {
                    clear_reviving();
                    diagnose::log("revival: window creation failed repeatedly; relaunching");
                    relaunch_self();
                    // relaunch_self exits the process on success; reaching
                    // here means the spawn failed. Stay alive - the watchdog
                    // retries.
                    return;
                }
                diagnose::log(format!(
                    "revival: window creation attempt {attempt}/{REVIVE_CREATE_ATTEMPTS} failed; retrying in {}s",
                    REVIVE_CREATE_RETRY_DELAY.as_secs()
                ));
                // REVIVING stays true while the delayed retry is pending.
                std::thread::spawn(|| {
                    std::thread::sleep(REVIVE_CREATE_RETRY_DELAY);
                    post_revive_ready();
                });
                return;
            }
        }
    };

    // Prevent a transient desktop flash if SetParent must detach the old
    // taskbar child before the new taskbar is ready.
    let _ = ShowWindow(hwnd, SW_HIDE);
    let target_taskbar_index = if widget_placement_needs_migration {
        preferred_taskbar_index
    } else {
        taskbar_index_for_placement(&widget_placement, preferred_taskbar_index)
    };
    if !attach_to_taskbar(hwnd, target_taskbar_index) {
        diagnose::log("revival: taskbar unavailable; keeping widget hidden");
        if let Err(error) = native_interop::detach_to_popup(hwnd) {
            diagnose::log(format!("revival detach from stale taskbar failed: {error}"));
        }
        let _ = ShowWindow(hwnd, SW_HIDE);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.embedded = false;
                s.taskbar_hwnd = None;
                s.tray_notify_hwnd = None;
            }
        }
        clear_reviving();
        return;
    }

    sync_tray_icons(hwnd);
    migrate_legacy_placements_if_needed();
    // Position and render before showing so the revived widget reappears in
    // place with content instead of flashing in and being moved.
    position_at_taskbar();
    render_layered();
    if widget_visible {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    // Provider polling is owned by the process-level broadcast helper and
    // therefore did not stop while this taskbar child was absent.
    schedule_countdown_timer();
    schedule_auto_update_check(hwnd);
    // Timers belong to the window they were set on, so a recreated widget
    // starts with none. The WinEvent hook that `attach_to_taskbar` reinstalls
    // catches a drag, but the periodic sample is what notices an order change
    // the hook missed - without this it stopped for good at the first Explorer
    // restart. When the broadcast helper could not be created at startup this
    // widget is also the poll controller, so polling, the credential watch and
    // the provider sweep died with the old window too; `revive_timer_plan`
    // owns that whole set.
    let widget_is_poll_controller = live_broadcast_helper_hwnd().is_none();
    if widget_was_recreated && widget_is_poll_controller {
        // Session notifications were registered on the window Explorer just
        // destroyed, and they are what tells this process the session locked
        // or disconnected.
        if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
            diagnose::log("revival: session notifications could not be re-registered");
        }
    }
    {
        let mut state = lock_state();
        let (poll_interval_ms, auth_watch_active) = state
            .as_ref()
            .map(|s| (s.poll_interval_ms, s.auth_watch_active))
            .unwrap_or((POLL_5_MIN, false));
        for (timer, interval_ms, label) in revive_timer_plan(
            widget_was_recreated,
            widget_is_poll_controller,
            poll_interval_ms,
            auth_watch_active,
        ) {
            match (timer, state.as_mut()) {
                // Goes through `arm_poll_timer` so `next_poll_deadline` stays
                // in step with the timer that was actually set.
                (TIMER_POLL, Some(s)) => arm_poll_timer(s, hwnd, interval_ms),
                _ => {
                    arm_timer(hwnd, timer, interval_ms, label);
                }
            }
        }
    }

    REVIVE_ATTEMPTS.store(0, Ordering::SeqCst);
    clear_reviving();
    diagnose::log("revival: complete");
}

/// Process-lifetime copy of the icons above.
///
/// `ExtractIconExW` hands back handles this process owns. The window class
/// takes its pair once at startup and holds them for good, but the consent
/// dialog can be opened again every time the user declines and then enables a
/// provider by hand, so extracting a fresh pair per open leaked two handles
/// each time.
static APP_ICONS: OnceLock<(isize, isize)> = OnceLock::new();

fn cached_app_icons() -> (HICON, HICON) {
    let (large, small) = *APP_ICONS.get_or_init(|| {
        let (large, small) = load_embedded_app_icons();
        (large.0 as isize, small.0 as isize)
    });
    (
        HICON(large as *mut std::ffi::c_void),
        HICON(small as *mut std::ffi::c_void),
    )
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
struct DetailPopupState {
    title: String,
    providers: Vec<DetailProviderGroup>,
    status: String,
    version: String,
    refreshing: bool,
}

#[derive(Clone)]
struct DetailProviderGroup {
    kind: tray_icon::TrayIconKind,
    name: String,
    /// Compact status shown on the provider header. Tone carries product
    /// meaning independently from the localized copy.
    badge: Option<DetailBadge>,
    rows: Vec<DetailUsageRow>,
    /// True when the rows come from a previous successful poll rather than
    /// the provider's current state.
    data_is_stale: bool,
    hint: Option<DetailHint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailBadgeTone {
    Degraded,
    ActionRequired,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailBadge {
    text: String,
    tone: DetailBadgeTone,
}

#[derive(Clone)]
struct DetailUsageRow {
    window_label: String,
    /// None while no data exists for this window (shown as "--").
    percent: Option<f64>,
    reset_text: String,
    dividers: i32,
    warn: bool,
}

static DETAIL_STATE: Mutex<Option<DetailPopupState>> = Mutex::new(None);
static DETAIL_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
/// When the popup was last destroyed, for the reopen-as-toggle suppression.
static DETAIL_LAST_DISMISS: Mutex<Option<Instant>> = Mutex::new(None);
/// Native owner-drawn buttons do not consistently report ODS_HOTLIGHT on all
/// supported Windows versions, so track the actual hovered child explicitly.
static DETAIL_HOT_BUTTON_ID: AtomicU32 = AtomicU32::new(0);
/// A native owner-drawn button receives ODS_FOCUS for both mouse and keyboard
/// focus. Track mouse-originated focus explicitly so only keyboard navigation
/// receives a visible focus cue.
static DETAIL_MOUSE_FOCUS_BUTTON_ID: AtomicU32 = AtomicU32::new(0);
/// The refresh button mirrors the footer's busy state, blocks duplicate
/// requests, and exposes a localized "refreshing" accessible name.
static DETAIL_REFRESHING: AtomicBool = AtomicBool::new(false);
/// The popup starts movable every time it opens. Locking only lasts for this
/// HWND's lifetime; its moved position is deliberately not persisted.
const DETAIL_DEFAULT_MOVEMENT_UNLOCKED: bool = true;
static DETAIL_MOVEMENT_UNLOCKED: AtomicBool = AtomicBool::new(DETAIL_DEFAULT_MOVEMENT_UNLOCKED);
/// While pinned the popup survives focus loss; only Esc, the close button, or
/// Exit dismiss it. Unlike the movement lock, this is a persisted preference.
const DETAIL_DEFAULT_PINNED: bool = false;
static DETAIL_PINNED: AtomicBool = AtomicBool::new(DETAIL_DEFAULT_PINNED);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DetailScrollState {
    offset: i32,
    max_offset: i32,
    wheel_remainder: i32,
    dragging: bool,
    drag_start_y: i32,
    drag_start_offset: i32,
}

static DETAIL_SCROLL_STATE: Mutex<DetailScrollState> = Mutex::new(DetailScrollState {
    offset: 0,
    max_offset: 0,
    wheel_remainder: 0,
    dragging: false,
    drag_start_y: 0,
    drag_start_offset: 0,
});

#[derive(Clone, Debug, PartialEq, Eq)]
struct WidgetTooltipRow {
    window_label: String,
    percent_text: String,
    reset_text: String,
    warn: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WidgetTooltipSnapshot {
    kind: tray_icon::TrayIconKind,
    provider_name: String,
    rows: Vec<WidgetTooltipRow>,
}

#[derive(Default)]
struct WidgetTooltipRuntime {
    hover_kind: Option<tray_icon::TrayIconKind>,
    hits: Vec<BadgeHit>,
    snapshot: Option<WidgetTooltipSnapshot>,
}

static WIDGET_TOOLTIP_RUNTIME: Mutex<WidgetTooltipRuntime> = Mutex::new(WidgetTooltipRuntime {
    hover_kind: None,
    hits: Vec::new(),
    snapshot: None,
});
static WIDGET_TOOLTIP_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);

fn lock_detail_state() -> MutexGuard<'static, Option<DetailPopupState>> {
    DETAIL_STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_detail_scroll_state() -> MutexGuard<'static, DetailScrollState> {
    DETAIL_SCROLL_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn detail_fallback_snapshot() -> DetailPopupState {
    DetailPopupState {
        title: "Gengchou".to_string(),
        providers: Vec::new(),
        status: LanguageId::English.strings().detail_waiting.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        refreshing: false,
    }
}

fn lock_widget_tooltip_runtime() -> MutexGuard<'static, WidgetTooltipRuntime> {
    WIDGET_TOOLTIP_RUNTIME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

const USAGE_CACHE_MAX_AGE_SECS: u64 = 48 * 60 * 60;

fn usage_cache_path() -> std::io::Result<PathBuf> {
    settings::app_data_file("usage-cache.json")
}

/// Snapshot of the last successful poll, persisted so a restart can show the
/// previous numbers immediately instead of "--" until the first poll lands.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheWindow {
    percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resets_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_label: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageCacheProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    windows: Vec<UsageCacheWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<UsageCacheWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weekly: Option<UsageCacheWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct UsageCacheFile {
    saved_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claude_code: Option<UsageCacheProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex: Option<UsageCacheProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    antigravity: Option<UsageCacheProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grok: Option<UsageCacheProvider>,
}

struct RevisionedSnapshot<T> {
    revision: u64,
    value: T,
}

struct UsageCacheSnapshot {
    saved_unix: u64,
    data: AppUsageData,
}

/// Must be called while `STATE` is locked so the revision and cloned data are
/// one ordered snapshot.
fn capture_usage_cache_snapshot(
    state: &AppState,
) -> Option<RevisionedSnapshot<UsageCacheSnapshot>> {
    state.data.clone().map(|data| RevisionedSnapshot {
        revision: USAGE_CACHE_PERSISTENCE.next_revision(),
        value: UsageCacheSnapshot {
            saved_unix: now_unix_secs(),
            data,
        },
    })
}

fn usage_window_to_cache(window: &UsageWindow) -> UsageCacheWindow {
    UsageCacheWindow {
        percent: window.percentage,
        resets_unix: window
            .resets_at
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        duration_seconds: window.duration_seconds,
        source_label: window.source_label.clone(),
    }
}

fn usage_window_from_cache(window: &UsageCacheWindow) -> UsageWindow {
    UsageWindow {
        // The file is user-writable: a corrupt-but-parseable value must not
        // panic at startup (SystemTime + Duration panics on overflow) or
        // paint absurd percentages.
        percentage: window.percent.clamp(0.0, 100.0),
        resets_at: window
            .resets_unix
            .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs))),
        duration_seconds: window.duration_seconds,
        source_label: window.source_label.clone(),
    }
}

fn usage_provider_to_cache(usage: &UsageData, updated_unix: Option<u64>) -> UsageCacheProvider {
    UsageCacheProvider {
        updated_unix,
        windows: usage.windows.iter().map(usage_window_to_cache).collect(),
        session: None,
        weekly: None,
    }
}

fn usage_provider_from_cache(provider: &UsageCacheProvider) -> UsageData {
    if !provider.windows.is_empty() {
        return UsageData::from_windows(
            provider
                .windows
                .iter()
                .map(usage_window_from_cache)
                .collect(),
        );
    }

    // Migrate the v2.0 cache shape. A zero/no-reset legacy section was the old
    // representation of a missing window, so do not recreate that ghost row.
    let mut windows = Vec::new();
    for (legacy, duration_seconds) in [
        (provider.session.as_ref(), FIVE_HOURS_SECONDS),
        (provider.weekly.as_ref(), ONE_WEEK_SECONDS),
    ] {
        if let Some(legacy) = legacy {
            if legacy.percent != 0.0 || legacy.resets_unix.is_some() {
                let mut window = usage_window_from_cache(legacy);
                window.duration_seconds = Some(duration_seconds);
                windows.push(window);
            }
        }
    }
    UsageData::from_windows(windows)
}

fn fresh_cached_provider(
    provider: Option<&UsageCacheProvider>,
    saved_unix: u64,
    now_unix: u64,
) -> Option<(UsageData, u64)> {
    provider.and_then(|provider| {
        let updated_unix = provider.updated_unix.unwrap_or(saved_unix);
        (now_unix.saturating_sub(updated_unix) <= USAGE_CACHE_MAX_AGE_SECS)
            .then(|| (usage_provider_from_cache(provider), updated_unix))
    })
}

/// The on-disk shape stays a field per provider: it is a published format, and
/// a stored key that no longer maps to a provider must stay readable.
///
/// Split from the file write so the provider-to-field mapping is testable.
/// Forgetting one provider here is invisible at runtime - that provider simply
/// never survives a restart - so it needs a test, not just review.
fn usage_cache_file_from(data: &AppUsageData, saved_unix: u64) -> UsageCacheFile {
    let to_cache = |kind: tray_icon::TrayIconKind| {
        data.usage(kind)
            .map(|usage| usage_provider_to_cache(usage, data.provider(kind).updated_unix))
    };
    UsageCacheFile {
        saved_unix,
        claude_code: to_cache(tray_icon::TrayIconKind::Claude),
        codex: to_cache(tray_icon::TrayIconKind::Codex),
        antigravity: to_cache(tray_icon::TrayIconKind::Antigravity),
        grok: to_cache(tray_icon::TrayIconKind::Grok),
    }
}

fn usage_cache_file_sections(
    file: &UsageCacheFile,
) -> [(tray_icon::TrayIconKind, Option<&UsageCacheProvider>); tray_icon::TrayIconKind::COUNT] {
    [
        (tray_icon::TrayIconKind::Claude, file.claude_code.as_ref()),
        (tray_icon::TrayIconKind::Codex, file.codex.as_ref()),
        (
            tray_icon::TrayIconKind::Antigravity,
            file.antigravity.as_ref(),
        ),
        (tray_icon::TrayIconKind::Grok, file.grok.as_ref()),
    ]
}

fn write_usage_cache(snapshot: &UsageCacheSnapshot) -> std::io::Result<()> {
    let file = usage_cache_file_from(&snapshot.data, snapshot.saved_unix);
    let path = usage_cache_path()?;
    let json = serde_json::to_string(&file).map_err(std::io::Error::other)?;
    settings::write_file_atomic(&path, &json).map_err(|error| {
        std::io::Error::new(error.kind(), format!("path={}: {error}", path.display()))
    })
}

fn save_usage_cache(snapshot: &RevisionedSnapshot<UsageCacheSnapshot>) {
    match USAGE_CACHE_PERSISTENCE
        .write_if_latest(snapshot.revision, || write_usage_cache(&snapshot.value))
    {
        Ok(_) => {}
        Err(error) => {
            settings::record_persistence_warning("Unable to save the usage cache", &error);
            diagnose::log_error("usage cache save failed", &error);
            post_persistence_warning();
        }
    }
}

fn load_usage_cache() -> Option<(AppUsageData, u64)> {
    let path = match usage_cache_path() {
        Ok(path) => path,
        Err(error) => {
            settings::record_persistence_warning(
                "Unable to locate the usage cache directory",
                &error,
            );
            return None;
        }
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            diagnose::log_error(
                &format!("usage cache read failed path={}", path.display()),
                error,
            );
            return None;
        }
    };
    let file: UsageCacheFile = match serde_json::from_str(&content) {
        Ok(file) => file,
        Err(error) => {
            diagnose::log_error(
                &format!("usage cache parse failed path={}", path.display()),
                error,
            );
            return None;
        }
    };
    let now = now_unix_secs();
    if now.saturating_sub(file.saved_unix) > USAGE_CACHE_MAX_AGE_SECS {
        return None;
    }
    let mut data = AppUsageData::default();
    for (kind, cached) in usage_cache_file_sections(&file) {
        if let Some((usage, updated_unix)) = fresh_cached_provider(cached, file.saved_unix, now) {
            let slot = data.provider_mut(kind);
            slot.usage = Some(usage);
            slot.updated_unix = Some(updated_unix);
        }
    }
    if !data.has_any_usage() {
        return None;
    }
    let last_success_unix = tray_icon::TrayIconKind::ALL
        .into_iter()
        .filter_map(|kind| data.provider(kind).updated_unix)
        .max()
        .unwrap_or(file.saved_unix);
    Some((data, last_success_unix))
}

fn save_state_settings() {
    let snapshot = {
        let state = lock_state();
        state.as_ref().map(|s| RevisionedSnapshot {
            revision: SETTINGS_PERSISTENCE.next_revision(),
            value: SettingsFile {
                placement_schema_version: PLACEMENT_SCHEMA_VERSION,
                widget_placement: (!s.widget_placement_needs_migration)
                    .then(|| s.widget_placement.clone()),
                floating_placement: (!s.floating_placement_needs_migration)
                    .then(|| s.floating_placement.clone()),
                // Legacy geometry remains readable for one compatibility window,
                // and is retained until that surface can be migrated safely.
                tray_offset: if s.widget_placement_needs_migration {
                    s.tray_offset
                } else {
                    0
                },
                taskbar_index: if s.widget_placement_needs_migration {
                    s.preferred_taskbar_index
                } else {
                    0
                },
                widget_default_position: s.widget_default_position,
                poll_interval_ms: s.poll_interval_ms,
                language: s
                    .language_override
                    .map(|language| language.code().to_string()),
                last_update_check_unix: s.last_update_check_unix,
                last_update_outcome: s.last_update_outcome.clone(),
                widget_visible: s.widget_visible,
                floating_visible: s.floating_visible,
                detailed_tray_icons: s.detailed_tray_icons,
                detail_pinned: s.detail_pinned,
                floating_x: s
                    .floating_placement_needs_migration
                    .then_some(s.floating_x)
                    .flatten(),
                floating_y: s
                    .floating_placement_needs_migration
                    .then_some(s.floating_y)
                    .flatten(),
                floating_default_position: s.floating_default_position,
                show_claude_code: s.show_claude_code,
                show_codex: s.show_codex,
                show_antigravity: s.show_antigravity,
                show_grok: s.show_grok,
                allow_claude_credentials: s.allow_claude_credentials,
                allow_codex_credentials: s.allow_codex_credentials,
                allow_antigravity_credentials: s.allow_antigravity_credentials,
                allow_grok_credentials: s.allow_grok_credentials,
                credential_consent_granted: s.credential_consent_granted,
                credential_consent_decided: s.credential_consent_decided,
                consent_schema_version: settings::CONSENT_SCHEMA_VERSION,
                claude_credential_access_decided: s.claude_credential_access_decided,
                codex_credential_access_decided: s.codex_credential_access_decided,
                antigravity_credential_access_decided: s.antigravity_credential_access_decided,
                grok_credential_access_decided: s.grok_credential_access_decided,
                claude_credential_access_revoked: s.claude_credential_access_revoked,
                codex_credential_access_revoked: s.codex_credential_access_revoked,
                antigravity_credential_access_revoked: s.antigravity_credential_access_revoked,
                grok_credential_access_revoked: s.grok_credential_access_revoked,
                claude_credential_access_pending: s.claude_credential_access_pending,
                codex_credential_access_pending: s.codex_credential_access_pending,
                antigravity_credential_access_pending: s.antigravity_credential_access_pending,
                grok_credential_access_pending: s.grok_credential_access_pending,
                incoming: settings::IncomingAccess::Canonical,
                provider_order: s.provider_order.clone(),
                notify_session_reset: s.notify_session_reset,
                notify_weekly_reset: s.notify_weekly_reset,
            },
        })
    };
    if let Some(snapshot) = snapshot {
        match SETTINGS_PERSISTENCE
            .write_if_latest(snapshot.revision, || settings::save(&snapshot.value))
        {
            Ok(_) => {}
            Err(error) => {
                settings::record_persistence_warning("Unable to save settings", &error);
                diagnose::log_error("settings save failed", &error);
                post_persistence_warning();
            }
        }
    }
}

const TRAY_TOOLTIP_MAX_UTF16: usize = 127;

fn truncate_utf16_with_ellipsis(text: &str, max_units: usize) -> String {
    if text.encode_utf16().count() <= max_units {
        return text.to_string();
    }
    if max_units == 0 {
        return String::new();
    }

    let content_units = max_units.saturating_sub(1);
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let units = ch.len_utf16();
        if used + units > content_units {
            break;
        }
        result.push(ch);
        used += units;
    }
    result.push('…');
    result
}

fn tray_tooltip_from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let lines = lines.into_iter().collect::<Vec<_>>();
    if lines.is_empty() {
        return String::new();
    }

    let full = lines.join("\n");
    if full.encode_utf16().count() <= TRAY_TOOLTIP_MAX_UTF16 {
        return full;
    }

    if lines.len() == 1 {
        return truncate_utf16_with_ellipsis(lines[0], TRAY_TOOLTIP_MAX_UTF16);
    }

    // Keep every included row complete. If the Shell's 127 UTF-16-unit field
    // cannot hold the remaining rows, say exactly how many were omitted rather
    // than ending on a plausible-looking partial value.
    for included in (1..lines.len()).rev() {
        let marker = format!("… (+{})", lines.len() - included);
        let candidate = format!("{}\n{marker}", lines[..included].join("\n"));
        if candidate.encode_utf16().count() <= TRAY_TOOLTIP_MAX_UTF16 {
            return candidate;
        }
    }

    let marker = format!("… (+{})", lines.len() - 1);
    let title_units = TRAY_TOOLTIP_MAX_UTF16
        .saturating_sub(marker.encode_utf16().count())
        .saturating_sub(1);
    format!(
        "{}\n{marker}",
        truncate_utf16_with_ellipsis(lines[0], title_units)
    )
}

fn provider_tooltip_lines<'a>(
    provider_name: &str,
    windows: impl IntoIterator<Item = &'a UsageWindow>,
    strings: Strings,
) -> Vec<String> {
    let mut lines = vec![provider_name.to_string()];
    for window in windows {
        let mut line = format!(
            "{}: {}",
            usage_window_label(window, strings),
            compact_view::display_percent_text(window.percentage)
        );
        if let Some(resets_at) = window.resets_at {
            line.push_str(" (");
            line.push_str(&detail_reset_line(resets_at, strings, false));
            line.push(')');
        }
        lines.push(line);
    }
    if lines.len() == 1 {
        lines.push("--".to_string());
    }
    lines
}

fn provider_status_badge_text(status: ProviderStatus, strings: Strings) -> &'static str {
    match status {
        ProviderStatus::NotSignedIn => strings.detail_badge_not_signed_in,
        ProviderStatus::AuthenticationFailed => strings.detail_badge_auth_failed,
        ProviderStatus::RateLimited
        | ProviderStatus::NetworkUnavailable
        | ProviderStatus::RequestFailed => strings.detail_badge_stale,
    }
}

fn provider_status_for_kind(
    _kind: tray_icon::TrayIconKind,
    error: poller::PollError,
) -> ProviderStatus {
    poller::provider_status(error)
}

fn provider_name_with_status(
    provider_name: &str,
    status: Option<ProviderStatus>,
    strings: Strings,
) -> String {
    match status {
        Some(status) => format!(
            "{provider_name} · {}",
            provider_status_badge_text(status, strings)
        ),
        _ => provider_name.to_string(),
    }
}

fn visible_provider_status(
    status: Option<ProviderStatus>,
    refresh_state: ProviderRefreshState,
    updated_unix: Option<u64>,
    poll_interval_ms: u32,
    now_unix: u64,
) -> Option<ProviderStatus> {
    match status {
        // Both credential states are permanent until the user acts, so they
        // stay visible immediately rather than waiting for a staleness window.
        Some(ProviderStatus::NotSignedIn | ProviderStatus::AuthenticationFailed) => status,
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        ) if provider_refresh_is_stale(
            status,
            refresh_state,
            updated_unix,
            poll_interval_ms,
            now_unix,
        ) =>
        {
            status
        }
        _ => None,
    }
}

fn provider_updated_ago_text(
    updated_unix: Option<u64>,
    strings: Strings,
    now_unix: u64,
) -> Option<String> {
    updated_unix.map(|updated_unix| {
        strings.detail_updated_ago.replace(
            "{ago}",
            &detail_duration_from_secs(now_unix.saturating_sub(updated_unix), strings),
        )
    })
}

fn localized_provider_name(kind: tray_icon::TrayIconKind, strings: Strings) -> &'static str {
    match kind {
        tray_icon::TrayIconKind::Claude => strings.claude_model,
        tray_icon::TrayIconKind::Codex => strings.codex_model,
        tray_icon::TrayIconKind::Antigravity => strings.antigravity_model,
        tray_icon::TrayIconKind::Grok => strings.grok_model,
    }
}

fn credential_action_text(
    kind: tray_icon::TrayIconKind,
    status: ProviderStatus,
    strings: Strings,
) -> String {
    let provider = localized_provider_name(kind, strings);
    // A provider with no local credential was never signed in, so "sign in
    // again" would be wrong - point at the first sign-in instead.
    if status == ProviderStatus::NotSignedIn {
        return strings
            .detail_not_signed_in_action
            .replace("{provider}", provider);
    }
    if kind == tray_icon::TrayIconKind::Claude && status == ProviderStatus::AuthenticationFailed {
        strings.detail_claude_login_action.to_string()
    } else {
        strings
            .detail_sign_in_again_action
            .replace("{provider}", provider)
    }
}

fn credential_notification_text(
    kind: tray_icon::TrayIconKind,
    status: ProviderStatus,
    strings: Strings,
) -> (String, String) {
    let provider = localized_provider_name(kind, strings);
    let title = format!(
        "{provider} · {}",
        provider_status_badge_text(status, strings)
    );
    let outcome = strings.detail_monitoring_resumes;
    let body = format!(
        "{}\n{}",
        credential_action_text(kind, status, strings),
        outcome
    );
    (title, body)
}

#[cfg(test)]
fn first_provider_credential_issue(
    data: Option<&AppUsageData>,
    global_status: Option<ProviderStatus>,
    provider_order: &[tray_icon::TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> Option<(tray_icon::TrayIconKind, ProviderStatus)> {
    for kind in shown_provider_order(
        provider_order,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
    ) {
        let status = data.and_then(|data| data.error(kind)).or(global_status);
        if let Some(status) = status.filter(|status| status.needs_credentials()) {
            return Some((kind, status));
        }
    }
    None
}

fn shown_provider_order(
    configured: &[tray_icon::TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> Vec<tray_icon::TrayIconKind> {
    let defaults = default_provider_order();
    let mut ordered = Vec::with_capacity(tray_icon::TrayIconKind::COUNT);
    for kind in configured.iter().chain(defaults.iter()).copied() {
        let shown = match kind {
            tray_icon::TrayIconKind::Claude => show_claude_code,
            tray_icon::TrayIconKind::Codex => show_codex,
            tray_icon::TrayIconKind::Antigravity => show_antigravity,
            tray_icon::TrayIconKind::Grok => show_grok,
        };
        if shown && !ordered.contains(&kind) {
            ordered.push(kind);
        }
    }
    ordered
}

fn provider_tooltip_with_freshness(
    provider_name: &str,
    usage: Option<&UsageData>,
    status: Option<ProviderStatus>,
    updated_unix: Option<u64>,
    strings: Strings,
) -> String {
    let windows = usage
        .filter(|usage| !usage.is_empty())
        .map(|usage| usage.windows.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    let title = provider_name_with_status(provider_name, status, strings);
    let mut lines = provider_tooltip_lines(&title, windows, strings);
    if status.is_some() {
        if let Some(updated) = provider_updated_ago_text(updated_unix, strings, now_unix_secs()) {
            lines.push(updated);
        }
    }
    tray_tooltip_from_lines(lines.iter().map(String::as_str))
}

#[cfg(test)]
fn provider_tooltip(
    provider_name: &str,
    usage: Option<&UsageData>,
    status: Option<ProviderStatus>,
    strings: Strings,
) -> String {
    provider_tooltip_with_freshness(provider_name, usage, status, None, strings)
}

fn widget_tooltip_reset_text(resets_at: SystemTime, strings: Strings) -> String {
    match resets_at.duration_since(SystemTime::now()) {
        Ok(duration) if duration.as_secs() > 0 => strings
            .detail_resets_in
            .replace("{duration}", &detail_duration_text(duration, strings)),
        _ => strings.detail_resets_now.to_string(),
    }
}

fn widget_tooltip_snapshot(kind: tray_icon::TrayIconKind) -> WidgetTooltipSnapshot {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return WidgetTooltipSnapshot {
            kind,
            provider_name: String::new(),
            rows: Vec::new(),
        };
    };
    let strings = s.language.strings();
    let provider_has_values = s
        .compact_vm
        .providers
        .iter()
        .any(|provider| provider.kind == kind && !provider.windows.is_empty());
    let global_status = |kind| {
        s.last_error
            .map(|error| provider_status_for_kind(kind, error))
    };
    let provider_name = localized_provider_name(kind, strings);
    let usage = provider_has_values
        .then(|| s.data.as_ref().and_then(|data| data.usage(kind)))
        .flatten();
    let status = s
        .data
        .as_ref()
        .and_then(|data| data.error(kind))
        .or_else(|| global_status(kind));
    let updated_unix = s
        .data
        .as_ref()
        .and_then(|data| data.provider(kind).updated_unix);
    let status = visible_provider_status(
        status,
        s.provider_refresh_states.for_kind(kind),
        updated_unix,
        s.poll_interval_ms,
        now_unix_secs(),
    );
    let mut rows = usage
        .filter(|usage| !usage.is_empty())
        .into_iter()
        .flat_map(|usage| usage.windows.iter())
        .map(|window| WidgetTooltipRow {
            window_label: usage_window_label(window, strings),
            percent_text: compact_view::display_percent_text(window.percentage),
            reset_text: window
                .resets_at
                .map(|resets_at| widget_tooltip_reset_text(resets_at, strings))
                .unwrap_or_else(|| strings.detail_reset_unavailable.to_string()),
            warn: compact_view::display_percent_warns(window.percentage),
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        let reset_text = match status {
            Some(ProviderStatus::NotSignedIn) => strings.detail_badge_not_signed_in,
            Some(ProviderStatus::AuthenticationFailed) => strings.detail_unavailable,
            Some(
                ProviderStatus::RateLimited
                | ProviderStatus::NetworkUnavailable
                | ProviderStatus::RequestFailed,
            ) => strings.detail_temporarily_unavailable,
            None => strings.detail_waiting,
        };
        rows.push(WidgetTooltipRow {
            window_label: "--".to_string(),
            percent_text: String::new(),
            reset_text: reset_text.to_string(),
            warn: false,
        });
    }

    let mut provider_name = provider_name_with_status(provider_name, status, strings);
    if status.is_some() {
        if let Some(updated) = provider_updated_ago_text(updated_unix, strings, now_unix_secs()) {
            provider_name.push_str(" · ");
            provider_name.push_str(&updated);
        }
    }
    WidgetTooltipSnapshot {
        kind,
        provider_name,
        rows,
    }
}

fn app_tooltip_provider_line_with_freshness(
    provider_name: &str,
    usage: Option<&UsageData>,
    status: Option<ProviderStatus>,
    updated_unix: Option<u64>,
    strings: Strings,
) -> String {
    let provider_name = provider_name_with_status(provider_name, status, strings);
    let updated = status
        .and_then(|_| provider_updated_ago_text(updated_unix, strings, now_unix_secs()))
        .map(|updated| format!(" · {updated}"))
        .unwrap_or_default();
    let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
        return format!("{provider_name}: --{updated}");
    };
    let windows = usage
        .windows
        .iter()
        .map(|window| {
            format!(
                "{} {}",
                usage_window_label(window, strings),
                compact_view::display_percent_text(window.percentage)
            )
        })
        .collect::<Vec<_>>();
    format!("{provider_name}: {}{updated}", windows.join(" · "))
}

#[cfg(test)]
fn app_tooltip_provider_line(
    provider_name: &str,
    usage: Option<&UsageData>,
    status: Option<ProviderStatus>,
    strings: Strings,
) -> String {
    app_tooltip_provider_line_with_freshness(provider_name, usage, status, None, strings)
}

fn provider_tray_icon(
    kind: tray_icon::TrayIconKind,
    provider_name: &str,
    usage: Option<&UsageData>,
    status: Option<ProviderStatus>,
    updated_unix: Option<u64>,
    widget: &ProviderWidgetData,
    strings: Strings,
) -> tray_icon::TrayIconData {
    tray_icon::TrayIconData {
        kind,
        percents: widget
            .windows
            .iter()
            .filter_map(|window| window.percent)
            .collect(),
        tooltip: provider_tooltip_with_freshness(
            provider_name,
            usage,
            status,
            updated_unix,
            strings,
        ),
    }
}

fn tray_icon_data_from_state() -> (Vec<tray_icon::TrayIconData>, bool, String) {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return (
            Vec::new(),
            true,
            LanguageId::English.strings().window_title.to_string(),
        );
    };
    let strings = s.language.strings();
    let empty = ProviderWidgetData::default();
    let mut icons = Vec::new();
    let mut app_tooltip_lines = vec![strings.window_title.to_string()];
    let data = s.last_poll_ok.then_some(s.data.as_ref()).flatten();
    let global_status = |kind| {
        s.last_error
            .map(|error| provider_status_for_kind(kind, error))
    };
    for kind in tray_surface_provider_order(
        &s.provider_order,
        s.show_claude_code,
        s.show_codex,
        s.show_antigravity,
        s.show_grok,
    ) {
        let provider_name = localized_provider_name(kind, strings);
        let (usage, status, updated_unix, widget) = (
            data.and_then(|data| data.usage(kind)),
            s.data
                .as_ref()
                .and_then(|data| data.error(kind))
                .or_else(|| global_status(kind)),
            s.data
                .as_ref()
                .and_then(|data| data.provider(kind).updated_unix),
            match kind {
                tray_icon::TrayIconKind::Claude => &s.claude_widget,
                tray_icon::TrayIconKind::Codex => &s.codex_widget,
                tray_icon::TrayIconKind::Antigravity => &s.antigravity_widget,
                tray_icon::TrayIconKind::Grok => &s.grok_widget,
            },
        );
        let status = visible_provider_status(
            status,
            s.provider_refresh_states.for_kind(kind),
            updated_unix,
            s.poll_interval_ms,
            now_unix_secs(),
        );
        icons.push(provider_tray_icon(
            kind,
            provider_name,
            usage,
            status,
            updated_unix,
            if s.last_poll_ok { widget } else { &empty },
            strings,
        ));
        app_tooltip_lines.push(app_tooltip_provider_line_with_freshness(
            provider_name,
            usage,
            status,
            updated_unix,
            strings,
        ));
    }
    let app_tooltip = tray_tooltip_from_lines(app_tooltip_lines.iter().map(String::as_str));
    // With no provider authorized there is nothing to monitor, and a provider
    // brand mark in the tray would advertise a service the user did not pick
    // and this app is not reading. Fall back to the application icon, which is
    // the same surface the "Provider tray icons" toggle already produces. The
    // stored preference is untouched, so the provider icons come back by
    // themselves once any provider is granted access again.
    let any_provider_authorized = tray_icon::TrayIconKind::ALL
        .into_iter()
        .any(|kind| provider_has_credential_access(s, kind));
    (
        icons,
        s.detailed_tray_icons && any_provider_authorized,
        app_tooltip,
    )
}

fn tray_surface_provider_order(
    configured: &[tray_icon::TrayIconKind],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> Vec<tray_icon::TrayIconKind> {
    shown_provider_order(
        configured,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_grok,
    )
}

fn sync_tray_icons(hwnd: HWND) {
    let (icons, detailed_icons, app_tooltip) = tray_icon_data_from_state();
    tray_icon::sync(hwnd, &icons, detailed_icons, &app_tooltip);
}

fn refresh_native_provider_tooltips(hwnd: HWND) {
    let (icons, detailed_icons, _) = tray_icon_data_from_state();
    if detailed_icons {
        tray_icon::update_provider_tooltips(hwnd, &icons);
    }
}

fn enabled_provider_kinds(state: &AppState) -> Vec<tray_icon::TrayIconKind> {
    default_provider_order()
        .into_iter()
        .filter(|kind| match kind {
            tray_icon::TrayIconKind::Claude => state.show_claude_code,
            tray_icon::TrayIconKind::Codex => state.show_codex,
            tray_icon::TrayIconKind::Antigravity => state.show_antigravity,
            tray_icon::TrayIconKind::Grok => state.show_grok,
        })
        .collect()
}

/// Replace only the relative slots occupied by currently visible providers.
/// Hidden providers keep their previous slot so toggling one back on does not
/// arbitrarily move the other providers.
fn merge_visible_provider_order(
    full_order: &[tray_icon::TrayIconKind],
    visible_order: &[tray_icon::TrayIconKind],
) -> Vec<tray_icon::TrayIconKind> {
    let mut visible = visible_order.iter().copied();
    full_order
        .iter()
        .map(|kind| {
            if visible_order.contains(kind) {
                visible.next().unwrap_or(*kind)
            } else {
                *kind
            }
        })
        .collect()
}

fn reset_pending_provider_order() {
    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.pending_provider_order = None;
        s.pending_provider_order_samples = 0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderOrderObservation {
    Current,
    Pending,
    Apply,
}

fn observe_provider_order_candidate(
    current: &[tray_icon::TrayIconKind],
    candidate: &[tray_icon::TrayIconKind],
    pending: &mut Option<Vec<tray_icon::TrayIconKind>>,
    samples: &mut u8,
) -> ProviderOrderObservation {
    if candidate == current {
        *pending = None;
        *samples = 0;
        return ProviderOrderObservation::Current;
    }

    if pending.as_deref() == Some(candidate) {
        *samples = samples.saturating_add(1);
    } else {
        *pending = Some(candidate.to_vec());
        *samples = 1;
    }

    if *samples >= TRAY_ORDER_STABLE_SAMPLES {
        *pending = None;
        *samples = 0;
        ProviderOrderObservation::Apply
    } else {
        ProviderOrderObservation::Pending
    }
}

/// Sample the public Shell rectangles for this app's active tray icons. A new
/// order must be observed twice before it changes the compact surfaces. The
/// first observation arms a short one-shot confirmation timer, so an actual
/// drag settles in about 120ms while transient Explorer layouts still cannot
/// make the UI flicker.
fn refresh_provider_order_from_tray(hwnd: HWND) -> bool {
    let (taskbar_hwnd, enabled, current_order, detailed_icons) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return false;
        };
        (
            s.taskbar_hwnd,
            enabled_provider_kinds(s),
            s.provider_order.clone(),
            s.detailed_tray_icons,
        )
    };

    if !detailed_icons || enabled.len() <= 1 {
        reset_pending_provider_order();
        return false;
    }
    let Some(taskbar_rect) = taskbar_hwnd.and_then(native_interop::get_taskbar_rect) else {
        reset_pending_provider_order();
        return false;
    };
    let Some(visible_order) = tray_icon::visible_order(hwnd, &enabled, &taskbar_rect) else {
        reset_pending_provider_order();
        return false;
    };
    let candidate = merge_visible_provider_order(&current_order, &visible_order);

    let (applied, confirm) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return false;
        };
        if s.last_observed_tray_order.as_ref() != Some(&visible_order) {
            diagnose::log(format!(
                "tray provider order observed visible={visible_order:?}"
            ));
            s.last_observed_tray_order = Some(visible_order.clone());
        }
        match observe_provider_order_candidate(
            &s.provider_order,
            &candidate,
            &mut s.pending_provider_order,
            &mut s.pending_provider_order_samples,
        ) {
            ProviderOrderObservation::Current => (false, false),
            ProviderOrderObservation::Pending => {
                diagnose::log(format!(
                    "tray provider order candidate visible={visible_order:?} full={candidate:?}"
                ));
                (false, true)
            }
            ProviderOrderObservation::Apply => {
                s.provider_order = candidate.clone();
                // Taskbar and floating surfaces render from this cached view
                // model, so reorder it immediately even when polling is
                // paused and `refresh_usage_texts` cannot rebuild it.
                compact_view::reorder_providers(&mut s.compact_vm, &candidate);
                (true, false)
            }
        }
    };

    unsafe {
        if confirm {
            if SetTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM, TRAY_ORDER_CONFIRM_MS, None) == 0 {
                diagnose::log("tray provider order confirmation timer failed");
            }
        } else if applied {
            let _ = KillTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM);
        }
    }

    if applied {
        diagnose::log(format!("tray provider order applied full={candidate:?}"));
        position_at_taskbar();
        render_layered();
        refresh_floating_monitor();
        refresh_detail_popup_if_open();
        // Persist after all visible surfaces have updated; a slow filesystem
        // must never delay the user's drag feedback.
        save_state_settings();
    }
    applied
}

fn toggle_widget_visibility(hwnd: HWND) {
    let (new_visible, embedded) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            (s.widget_visible, s.embedded)
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible && embedded {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else if new_visible {
            let _ = ShowWindow(hwnd, SW_HIDE);
            revive_request();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn attach_to_taskbar(hwnd: HWND, requested_index: usize) -> bool {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        diagnose::log("taskbar not found; taskbar widget remains hidden");
        return false;
    }

    let index = requested_index.min(taskbars.len().saturating_sub(1));
    let taskbar = taskbars[index];
    diagnose::log(format!(
        "taskbar selected index={index} count={} hwnd={:?} rect=({}, {}, {}, {})",
        taskbars.len(),
        taskbar.hwnd,
        taskbar.rect.left,
        taskbar.rect.top,
        taskbar.rect.right,
        taskbar.rect.bottom
    ));

    let old_hook = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| s.win_event_hook.take())
    };
    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook);
    }

    if let Err(error) = native_interop::embed_in_taskbar(hwnd, taskbar.hwnd) {
        diagnose::log(format!(
            "taskbar embedding failed; keeping widget hidden: {error}"
        ));
        if let Err(detach_error) = native_interop::detach_to_popup(hwnd) {
            diagnose::log(format!("detach after embed error failed: {detach_error}"));
        }
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.taskbar_hwnd = None;
            s.tray_notify_hwnd = None;
            s.win_event_hook = None;
            s.embedded = false;
        }
        return false;
    }

    let tray_notify = native_interop::find_child_window(taskbar.hwnd, "TrayNotifyWnd");
    if tray_notify.is_some() {
        diagnose::log("TrayNotifyWnd found");
    } else {
        diagnose::log("TrayNotifyWnd not found");
    }

    let hook = tray_notify.and_then(|tray_hwnd| {
        let thread_id = native_interop::get_window_thread_id(tray_hwnd);
        native_interop::set_tray_event_hook(thread_id, on_tray_location_changed)
    });
    if hook.is_some() {
        diagnose::log("tray event hook installed");
    } else {
        diagnose::log("tray event hook could not be installed");
    }

    let monitor = unsafe { monitor_identity_for_taskbar(&taskbar) };
    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.taskbar_hwnd = Some(taskbar.hwnd);
        s.tray_notify_hwnd = tray_notify;
        s.win_event_hook = hook;
        s.taskbar_index = index;
        s.taskbar_monitor = monitor;
        s.embedded = true;
    }
    true
}

unsafe fn monitor_identity_from_handle(monitor: HMONITOR) -> Option<MonitorIdentity> {
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if !GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
        return None;
    }
    let end = info
        .szDevice
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(info.szDevice.len());
    let device = String::from_utf16_lossy(&info.szDevice[..end]);
    if device.is_empty() {
        return None;
    }
    let device_path = native_interop::stable_monitor_device_path(&device);
    Some(MonitorIdentity {
        device,
        device_path,
        is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

fn active_monitor_identities() -> Vec<MonitorIdentity> {
    unsafe extern "system" fn enum_monitor(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data.0 as *mut Vec<MonitorIdentity>);
        if let Some(identity) = monitor_identity_from_handle(monitor) {
            monitors.push(identity);
        }
        BOOL(1)
    }

    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(enum_monitor),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors
}

unsafe fn monitor_handle_for_key(key: &MonitorKey) -> Option<HMONITOR> {
    struct Search<'a> {
        key: &'a MonitorKey,
        found: Option<HMONITOR>,
    }

    unsafe extern "system" fn enum_monitor(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let search = &mut *(data.0 as *mut Search<'_>);
        if monitor_identity_from_handle(monitor)
            .as_ref()
            .is_some_and(|identity| identity.matches_key(search.key))
        {
            search.found = Some(monitor);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search { key, found: None };
    let _ = EnumDisplayMonitors(
        HDC::default(),
        None,
        Some(enum_monitor),
        LPARAM(&mut search as *mut _ as isize),
    );
    search.found
}

unsafe fn primary_monitor_handle() -> Option<HMONITOR> {
    struct Search {
        found: Option<HMONITOR>,
    }

    unsafe extern "system" fn enum_monitor(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let search = &mut *(data.0 as *mut Search);
        if monitor_identity_from_handle(monitor).is_some_and(|identity| identity.is_primary) {
            search.found = Some(monitor);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search { found: None };
    let _ = EnumDisplayMonitors(
        HDC::default(),
        None,
        Some(enum_monitor),
        LPARAM(&mut search as *mut _ as isize),
    );
    search.found
}

unsafe fn monitor_identity_for_taskbar(
    taskbar: &native_interop::TaskbarWindow,
) -> Option<MonitorIdentity> {
    let point = POINT {
        x: taskbar.rect.left + (taskbar.rect.right - taskbar.rect.left) / 2,
        y: taskbar.rect.top + (taskbar.rect.bottom - taskbar.rect.top) / 2,
    };
    monitor_identity_from_handle(MonitorFromPoint(point, MONITOR_DEFAULTTONULL))
}

unsafe fn monitor_identity_for_window(hwnd: HWND) -> Option<MonitorIdentity> {
    monitor_identity_from_handle(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL))
}

unsafe fn monitor_identity_for_rect(rect: &RECT) -> Option<MonitorIdentity> {
    monitor_identity_from_handle(MonitorFromRect(rect, MONITOR_DEFAULTTONULL))
}

unsafe fn monitor_effective_dpi(monitor: HMONITOR) -> u32 {
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() {
        dpi_x.max(96)
    } else {
        GetDpiForSystem().max(96)
    }
}

// The legacy format stored only derived top-left coordinates/offsets, so it
// cannot distinguish a preset that drifted after a content change from a
// nearby manual position. Keep the one-time inference deliberately bounded:
// it repairs the known preset drift range without collapsing positions well
// inside the taskbar or work area.
const LEGACY_PRESET_DRIFT_TOLERANCE_DIP: i32 = 160;

fn migrate_legacy_widget_placement(
    default_position: WidgetDefaultPosition,
    legacy_tray_offset: i32,
    taskbar_rect: PlacementRect,
    tray_left: i32,
    widget_width: i32,
    dpi: u32,
    taskbar_monitor: &MonitorIdentity,
) -> WidgetPlacement {
    let preset = match default_position {
        WidgetDefaultPosition::PrimaryTaskbarLeft => WidgetPlacement::PrimaryLeft,
        WidgetDefaultPosition::PrimaryTaskbarRight => WidgetPlacement::PrimaryRight,
    };
    let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
    let effective_offset = legacy_tray_offset.clamp(0, max_offset);
    let legacy_left = tray_left - widget_width - effective_offset;
    let left_gap = legacy_left - taskbar_rect.left;
    let looks_like_preset = match default_position {
        WidgetDefaultPosition::PrimaryTaskbarLeft => {
            left_gap <= placement::scale_dip(LEGACY_PRESET_DRIFT_TOLERANCE_DIP, dpi)
        }
        WidgetDefaultPosition::PrimaryTaskbarRight => {
            legacy_tray_offset <= placement::scale_dip(2, dpi)
        }
    };
    if looks_like_preset && taskbar_monitor.is_primary {
        preset
    } else {
        let (anchor, gap_dip) = placement::custom_widget_anchor(
            taskbar_rect.left,
            tray_left,
            legacy_left,
            widget_width,
            dpi,
        );
        WidgetPlacement::Custom {
            monitor: taskbar_monitor.key(),
            anchor,
            gap_dip,
        }
    }
}

fn migrate_legacy_floating_placement(
    default_position: FloatingDefaultPosition,
    legacy_rect: PlacementRect,
    work: PlacementRect,
    width: i32,
    height: i32,
    dpi: u32,
    monitor: &MonitorIdentity,
) -> FloatingPlacement {
    let preset = match default_position {
        FloatingDefaultPosition::PrimaryBottomLeft => FloatingPlacement::PrimaryBottomLeft,
        FloatingDefaultPosition::PrimaryBottomRight => FloatingPlacement::PrimaryBottomRight,
    };
    let expected = placement::resolve_floating_rect(&preset, work, width, height, dpi);
    let tolerance = placement::scale_dip(LEGACY_PRESET_DRIFT_TOLERANCE_DIP, dpi);
    if (expected.left - legacy_rect.left).abs() <= tolerance
        && (expected.top - legacy_rect.top).abs() <= tolerance
        && monitor.is_primary
    {
        preset
    } else {
        let (horizontal_anchor, vertical_anchor, horizontal_gap_dip, vertical_gap_dip) =
            placement::custom_floating_anchors(work, legacy_rect, dpi);
        FloatingPlacement::Custom {
            monitor: monitor.key(),
            horizontal_anchor,
            vertical_anchor,
            horizontal_gap_dip,
            vertical_gap_dip,
        }
    }
}

fn migrate_legacy_placements_if_needed() {
    let (
        widget_needs_migration,
        widget_default_position,
        legacy_tray_offset,
        taskbar_hwnd,
        taskbar_monitor,
        legacy_taskbar_binding_matches,
        floating_needs_migration,
        floating_default_position,
        floating_x,
        floating_y,
    ) = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        (
            state.widget_placement_needs_migration,
            state.widget_default_position,
            state.tray_offset,
            state.taskbar_hwnd,
            state.taskbar_monitor.clone(),
            state.taskbar_index == state.preferred_taskbar_index,
            state.floating_placement_needs_migration,
            state.floating_default_position,
            state.floating_x,
            state.floating_y,
        )
    };
    if !widget_needs_migration && !floating_needs_migration {
        return;
    }

    let migrated_widget = if widget_needs_migration {
        if !legacy_taskbar_binding_matches {
            None
        } else {
            match (
                taskbar_hwnd,
                taskbar_hwnd.and_then(native_interop::get_taskbar_rect),
                taskbar_monitor,
            ) {
                (Some(taskbar_hwnd), Some(taskbar_rect), Some(taskbar_monitor)) => {
                    let _dpi_scope = DpiScope::for_window(taskbar_hwnd);
                    let dpi = unsafe { GetDpiForWindow(taskbar_hwnd) }.max(96);
                    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
                    let widget_width = total_widget_width();
                    Some(migrate_legacy_widget_placement(
                        widget_default_position,
                        legacy_tray_offset,
                        placement_rect(taskbar_rect),
                        tray_left,
                        widget_width,
                        dpi,
                        &taskbar_monitor,
                    ))
                }
                _ => None,
            }
        }
    } else {
        None
    };

    let migrated_floating = if floating_needs_migration {
        let preset = match floating_default_position {
            FloatingDefaultPosition::PrimaryBottomLeft => FloatingPlacement::PrimaryBottomLeft,
            FloatingDefaultPosition::PrimaryBottomRight => FloatingPlacement::PrimaryBottomRight,
        };
        match (floating_x, floating_y) {
            (Some(x), Some(y)) => unsafe {
                let probe_rect = RECT {
                    left: x,
                    top: y,
                    right: x.saturating_add(1),
                    bottom: y.saturating_add(1),
                };
                let monitor_handle = MonitorFromRect(&probe_rect, MONITOR_DEFAULTTONULL);
                if monitor_handle.is_invalid() {
                    None
                } else {
                    let dpi = monitor_effective_dpi(monitor_handle);
                    let _dpi_scope = DpiScope::new(dpi);
                    let (width, height) = floating_monitor_size(None);
                    let legacy_rect = RECT {
                        left: x,
                        top: y,
                        right: x + width,
                        bottom: y + height,
                    };
                    let identity = monitor_identity_from_handle(monitor_handle);
                    let work = monitor_work_area(monitor_handle);
                    match (identity, work) {
                        (Some(identity), Some(work)) => Some(migrate_legacy_floating_placement(
                            floating_default_position,
                            placement_rect(legacy_rect),
                            placement_rect(work),
                            width,
                            height,
                            dpi,
                            &identity,
                        )),
                        _ => None,
                    }
                }
            },
            _ => Some(preset),
        }
    } else {
        None
    };
    let migrated_any = migrated_widget.is_some() || migrated_floating.is_some();

    {
        let mut state = lock_state();
        if let Some(state) = state.as_mut() {
            if let Some(placement) = migrated_widget {
                state.widget_placement = placement;
                state.widget_placement_needs_migration = false;
            }
            if let Some(placement) = migrated_floating {
                state.floating_placement = placement;
                state.floating_placement_needs_migration = false;
            }
        }
    }
    if migrated_any {
        diagnose::log("placement settings migrated to semantic anchors");
        save_state_settings();
    }
}

fn secondary_monitor_disappeared(
    previous: Option<&MonitorIdentity>,
    active: &[MonitorIdentity],
) -> bool {
    previous.is_some_and(|previous| {
        !previous.is_primary && !active.iter().any(|monitor| monitor.matches(previous))
    })
}

fn taskbar_at_point(pt: POINT) -> Option<(usize, native_interop::TaskbarWindow)> {
    native_interop::find_taskbars()
        .into_iter()
        .enumerate()
        .find(|(_, taskbar)| {
            pt.x >= taskbar.rect.left
                && pt.x < taskbar.rect.right
                && pt.y >= taskbar.rect.top
                && pt.y < taskbar.rect.bottom
        })
}

fn taskbar_index_for_placement(placement: &WidgetPlacement, legacy_index: usize) -> usize {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        return 0;
    }
    let identities = taskbars
        .iter()
        .map(|taskbar| unsafe { monitor_identity_for_taskbar(taskbar) })
        .collect::<Vec<_>>();
    let primary_index = identities
        .iter()
        .position(|identity| {
            identity
                .as_ref()
                .is_some_and(|identity| identity.is_primary)
        })
        .unwrap_or_else(|| legacy_index.min(taskbars.len().saturating_sub(1)));
    match placement {
        WidgetPlacement::PrimaryLeft | WidgetPlacement::PrimaryRight => primary_index,
        WidgetPlacement::Custom { monitor, .. } => identities
            .iter()
            .position(|identity| {
                identity
                    .as_ref()
                    .is_some_and(|identity| identity.matches_key(monitor))
            })
            .unwrap_or(primary_index),
    }
}

fn tray_left_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT) -> i32 {
    let mut tray_left = taskbar_rect.right;
    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }
    tray_left
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    if !updater::update_channel_configured() {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        }
        return;
    }
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            arm_timer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), "update check");
        }
    }
}

fn approximately(actual: u64, expected: u64) -> bool {
    actual >= expected.saturating_mul(95) / 100 && actual <= expected.saturating_mul(105) / 100
}

fn usage_window_label(window: &UsageWindow, strings: Strings) -> String {
    if let Some(seconds) = window.duration_seconds {
        if approximately(seconds, 5 * 60 * 60) {
            return strings.session_window.to_string();
        }
        if approximately(seconds, 24 * 60 * 60) {
            return "1d".to_string();
        }
        if approximately(seconds, 7 * 24 * 60 * 60) {
            return strings.weekly_window.to_string();
        }
        if approximately(seconds, 30 * 24 * 60 * 60) {
            return "30d".to_string();
        }
        if approximately(seconds, 365 * 24 * 60 * 60) {
            return "365d".to_string();
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
    }

    window
        .source_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(8).collect())
        .unwrap_or_else(|| strings.quota_window.to_string())
}

fn usage_window_dividers(window: &UsageWindow) -> i32 {
    let Some(seconds) = window.duration_seconds else {
        return 1;
    };
    let units = if seconds <= 24 * 60 * 60 && seconds % (60 * 60) == 0 {
        seconds / (60 * 60)
    } else if seconds % (24 * 60 * 60) == 0 {
        seconds / (24 * 60 * 60)
    } else {
        1
    };
    units.clamp(1, 10) as i32
}

fn placeholder_widget() -> ProviderWidgetData {
    ProviderWidgetData {
        windows: vec![WidgetUsageWindow { percent: None }],
    }
}

fn provider_widget_from_usage(usage: Option<&UsageData>) -> ProviderWidgetData {
    let Some(usage) = usage.filter(|usage| !usage.is_empty()) else {
        return placeholder_widget();
    };

    ProviderWidgetData {
        windows: compact_view::compact_usage_windows(usage)
            .into_iter()
            .map(|window| WidgetUsageWindow {
                percent: Some(window.percentage.clamp(0.0, 100.0)),
            })
            .collect(),
    }
}

fn set_widget_placeholders(state: &mut AppState, text: &str) {
    state.claude_widget = placeholder_widget();
    state.codex_widget = placeholder_widget();
    state.antigravity_widget = placeholder_widget();
    state.grok_widget = placeholder_widget();
    state.compact_vm = compact_view::placeholder_model(
        text,
        &state.provider_order,
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
        state.show_grok,
    );
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let data = state.data.as_ref();
    state.claude_widget = provider_widget_from_usage(
        data.and_then(|data| data.usage(tray_icon::TrayIconKind::Claude)),
    );
    state.codex_widget = provider_widget_from_usage(
        data.and_then(|data| data.usage(tray_icon::TrayIconKind::Codex)),
    );
    state.antigravity_widget = provider_widget_from_usage(
        data.and_then(|data| data.usage(tray_icon::TrayIconKind::Antigravity)),
    );
    state.grok_widget =
        provider_widget_from_usage(data.and_then(|data| data.usage(tray_icon::TrayIconKind::Grok)));
    state.compact_vm = compact_view::build(
        data,
        strings,
        &state.provider_order,
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
        state.show_grok,
    );

    let global_status = |kind| {
        state
            .last_error
            .map(|error| provider_status_for_kind(kind, error))
    };
    let claude_status = data
        .and_then(|data| data.error(tray_icon::TrayIconKind::Claude))
        .or_else(|| global_status(tray_icon::TrayIconKind::Claude));
    let codex_status = data
        .and_then(|data| data.error(tray_icon::TrayIconKind::Codex))
        .or_else(|| global_status(tray_icon::TrayIconKind::Codex));
    let antigravity_status = data
        .and_then(|data| data.error(tray_icon::TrayIconKind::Antigravity))
        .or_else(|| global_status(tray_icon::TrayIconKind::Antigravity));
    let claude_updated_unix =
        data.and_then(|data| data.provider(tray_icon::TrayIconKind::Claude).updated_unix);
    let codex_updated_unix =
        data.and_then(|data| data.provider(tray_icon::TrayIconKind::Codex).updated_unix);
    let antigravity_updated_unix = data.and_then(|data| {
        data.provider(tray_icon::TrayIconKind::Antigravity)
            .updated_unix
    });
    let grok_status = data
        .and_then(|data| data.error(tray_icon::TrayIconKind::Grok))
        .or_else(|| global_status(tray_icon::TrayIconKind::Grok));
    let grok_updated_unix =
        data.and_then(|data| data.provider(tray_icon::TrayIconKind::Grok).updated_unix);
    let refresh_states = state.provider_refresh_states;
    let poll_interval_ms = state.poll_interval_ms;
    let now_unix = now_unix_secs();
    for provider in &mut state.compact_vm.providers {
        let (status, updated_unix) = match provider.kind {
            tray_icon::TrayIconKind::Claude => (claude_status, claude_updated_unix),
            tray_icon::TrayIconKind::Codex => (codex_status, codex_updated_unix),
            tray_icon::TrayIconKind::Antigravity => (antigravity_status, antigravity_updated_unix),
            tray_icon::TrayIconKind::Grok => (grok_status, grok_updated_unix),
        };
        provider.attention = compact_attention_for_provider_status(
            provider.attention,
            status,
            refresh_states.for_kind(provider.kind),
            updated_unix,
            poll_interval_ms,
            now_unix,
        );
    }
}

fn compact_stale_after_secs(poll_interval_ms: u32) -> u64 {
    let twice_interval_ms = u64::from(poll_interval_ms)
        .saturating_mul(2)
        .saturating_add(999);
    (twice_interval_ms / 1_000).max(COMPACT_STALE_MIN_AGE_SECS)
}

fn provider_refresh_is_stale(
    status: Option<ProviderStatus>,
    refresh_state: ProviderRefreshState,
    updated_unix: Option<u64>,
    poll_interval_ms: u32,
    now_unix: u64,
) -> bool {
    let freshness_reference = updated_unix.or(refresh_state.unavailable_since_unix);
    let too_old = freshness_reference.is_some_and(|reference| {
        now_unix.saturating_sub(reference) >= compact_stale_after_secs(poll_interval_ms)
    });
    match status {
        Some(ProviderStatus::NetworkUnavailable | ProviderStatus::RequestFailed) => {
            refresh_state.consecutive_failures >= COMPACT_REQUEST_FAILURE_THRESHOLD || too_old
        }
        Some(ProviderStatus::RateLimited) => too_old,
        _ => false,
    }
}

fn provider_stale_transition_delay(
    status: Option<ProviderStatus>,
    refresh_state: ProviderRefreshState,
    updated_unix: Option<u64>,
    poll_interval_ms: u32,
    now_unix: u64,
) -> Option<Duration> {
    if !matches!(
        status,
        Some(
            ProviderStatus::RateLimited
                | ProviderStatus::NetworkUnavailable
                | ProviderStatus::RequestFailed
        )
    ) {
        return None;
    }
    if !matches!(status, Some(ProviderStatus::RateLimited))
        && refresh_state.consecutive_failures >= COMPACT_REQUEST_FAILURE_THRESHOLD
    {
        return None;
    }
    let reference = updated_unix.or(refresh_state.unavailable_since_unix)?;
    let threshold = compact_stale_after_secs(poll_interval_ms);
    let elapsed = now_unix.saturating_sub(reference);
    (elapsed < threshold).then(|| Duration::from_secs(threshold - elapsed))
}

fn compact_attention_for_provider_status(
    current: compact_view::Attention,
    status: Option<ProviderStatus>,
    refresh_state: ProviderRefreshState,
    updated_unix: Option<u64>,
    poll_interval_ms: u32,
    now_unix: u64,
) -> compact_view::Attention {
    match status {
        Some(ProviderStatus::AuthenticationFailed) => compact_view::Attention::ActionRequired,
        // See `compact_view::provider_view`: never signed in is a resting
        // state, so it keeps whatever the quota values already warranted.
        Some(ProviderStatus::NotSignedIn) => current,
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        ) if provider_refresh_is_stale(
            status,
            refresh_state,
            updated_unix,
            poll_interval_ms,
            now_unix,
        ) =>
        {
            compact_view::Attention::Stale
        }
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        ) => current,
        None => current,
    }
}

#[allow(clippy::too_many_arguments)]
fn update_provider_refresh_state(
    state: &mut ProviderRefreshState,
    provider: &str,
    enabled: bool,
    attempted: bool,
    status: Option<ProviderStatus>,
    has_history: bool,
    retry_after_ms: Option<u32>,
    poll_interval_ms: u32,
    now_unix: u64,
    now: Instant,
) -> bool {
    if !enabled {
        *state = ProviderRefreshState::default();
        return false;
    }
    if !attempted {
        return false;
    }

    let previous_failures = state.consecutive_failures;
    // Only an existing credential that cannot be used is worth a balloon -
    // rejected by the provider, or unreadable here; see
    // `ProviderStatus::warrants_credential_alert`.
    let auth_transition =
        status.is_some_and(ProviderStatus::warrants_credential_alert) && !state.auth_failure_active;
    match status {
        None => {
            if previous_failures > 0 {
                diagnose::log(format!(
                    "{provider} usage poll recovered after {previous_failures} consecutive transient failure(s)"
                ));
            }
            *state = ProviderRefreshState::default();
        }
        Some(ProviderStatus::AuthenticationFailed) => {
            state.consecutive_failures = 0;
            state.unavailable_since_unix = None;
            state.rate_limit_until = None;
            state.auth_failure_active = true;
        }
        // Never signed in parks the provider the same way, but it is not an
        // active authentication failure: leaving the flag clear lets a later
        // rejection of a newly added credential still register as a
        // transition and raise its one balloon.
        Some(ProviderStatus::NotSignedIn) => {
            state.consecutive_failures = 0;
            state.unavailable_since_unix = None;
            state.rate_limit_until = None;
            state.auth_failure_active = false;
        }
        Some(ProviderStatus::RateLimited) => {
            state.auth_failure_active = false;
            if !has_history && state.unavailable_since_unix.is_none() {
                state.unavailable_since_unix = Some(now_unix);
            }
            let retry_ms = rate_limit_retry_ms(retry_after_ms, poll_interval_ms);
            state.rate_limit_until = Some(now + Duration::from_millis(u64::from(retry_ms)));
            diagnose::log(format!(
                "{provider} usage poll rate limited; provider cooldown={}s",
                retry_ms / 1_000
            ));
        }
        Some(ProviderStatus::NetworkUnavailable | ProviderStatus::RequestFailed) => {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            state.auth_failure_active = false;
            state.rate_limit_until = None;
            if !has_history && state.unavailable_since_unix.is_none() {
                state.unavailable_since_unix = Some(now_unix);
            }
            diagnose::log(format!(
                "{provider} usage poll consecutive transient failures={}",
                state.consecutive_failures
            ));
        }
    }
    auth_transition
}

fn provider_has_history(data: Option<&AppUsageData>, kind: tray_icon::TrayIconKind) -> bool {
    data.and_then(|data| data.provider(kind).updated_unix)
        .is_some()
}

fn update_provider_refresh_states(
    state: &mut AppState,
    plan: PollPassPlan,
    data: &AppUsageData,
    now_unix: u64,
    now: Instant,
) -> Vec<tray_icon::TrayIconKind> {
    let previous = state.data.clone();
    update_refresh_states_for_pass(
        &mut state.provider_refresh_states,
        previous.as_ref(),
        state.poll_interval_ms,
        plan,
        data,
        now_unix,
        now,
    )
}

/// Advance the per-provider failure-recovery state for one poll pass.
///
/// Takes the pieces rather than the whole `AppState` so the pass can be tested
/// without building a window: a provider silently missing from here still
/// polls and still shows its error, but records no cooldown, counts no
/// consecutive failures, never goes stale, and never raises its one
/// authentication balloon.
#[allow(clippy::too_many_arguments)]
fn update_refresh_states_for_pass(
    states: &mut ProviderRefreshStates,
    previous: Option<&AppUsageData>,
    poll_interval_ms: u32,
    plan: PollPassPlan,
    data: &AppUsageData,
    now_unix: u64,
    now: Instant,
) -> Vec<tray_icon::TrayIconKind> {
    let shown = plan.shown_flags();
    let mut transitions = Vec::new();

    for kind in tray_icon::TrayIconKind::ALL {
        let has_history =
            provider_has_history(previous, kind) || provider_has_history(Some(data), kind);
        let refresh_state = states.state_mut(kind);
        if update_provider_refresh_state(
            refresh_state,
            kind.diagnostic_label(),
            shown[kind.index()],
            plan.polled(kind),
            data.error(kind),
            has_history,
            data.provider(kind).retry_after_ms,
            poll_interval_ms,
            now_unix,
            now,
        ) {
            transitions.push(kind);
        }
    }

    transitions
}

fn merge_missing_provider_data(
    previous: Option<&AppUsageData>,
    mut next: AppUsageData,
    keep: [bool; tray_icon::TrayIconKind::COUNT],
) -> AppUsageData {
    for kind in tray_icon::TrayIconKind::ALL {
        if !keep[kind.index()] {
            *next.provider_mut(kind) = Default::default();
        }
    }
    if let Some(previous) = previous {
        for kind in tray_icon::TrayIconKind::ALL {
            if keep[kind.index()] && next.provider(kind).usage.is_none() {
                let carried = previous.provider(kind).clone();
                let slot = next.provider_mut(kind);
                slot.usage = carried.usage;
                slot.updated_unix = carried.updated_unix;
            }
        }
    }
    next
}

fn stamp_provider_updates(data: &mut AppUsageData, updated_unix: u64) {
    for kind in tray_icon::TrayIconKind::ALL {
        let slot = data.provider_mut(kind);
        if slot.usage.is_some() && slot.error.is_none() {
            slot.updated_unix = Some(updated_unix);
        }
    }
}

#[derive(Clone)]
struct ResetNotification {
    kind: tray_icon::TrayIconKind,
    title: String,
    body: String,
}

fn collect_reset_notifications(
    previous: Option<&AppUsageData>,
    next: &AppUsageData,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    strings: Strings,
) -> Vec<ResetNotification> {
    if !notify_session_reset && !notify_weekly_reset {
        return Vec::new();
    }
    let Some(previous) = previous else {
        return Vec::new();
    };

    let mut notifications = Vec::new();
    for kind in tray_icon::TrayIconKind::ALL {
        push_provider_reset_notifications(
            &mut notifications,
            kind,
            localized_provider_name(kind, strings),
            previous.usage(kind),
            next.usage(kind),
            notify_session_reset,
            notify_weekly_reset,
            strings,
        );
    }
    notifications
}

// Keeping the provider/reset inputs explicit makes the notification policy
// auditable; wrapping them in a one-use options object would add indirection.
#[allow(clippy::too_many_arguments)]
fn push_provider_reset_notifications(
    notifications: &mut Vec<ResetNotification>,
    kind: tray_icon::TrayIconKind,
    provider_label: &str,
    previous: Option<&UsageData>,
    next: Option<&UsageData>,
    notify_session_reset: bool,
    notify_weekly_reset: bool,
    strings: Strings,
) {
    let (Some(previous), Some(next)) = (previous, next) else {
        return;
    };

    for next_window in &next.windows {
        let Some(previous_window) = previous
            .windows
            .iter()
            .find(|previous_window| same_usage_window(previous_window, next_window))
        else {
            continue;
        };
        let enabled = if is_long_usage_window(next_window) {
            notify_weekly_reset
        } else {
            notify_session_reset
        };
        if enabled && reset_window_refreshed(previous_window, next_window) {
            notifications.push(make_reset_notification(
                kind,
                provider_label,
                &usage_window_label(next_window, strings),
                strings,
            ));
        }
    }
}

fn same_usage_window(left: &UsageWindow, right: &UsageWindow) -> bool {
    match (left.duration_seconds, right.duration_seconds) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left.source_label.as_deref() == right.source_label.as_deref(),
        _ => false,
    }
}

fn is_long_usage_window(window: &UsageWindow) -> bool {
    window
        .duration_seconds
        .is_some_and(|seconds| seconds >= 6 * 24 * 60 * 60)
}

fn reset_window_refreshed(previous: &UsageWindow, next: &UsageWindow) -> bool {
    let (Some(previous_reset), Some(next_reset)) = (previous.resets_at, next.resets_at) else {
        return false;
    };

    SystemTime::now().duration_since(previous_reset).is_ok()
        && next_reset != previous_reset
        && next_reset.duration_since(previous_reset).is_ok()
}

fn make_reset_notification(
    kind: tray_icon::TrayIconKind,
    provider_label: &str,
    window_label: &str,
    strings: Strings,
) -> ResetNotification {
    let body = strings
        .reset_notification_body
        .replace("{provider}", provider_label)
        .replace("{window}", window_label);
    ResetNotification {
        kind,
        title: strings.reset_notification_title.to_string(),
        body,
    }
}
fn rate_limit_retry_ms(retry_after_ms: Option<u32>, poll_interval_ms: u32) -> u32 {
    let requested = retry_after_ms.unwrap_or_else(|| poll_interval_ms.max(RATE_LIMIT_MIN_RETRY_MS));
    requested
        .max(poll_interval_ms)
        .clamp(RATE_LIMIT_MIN_RETRY_MS, RATE_LIMIT_MAX_RETRY_MS)
}

#[derive(Clone, Copy, Debug)]
struct PollPassPlan {
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    poll_claude_code: bool,
    poll_codex: bool,
    poll_antigravity: bool,
    poll_grok: bool,
    claude_cooldown_ms: Option<u32>,
    codex_cooldown_ms: Option<u32>,
    antigravity_cooldown_ms: Option<u32>,
    grok_cooldown_ms: Option<u32>,
}

impl PollPassPlan {
    fn from_state(state: &AppState, now: Instant) -> Self {
        let claude_cooldown_ms = provider_cooldown_remaining_ms(
            state
                .provider_refresh_states
                .for_kind(tray_icon::TrayIconKind::Claude),
            now,
        );
        let codex_cooldown_ms = provider_cooldown_remaining_ms(
            state
                .provider_refresh_states
                .for_kind(tray_icon::TrayIconKind::Codex),
            now,
        );
        let antigravity_cooldown_ms = provider_cooldown_remaining_ms(
            state
                .provider_refresh_states
                .for_kind(tray_icon::TrayIconKind::Antigravity),
            now,
        );
        let grok_cooldown_ms = provider_cooldown_remaining_ms(
            state
                .provider_refresh_states
                .for_kind(tray_icon::TrayIconKind::Grok),
            now,
        );
        // Mirrors `state_credential_poll_selection`; see the note there.
        let selection = credential_read_scope(state, CredentialReadReason::Poll).flags();
        let show_claude_code = selection[tray_icon::TrayIconKind::Claude.index()];
        let show_codex = selection[tray_icon::TrayIconKind::Codex.index()];
        let show_antigravity = selection[tray_icon::TrayIconKind::Antigravity.index()];
        let show_grok = selection[tray_icon::TrayIconKind::Grok.index()];
        Self {
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            poll_claude_code: show_claude_code && claude_cooldown_ms.is_none(),
            poll_codex: show_codex && codex_cooldown_ms.is_none(),
            poll_antigravity: show_antigravity && antigravity_cooldown_ms.is_none(),
            poll_grok: show_grok && grok_cooldown_ms.is_none(),
            claude_cooldown_ms,
            codex_cooldown_ms,
            antigravity_cooldown_ms,
            grok_cooldown_ms,
        }
    }

    /// Per-provider visibility and poll decisions, indexed the same way as
    /// `AppUsageData`. Every consumer that used to spell out three providers
    /// reads these instead, so a new provider cannot be left out of one branch
    /// of the failure handling while being present in another.
    fn shown_flags(self) -> [bool; tray_icon::TrayIconKind::COUNT] {
        [
            self.show_claude_code,
            self.show_codex,
            self.show_antigravity,
            self.show_grok,
        ]
    }

    fn polled(self, kind: tray_icon::TrayIconKind) -> bool {
        [
            self.poll_claude_code,
            self.poll_codex,
            self.poll_antigravity,
            self.poll_grok,
        ][kind.index()]
    }

    fn has_poll_target(self) -> bool {
        self.poll_claude_code || self.poll_codex || self.poll_antigravity || self.poll_grok
    }

    fn apply_skipped_rate_limits(self, data: &mut AppUsageData) {
        for (kind, shown, polled, cooldown_ms) in [
            (
                tray_icon::TrayIconKind::Claude,
                self.show_claude_code,
                self.poll_claude_code,
                self.claude_cooldown_ms,
            ),
            (
                tray_icon::TrayIconKind::Codex,
                self.show_codex,
                self.poll_codex,
                self.codex_cooldown_ms,
            ),
            (
                tray_icon::TrayIconKind::Antigravity,
                self.show_antigravity,
                self.poll_antigravity,
                self.antigravity_cooldown_ms,
            ),
            (
                tray_icon::TrayIconKind::Grok,
                self.show_grok,
                self.poll_grok,
                self.grok_cooldown_ms,
            ),
        ] {
            if shown && !polled {
                let slot = data.provider_mut(kind);
                slot.error = Some(ProviderStatus::RateLimited);
                slot.retry_after_ms = cooldown_ms;
            }
        }
    }
}

fn provider_cooldown_remaining_ms(state: ProviderRefreshState, now: Instant) -> Option<u32> {
    let remaining = state.rate_limit_until?.checked_duration_since(now)?;
    Some(remaining.as_millis().max(1).min(u128::from(u32::MAX)) as u32)
}

fn poll_delay_with_provider_cooldowns(state: &AppState, base_ms: u32, now: Instant) -> u32 {
    tray_icon::TrayIconKind::ALL
        .into_iter()
        .filter_map(|kind| {
            provider_cooldown_remaining_ms(state.provider_refresh_states.for_kind(kind), now)
        })
        .fold(base_ms.max(1), u32::min)
}

/// Whether signing in could clear this failure.
///
/// The mode itself now comes from `credential_watch_mode_for_shown`, which is
/// knowable before the poll runs - that is what lets the credentials be
/// sampled either side of a poll and compared (see `auth_watch_decision`).
///
#[cfg(test)]
fn poll_error_needs_credential_watch(error: poller::PollError) -> bool {
    matches!(
        error,
        poller::PollError::AuthRequired
            | poller::PollError::AuthForbidden
            | poller::PollError::NoCredentials
    )
}
/// Which credentials the shown providers could sign in through.
///
/// Derived from what is shown rather than from what failed, so the same mode
/// can be sampled before and after a poll and the two snapshots compared -
/// see `auth_watch_decision`. Mirrors `credential_watch_mode_for_failure`'s
/// shape for consistency.
fn credential_watch_mode_for_shown(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> Option<poller::CredentialWatchMode> {
    let shown_count =
        show_claude_code as u8 + show_codex as u8 + show_antigravity as u8 + show_grok as u8;
    if shown_count == 0 {
        return None;
    }
    if shown_count > 1 {
        // Name the providers rather than saying "all". The callers pass the
        // shown-and-allowed selection, so a revoked provider is excluded here
        // exactly as it is from a poll and from a detection sweep.
        let mut watched = [false; tray_icon::TrayIconKind::COUNT];
        watched[tray_icon::TrayIconKind::Claude.index()] = show_claude_code;
        watched[tray_icon::TrayIconKind::Codex.index()] = show_codex;
        watched[tray_icon::TrayIconKind::Antigravity.index()] = show_antigravity;
        watched[tray_icon::TrayIconKind::Grok.index()] = show_grok;
        return Some(poller::CredentialWatchMode::Providers(watched));
    }
    if show_codex {
        return Some(poller::CredentialWatchMode::Codex);
    }
    if show_antigravity {
        return Some(poller::CredentialWatchMode::Antigravity);
    }
    if show_grok {
        return Some(poller::CredentialWatchMode::Grok);
    }
    // Claude can use the Windows file or any WSL distribution. Watching only
    // the credential selected for the failed request lets an expired Windows
    // file mask a newly refreshed WSL credential forever.
    Some(poller::CredentialWatchMode::ClaudeSources)
}

/// True when a provider the user can see is asking them to sign in.
///
/// `credential_watch_mode_for_failure` only covers the every-provider-failed
/// case. With two providers enabled, one healthy provider keeps the poll
/// "successful", so a provider whose token expired would otherwise sit on its
/// "sign in" marker until the next poll interval (up to an hour) even though
/// the user re-authenticated seconds later.
fn shown_provider_needs_auth(
    data: &AppUsageData,
    shown: [bool; tray_icon::TrayIconKind::COUNT],
) -> bool {
    tray_icon::TrayIconKind::ALL.into_iter().any(|kind| {
        shown[kind.index()]
            && data
                .error(kind)
                .is_some_and(ProviderStatus::needs_credentials)
    })
}

fn all_shown_providers_need_auth(
    data: &AppUsageData,
    shown: [bool; tray_icon::TrayIconKind::COUNT],
) -> bool {
    // Both credential states park a provider until the user supplies a login,
    // so either one counts here: pausing the poll and watching the credential
    // sources is exactly as correct for "never signed in" as for "rejected".
    shown.iter().any(|shown| *shown)
        && tray_icon::TrayIconKind::ALL.into_iter().all(|kind| {
            !shown[kind.index()]
                || data
                    .error(kind)
                    .is_some_and(ProviderStatus::needs_credentials)
        })
}

/// What to do with the credential watch once a poll has been evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthWatchDecision {
    /// Nothing shown needs auth: stop watching.
    Stop,
    /// Watch from the post-poll baseline.
    Watch,
    /// The credentials changed while the poll was in flight, so this result
    /// was decided against credentials that no longer exist. Watching from
    /// the post-poll baseline would compare the refreshed signature against
    /// itself and never fire, leaving the stale "sign in" marker up until the
    /// next interval - poll again instead.
    WatchAndPollNow,
}

fn auth_watch_decision(
    shown_provider_needs_auth: bool,
    pre_poll: Option<&poller::CredentialWatchSnapshot>,
    post_poll: Option<&poller::CredentialWatchSnapshot>,
) -> AuthWatchDecision {
    if !shown_provider_needs_auth {
        return AuthWatchDecision::Stop;
    }
    match (pre_poll, post_poll) {
        (Some(pre), Some(post)) if pre != post => AuthWatchDecision::WatchAndPollNow,
        _ => AuthWatchDecision::Watch,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollTimerAction {
    PollNow,
    CheckCredentials,
}

fn poll_failure_needs_auth_rejection_recheck(
    error: poller::PollError,
    data: &AppUsageData,
) -> bool {
    matches!(
        error,
        poller::PollError::AuthRequired | poller::PollError::AuthForbidden
    ) || data.remote_auth_rejection
}

fn auth_recovery_recheck_deadline(
    needs_remote_recheck: bool,
    poll_interval_ms: u32,
    now: Instant,
) -> Option<Instant> {
    let delay_ms = if needs_remote_recheck {
        poller::AUTH_REJECTION_RECHECK_MS
    } else {
        poll_interval_ms.max(1)
    };
    Some(now + Duration::from_millis(u64::from(delay_ms)))
}

fn poll_timer_action(
    auth_error_paused_polling: bool,
    auth_recovery_recheck_deadline: Option<Instant>,
    now: Instant,
) -> PollTimerAction {
    if !auth_error_paused_polling
        || auth_recovery_recheck_deadline.is_some_and(|deadline| now >= deadline)
    {
        PollTimerAction::PollNow
    } else {
        PollTimerAction::CheckCredentials
    }
}

fn paused_poll_timer_interval_ms(
    poll_interval_ms: u32,
    auth_recovery_recheck_deadline: Option<Instant>,
    now: Instant,
) -> u32 {
    let Some(deadline) = auth_recovery_recheck_deadline else {
        return poll_interval_ms;
    };
    let remaining = deadline.saturating_duration_since(now);
    let remaining_ms = u32::try_from(remaining.as_millis())
        .unwrap_or(u32::MAX)
        .saturating_add(u32::from(remaining.subsec_nanos() % 1_000_000 != 0))
        .max(1);
    poll_interval_ms.min(remaining_ms)
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn dialog_owner_hwnd() -> HWND {
    live_broadcast_helper_hwnd().unwrap_or_default()
}

fn show_info_message(title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            dialog_owner_hwnd(),
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
        );
    }
}

fn credential_consent_fallback_message_box_style() -> MESSAGEBOX_STYLE {
    MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND
}

/// Give the consent dialog this application's own icon.
///
/// The dialog is deliberately created with no parent so the user can minimize
/// it and decide later, which also means there is no parent window whose icon
/// it could inherit: left alone it shows a generic shell icon and does not
/// look like it came from Gengchou. A task dialog's title-bar icon is not
/// covered by `hMainIcon` - that only fills the content area - so it is set
/// here, once the window exists.
unsafe extern "system" fn credential_consent_task_dialog_callback(
    hwnd: HWND,
    msg: TASKDIALOG_NOTIFICATIONS,
    _wparam: WPARAM,
    _lparam: LPARAM,
    _data: isize,
) -> windows::core::HRESULT {
    if msg == TDN_CREATED {
        let (large_icon, small_icon) = cached_app_icons();
        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }
    }
    windows::core::HRESULT(0)
}

fn credential_consent_task_dialog_flags() -> TASKDIALOG_FLAGS {
    TDF_ALLOW_DIALOG_CANCELLATION | TDF_CAN_BE_MINIMIZED
}

fn credential_consent_task_dialog_buttons() -> TASKDIALOG_COMMON_BUTTON_FLAGS {
    TDCBF_YES_BUTTON | TDCBF_NO_BUTTON
}

fn credential_consent_default_button() -> i32 {
    IDNO.0
}

type TaskDialogIndirectFn =
    unsafe extern "system" fn(*const TASKDIALOGCONFIG, *mut i32, *mut i32, *mut BOOL) -> HRESULT;

/// Resolve the version-6 common-controls entry point at runtime. Keeping it
/// out of the PE import table lets the process start and use the MessageBox
/// fallback even if Windows cannot activate Comctl32 v6 for this launch.
unsafe fn call_task_dialog_indirect_if_available(
    config: &TASKDIALOGCONFIG,
    selected_button: &mut i32,
) -> Option<HRESULT> {
    let module = GetModuleHandleW(windows::core::w!("comctl32.dll")).ok()?;
    let raw = GetProcAddress(module, windows::core::s!("TaskDialogIndirect"))?;
    let task_dialog: TaskDialogIndirectFn = std::mem::transmute(raw);
    Some(task_dialog(
        config,
        selected_button,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ))
}

/// Whether credentials may be read: the one-time prompt covering every
/// provider, and the per-provider switch the user can revoke from the context
/// menu, must both be on.
fn credential_access_granted(consent_granted: bool, provider_allowed: bool) -> bool {
    consent_granted && provider_allowed
}

fn provider_pending_flag(state: &AppState, kind: tray_icon::TrayIconKind) -> bool {
    match kind {
        tray_icon::TrayIconKind::Claude => state.claude_credential_access_pending,
        tray_icon::TrayIconKind::Codex => state.codex_credential_access_pending,
        tray_icon::TrayIconKind::Antigravity => state.antigravity_credential_access_pending,
        tray_icon::TrayIconKind::Grok => state.grok_credential_access_pending,
    }
}

/// A provider that is visible but has no access sits on a placeholder with no
/// explanation, so the detail popup says how to turn it back on.
///
/// Requires the prompt to have been answered: the consent dialog is
/// deliberately minimizable rather than modal, so the widget stays visible
/// behind it, and telling the user they declined while the question is still
/// on screen would be wrong.
fn access_is_revoked(shown: bool, consent_decided: bool, has_access: bool) -> bool {
    shown && consent_decided && !has_access
}

fn provider_has_credential_access(state: &AppState, kind: tray_icon::TrayIconKind) -> bool {
    credential_access_granted(
        state.credential_consent_granted,
        match kind {
            tray_icon::TrayIconKind::Claude => state.allow_claude_credentials,
            tray_icon::TrayIconKind::Codex => state.allow_codex_credentials,
            tray_icon::TrayIconKind::Antigravity => state.allow_antigravity_credentials,
            tray_icon::TrayIconKind::Grok => state.allow_grok_credentials,
        },
    )
}

fn provider_is_shown(state: &AppState, kind: tray_icon::TrayIconKind) -> bool {
    match kind {
        tray_icon::TrayIconKind::Claude => state.show_claude_code,
        tray_icon::TrayIconKind::Codex => state.show_codex,
        tray_icon::TrayIconKind::Antigravity => state.show_antigravity,
        tray_icon::TrayIconKind::Grok => state.show_grok,
    }
}

fn provider_access_revoked(state: &AppState, kind: tray_icon::TrayIconKind) -> bool {
    !provider_pending_flag(state, kind)
        && access_is_revoked(
            provider_is_shown(state, kind),
            state.credential_consent_decided,
            provider_has_credential_access(state, kind),
        )
}

/// Whether a poll pass has anything to contact. The gate `request_poll_with`
/// uses, named so a test can reach it.
fn poll_selection_has_target(selection: [bool; tray_icon::TrayIconKind::COUNT]) -> bool {
    selection.into_iter().any(|polled| polled)
}

fn state_credential_poll_selection(state: &AppState) -> [bool; tray_icon::TrayIconKind::COUNT] {
    // Must stay in step with `PollPassPlan::from_state`: this decides whether
    // a poll worker starts at all, that decides what the pass targets. If the
    // two disagree a worker can start with nothing to do, or worse, a target
    // can be polled without a running worker to commit it.
    credential_read_scope(state, CredentialReadReason::Poll).flags()
}

fn clear_provider_usage(state: &mut AppState, kind: tray_icon::TrayIconKind) {
    match kind {
        tray_icon::TrayIconKind::Claude => {
            state.provider_refresh_states.claude_code = ProviderRefreshState::default()
        }
        tray_icon::TrayIconKind::Codex => {
            state.provider_refresh_states.codex = ProviderRefreshState::default()
        }
        tray_icon::TrayIconKind::Antigravity => {
            state.provider_refresh_states.antigravity = ProviderRefreshState::default()
        }
        tray_icon::TrayIconKind::Grok => {
            state.provider_refresh_states.grok = ProviderRefreshState::default()
        }
    }
    if let Some(data) = state.data.as_mut() {
        let slot = data.provider_mut(kind);
        slot.usage = None;
        slot.updated_unix = None;
        slot.error = None;
    }
}

fn set_provider_credential_access(kind: tray_icon::TrayIconKind, allowed: bool) {
    let cache_snapshot = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| {
            match kind {
                tray_icon::TrayIconKind::Claude => {
                    s.allow_claude_credentials = allowed;
                    s.claude_credential_access_decided = true;
                    s.claude_credential_access_revoked = !allowed;
                    s.claude_credential_access_pending = false;
                    if allowed {
                        s.show_claude_code = true;
                    }
                }
                tray_icon::TrayIconKind::Codex => {
                    s.allow_codex_credentials = allowed;
                    s.codex_credential_access_decided = true;
                    s.codex_credential_access_revoked = !allowed;
                    s.codex_credential_access_pending = false;
                    if allowed {
                        s.show_codex = true;
                    }
                }
                tray_icon::TrayIconKind::Antigravity => {
                    s.allow_antigravity_credentials = allowed;
                    s.antigravity_credential_access_decided = true;
                    s.antigravity_credential_access_revoked = !allowed;
                    s.antigravity_credential_access_pending = false;
                    if allowed {
                        s.show_antigravity = true;
                    }
                }
                tray_icon::TrayIconKind::Grok => {
                    s.allow_grok_credentials = allowed;
                    s.grok_credential_access_decided = true;
                    s.grok_credential_access_revoked = !allowed;
                    s.grok_credential_access_pending = false;
                    if allowed {
                        s.show_grok = true;
                    }
                }
            }
            if !allowed {
                clear_provider_usage(s, kind);
            }
            s.auth_watch_active = false;
            s.auth_watch_mode = poller::CredentialWatchMode::ClaudeSources;
            s.auth_watch_snapshot.clear();
            set_widget_placeholders(s, "...");
            if s.last_poll_ok {
                refresh_usage_texts(s);
            }
            // Keep this generation bump under the same state lock that the
            // poll worker uses before committing. A revoked provider can
            // therefore never win the gap between changing permission and
            // invalidating an in-flight result.
            POLL_COORDINATOR.invalidate_pending();
            capture_usage_cache_snapshot(s)
        })
    };

    unsafe {
        let _ = KillTimer(poll_controller_hwnd(), TIMER_AUTH_WATCH);
    }
    save_state_settings();
    if let Some(snapshot) = cache_snapshot.as_ref() {
        save_usage_cache(snapshot);
    }
    diagnose::log(format!(
        "{} credential access {}",
        kind.diagnostic_label(),
        if allowed { "granted" } else { "revoked" }
    ));
}

/// Ask once, for every provider at once.
///
/// Naming a provider here would be wrong twice over: the answer covers all of
/// them, and at this point nothing has been detected yet, so the app does not
/// know which ones the machine even has. The exact credential sources are
/// documented in the README privacy section rather than crammed in here.
fn show_credential_consent_prompt(hwnd: HWND, language: LanguageId) -> bool {
    let copy = localization::credential_consent_copy(language);
    let title = copy.title.to_string();
    let message = copy.body.to_string();

    diagnose::log("showing credential access prompt");
    let allowed = unsafe {
        let title_wide = native_interop::wide_str(&title);
        let message_wide = native_interop::wide_str(&message);
        let config = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            // A parent would make the task dialog modal and suppress its
            // minimize button. Keep the consent surface unowned so the user
            // can safely defer the decision without granting access.
            hwndParent: HWND::default(),
            dwFlags: credential_consent_task_dialog_flags(),
            dwCommonButtons: credential_consent_task_dialog_buttons(),
            pszWindowTitle: PCWSTR::from_raw(title_wide.as_ptr()),
            pszMainInstruction: PCWSTR::from_raw(title_wide.as_ptr()),
            pszContent: PCWSTR::from_raw(message_wide.as_ptr()),
            nDefaultButton: credential_consent_default_button(),
            pfCallback: Some(credential_consent_task_dialog_callback),
            ..Default::default()
        };
        let mut selected_button = credential_consent_default_button();
        match call_task_dialog_indirect_if_available(&config, &mut selected_button) {
            Some(result) if result.is_ok() => selected_button == IDYES.0,
            result => {
                diagnose::log(format!(
                    "credential task dialog unavailable ({}); using message box fallback",
                    result
                        .map(|error| windows::core::Error::from(error).to_string())
                        .unwrap_or_else(|| "TaskDialogIndirect entry point missing".to_string())
                ));
                MessageBoxW(
                    hwnd,
                    PCWSTR::from_raw(message_wide.as_ptr()),
                    PCWSTR::from_raw(title_wide.as_ptr()),
                    credential_consent_fallback_message_box_style(),
                ) == IDYES
            }
        }
    };
    diagnose::log(format!(
        "credential access prompt answered: {}",
        if allowed { "allow" } else { "deny" }
    ));
    allowed
}

/// Ask whether a LegacyNeedsReview provider may be read. `Some(true)` allows
/// access, `Some(false)` keeps it closed, `None` leaves the pending state.
/// "Decide later", and the default answer of the pending review dialog.
///
/// Deliberately `IDCANCEL`: the title-bar close button and Esc already produce
/// it, so the visible third choice and the two invisible ways out cannot drift
/// apart. Defaulting to "keep closed" instead would let one stray Enter record
/// a revocation the user never chose - the exact guess this release stopped
/// making on their behalf.
const PENDING_ACCESS_DEFAULT_BUTTON: i32 = IDCANCEL.0;

/// Which answer a pending review dialog button stands for. `None` leaves the
/// provider pending, so an unrecognized id is safe.
fn pending_access_answer(selected_button: i32) -> Option<bool> {
    match selected_button {
        id if id == IDYES.0 => Some(true),
        id if id == IDNO.0 => Some(false),
        _ => None,
    }
}

fn confirm_pending_provider_access(
    hwnd: HWND,
    kind: tray_icon::TrayIconKind,
    language: LanguageId,
) -> Option<bool> {
    let strings = language.strings();
    let provider = localized_provider_name(kind, strings);
    let title = strings.pending_access_title.replace("{provider}", provider);
    let message = strings.pending_access_body.replace("{provider}", provider);
    diagnose::log(format!(
        "showing pending access review for {}",
        kind.diagnostic_label()
    ));
    let answered = unsafe {
        let title_wide = native_interop::wide_str(&title);
        let message_wide = native_interop::wide_str(&message);
        let allow_wide = native_interop::wide_str(strings.access_allow);
        let keep_wide = native_interop::wide_str(strings.access_keep_closed);
        let later_wide = native_interop::wide_str(strings.access_decide_later);
        let buttons = [
            TASKDIALOG_BUTTON {
                nButtonID: IDYES.0,
                pszButtonText: PCWSTR::from_raw(allow_wide.as_ptr()),
            },
            TASKDIALOG_BUTTON {
                nButtonID: IDNO.0,
                pszButtonText: PCWSTR::from_raw(keep_wide.as_ptr()),
            },
            TASKDIALOG_BUTTON {
                nButtonID: PENDING_ACCESS_DEFAULT_BUTTON,
                pszButtonText: PCWSTR::from_raw(later_wide.as_ptr()),
            },
        ];
        let config = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            hwndParent: hwnd,
            dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS,
            pszWindowTitle: PCWSTR::from_raw(title_wide.as_ptr()),
            pszMainInstruction: PCWSTR::from_raw(title_wide.as_ptr()),
            pszContent: PCWSTR::from_raw(message_wide.as_ptr()),
            cButtons: buttons.len() as u32,
            pButtons: buttons.as_ptr(),
            nDefaultButton: PENDING_ACCESS_DEFAULT_BUTTON,
            pfCallback: Some(credential_consent_task_dialog_callback),
            ..Default::default()
        };
        let mut selected_button = PENDING_ACCESS_DEFAULT_BUTTON;
        match call_task_dialog_indirect_if_available(&config, &mut selected_button) {
            Some(result) if result.is_ok() => pending_access_answer(selected_button),
            result => {
                diagnose::log(format!(
                    "pending access task dialog unavailable ({}); using message box fallback",
                    result
                        .map(|error| windows::core::Error::from(error).to_string())
                        .unwrap_or_else(|| "TaskDialogIndirect entry point missing".to_string())
                ));
                // Cancel is the fallback's "decide later": same third answer,
                // and the same default, so neither dialog can revoke a
                // provider on a stray Enter.
                pending_access_answer(
                    MessageBoxW(
                        hwnd,
                        PCWSTR::from_raw(message_wide.as_ptr()),
                        PCWSTR::from_raw(title_wide.as_ptr()),
                        MB_YESNOCANCEL | MB_DEFBUTTON3 | MB_ICONQUESTION,
                    )
                    .0,
                )
            }
        }
    };
    diagnose::log(format!(
        "{} pending access review: {}",
        kind.diagnostic_label(),
        match answered {
            Some(true) => "allow",
            Some(false) => "keep closed",
            None => "cancelled",
        }
    ));
    answered
}

fn review_pending_provider(hwnd: HWND, kind: tray_icon::TrayIconKind) {
    let language = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.language)
            .unwrap_or(LanguageId::English)
    };
    if !ensure_credential_consent(hwnd) {
        return;
    }
    match confirm_pending_provider_access(hwnd, kind, language) {
        Some(true) => set_provider_credential_access(kind, true),
        Some(false) => set_provider_credential_access(kind, false),
        None => {}
    }
}

/// Returns whether any provider's access actually changed, because the caller
/// owes the surfaces a refresh and a poll in that case. The detection pass
/// that follows will not do it: granting already wrote `show` and `allow`, so
/// the pass finds nothing new and returns early.
#[must_use]
fn confirm_pending_providers_for_manual(hwnd: HWND) -> bool {
    let (language, pending) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return false;
        };
        let pending = tray_icon::TrayIconKind::ALL
            .into_iter()
            .filter(|kind| provider_pending_flag(s, *kind))
            .collect::<Vec<_>>();
        (s.language, pending)
    };
    let mut decided = false;
    for kind in pending {
        if let Some(allowed) = confirm_pending_provider_access(hwnd, kind, language) {
            set_provider_credential_access(kind, allowed);
            decided = true;
        }
    }
    decided
}

/// Record the answer to the one-time prompt.
///
/// Declining clears every provider's usage so nothing read under a previous
/// grant stays on screen.
fn set_credential_consent(granted: bool) {
    let cache_snapshot = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| {
            s.credential_consent_granted = granted;
            s.credential_consent_decided = true;
            if !granted {
                for kind in tray_icon::TrayIconKind::ALL {
                    clear_provider_usage(s, kind);
                }
                s.auth_watch_active = false;
                s.auth_watch_snapshot.clear();
            }
            set_widget_placeholders(s, "...");
            POLL_COORDINATOR.invalidate_pending();
            capture_usage_cache_snapshot(s)
        })
    };
    save_state_settings();
    if let Some(snapshot) = cache_snapshot.as_ref() {
        save_usage_cache(snapshot);
    }
    diagnose::log(format!(
        "credential access {}",
        if granted { "granted" } else { "declined" }
    ));
}

/// Show the one-time prompt if it has never been answered, then kick off the
/// first detection pass when it was granted.
fn prompt_for_initial_consent(hwnd: HWND) {
    let language = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        if s.credential_consent_decided {
            return;
        }
        s.language
    };

    let granted = show_credential_consent_prompt(hwnd, language);
    set_credential_consent(granted);
    if granted {
        spawn_provider_detection(DetectionReason::FirstRun);
    }
}

/// Make sure access has been granted before an action that needs it.
///
/// A user who declined the first-run prompt and later asks for a provider by
/// hand is asking for exactly what the prompt covers, so ask again rather
/// than silently upgrading their answer - or worse, leaving the menu toggle
/// looking broken because the global gate is still shut.
fn ensure_credential_consent(hwnd: HWND) -> bool {
    let language = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return false;
        };
        if s.credential_consent_granted {
            return true;
        }
        s.language
    };
    let granted = show_credential_consent_prompt(hwnd, language);
    set_credential_consent(granted);
    granted
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetectionReason {
    /// Straight after access was granted: turn on whatever is installed.
    FirstRun,
    /// Periodic sweep: announce what is new and change nothing else.
    Rescan,
    /// The user asked for a sweep from the menu: turn on what is found.
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialReadReason {
    FirstRun,
    Rescan,
    Manual,
    Poll,
    CredentialWatch,
}

impl From<DetectionReason> for CredentialReadReason {
    fn from(reason: DetectionReason) -> Self {
        match reason {
            DetectionReason::FirstRun => Self::FirstRun,
            DetectionReason::Rescan => Self::Rescan,
            DetectionReason::Manual => Self::Manual,
        }
    }
}

fn credential_read_allowed(
    consent_granted: bool,
    allow: bool,
    revoked: bool,
    pending: bool,
    shown: bool,
    announced: bool,
    reason: CredentialReadReason,
) -> bool {
    if !consent_granted {
        return false;
    }
    match reason {
        CredentialReadReason::FirstRun => !revoked && !pending,
        CredentialReadReason::Rescan => !revoked && !pending && !shown && !announced,
        CredentialReadReason::Manual => allow && !revoked && !pending,
        CredentialReadReason::Poll | CredentialReadReason::CredentialWatch => {
            shown && allow && !revoked && !pending
        }
    }
}

/// Policy source of truth for every silent credential-read path.
///
/// Split from `AppState` so the per-provider wiring is reachable from a test:
/// `credential_read_allowed` is a pure predicate that tests already pin, but
/// the loop that feeds it one provider's five flags is where a copy-paste
/// reaches the wrong field - the shape that once excluded Grok from the poll
/// gate for a whole release.
fn credential_read_scope_for(
    visibility: &settings::ProviderVisibility,
    consent_granted: bool,
    reason: CredentialReadReason,
) -> poller::DetectionScope {
    let mut flags = [false; tray_icon::TrayIconKind::COUNT];
    for kind in tray_icon::TrayIconKind::ALL {
        flags[kind.index()] = credential_read_allowed(
            consent_granted,
            visibility.allow(kind),
            visibility.revoked(kind),
            visibility.pending(kind),
            visibility.shown(kind),
            visibility.announced(kind),
            reason,
        );
    }
    poller::DetectionScope::from_flags(flags)
}

fn credential_read_scope(state: &AppState, reason: CredentialReadReason) -> poller::DetectionScope {
    credential_read_scope_for(
        &state_provider_visibility(state),
        state.credential_consent_granted,
        reason,
    )
}

fn state_provider_visibility(state: &AppState) -> settings::ProviderVisibility {
    settings::ProviderVisibility {
        show_claude_code: state.show_claude_code,
        show_codex: state.show_codex,
        show_antigravity: state.show_antigravity,
        show_grok: state.show_grok,
        allow_claude_credentials: state.allow_claude_credentials,
        allow_codex_credentials: state.allow_codex_credentials,
        allow_antigravity_credentials: state.allow_antigravity_credentials,
        allow_grok_credentials: state.allow_grok_credentials,
        claude_announced: state.claude_credential_access_decided,
        codex_announced: state.codex_credential_access_decided,
        antigravity_announced: state.antigravity_credential_access_decided,
        grok_announced: state.grok_credential_access_decided,
        claude_credential_access_revoked: state.claude_credential_access_revoked,
        codex_credential_access_revoked: state.codex_credential_access_revoked,
        antigravity_credential_access_revoked: state.antigravity_credential_access_revoked,
        grok_credential_access_revoked: state.grok_credential_access_revoked,
        claude_credential_access_pending: state.claude_credential_access_pending,
        codex_credential_access_pending: state.codex_credential_access_pending,
        antigravity_credential_access_pending: state.antigravity_credential_access_pending,
        grok_credential_access_pending: state.grok_credential_access_pending,
    }
}

fn apply_provider_visibility(state: &mut AppState, visibility: settings::ProviderVisibility) {
    state.show_claude_code = visibility.show_claude_code;
    state.show_codex = visibility.show_codex;
    state.show_antigravity = visibility.show_antigravity;
    state.show_grok = visibility.show_grok;
    state.allow_claude_credentials = visibility.allow_claude_credentials;
    state.allow_codex_credentials = visibility.allow_codex_credentials;
    state.allow_antigravity_credentials = visibility.allow_antigravity_credentials;
    state.allow_grok_credentials = visibility.allow_grok_credentials;
    state.claude_credential_access_decided = visibility.claude_announced;
    state.codex_credential_access_decided = visibility.codex_announced;
    state.antigravity_credential_access_decided = visibility.antigravity_announced;
    state.grok_credential_access_decided = visibility.grok_announced;
    state.claude_credential_access_revoked = visibility.claude_credential_access_revoked;
    state.codex_credential_access_revoked = visibility.codex_credential_access_revoked;
    state.antigravity_credential_access_revoked = visibility.antigravity_credential_access_revoked;
    state.grok_credential_access_revoked = visibility.grok_credential_access_revoked;
    state.claude_credential_access_pending = visibility.claude_credential_access_pending;
    state.codex_credential_access_pending = visibility.codex_credential_access_pending;
    state.antigravity_credential_access_pending = visibility.antigravity_credential_access_pending;
    state.grok_credential_access_pending = visibility.grok_credential_access_pending;
}

/// Drop whatever a pass found for providers that are no longer in scope.
fn mask_detection(
    detected: poller::DetectedProviders,
    scope: poller::DetectionScope,
) -> poller::DetectedProviders {
    poller::DetectedProviders {
        claude: detected.claude && scope.claude,
        codex: detected.codex && scope.codex,
        antigravity: detected.antigravity && scope.antigravity,
        grok: detected.grok && scope.grok,
    }
}

/// Detection shells out to `wsl.exe` and reads the Windows keyring, so it runs
/// on its own short-lived thread and never on the UI thread.
fn spawn_provider_detection(reason: DetectionReason) {
    let scope = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };
        if !s.credential_consent_granted {
            return;
        }
        let scope = credential_read_scope(s, reason.into());
        diagnose::log(format!(
            "provider detection starting reason={reason:?} scope: claude={} codex={} antigravity={} grok={}",
            scope.claude, scope.codex, scope.antigravity, scope.grok
        ));
        scope
    };
    std::thread::spawn(move || {
        let detected = poller::detect_signed_in_providers(scope);
        apply_provider_detection(reason, detected);
    });
}

/// What a finished detection pass is allowed to change.
struct DetectionOutcome {
    after: settings::ProviderVisibility,
    announcements: Vec<tray_icon::TrayIconKind>,
    changed: bool,
}

/// Decide a finished pass against the scope that is current *now*.
///
/// The worker sampled its scope before it started, and a provider can be
/// revoked or moved to pending while it was out reading credentials - a WSL
/// probe alone can take seconds. Applying the raw result would let a stale
/// "found it" undo a revocation the user made in the meantime, and on the
/// Manual path that also re-enables the provider, so its token goes out on the
/// next poll. Kept as a pure function so that guarantee is testable without an
/// `AppState`; the caller's remaining job is to sample the scope and the
/// visibility in the same locked section it writes the answer back to.
fn resolve_detection(
    reason: DetectionReason,
    detected: poller::DetectedProviders,
    before: settings::ProviderVisibility,
    scope: poller::DetectionScope,
) -> DetectionOutcome {
    let detected = mask_detection(detected, scope);
    let mut after = before;
    let announcements = match reason {
        DetectionReason::FirstRun => {
            settings::apply_first_run_detection(&mut after, detected);
            Vec::new()
        }
        DetectionReason::Manual => {
            settings::apply_manual_detection(&mut after, detected);
            Vec::new()
        }
        DetectionReason::Rescan => settings::take_detection_announcements(&mut after, detected),
    };
    let changed = after != before;
    DetectionOutcome {
        after,
        announcements,
        changed,
    }
}

fn apply_provider_detection(reason: DetectionReason, detected: poller::DetectedProviders) {
    let (changed, announcements, language) = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        // Access can be declined while a pass is in flight.
        if !s.credential_consent_granted {
            return;
        }
        // Scope, decision and write-back all inside this one lock: a
        // revocation cannot slip between reading the scope and applying the
        // answer.
        let outcome = resolve_detection(
            reason,
            detected,
            state_provider_visibility(s),
            credential_read_scope(s, reason.into()),
        );
        let DetectionOutcome {
            after,
            announcements,
            changed,
        } = outcome;
        if changed {
            apply_provider_visibility(s, after);
            s.provider_refresh_states.reset_hidden(
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
                s.show_grok,
            );
            set_widget_placeholders(s, "...");
        }
        (changed, announcements, s.language)
    };

    if !changed && announcements.is_empty() {
        return;
    }
    // Outside the lock: both take it.
    save_state_settings();
    post_usage_updated();
    if changed {
        request_poll();
    }

    let main_hwnd = current_main_hwnd();
    if main_hwnd == HWND::default() {
        return;
    }
    let strings = language.strings();
    for kind in announcements {
        let provider = localized_provider_name(kind, strings);
        let title = strings
            .provider_detected_title
            .replace("{provider}", provider);
        diagnose::log(format!("detected newly available provider {provider}"));
        tray_icon::notify_balloon(
            main_hwnd,
            kind,
            tray_icon::BalloonTone::Info,
            &title,
            strings.provider_detected_body,
        );
    }
}

/// Arm the recurring detection sweep.
///
/// Only meaningful once access has been granted; the timer checks again when
/// it fires, so a user who grants access later still gets swept.
///
/// `sweep_now` runs one pass without waiting for the first interval. The
/// sweep is the only path that can announce a provider which appeared after
/// first run, including one that shipped in the update the user just
/// installed, so waiting a full interval detaches that balloon from the
/// upgrade that earned it - and a user who quits before the interval elapses
/// defers it again. Tray icons already exist by this point and the pass runs
/// on a background thread, so there is nothing to wait for.
///
/// It is deliberately not passed on a fresh install: `FirstRun` detection is
/// already in flight there and is what marks detected providers as announced,
/// so a concurrent sweep would race it and balloon about providers first-run
/// detection is in the middle of enabling.
///
/// Armed on the process-level helper rather than the embedded widget, for the
/// same reason as the poll timer: explorer destroying the taskbar stops the
/// embedded child receiving any message, `WM_TIMER` included. `WM_TIMER` is
/// delivered to the window it was armed on, so arming it anywhere whose window
/// procedure lacks a `TIMER_PROVIDER_DETECT` arm drops the sweep silently -
/// which is exactly how this went unnoticed from v2.4.1 to v2.5.0.
fn schedule_provider_detection(sweep_now: bool) {
    unsafe {
        arm_timer(
            poll_controller_hwnd(),
            TIMER_PROVIDER_DETECT,
            PROVIDER_DETECT_INTERVAL_MS,
            "provider detection",
        );
        if sweep_now {
            handle_provider_detect_timer();
        }
    }
}

unsafe fn handle_provider_detect_timer() {
    // Consent and the per-provider scope are both checked inside.
    spawn_provider_detection(DetectionReason::Rescan);
}

/// Exit the process deliberately from the UI thread.
///
/// Update workers reach this through the process-level `WM_APP_REQUEST_QUIT`
/// channel; menu Exit and a normal `WM_CLOSE` call it directly. Mark the quit
/// before ending the message loop so a concurrent window destruction
/// can never be mistaken for an explorer-triggered teardown and revived.
unsafe fn request_quit(hwnd: HWND) {
    if QUIT_REQUESTED.swap(true, Ordering::SeqCst) {
        return;
    }

    diagnose::log("deliberate quit requested");
    let (hook, detail_hwnd, floating_hwnd) = {
        let mut state = lock_state();
        match state.as_mut() {
            Some(s) => (s.win_event_hook.take(), s.details_hwnd, s.floating_hwnd),
            None => (None, None, None),
        }
    };
    if let Some(hook) = hook {
        native_interop::unhook_win_event(hook);
    }
    if let Some(detail_hwnd) = detail_hwnd {
        let _ = DestroyWindow(detail_hwnd);
    }
    if let Some(floating_hwnd) = floating_hwnd {
        let _ = DestroyWindow(floating_hwnd);
    }

    if hwnd == HWND::default() || !IsWindow(hwnd).as_bool() {
        diagnose::log("deliberate quit: main window unavailable; ending message loop directly");
        PostQuitMessage(0);
        return;
    }
    if let Err(error) = DestroyWindow(hwnd) {
        diagnose::log_error(
            "deliberate quit: failed to destroy main window; ending message loop directly",
            error,
        );
        PostQuitMessage(0);
    }
}

/// Kick off a poll while preserving the last valid values. Shared by the
/// context-menu Refresh entry and the detail popup's refresh button.
fn trigger_manual_refresh(_hwnd: HWND) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.manual_refresh_in_progress = true;
        }
    }
    refresh_detail_popup_if_open();
    request_poll_with(true);
}

fn detail_should_animate(client_area_animation: bool, high_contrast: bool) -> bool {
    client_area_animation && !high_contrast
}

unsafe fn client_area_animation_enabled() -> bool {
    let mut enabled = TRUE;
    if SystemParametersInfoW(
        SPI_GETCLIENTAREAANIMATION,
        0,
        Some(&mut enabled as *mut BOOL as *mut std::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_err()
    {
        // Preserve the existing native-flyout behavior if Windows cannot
        // report the preference; only an explicit disabled value suppresses.
        return true;
    }
    enabled.as_bool()
}

fn show_usage_details(_tray_hwnd: HWND, anchor: Option<POINT>) {
    // Clicking the tray icon (or the widget) while the popup is open first
    // dismisses it via focus loss, then delivers this open request. Treat an
    // open that lands right after a dismissal as a toggle-close instead of
    // flickering the popup shut and open again.
    {
        let last_dismiss = DETAIL_LAST_DISMISS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *last_dismiss {
            if at.elapsed().as_millis() < DETAIL_REOPEN_SUPPRESS_MS {
                diagnose::log("detail popup: open request treated as toggle-close");
                return;
            }
        }
    }

    diagnose::log("detail popup: open requested");
    let snapshot = detail_popup_snapshot();
    let title = snapshot.title.clone();

    {
        let mut detail_state = lock_detail_state();
        *detail_state = Some(snapshot.clone());
    }
    DETAIL_REFRESHING.store(snapshot.refreshing, Ordering::SeqCst);

    let existing = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };

    unsafe {
        if let Some(detail_hwnd) = existing {
            if IsWindow(detail_hwnd).as_bool() {
                let _dpi_scope = DpiScope::for_window(detail_hwnd);
                if DETAIL_PINNED.load(Ordering::SeqCst) {
                    // A pinned popup stays where the user put it; re-opening
                    // must not snap it back to the anchor.
                    update_detail_header_buttons(detail_hwnd);
                    let _ = InvalidateRect(detail_hwnd, None, false);
                    let _ = SetForegroundWindow(detail_hwnd);
                    return;
                }
                let (x, y, width, height) = detail_popup_geometry(&snapshot, anchor);
                let _ = SetWindowPos(
                    detail_hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    width,
                    height,
                    SWP_SHOWWINDOW,
                );
                update_detail_header_buttons(detail_hwnd);
                let _ = InvalidateRect(detail_hwnd, None, false);
                let _ = SetForegroundWindow(detail_hwnd);
                remember_detail_monitor(detail_hwnd);
                return;
            }
        }
    }

    if !ensure_detail_window_class() {
        return;
    }

    unsafe {
        // Provisional geometry is replaced with the new HWND's own monitor
        // DPI immediately after creation, before the popup is shown.
        let (x, y, width, height) = detail_popup_geometry(&snapshot, anchor);
        DETAIL_MOVEMENT_UNLOCKED.store(DETAIL_DEFAULT_MOVEMENT_UNLOCKED, Ordering::SeqCst);
        DETAIL_REFRESHING.store(snapshot.refreshing, Ordering::SeqCst);
        *lock_detail_scroll_state() = DetailScrollState::default();
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("detail popup: GetModuleHandleW failed", error);
                return;
            }
        };
        let class_name = native_interop::wide_str(DETAIL_WINDOW_CLASS_NAME);
        let title_wide = native_interop::wide_str(&title);
        // Deliberately unowned. The main widget is a WS_CHILD embedded in the
        // taskbar, so passing it as owner would make Win32 resolve the owner
        // to its top-level ancestor - explorer's taskbar window. A popup owned
        // by a foreign process's window ties its lifetime and z-order to
        // explorer's whims; an unowned topmost tool window is self-contained
        // (Exit cleans it up explicitly, and WS_EX_TOOLWINDOW keeps it out of
        // the taskbar).
        let detail_hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            WS_POPUP,
            x,
            y,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("detail popup: CreateWindowExW failed", error);
                let mut detail_state = lock_detail_state();
                *detail_state = None;
                return;
            }
        };

        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.details_hwnd = Some(detail_hwnd);
            }
        }

        {
            let _dpi_scope = DpiScope::for_window(detail_hwnd);
            let (x, y, width, height) = detail_popup_geometry(&snapshot, anchor);
            if let Err(error) = SetWindowPos(
                detail_hwnd,
                HWND_TOPMOST,
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            ) {
                diagnose::log_error("detail popup: DPI-aware initial positioning failed", error);
            }
            remember_detail_monitor(detail_hwnd);
        }

        diagnose::log(format!("detail popup: created hwnd={:?}", detail_hwnd));
        if SetTimer(detail_hwnd, TIMER_DETAIL_REFRESH, DETAIL_REFRESH_MS, None) == 0 {
            diagnose::log("detail popup: unable to start live countdown timer");
        }
        // Rounded corners on Windows 11, matching native tray flyouts; a no-op
        // (harmless error) on Windows 10.
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            detail_hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&corner) as u32,
        );
        // Fade in like the native tray flyouts (WM_PRINTCLIENT is handled so
        // the blend has real content); fall back to a plain show on failure.
        // AW_ACTIVATE matters: without activation the popup never receives
        // WA_INACTIVE, so click-outside-to-dismiss would silently stop
        // working whenever SetForegroundWindow below is denied.
        if !detail_should_animate(client_area_animation_enabled(), theme::is_high_contrast())
            || AnimateWindow(detail_hwnd, 120, AW_BLEND | AW_ACTIVATE).is_err()
        {
            let _ = ShowWindow(detail_hwnd, SW_SHOWNORMAL);
        }
        let _ = UpdateWindow(detail_hwnd);
        let _ = SetForegroundWindow(detail_hwnd);
    }
}

fn detail_popup_snapshot() -> DetailPopupState {
    let state = lock_state();
    let Some(s) = state.as_ref() else {
        return detail_fallback_snapshot();
    };

    let strings = s.language.strings();
    // PollFailure normally carries exact per-provider errors in `data`. Keep
    // the aggregate error as a defensive fallback for legacy/cached state and
    // unexpected failures that do not identify a provider.
    let global_error = |kind| {
        s.last_error
            .map(|error| provider_status_for_kind(kind, error))
    };

    let mut providers = Vec::new();
    let now_unix = now_unix_secs();
    for kind in shown_provider_order(
        &s.provider_order,
        s.show_claude_code,
        s.show_codex,
        s.show_antigravity,
        s.show_grok,
    ) {
        let name = localized_provider_name(kind, strings);
        let usage = s.data.as_ref().and_then(|data| data.usage(kind));
        let error = s.data.as_ref().and_then(|data| data.error(kind));
        let updated_unix = s
            .data
            .as_ref()
            .and_then(|data| data.provider(kind).updated_unix);
        let error = error.or_else(|| global_error(kind));
        let persistent_refresh_issue = provider_refresh_is_stale(
            error,
            s.provider_refresh_states.for_kind(kind),
            updated_unix,
            s.poll_interval_ms,
            now_unix,
        );
        let mut group = detail_provider_group_with_freshness(
            kind,
            name,
            usage,
            error,
            DetailDataFreshness {
                persistent_refresh_issue,
                updated_unix,
                data_is_cached: s.data_is_cached,
            },
            strings,
        );
        // A provider whose access was declined or revoked is never polled, so
        // it carries no status of its own and would otherwise sit on a bare
        // "waiting for usage data" row forever. Say what happened and where
        // to undo it. Applied here rather than inside the group builder
        // because permission is application state, not poll state.
        if provider_pending_flag(s, kind) {
            group.hint = Some(DetailHint {
                action: strings
                    .detail_access_pending_hint
                    .replace("{provider}", name),
                outcome: strings.detail_access_pending_outcome.to_string(),
            });
        } else if provider_access_revoked(s, kind) {
            group.hint = Some(DetailHint {
                action: strings
                    .detail_access_revoked_hint
                    .replace("{provider}", name),
                outcome: strings.detail_access_revoked_outcome.to_string(),
            });
        }
        providers.push(group);
    }

    DetailPopupState {
        title: strings.window_title.to_string(),
        providers,
        status: detail_status_text(s, strings),
        version: env!("CARGO_PKG_VERSION").to_string(),
        refreshing: s.manual_refresh_in_progress,
    }
}

#[derive(Clone, Copy)]
struct DetailDataFreshness {
    persistent_refresh_issue: bool,
    updated_unix: Option<u64>,
    data_is_cached: bool,
}

fn detail_provider_group_with_freshness(
    kind: tray_icon::TrayIconKind,
    name: &str,
    usage: Option<&UsageData>,
    error: Option<ProviderStatus>,
    freshness: DetailDataFreshness,
    strings: Strings,
) -> DetailProviderGroup {
    let DetailDataFreshness {
        persistent_refresh_issue,
        updated_unix,
        data_is_cached,
    } = freshness;
    let badge = match error {
        Some(ProviderStatus::AuthenticationFailed) => Some(DetailBadge {
            text: strings.detail_badge_auth_failed.to_string(),
            tone: DetailBadgeTone::ActionRequired,
        }),
        // Never signed in states a fact rather than demanding a fix, so it
        // uses the neutral-leaning degraded tone instead of the alarm tone.
        Some(ProviderStatus::NotSignedIn) => Some(DetailBadge {
            text: strings.detail_badge_not_signed_in.to_string(),
            tone: DetailBadgeTone::Degraded,
        }),
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        ) if persistent_refresh_issue => Some(DetailBadge {
            text: strings.detail_badge_stale.to_string(),
            tone: DetailBadgeTone::Degraded,
        }),
        Some(
            ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed,
        )
        | None => None,
    };

    let rows = match usage.filter(|usage| !usage.is_empty()) {
        Some(usage) => usage
            .windows
            .iter()
            .map(|window| {
                detail_usage_row(
                    compact_view::compact_usage_window_label(window, strings),
                    Some(window),
                    usage_window_dividers(window),
                    strings,
                )
            })
            .collect(),
        None => {
            let reset_text = match error {
                Some(ProviderStatus::AuthenticationFailed) => strings.detail_unavailable,
                // Without this arm the row would fall through to "waiting for
                // usage data" and the never-signed-in state would never be
                // stated in the detail popup.
                Some(ProviderStatus::NotSignedIn) => strings.detail_badge_not_signed_in,
                Some(
                    ProviderStatus::RateLimited
                    | ProviderStatus::NetworkUnavailable
                    | ProviderStatus::RequestFailed,
                ) if persistent_refresh_issue => strings.detail_temporarily_unavailable,
                _ => strings.detail_waiting,
            };
            vec![DetailUsageRow {
                window_label: String::new(),
                percent: None,
                reset_text: reset_text.to_string(),
                dividers: 1,
                warn: false,
            }]
        }
    };

    let badge = badge.or_else(|| {
        if rows
            .iter()
            .any(|row| detail_percent_reached_limit(row.percent))
        {
            Some(DetailBadge {
                text: strings.detail_badge_limit_reached.to_string(),
                tone: DetailBadgeTone::Critical,
            })
        } else if rows.iter().any(|row| row.warn) {
            Some(DetailBadge {
                text: strings.detail_badge_near_limit.to_string(),
                tone: DetailBadgeTone::Critical,
            })
        } else {
            None
        }
    });

    let hint = match error {
        Some(status) if status.needs_credentials() => {
            let has_usage = usage.is_some_and(|usage| !usage.is_empty());
            let outcome = if has_usage {
                provider_updated_ago_text(updated_unix, strings, now_unix_secs())
                    .unwrap_or_else(|| strings.detail_monitoring_resumes.to_string())
            } else {
                strings.detail_monitoring_resumes.to_string()
            };
            Some(DetailHint {
                action: credential_action_text(kind, status, strings),
                outcome,
            })
        }
        Some(
            status @ (ProviderStatus::RateLimited
            | ProviderStatus::NetworkUnavailable
            | ProviderStatus::RequestFailed),
        ) if persistent_refresh_issue => {
            let has_usage = usage.is_some_and(|usage| !usage.is_empty());
            let action = if status == ProviderStatus::NetworkUnavailable {
                strings.detail_network_action
            } else {
                strings.detail_network_outcome
            };
            let outcome = if has_usage {
                updated_unix
                    .map(|updated_unix| {
                        strings.detail_updated_ago.replace(
                            "{ago}",
                            &detail_duration_from_secs(
                                now_unix_secs().saturating_sub(updated_unix),
                                strings,
                            ),
                        )
                    })
                    .unwrap_or_else(|| strings.detail_network_outcome.to_string())
            } else if status == ProviderStatus::NetworkUnavailable {
                strings.detail_network_outcome.to_string()
            } else {
                strings.detail_temporarily_unavailable.to_string()
            };
            Some(DetailHint {
                action: action.to_string(),
                outcome,
            })
        }
        _ => None,
    };
    let data_is_stale = usage.is_some_and(|usage| !usage.is_empty())
        && (data_is_cached
            || error.is_some_and(ProviderStatus::needs_credentials)
            || persistent_refresh_issue);

    DetailProviderGroup {
        kind,
        name: name.to_string(),
        badge,
        rows,
        data_is_stale,
        hint,
    }
}

fn detail_provider_group(
    kind: tray_icon::TrayIconKind,
    name: &str,
    usage: Option<&UsageData>,
    error: Option<ProviderStatus>,
    persistent_refresh_issue: bool,
    data_is_cached: bool,
    strings: Strings,
) -> DetailProviderGroup {
    detail_provider_group_with_freshness(
        kind,
        name,
        usage,
        error,
        DetailDataFreshness {
            persistent_refresh_issue,
            updated_unix: None,
            data_is_cached,
        },
        strings,
    )
}

fn detail_usage_row(
    window_label: String,
    section: Option<&UsageWindow>,
    dividers: i32,
    strings: Strings,
) -> DetailUsageRow {
    let percent = section.map(|section| section.percentage.clamp(0.0, 100.0));
    let reset_text = match section {
        None => strings.detail_waiting.to_string(),
        Some(section) => match section.resets_at {
            None => strings.detail_reset_unavailable.to_string(),
            Some(resets_at) => detail_reset_line(resets_at, strings, true),
        },
    };

    DetailUsageRow {
        window_label,
        percent,
        reset_text,
        dividers,
        warn: percent.is_some_and(compact_view::display_percent_warns),
    }
}

fn detail_percent_uses_muted_tone(percent: Option<f64>) -> bool {
    percent.is_none_or(|percent| compact_view::display_percent(percent) == 0)
}

fn detail_percent_reached_limit(percent: Option<f64>) -> bool {
    percent.is_some_and(|percent| compact_view::display_percent(percent) >= 100)
}

/// "Resets in 2h 13m (21:30)" - relative countdown plus the absolute local
/// time, which is what people actually plan around for longer quota windows.
/// `compact` selects the popup's tightened "… · HH:MM" form; the tray tooltip
/// passes false to keep its parenthesised "… (HH:MM)" wrapping.
fn detail_reset_line(resets_at: SystemTime, strings: Strings, compact: bool) -> String {
    match resets_at.duration_since(SystemTime::now()) {
        Ok(duration) if duration.as_secs() > 0 => detail_reset_line_from_parts(
            duration,
            format_local_time(resets_at, strings),
            strings,
            compact,
        ),
        _ => strings.detail_resets_now.to_string(),
    }
}

fn detail_reset_line_from_parts(
    duration: Duration,
    at: Option<String>,
    strings: Strings,
    compact: bool,
) -> String {
    let mut text = strings
        .detail_resets_in
        .replace("{duration}", &detail_duration_text(duration, strings));
    if let Some(at) = at.filter(|at| !at.is_empty()) {
        if compact {
            text.push_str(" · ");
            text.push_str(&at);
        } else {
            text.push_str(" (");
            text.push_str(&at);
            text.push(')');
        }
    }
    text
}

/// Format a SystemTime as local wall-clock time: "21:30" today, "Wed 21:30"
/// within the next six days, "7/16 21:30" beyond that.
fn format_local_time(t: SystemTime, strings: Strings) -> Option<String> {
    let unix = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    // Unix seconds -> FILETIME (100ns ticks since 1601-01-01).
    let ticks = unix
        .checked_mul(10_000_000)?
        .checked_add(116_444_736_000_000_000)?;
    let filetime = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&filetime, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
    }
    let now = unsafe { GetLocalTime() };
    Some(format_local_time_components(
        &local,
        &now,
        unix.saturating_sub(now_unix_secs()),
        strings,
    ))
}

fn format_local_time_components(
    local: &SYSTEMTIME,
    now: &SYSTEMTIME,
    seconds_until: u64,
    strings: Strings,
) -> String {
    let time = format!("{:02}:{:02}", local.wHour, local.wMinute);
    if local.wYear == now.wYear && local.wMonth == now.wMonth && local.wDay == now.wDay {
        return time;
    }
    if seconds_until < 6 * 86_400 {
        let weekday = strings
            .weekdays
            .get(local.wDayOfWeek as usize)
            .copied()
            .unwrap_or("");
        format!("{weekday} {time}")
    } else {
        let date = format_local_short_date(local, strings)
            .unwrap_or_else(|| format!("{:04}-{:02}-{:02}", local.wYear, local.wMonth, local.wDay));
        format!("{date} {time}")
    }
}

fn format_local_short_date(date: &SYSTEMTIME, strings: Strings) -> Option<String> {
    let locale = native_interop::wide_str(strings.locale_name);
    let mut buffer = [0u16; 80];
    let length = unsafe {
        GetDateFormatEx(
            PCWSTR::from_raw(locale.as_ptr()),
            DATE_SHORTDATE,
            Some(date),
            PCWSTR::null(),
            Some(&mut buffer),
            PCWSTR::null(),
        )
    };
    (length > 1).then(|| String::from_utf16_lossy(&buffer[..length as usize - 1]))
}

fn detail_status_text(state: &AppState, strings: Strings) -> String {
    if state.manual_refresh_in_progress {
        return strings.detail_refreshing.to_string();
    }
    let (issues, shown) = detail_refresh_issue_count(state);
    if issues > 0 {
        return if issues == shown {
            strings.detail_all_not_updated
        } else {
            strings.detail_some_not_updated
        }
        .to_string();
    }
    let Some(last_success_unix) = state.last_success_unix else {
        return strings.detail_waiting.to_string();
    };

    detail_poll_timing_status(
        last_success_unix,
        state.data_is_cached,
        state.poll_interval_ms,
        state.next_poll_deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0))
        }),
        strings,
        now_unix_secs(),
    )
}

fn detail_refresh_issue_count(state: &AppState) -> (usize, usize) {
    let now_unix = now_unix_secs();
    let mut issues = 0;
    let shown = shown_provider_order(
        &state.provider_order,
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
        state.show_grok,
    );
    for kind in &shown {
        let status = state.data.as_ref().and_then(|data| data.error(*kind));
        let updated_unix = state
            .data
            .as_ref()
            .and_then(|data| data.provider(*kind).updated_unix);
        if status.is_some_and(ProviderStatus::needs_credentials)
            || provider_refresh_is_stale(
                status,
                state.provider_refresh_states.for_kind(*kind),
                updated_unix,
                state.poll_interval_ms,
                now_unix,
            )
        {
            issues += 1;
        }
    }
    (issues, shown.len())
}

fn detail_poll_timing_status(
    last_success_unix: u64,
    data_is_cached: bool,
    poll_interval_ms: u32,
    next_poll_in_secs: Option<u64>,
    strings: Strings,
    now_unix: u64,
) -> String {
    let elapsed = now_unix.saturating_sub(last_success_unix);
    let updated = strings
        .detail_updated_ago
        .replace("{ago}", &detail_duration_from_secs(elapsed, strings));
    let mut status = if data_is_cached {
        updated
    } else {
        strings.detail_poll_every.replace(
            "{interval}",
            &detail_duration_from_secs((poll_interval_ms / 1000) as u64, strings),
        )
    };

    if let Some(next) = next_poll_in_secs.filter(|_| !data_is_cached) {
        status.push_str(" · ");
        status.push_str(
            &strings
                .detail_next_in
                .replace("{next}", &detail_duration_from_secs(next, strings)),
        );
    }
    status
}

fn detail_duration_text(duration: Duration, strings: Strings) -> String {
    detail_duration_from_secs(duration.as_secs(), strings)
}

fn detail_duration_from_secs(total_secs: u64, strings: Strings) -> String {
    if total_secs < 60 {
        return format!("{}{}", total_secs, strings.second_suffix);
    }

    let total_minutes = poller::display_minutes_from_secs(total_secs);
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;
    let joiner = if [
        strings.day_suffix,
        strings.hour_suffix,
        strings.minute_suffix,
    ]
    .iter()
    .any(|suffix| suffix.chars().any(is_east_asian_character))
    {
        ""
    } else {
        " "
    };

    if days > 0 {
        if hours > 0 {
            format!(
                "{}{}{joiner}{}{}",
                days, strings.day_suffix, hours, strings.hour_suffix,
            )
        } else {
            format!("{}{}", days, strings.day_suffix)
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!(
                "{}{}{joiner}{}{}",
                hours, strings.hour_suffix, minutes, strings.minute_suffix,
            )
        } else {
            format!("{}{}", hours, strings.hour_suffix)
        }
    } else {
        format!("{}{}", minutes, strings.minute_suffix)
    }
}

fn refresh_detail_popup_if_open() {
    let detail_hwnd = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };
    let Some(detail_hwnd) = detail_hwnd else {
        return;
    };

    unsafe {
        if !IsWindow(detail_hwnd).as_bool() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.details_hwnd = None;
            }
            return;
        }
    }

    let _dpi_scope = DpiScope::for_window(detail_hwnd);
    let snapshot = detail_popup_snapshot();
    let title = snapshot.title.clone();
    let work = unsafe { detail_work_area_for_window(detail_hwnd) };
    let (width, height) = detail_popup_fitted_size(&snapshot, work);
    DETAIL_REFRESHING.store(snapshot.refreshing, Ordering::SeqCst);
    {
        let mut detail_state = lock_detail_state();
        *detail_state = Some(snapshot.clone());
    }
    unsafe {
        // When the row set changes (Models toggled), keep the bottom edge
        // anchored - the popup sits above the tray, so growing downwards
        // would push it off the screen.
        let mut old_rect = RECT::default();
        if GetWindowRect(detail_hwnd, &mut old_rect).is_ok() {
            let (x, y) = detail_position_inside_work_area(
                work,
                old_rect.left,
                old_rect.bottom - height,
                width,
                height,
            );
            if old_rect.left != x
                || old_rect.top != y
                || old_rect.right - old_rect.left != width
                || old_rect.bottom - old_rect.top != height
            {
                let _ = SetWindowPos(
                    detail_hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE,
                );
            }
        }
        let title = native_interop::wide_str(&title);
        let _ = SetWindowTextW(detail_hwnd, PCWSTR::from_raw(title.as_ptr()));
        position_detail_header_buttons(detail_hwnd);
        update_detail_header_buttons(detail_hwnd);
        if let Some((_, _, _, metrics)) = detail_scroll_context(detail_hwnd) {
            let _ = sync_detail_scroll_state(metrics);
        }
        let _ = InvalidateRect(detail_hwnd, None, false);
    }
}

unsafe fn remember_detail_monitor(hwnd: HWND) {
    let monitor = monitor_identity_for_window(hwnd);
    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        if s.details_hwnd.is_some_and(|stored| stored.0 == hwnd.0) {
            s.details_monitor = monitor;
        }
    }
}

fn detail_group_height(group: &DetailProviderGroup) -> i32 {
    2 * DETAIL_GROUP_PAD_V
        + DETAIL_GROUP_HEADER_H
        + group.rows.len() as i32 * DETAIL_WINDOW_ROW_H
        + if group.hint.is_some() {
            DETAIL_HINT_H
        } else {
            0
        }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DetailHint {
    action: String,
    outcome: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailPopupDumpFixture {
    Usage,
    ClaudeUpdate,
    ClaudeLogin,
}

fn detail_popup_body_height(snapshot: &DetailPopupState) -> i32 {
    let content_h = if snapshot.providers.is_empty() {
        DETAIL_EMPTY_H
    } else {
        let groups: i32 = snapshot.providers.iter().map(detail_group_height).sum();
        groups + (snapshot.providers.len() as i32 - 1) * DETAIL_GROUP_GAP
    };
    content_h + DETAIL_CONTENT_BOTTOM_PAD
}

fn detail_popup_size(snapshot: &DetailPopupState) -> (i32, i32) {
    (
        sc(DETAIL_POPUP_WIDTH),
        sc(DETAIL_HEADER_H) + sc(detail_popup_body_height(snapshot)) + sc(DETAIL_FOOTER_H),
    )
}

/// Cap the desired flyout size to the monitor work area. Header and footer
/// remain fixed; `detail_scroll_metrics` makes only the body scroll when the
/// desired height is larger than the available viewport.
fn detail_popup_fitted_size(snapshot: &DetailPopupState, work: RECT) -> (i32, i32) {
    let (desired_width, desired_height) = detail_popup_size(snapshot);
    let margin = sc(DETAIL_WORK_AREA_MARGIN);
    let available_width = (work.right - work.left - 2 * margin).max(1);
    let available_height = (work.bottom - work.top - 2 * margin).max(1);
    (
        desired_width.min(available_width),
        desired_height.min(available_height),
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DetailScrollMetrics {
    viewport_top: i32,
    viewport_bottom: i32,
    viewport_height: i32,
    content_height: i32,
    max_offset: i32,
}

fn detail_scroll_metrics(snapshot: &DetailPopupState, client_height: i32) -> DetailScrollMetrics {
    let viewport_top = sc(DETAIL_HEADER_H);
    let viewport_bottom = (client_height - sc(DETAIL_FOOTER_H)).max(viewport_top);
    let viewport_height = (viewport_bottom - viewport_top).max(0);
    let content_height = sc(detail_popup_body_height(snapshot));
    DetailScrollMetrics {
        viewport_top,
        viewport_bottom,
        viewport_height,
        content_height,
        max_offset: (content_height - viewport_height).max(0),
    }
}

fn detail_scroll_thumb_rect(width: i32, metrics: DetailScrollMetrics, offset: i32) -> Option<RECT> {
    if metrics.max_offset <= 0 || metrics.viewport_height <= 0 || metrics.content_height <= 0 {
        return None;
    }
    let inset = sc(4);
    let track_top = metrics.viewport_top + inset;
    let track_bottom = (metrics.viewport_bottom - inset).max(track_top);
    let track_height = track_bottom - track_top;
    if track_height <= 0 {
        return None;
    }
    let thumb_height = ((track_height as i64 * metrics.viewport_height as i64)
        / metrics.content_height.max(1) as i64) as i32;
    let thumb_height = thumb_height.clamp(
        sc(DETAIL_SCROLL_THUMB_MIN_H).min(track_height),
        track_height,
    );
    let travel = track_height - thumb_height;
    let thumb_top = if travel <= 0 {
        track_top
    } else {
        track_top
            + ((travel as i64 * offset.clamp(0, metrics.max_offset) as i64)
                / metrics.max_offset as i64) as i32
    };
    let right = width - sc(5);
    Some(RECT {
        left: right - sc(DETAIL_SCROLL_THUMB_W),
        top: thumb_top,
        right,
        bottom: thumb_top + thumb_height,
    })
}

fn detail_scroll_gutter_rect(width: i32, metrics: DetailScrollMetrics) -> Option<RECT> {
    (metrics.max_offset > 0).then_some(RECT {
        left: width - sc(DETAIL_SCROLL_GUTTER_W),
        top: metrics.viewport_top,
        right: width,
        bottom: metrics.viewport_bottom,
    })
}

fn detail_scroll_offset_from_drag(
    start_offset: i32,
    delta_y: i32,
    metrics: DetailScrollMetrics,
    thumb_height: i32,
) -> i32 {
    let track_height = (metrics.viewport_height - 2 * sc(4)).max(0);
    let travel = (track_height - thumb_height).max(0);
    if travel == 0 || metrics.max_offset == 0 {
        0
    } else {
        (start_offset + ((delta_y as i64 * metrics.max_offset as i64) / travel as i64) as i32)
            .clamp(0, metrics.max_offset)
    }
}

/// Diagnostic: render the taskbar badge strip and floating numeric monitor at
/// 125% DPI with representative values. The output deliberately uses the
/// production scene and GDI executor, so layout previews exercise the same
/// fonts, provider tiles, colors, and clipping as the live Debug windows.
pub fn dump_widget(dir: &str) -> i32 {
    let _dpi = DpiScope::new(120);
    let window = |label: &str, percent: f64, countdown: &str, severity: compact_view::Severity| {
        compact_view::WindowView {
            label: label.to_string(),
            percent: Some(percent),
            display_percent: compact_view::display_percent(percent),
            percent_text: compact_view::display_percent_text(percent),
            countdown: countdown.to_string(),
            duration_seconds: None,
            severity,
        }
    };
    let provider = |kind, windows, attention| compact_view::ProviderView {
        kind,
        badge: None,
        windows,
        placeholder: None,
        attention,
    };
    let warn_vm = CompactViewModel {
        providers: vec![
            provider(
                tray_icon::TrayIconKind::Claude,
                vec![
                    window("7d", 92.0, "\u{00b7}4d", compact_view::Severity::Warn),
                    window("5h", 64.0, "\u{00b7}3h", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Warn,
            ),
            provider(
                tray_icon::TrayIconKind::Codex,
                vec![window(
                    "7d",
                    51.0,
                    "\u{00b7}6d",
                    compact_view::Severity::Normal,
                )],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Antigravity,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 1.0, "\u{00b7}2d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
            // Grok reports one billing period, so it is the single-window case.
            provider(
                tray_icon::TrayIconKind::Grok,
                vec![window(
                    "7d",
                    23.0,
                    "\u{00b7}5d",
                    compact_view::Severity::Normal,
                )],
                compact_view::Attention::Normal,
            ),
        ],
    };
    if let Err(error) = std::fs::create_dir_all(dir) {
        diagnose::log_error("dump compact surfaces: create directory failed", error);
        return 1;
    }

    let mut normal_vm = warn_vm.clone();
    for provider in &mut normal_vm.providers {
        provider.attention = compact_view::Attention::Normal;
        for window in &mut provider.windows {
            window.severity = compact_view::Severity::Normal;
            if window.display_percent >= compact_view::WARN_THRESHOLD_PERCENT {
                window.percent = Some(82.0);
                window.display_percent = 82;
                window.percent_text = "82%".to_string();
            }
        }
        if provider.kind == tray_icon::TrayIconKind::Claude {
            provider
                .windows
                .sort_by_key(|window| if window.label == "5h" { 0 } else { 1 });
        }
    }
    // Regression fixture for mixed-width percentages inside one provider. The
    // text stays right-aligned, while each gauge must begin under the displayed
    // value instead of under the widest value in the group.
    let alignment_vm = CompactViewModel {
        providers: vec![
            provider(
                tray_icon::TrayIconKind::Claude,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 29.0, "\u{00b7}5d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Codex,
                vec![window(
                    "7d",
                    27.0,
                    "\u{00b7}6d",
                    compact_view::Severity::Normal,
                )],
                compact_view::Attention::Normal,
            ),
            provider(
                tray_icon::TrayIconKind::Antigravity,
                vec![
                    window("5h", 0.0, "", compact_view::Severity::Normal),
                    window("7d", 1.0, "\u{00b7}1d", compact_view::Severity::Normal),
                ],
                compact_view::Attention::Normal,
            ),
        ],
    };
    let nodata_vm = CompactViewModel {
        providers: tray_icon::TrayIconKind::ALL
            .into_iter()
            .map(|kind| compact_view::ProviderView {
                kind,
                badge: None,
                windows: Vec::new(),
                placeholder: Some("--".to_string()),
                attention: compact_view::Attention::Normal,
            })
            .collect(),
    };
    let mut auth_vm = normal_vm.clone();
    if let Some(provider) = auth_vm
        .providers
        .iter_mut()
        .find(|provider| provider.kind == tray_icon::TrayIconKind::Claude)
    {
        provider.badge = None;
        provider.windows.clear();
        provider.placeholder = Some("--".to_string());
        provider.attention = compact_view::Attention::ActionRequired;
    }
    let mut error_vm = normal_vm.clone();
    if let Some(provider) = error_vm
        .providers
        .iter_mut()
        .find(|provider| provider.kind == tray_icon::TrayIconKind::Codex)
    {
        provider.attention = compact_view::Attention::ActionRequired;
    }
    let mut stale_vm = normal_vm.clone();
    if let Some(provider) = stale_vm
        .providers
        .iter_mut()
        .find(|provider| provider.kind == tray_icon::TrayIconKind::Codex)
    {
        provider.attention = compact_view::Attention::Stale;
    }

    let mut failed = false;
    for (state_name, vm) in [
        ("normal", &normal_vm),
        ("warn", &warn_vm),
        ("nodata", &nodata_vm),
        ("auth", &auth_vm),
        ("stale", &stale_vm),
        ("error", &error_vm),
    ] {
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            for (surface_name, floating) in [("badges", false), ("rows", true)] {
                let name = format!("{surface_name}-{state_name}-{theme_name}.bmp");
                if dump_compact_surface_bmp(dir, &name, vm, floating, is_dark, false).is_err() {
                    failed = true;
                }
            }
        }
    }
    for (state_name, vm) in [
        ("warn", &warn_vm),
        ("stale", &stale_vm),
        ("error", &error_vm),
    ] {
        for (surface_name, floating) in [("badges", false), ("rows", true)] {
            let name = format!("{surface_name}-{state_name}-hc-dark.bmp");
            if dump_compact_surface_bmp(dir, &name, vm, floating, true, true).is_err() {
                failed = true;
            }
        }
    }
    for (theme_name, is_dark) in [("dark", true), ("light", false)] {
        let name = format!("rows-alignment-{theme_name}.bmp");
        if dump_compact_surface_bmp(dir, &name, &alignment_vm, true, is_dark, false).is_err() {
            failed = true;
        }
    }
    let tooltip_snapshots = [
        WidgetTooltipSnapshot {
            kind: tray_icon::TrayIconKind::Claude,
            provider_name: "Claude".to_string(),
            rows: vec![
                WidgetTooltipRow {
                    window_label: "5h".to_string(),
                    percent_text: "63%".to_string(),
                    reset_text: "3小时5分钟后重置".to_string(),
                    warn: false,
                },
                WidgetTooltipRow {
                    window_label: "7d".to_string(),
                    percent_text: "92%".to_string(),
                    reset_text: "6天11小时后重置".to_string(),
                    warn: true,
                },
            ],
        },
        WidgetTooltipSnapshot {
            kind: tray_icon::TrayIconKind::Codex,
            provider_name: "Codex".to_string(),
            rows: vec![WidgetTooltipRow {
                window_label: "7d".to_string(),
                percent_text: "31%".to_string(),
                reset_text: "6天11小时后重置".to_string(),
                warn: false,
            }],
        },
    ];
    for snapshot in &tooltip_snapshots {
        let provider = match snapshot.kind {
            tray_icon::TrayIconKind::Claude => "claude",
            tray_icon::TrayIconKind::Codex => "codex",
            tray_icon::TrayIconKind::Antigravity => "antigravity",
            tray_icon::TrayIconKind::Grok => "grok",
        };
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            let name = format!("tooltip-{provider}-{theme_name}.bmp");
            if dump_widget_tooltip_bmp(dir, &name, snapshot, is_dark, false).is_err() {
                failed = true;
            }
        }
    }
    if dump_widget_tooltip_bmp(
        dir,
        "tooltip-claude-hc-dark.bmp",
        &tooltip_snapshots[0],
        true,
        true,
    )
    .is_err()
    {
        failed = true;
    }
    i32::from(failed)
}

fn dump_compact_surface_bmp(
    dir: &str,
    name: &str,
    vm: &CompactViewModel,
    floating: bool,
    is_dark: bool,
    high_contrast: bool,
) -> Result<(), String> {
    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }
        let scene = compact_scene(screen_dc, vm, high_contrast, floating);
        let width = if floating {
            sc(FLOATING_CONTENT_LEFT_MARGIN) + scene.width
        } else {
            sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN) + scene.width
        };
        let height = scene.height;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return Err("CreateDIBSection failed".to_string());
        }
        let old_bmp = SelectObject(mem_dc, dib);
        paint_compact_surface(
            mem_dc,
            width,
            height,
            &scene,
            floating,
            is_dark,
            high_contrast,
        );
        let _ = GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = PathBuf::from(dir).join(name);
        std::fs::write(&path, file).map_err(|error| error.to_string())?;
        diagnose::log(format!("dumped compact surface to {}", path.display()));
        Ok(())
    }
}

fn dump_widget_tooltip_bmp(
    dir: &str,
    name: &str,
    snapshot: &WidgetTooltipSnapshot,
    is_dark: bool,
    high_contrast: bool,
) -> Result<(), String> {
    unsafe {
        let screen_dc = GetDC(HWND::default());
        if screen_dc.0.is_null() {
            return Err("GetDC failed".to_string());
        }
        let layout = widget_tooltip_layout(screen_dc, snapshot);
        let width = layout.width;
        let height = layout.height;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return Err("CreateDIBSection failed".to_string());
        }
        let old_bmp = SelectObject(mem_dc, dib);
        paint_widget_tooltip_content(mem_dc, width, height, snapshot, is_dark, high_contrast);
        let _ = GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut pixels = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = PathBuf::from(dir).join(name);
        std::fs::write(&path, file).map_err(|error| error.to_string())?;
        diagnose::log(format!("dumped widget tooltip to {}", path.display()));
        Ok(())
    }
}

/// Diagnostic: render the detail popup with representative data (matching the
/// README screenshot's providers) to a BMP and exit. Mirrors
/// `tray_icon::dump_icons`; lets popup layout changes be eyeballed without
/// hunting for the live tray popup. Renders at 125%, matching the target
/// desktop used for final visual review.
pub fn dump_detail_popup(
    dir: &str,
    english: bool,
    force_dark: Option<bool>,
    fixture: DetailPopupDumpFixture,
) -> i32 {
    ACTIVE_WINDOW_DPI.with(|dpi| dpi.set(120));

    let row = |label: &str, percent: f64, reset: String, dividers: i32| DetailUsageRow {
        window_label: label.to_string(),
        percent: Some(percent),
        reset_text: reset,
        dividers,
        warn: compact_view::display_percent_warns(percent),
    };
    let strings = if english {
        LanguageId::English.strings()
    } else {
        LanguageId::SimplifiedChinese.strings()
    };
    // A fixed local-time scene keeps README images deterministic while the
    // production formatting helpers supply every word, separator, weekday,
    // and countdown. This prevents preview copy from drifting from the live UI.
    let local_time = |day: u16, weekday: u16, hour: u16, minute: u16| SYSTEMTIME {
        wYear: 2030,
        wMonth: 1,
        wDayOfWeek: weekday,
        wDay: day,
        wHour: hour,
        wMinute: minute,
        ..Default::default()
    };
    let fixture_now = local_time(6, 0, 21, 55); // Sunday
    let reset_specs = [
        (3 * 3_600 + 5 * 60, local_time(7, 1, 1, 0)),
        (5 * 3_600 + 5 * 60, local_time(7, 1, 3, 0)),
        (5 * 86_400 + 9 * 3_600 + 5 * 60, local_time(12, 6, 7, 0)),
        (5 * 3_600, local_time(7, 1, 2, 55)),
        (3 * 86_400 + 3 * 3_600 + 56 * 60, local_time(10, 4, 1, 51)),
        (4 * 86_400 + 6 * 3_600 + 5 * 60, local_time(11, 5, 4, 0)),
    ];
    let resets = reset_specs.map(|(seconds, target)| {
        detail_reset_line_from_parts(
            Duration::from_secs(seconds),
            Some(format_local_time_components(
                &target,
                &fixture_now,
                seconds,
                strings,
            )),
            strings,
            true,
        )
    });
    let usage_providers = vec![
        DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Claude,
            name: strings.claude_model.to_string(),
            badge: Some(DetailBadge {
                text: strings.detail_badge_near_limit.to_string(),
                tone: DetailBadgeTone::Critical,
            }),
            rows: vec![
                row("5h", 8.0, resets[0].clone(), 5),
                row("7d", 92.0, resets[1].clone(), 7),
            ],
            data_is_stale: false,
            hint: None,
        },
        DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Codex,
            name: "Codex".to_string(),
            badge: None,
            rows: vec![row("7d", 51.0, resets[2].clone(), 7)],
            data_is_stale: false,
            hint: None,
        },
        DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Antigravity,
            name: "Antigravity".to_string(),
            badge: None,
            rows: vec![
                row("5h", 0.0, resets[3].clone(), 5),
                row("7d", 1.0, resets[4].clone(), 7),
            ],
            data_is_stale: false,
            hint: None,
        },
        DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Grok,
            name: "Grok".to_string(),
            badge: None,
            rows: vec![row("7d", 23.0, resets[5].clone(), 7)],
            data_is_stale: false,
            hint: None,
        },
    ];
    let (providers, status) = match fixture {
        DetailPopupDumpFixture::Usage => (
            usage_providers,
            detail_poll_timing_status(1_000, false, POLL_1_MIN, Some(44), strings, 1_000),
        ),
        DetailPopupDumpFixture::ClaudeUpdate | DetailPopupDumpFixture::ClaudeLogin => {
            let auth_status = ProviderStatus::AuthenticationFailed;
            let mut providers = usage_providers;
            providers[0] = detail_provider_group(
                tray_icon::TrayIconKind::Claude,
                strings.claude_model,
                None,
                Some(auth_status),
                false,
                false,
                strings,
            );
            (providers, strings.detail_some_not_updated.to_string())
        }
    };
    let snapshot = DetailPopupState {
        title: strings.window_title.to_string(),
        providers,
        status,
        version: env!("CARGO_PKG_VERSION").to_string(),
        refreshing: false,
    };

    if let Err(error) = std::fs::create_dir_all(dir) {
        diagnose::log_error("dump detail popup: create directory failed", error);
        return 1;
    }

    let (width, height) = detail_popup_size(&snapshot);
    unsafe {
        let screen_dc = GetDC(HWND::default());
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return 1;
        }
        let old_bmp = SelectObject(mem_dc, dib);
        let is_dark = force_dark.unwrap_or_else(theme::is_dark_mode);
        let high_contrast = if force_dark.is_some() {
            false
        } else {
            theme::is_high_contrast()
        };
        paint_detail_content(mem_dc, width, height, &snapshot, is_dark, high_contrast, 0);
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();

        let byte_count = (width * height * 4) as usize;
        let mut buf = std::slice::from_raw_parts(bits as *const u8, byte_count).to_vec();
        // GDI fills/text leave the DIB alpha byte at 0; force opaque so a PNG
        // conversion of this BMP does not come out fully transparent.
        for px in buf.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }

        let mut file = Vec::with_capacity(54 + byte_count);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + byte_count as u32).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&width.to_le_bytes());
        file.extend_from_slice(&(-height).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&(byte_count as u32).to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&buf);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        let path = format!("{dir}/detail-popup.bmp");
        match std::fs::write(&path, file) {
            Ok(_) => {
                diagnose::log(format!("dumped detail popup to {path}"));
                0
            }
            Err(error) => {
                diagnose::log_error("dump detail popup: write failed", error);
                1
            }
        }
    }
}

fn virtual_screen_rect() -> RECT {
    RECT {
        left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        right: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) }
            + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        bottom: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) }
            + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    }
}

unsafe fn monitor_work_area(monitor: HMONITOR) -> Option<RECT> {
    if monitor.is_invalid() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    GetMonitorInfoW(monitor, &mut info)
        .as_bool()
        .then_some(info.rcWork)
}

fn detail_popup_position_in_work_area(
    pt: POINT,
    work: RECT,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let margin = sc(DETAIL_WORK_AREA_MARGIN);
    let min_x = work.left + margin;
    let max_x = work.right - width - margin;
    let min_y = work.top + margin;
    let max_y = work.bottom - height - margin;

    let x = pt.x - width + sc(28);
    let mut y = pt.y - height - sc(12);
    if y < min_y {
        y = pt.y + sc(12);
    }

    (clamp_i32(x, min_x, max_x), clamp_i32(y, min_y, max_y))
}

unsafe fn detail_popup_geometry(
    snapshot: &DetailPopupState,
    anchor: Option<POINT>,
) -> (i32, i32, i32, i32) {
    let mut pt = anchor.unwrap_or_default();
    if anchor.is_none() && GetCursorPos(&mut pt).is_err() {
        pt.x = GetSystemMetrics(SM_CXSCREEN) - sc(16);
        pt.y = GetSystemMetrics(SM_CYSCREEN) - sc(48);
    }

    let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let work = monitor_work_area(monitor).unwrap_or_else(virtual_screen_rect);
    let (width, height) = detail_popup_fitted_size(snapshot, work);
    let (x, y) = detail_popup_position_in_work_area(pt, work, width, height);
    (x, y, width, height)
}

fn rect_center_point(rect: RECT) -> POINT {
    POINT {
        x: rect.left + (rect.right - rect.left) / 2,
        y: rect.top + (rect.bottom - rect.top) / 2,
    }
}

fn tray_icon_anchor_point(hwnd: HWND, kind: Option<tray_icon::TrayIconKind>) -> Option<POINT> {
    let rect = match kind {
        Some(kind) => tray_icon::rect(hwnd, kind),
        None => tray_icon::app_rect(hwnd),
    }?;
    Some(rect_center_point(rect))
}

unsafe fn detail_work_area_for_window(hwnd: HWND) -> RECT {
    monitor_work_area(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))
        .unwrap_or_else(virtual_screen_rect)
}

fn detail_position_inside_work_area(
    work: RECT,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let margin = sc(DETAIL_WORK_AREA_MARGIN);
    (
        clamp_i32(x, work.left + margin, work.right - width - margin),
        clamp_i32(y, work.top + margin, work.bottom - height - margin),
    )
}

fn clamp_i32(value: i32, min_value: i32, max_value: i32) -> i32 {
    if max_value < min_value {
        return min_value;
    }
    value.max(min_value).min(max_value)
}

fn bottom_right_default_position(work: RECT, width: i32, height: i32, margin: i32) -> (i32, i32) {
    let min_x = work.left + margin;
    let min_y = work.top + margin;
    (
        clamp_i32(
            work.right - width - margin,
            min_x,
            work.right - width - margin,
        ),
        clamp_i32(
            work.bottom - height - margin,
            min_y,
            work.bottom - height - margin,
        ),
    )
}

unsafe fn reset_detail_popup_to_primary_default() {
    let detail_hwnd = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };
    let Some(detail_hwnd) = detail_hwnd.filter(|hwnd| IsWindow(*hwnd).as_bool()) else {
        return;
    };

    let _dpi_scope = DpiScope::new(GetDpiForSystem());
    let snapshot = detail_popup_snapshot();
    let work = primary_work_area();
    let (width, height) = detail_popup_fitted_size(&snapshot, work);
    let (x, y) = bottom_right_default_position(work, width, height, sc(DETAIL_WORK_AREA_MARGIN));
    let _ = SetWindowPos(
        detail_hwnd,
        HWND_TOPMOST,
        x,
        y,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    remember_detail_monitor(detail_hwnd);
    let _ = InvalidateRect(detail_hwnd, None, false);
    diagnose::log("detail popup: disconnected secondary monitor; reset to primary default");
}

fn ensure_detail_window_class() -> bool {
    if DETAIL_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("detail popup: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(DETAIL_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            // CS_DROPSHADOW matches the native tray flyouts (pairs with the
            // DWM rounded corners set at creation).
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(detail_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            // Do not latch the registered flag on failure: a later attempt
            // (e.g. after handle pressure eases) can still succeed.
            diagnose::log("detail popup: RegisterClassExW failed");
            return false;
        }
    }

    DETAIL_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

unsafe fn detail_scroll_context(
    hwnd: HWND,
) -> Option<(DetailPopupState, i32, i32, DetailScrollMetrics)> {
    let snapshot = lock_detail_state()
        .clone()
        .unwrap_or_else(detail_fallback_snapshot);
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return None;
    }
    let width = client.right - client.left;
    let height = client.bottom - client.top;
    if width <= 0 || height <= 0 {
        return None;
    }
    let metrics = detail_scroll_metrics(&snapshot, height);
    Some((snapshot, width, height, metrics))
}

fn sync_detail_scroll_state(metrics: DetailScrollMetrics) -> i32 {
    let mut scroll = lock_detail_scroll_state();
    scroll.max_offset = metrics.max_offset;
    scroll.offset = scroll.offset.clamp(0, metrics.max_offset);
    if metrics.max_offset == 0 {
        scroll.wheel_remainder = 0;
        scroll.dragging = false;
    }
    scroll.offset
}

unsafe fn set_detail_scroll_offset(hwnd: HWND, requested: i32) -> bool {
    let Some((_, _, _, metrics)) = detail_scroll_context(hwnd) else {
        return false;
    };
    let requested = requested.clamp(0, metrics.max_offset);
    let changed = {
        let mut scroll = lock_detail_scroll_state();
        scroll.max_offset = metrics.max_offset;
        let changed = scroll.offset != requested;
        scroll.offset = requested;
        changed
    };
    if changed {
        let _ = InvalidateRect(hwnd, None, false);
    }
    changed
}

unsafe fn scroll_detail_mouse_wheel(hwnd: HWND, delta: i32) -> bool {
    let Some((_, _, _, metrics)) = detail_scroll_context(hwnd) else {
        return false;
    };
    if metrics.max_offset == 0 {
        return false;
    }
    let (steps, target) = {
        let mut scroll = lock_detail_scroll_state();
        scroll.max_offset = metrics.max_offset;
        scroll.offset = scroll.offset.clamp(0, metrics.max_offset);
        scroll.wheel_remainder = scroll.wheel_remainder.saturating_add(delta);
        let steps = scroll.wheel_remainder / WHEEL_DELTA as i32;
        scroll.wheel_remainder %= WHEEL_DELTA as i32;
        (
            steps,
            scroll
                .offset
                .saturating_sub(steps.saturating_mul(sc(DETAIL_SCROLL_ROW_STEP))),
        )
    };
    if steps != 0 {
        let _ = set_detail_scroll_offset(hwnd, target);
    }
    true
}

unsafe fn begin_detail_scrollbar_pointer(hwnd: HWND, x: i32, y: i32) -> bool {
    let Some((_, width, _, metrics)) = detail_scroll_context(hwnd) else {
        return false;
    };
    let Some(gutter) = detail_scroll_gutter_rect(width, metrics) else {
        return false;
    };
    if !point_in_rect(x, y, &gutter) {
        return false;
    }

    let offset = sync_detail_scroll_state(metrics);
    let Some(thumb) = detail_scroll_thumb_rect(width, metrics, offset) else {
        return false;
    };
    let _ = SetFocus(hwnd);
    if point_in_rect(x, y, &thumb) {
        let mut scroll = lock_detail_scroll_state();
        scroll.dragging = true;
        scroll.drag_start_y = y;
        scroll.drag_start_offset = offset;
        let _ = SetCapture(hwnd);
    } else {
        let page = metrics.viewport_height.max(sc(DETAIL_SCROLL_LINE_STEP));
        let target = if y < thumb.top {
            offset.saturating_sub(page)
        } else {
            offset.saturating_add(page)
        };
        let _ = set_detail_scroll_offset(hwnd, target);
    }
    true
}

unsafe fn drag_detail_scrollbar(hwnd: HWND, y: i32) -> bool {
    let (dragging, drag_start_y, drag_start_offset) = {
        let scroll = lock_detail_scroll_state();
        (
            scroll.dragging,
            scroll.drag_start_y,
            scroll.drag_start_offset,
        )
    };
    if !dragging {
        return false;
    }
    let Some((_, width, _, metrics)) = detail_scroll_context(hwnd) else {
        return true;
    };
    let thumb_height = detail_scroll_thumb_rect(width, metrics, drag_start_offset)
        .map(|thumb| thumb.bottom - thumb.top)
        .unwrap_or(0);
    let target =
        detail_scroll_offset_from_drag(drag_start_offset, y - drag_start_y, metrics, thumb_height);
    let _ = set_detail_scroll_offset(hwnd, target);
    true
}

unsafe fn end_detail_scrollbar_pointer() -> bool {
    let was_dragging = {
        let mut scroll = lock_detail_scroll_state();
        let was_dragging = scroll.dragging;
        scroll.dragging = false;
        was_dragging
    };
    if was_dragging {
        let _ = ReleaseCapture();
    }
    was_dragging
}

fn rescale_detail_scroll_offset(old_dpi: u32, new_dpi: u32) {
    let old_dpi = normalize_dpi(old_dpi);
    let new_dpi = normalize_dpi(new_dpi);
    let mut scroll = lock_detail_scroll_state();
    scroll.offset = ((scroll.offset as i64 * new_dpi as i64) / old_dpi as i64) as i32;
    scroll.dragging = false;
}

unsafe fn scroll_detail_for_key(hwnd: HWND, key: u32) -> bool {
    let Some((_, _, _, metrics)) = detail_scroll_context(hwnd) else {
        return false;
    };
    if metrics.max_offset == 0 {
        return false;
    }
    let current = sync_detail_scroll_state(metrics);
    let target = match key {
        key if key == VK_UP.0 as u32 => current.saturating_sub(sc(DETAIL_SCROLL_LINE_STEP)),
        key if key == VK_DOWN.0 as u32 => current.saturating_add(sc(DETAIL_SCROLL_LINE_STEP)),
        key if key == VK_PRIOR.0 as u32 => current.saturating_sub(metrics.viewport_height),
        key if key == VK_NEXT.0 as u32 => current.saturating_add(metrics.viewport_height),
        key if key == VK_HOME.0 as u32 => 0,
        key if key == VK_END.0 as u32 => metrics.max_offset,
        _ => return false,
    };
    let _ = set_detail_scroll_offset(hwnd, target);
    true
}

unsafe fn constrain_detail_window_to_work_area(hwnd: HWND, preserve_bottom: bool) {
    let snapshot = lock_detail_state()
        .clone()
        .unwrap_or_else(detail_fallback_snapshot);
    let work = detail_work_area_for_window(hwnd);
    let (width, height) = detail_popup_fitted_size(&snapshot, work);
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let desired_y = if preserve_bottom {
        rect.bottom - height
    } else {
        rect.top
    };
    let (x, y) = detail_position_inside_work_area(work, rect.left, desired_y, width, height);
    if x != rect.left
        || y != rect.top
        || width != rect.right - rect.left
        || height != rect.bottom - rect.top
    {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE);
    }
    if let Some((_, _, _, metrics)) = detail_scroll_context(hwnd) {
        let _ = sync_detail_scroll_state(metrics);
    }
}

extern "system" fn detail_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        detail_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in detail_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn activate_detail_header_button(hwnd: HWND, id: u16) {
    match id {
        IDC_DETAIL_CLOSE => {
            diagnose::log("detail popup: close button activated");
            let _ = DestroyWindow(hwnd);
        }
        IDC_DETAIL_REFRESH => {
            if DETAIL_REFRESHING.load(Ordering::SeqCst) {
                diagnose::log("detail popup: duplicate refresh ignored while busy");
                return;
            }
            diagnose::log("detail popup: refresh button activated");
            let main_hwnd = {
                let state = lock_state();
                state.as_ref().map(|state| state.hwnd.to_hwnd())
            };
            if let Some(main_hwnd) = main_hwnd {
                trigger_manual_refresh(main_hwnd);
            }
        }
        IDC_DETAIL_MOVE => {
            let unlocked = !DETAIL_MOVEMENT_UNLOCKED.fetch_xor(true, Ordering::SeqCst);
            diagnose::log(if unlocked {
                "detail popup: movement unlocked"
            } else {
                "detail popup: movement locked"
            });
            update_detail_header_buttons(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
        IDC_DETAIL_PIN => {
            let pinned = !DETAIL_PINNED.fetch_xor(true, Ordering::SeqCst);
            {
                let mut state = lock_state();
                if let Some(state) = state.as_mut() {
                    state.detail_pinned = pinned;
                }
            }
            save_state_settings();
            diagnose::log(if pinned {
                "detail popup: pinned open"
            } else {
                "detail popup: unpinned"
            });
            update_detail_header_buttons(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
        _ => {}
    }
}

unsafe fn detail_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_CREATE => {
            create_detail_header_buttons(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED_MSG => {
            let old_dpi = active_window_dpi();
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            rescale_detail_scroll_offset(old_dpi, new_dpi);
            apply_suggested_dpi_rect(hwnd, lparam, "detail popup");
            constrain_detail_window_to_work_area(hwnd, false);
            position_detail_header_buttons(hwnd);
            remember_detail_monitor(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            diagnose::log(format!("detail popup: dpi changed dpi={new_dpi}"));
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            remember_detail_monitor(hwnd);
            LRESULT(0)
        }
        WM_SIZE => {
            position_detail_header_buttons(hwnd);
            if let Some((_, _, _, metrics)) = detail_scroll_context(hwnd) {
                let _ = sync_detail_scroll_state(metrics);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            constrain_detail_window_to_work_area(hwnd, false);
            let _ = InvalidateRect(hwnd, None, false);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_detail_popup(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // AnimateWindow(AW_BLEND) asks for the content through this message;
        // without it the fade-in would start from an empty frame.
        WM_PRINTCLIENT => {
            paint_detail_popup(HDC(wparam.0 as *mut _), hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DRAWITEM => {
            let item = (lparam.0 as *const DRAWITEMSTRUCT).as_ref();
            if let Some(item) = item.filter(|item| {
                matches!(
                    item.CtlID as u16,
                    IDC_DETAIL_PIN | IDC_DETAIL_MOVE | IDC_DETAIL_REFRESH | IDC_DETAIL_CLOSE
                )
            }) {
                draw_detail_header_button(item);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u16;
            let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if notification == BN_CLICKED
                && matches!(
                    id,
                    IDC_DETAIL_PIN | IDC_DETAIL_MOVE | IDC_DETAIL_REFRESH | IDC_DETAIL_CLOSE
                )
            {
                activate_detail_header_button(hwnd, id);
                return LRESULT(0);
            }
            // IsDialogMessageW turns Escape from a focused child button into
            // the conventional IDCANCEL command.
            if id == 2 {
                diagnose::log("detail popup: closed via Escape");
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER if wparam.0 == TIMER_DETAIL_REFRESH => {
            refresh_detail_popup_if_open();
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if scroll_detail_mouse_wheel(hwnd, delta) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if begin_detail_scrollbar_pointer(hwnd, x, y) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_MOUSEMOVE => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if drag_detail_scrollbar(hwnd, y) {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_LBUTTONUP => {
            if end_detail_scrollbar_pointer() {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_CAPTURECHANGED => {
            let mut scroll = lock_detail_scroll_state();
            scroll.dragging = false;
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_NCHITTEST if DETAIL_MOVEMENT_UNLOCKED.load(Ordering::SeqCst) => {
            let mut point = POINT {
                x: (lparam.0 & 0xFFFF) as i16 as i32,
                y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            };
            let mut rect = RECT::default();
            if ScreenToClient(hwnd, &mut point).as_bool()
                && GetClientRect(hwnd, &mut rect).is_ok()
                && detail_header_is_draggable(point.x, point.y, rect.right - rect.left)
            {
                return LRESULT(HTCAPTION as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            diagnose::log("detail popup: WM_CLOSE received");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        // Tray-flyout conventions: Esc closes, and clicking anywhere else
        // (focus loss) dismisses the popup.
        WM_KEYDOWN if wparam.0 as u32 == VK_ESCAPE.0 as u32 => {
            diagnose::log("detail popup: closed via Escape");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
            if DETAIL_PINNED.load(Ordering::SeqCst) {
                diagnose::log("detail popup: focus loss ignored while pinned");
                return LRESULT(0);
            }
            diagnose::log("detail popup: dismissed on focus loss");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            diagnose::log(format!("detail popup: destroyed hwnd={:?}", hwnd));
            let _ = KillTimer(hwnd, TIMER_DETAIL_REFRESH);
            {
                let mut last_dismiss = DETAIL_LAST_DISMISS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *last_dismiss = Some(Instant::now());
            }
            DETAIL_HOT_BUTTON_ID.store(0, Ordering::SeqCst);
            DETAIL_MOUSE_FOCUS_BUTTON_ID.store(0, Ordering::SeqCst);
            DETAIL_REFRESHING.store(false, Ordering::SeqCst);
            DETAIL_MOVEMENT_UNLOCKED.store(DETAIL_DEFAULT_MOVEMENT_UNLOCKED, Ordering::SeqCst);
            *lock_detail_scroll_state() = DetailScrollState::default();
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.details_hwnd.is_some_and(|stored| stored.0 == hwnd.0) {
                        s.details_hwnd = None;
                        s.details_monitor = None;
                    }
                }
            }
            let mut detail_state = lock_detail_state();
            *detail_state = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_floating_window_class() -> bool {
    if FLOATING_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("floating monitor: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(FLOATING_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(floating_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            diagnose::log("floating monitor: RegisterClassExW failed");
            return false;
        }
    }

    FLOATING_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

unsafe fn primary_work_area() -> RECT {
    let mut work = RECT::default();
    if SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut RECT as *mut std::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
    {
        work
    } else {
        RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

fn placement_rect(rect: RECT) -> PlacementRect {
    PlacementRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

unsafe fn floating_work_area(placement: &FloatingPlacement) -> (RECT, bool) {
    let requested = match placement {
        FloatingPlacement::Custom { monitor, .. } => monitor_handle_for_key(monitor),
        FloatingPlacement::PrimaryBottomLeft | FloatingPlacement::PrimaryBottomRight => {
            primary_monitor_handle()
        }
    };
    let missing_monitor =
        matches!(placement, FloatingPlacement::Custom { .. }) && requested.is_none();
    let monitor = requested.or_else(|| primary_monitor_handle());
    let work = monitor
        .and_then(|monitor| monitor_work_area(monitor))
        .unwrap_or_else(|| primary_work_area());
    (work, missing_monitor)
}

unsafe fn floating_target_position(
    width: i32,
    height: i32,
    placement: &FloatingPlacement,
) -> (i32, i32, bool) {
    let (work, missing_monitor) = floating_work_area(placement);
    let rect = placement::resolve_floating_rect(
        placement,
        placement_rect(work),
        width,
        height,
        active_window_dpi(),
    );
    (rect.left, rect.top, missing_monitor)
}

fn floating_monitor_size(hwnd: Option<HWND>) -> (i32, i32) {
    let state = lock_state();
    let Some(state) = state.as_ref() else {
        return (sc(180), sc(52));
    };
    let scene = compact_scene_for_hwnd(
        hwnd.unwrap_or_default(),
        &state.compact_vm,
        state.is_high_contrast,
        true,
    );
    (sc(FLOATING_CONTENT_LEFT_MARGIN) + scene.width, scene.height)
}

const DWM_COLOR_NONE: u32 = 0xFFFF_FFFE;

unsafe fn apply_floating_dwm_style(hwnd: HWND, is_dark: bool, high_contrast: bool) {
    let corner = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &corner as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&corner) as u32,
    );
    let dark_mode = BOOL::from(is_dark);
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark_mode as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&dark_mode) as u32,
    );
    let border_color = if high_contrast {
        theme::system_color(COLOR_WINDOWTEXT).to_colorref()
    } else {
        DWM_COLOR_NONE
    };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border_color as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&border_color) as u32,
    );
}

fn floating_drag_distance_exceeded(delta_x: i32, delta_y: i32) -> bool {
    delta_x.abs() >= sc(FLOATING_DRAG_THRESHOLD) || delta_y.abs() >= sc(FLOATING_DRAG_THRESHOLD)
}

fn ensure_floating_monitor_window() -> Option<HWND> {
    let existing = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.floating_hwnd)
    };
    if let Some(hwnd) = existing {
        if unsafe { IsWindow(hwnd).as_bool() } {
            return Some(hwnd);
        }
    }
    if !ensure_floating_window_class() {
        return None;
    }

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("floating monitor: GetModuleHandleW failed", error);
                return None;
            }
        };
        let (title, floating_placement, is_dark, high_contrast) = {
            let state = lock_state();
            let s = state.as_ref()?;
            (
                s.language.strings().window_title,
                s.floating_placement.clone(),
                s.is_dark,
                s.is_high_contrast,
            )
        };
        let (width, height) = floating_monitor_size(None);
        let (mut x, mut y, mut position_reset) =
            floating_target_position(width, height, &floating_placement);
        let class_name = native_interop::wide_str(FLOATING_WINDOW_CLASS_NAME);
        let title = native_interop::wide_str(title);
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            x,
            y,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(hinstance.0),
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("floating monitor: CreateWindowExW failed", error);
                return None;
            }
        };
        {
            let _dpi_scope = DpiScope::for_window(hwnd);
            let (width, height) = floating_monitor_size(Some(hwnd));
            let (next_x, next_y, reset) =
                floating_target_position(width, height, &floating_placement);
            x = next_x;
            y = next_y;
            position_reset |= reset;
            if let Err(error) =
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, width, height, SWP_NOACTIVATE)
            {
                diagnose::log_error(
                    "floating monitor: DPI-aware initial positioning failed",
                    error,
                );
            }
        }
        apply_floating_dwm_style(hwnd, is_dark, high_contrast);
        let monitor = monitor_identity_for_window(hwnd);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_hwnd = Some(hwnd);
                s.floating_monitor = monitor;
                if position_reset {
                    s.floating_x = Some(x);
                    s.floating_y = Some(y);
                }
            }
        }
        if position_reset {
            diagnose::log(
                "floating monitor: custom display unavailable; showing temporarily on primary",
            );
        }
        diagnose::log(format!("floating monitor: created hwnd={:?}", hwnd));
        Some(hwnd)
    }
}

fn refresh_floating_monitor() {
    let (visible, floating_placement) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (s.floating_visible, s.floating_placement.clone()),
            None => return,
        }
    };
    // Countdown/theme refreshes also reach this function. Do not create a
    // permanently hidden HWND for users who never enable the floating window.
    // Resetting while hidden still records the primary-work-area default.
    if !visible {
        let _dpi_scope = DpiScope::new(unsafe { GetDpiForSystem() });
        let (width, height) = floating_monitor_size(None);
        let (x, y, missing_monitor) =
            unsafe { floating_target_position(width, height, &floating_placement) };
        {
            let rect = RECT {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            };
            let monitor = unsafe { monitor_identity_for_rect(&rect) };
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_x = Some(x);
                s.floating_y = Some(y);
                s.floating_monitor = monitor;
            }
        }
        if missing_monitor {
            diagnose::log(
                "floating monitor: hidden custom display unavailable; resolved temporarily on primary",
            );
        }
        return;
    }

    let hwnd = match ensure_floating_monitor_window() {
        Some(hwnd) => hwnd,
        None => return,
    };
    let _dpi_scope = DpiScope::for_window(hwnd);
    let (width, height) = floating_monitor_size(Some(hwnd));
    unsafe {
        if FLOATING_MOVING.load(Ordering::Acquire) {
            let _ = InvalidateRect(hwnd, None, false);
            return;
        }
        let (x, y, missing_monitor) = floating_target_position(width, height, &floating_placement);
        // WS_EX_TOPMOST keeps the window in the topmost band. Preserve its
        // relative z-order here: this path runs on every countdown update and
        // must not repeatedly jump ahead of unrelated topmost windows.
        let flags = SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW;
        let _ = SetWindowPos(hwnd, HWND::default(), x, y, width, height, flags);
        let _ = InvalidateRect(hwnd, None, false);
        let monitor = monitor_identity_for_window(hwnd);
        {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_monitor = monitor;
                s.floating_x = Some(x);
                s.floating_y = Some(y);
            }
        }
        if missing_monitor {
            diagnose::log(
                "floating monitor: custom display unavailable; showing temporarily on primary",
            );
        }
    }
}

fn toggle_floating_monitor() {
    let visible = {
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return;
        };
        s.floating_visible = !s.floating_visible;
        s.floating_visible
    };
    if visible {
        if ensure_floating_monitor_window().is_none() {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.floating_visible = false;
            }
        } else {
            refresh_floating_monitor();
        }
    } else {
        let hwnd = {
            let state = lock_state();
            state.as_ref().and_then(|s| s.floating_hwnd)
        };
        if let Some(hwnd) = hwnd {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
    save_state_settings();
}

fn set_floating_default_position(position: FloatingDefaultPosition) {
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.floating_default_position = position;
            s.floating_placement = match position {
                FloatingDefaultPosition::PrimaryBottomLeft => FloatingPlacement::PrimaryBottomLeft,
                FloatingDefaultPosition::PrimaryBottomRight => {
                    FloatingPlacement::PrimaryBottomRight
                }
            };
            s.floating_placement_needs_migration = false;
        }
    }
    refresh_floating_monitor();
    save_state_settings();
}

fn remember_floating_position(hwnd: HWND) -> bool {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        let monitor_handle = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let Some(monitor) = monitor_identity_from_handle(monitor_handle) else {
            return false;
        };
        let Some(work) = monitor_work_area(monitor_handle) else {
            return false;
        };
        let dpi = GetDpiForWindow(hwnd).max(96);
        let (horizontal_anchor, vertical_anchor, horizontal_gap_dip, vertical_gap_dip) =
            placement::custom_floating_anchors(placement_rect(work), placement_rect(rect), dpi);
        let custom = FloatingPlacement::Custom {
            monitor: monitor.key(),
            horizontal_anchor,
            vertical_anchor,
            horizontal_gap_dip,
            vertical_gap_dip,
        };
        let mut state = lock_state();
        let Some(s) = state.as_mut() else {
            return false;
        };
        s.floating_x = Some(rect.left);
        s.floating_y = Some(rect.top);
        s.floating_monitor = Some(monitor);
        s.floating_placement = custom;
        s.floating_placement_needs_migration = false;
        true
    }
}

extern "system" fn floating_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        floating_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in floating_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn floating_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_DPICHANGED_MSG => {
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            apply_suggested_dpi_rect(hwnd, lparam, "floating monitor");
            refresh_floating_monitor();
            let _ = InvalidateRect(hwnd, None, false);
            diagnose::log(format!("floating monitor: dpi changed dpi={new_dpi}"));
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let mut cursor = POINT::default();
            let mut rect = RECT::default();
            if GetCursorPos(&mut cursor).is_ok() && GetWindowRect(hwnd, &mut rect).is_ok() {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                drag.tracking = true;
                drag.moved = false;
                drag.start_cursor_x = cursor.x;
                drag.start_cursor_y = cursor.y;
                drag.start_window_x = rect.left;
                drag.start_window_y = rect.top;
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut cursor = POINT::default();
            if GetCursorPos(&mut cursor).is_err() {
                return LRESULT(0);
            }
            let target = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !drag.tracking {
                    None
                } else {
                    let delta_x = cursor.x - drag.start_cursor_x;
                    let delta_y = cursor.y - drag.start_cursor_y;
                    if !drag.moved && floating_drag_distance_exceeded(delta_x, delta_y) {
                        drag.moved = true;
                        FLOATING_MOVING.store(true, Ordering::Release);
                    }
                    drag.moved
                        .then_some((drag.start_window_x + delta_x, drag.start_window_y + delta_y))
                }
            };
            if let Some((x, y)) = target {
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    x,
                    y,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (tracking, moved) = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = (drag.tracking, drag.moved);
                drag.tracking = false;
                drag.moved = false;
                result
            };
            if tracking {
                let _ = ReleaseCapture();
            }
            FLOATING_MOVING.store(false, Ordering::Release);

            if moved {
                let _ = remember_floating_position(hwnd);
                refresh_floating_monitor();
                save_state_settings();
            } else {
                show_usage_details(hwnd, None);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            let moved = {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let moved = drag.moved;
                drag.tracking = false;
                drag.moved = false;
                moved
            };
            FLOATING_MOVING.store(false, Ordering::Release);
            if moved {
                let _ = remember_floating_position(hwnd);
                refresh_floating_monitor();
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let main_hwnd = current_main_hwnd();
            if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
                show_context_menu(main_hwnd, None);
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            refresh_floating_monitor();
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = ShowWindow(hwnd, SW_HIDE);
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.floating_visible = false;
                }
            }
            save_state_settings();
            LRESULT(0)
        }
        WM_DESTROY => {
            FLOATING_MOVING.store(false, Ordering::Release);
            {
                let mut drag = FLOATING_DRAG_STATE
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                drag.tracking = false;
                drag.moved = false;
            }
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if s.floating_hwnd.is_some_and(|stored| stored.0 == hwnd.0) {
                    s.floating_hwnd = None;
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn point_in_rect(x: i32, y: i32, rect: &RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn detail_close_rect(width: i32) -> RECT {
    let right = width - sc(16);
    RECT {
        left: right - sc(DETAIL_HEADER_BUTTON_SIZE),
        top: sc(DETAIL_HEADER_BUTTON_TOP),
        right,
        bottom: sc(DETAIL_HEADER_BUTTON_TOP + DETAIL_HEADER_BUTTON_SIZE),
    }
}

fn detail_move_rect(width: i32) -> RECT {
    let right = detail_close_rect(width).left - sc(DETAIL_HEADER_BUTTON_GAP);
    RECT {
        left: right - sc(DETAIL_HEADER_BUTTON_SIZE),
        top: sc(DETAIL_HEADER_BUTTON_TOP),
        right,
        bottom: sc(DETAIL_HEADER_BUTTON_TOP + DETAIL_HEADER_BUTTON_SIZE),
    }
}

fn detail_pin_rect(width: i32) -> RECT {
    let right = detail_move_rect(width).left - sc(DETAIL_HEADER_BUTTON_GAP);
    RECT {
        left: right - sc(DETAIL_HEADER_BUTTON_SIZE),
        top: sc(DETAIL_HEADER_BUTTON_TOP),
        right,
        bottom: sc(DETAIL_HEADER_BUTTON_TOP + DETAIL_HEADER_BUTTON_SIZE),
    }
}

fn detail_refresh_rect(width: i32) -> RECT {
    let right = detail_pin_rect(width).left - sc(DETAIL_HEADER_REFRESH_GROUP_GAP);
    RECT {
        left: right - sc(DETAIL_HEADER_BUTTON_SIZE),
        top: sc(DETAIL_HEADER_BUTTON_TOP),
        right,
        bottom: sc(DETAIL_HEADER_BUTTON_TOP + DETAIL_HEADER_BUTTON_SIZE),
    }
}

fn detail_header_button_rect(id: u16, width: i32) -> Option<RECT> {
    match id {
        IDC_DETAIL_PIN => Some(detail_pin_rect(width)),
        IDC_DETAIL_MOVE => Some(detail_move_rect(width)),
        IDC_DETAIL_REFRESH => Some(detail_refresh_rect(width)),
        IDC_DETAIL_CLOSE => Some(detail_close_rect(width)),
        _ => None,
    }
}

fn detail_header_button_label(id: u16, strings: Strings) -> &'static str {
    match id {
        IDC_DETAIL_PIN if DETAIL_PINNED.load(Ordering::SeqCst) => strings.detail_unpin_action,
        IDC_DETAIL_PIN => strings.detail_pin_action,
        IDC_DETAIL_MOVE if DETAIL_MOVEMENT_UNLOCKED.load(Ordering::SeqCst) => {
            strings.detail_lock_position_action
        }
        IDC_DETAIL_MOVE => strings.detail_unlock_position_action,
        IDC_DETAIL_REFRESH if DETAIL_REFRESHING.load(Ordering::SeqCst) => strings.detail_refreshing,
        IDC_DETAIL_REFRESH => strings.refresh,
        IDC_DETAIL_CLOSE => strings.detail_close_action,
        _ => "",
    }
}

unsafe fn detail_header_button(hwnd: HWND, id: u16) -> Option<HWND> {
    GetDlgItem(hwnd, id as i32)
        .ok()
        .filter(|button| IsWindow(*button).as_bool())
}

unsafe extern "system" fn detail_header_button_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    detail_id: usize,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        detail_header_button_subclass_impl(hwnd, msg, wparam, lparam, subclass_id, detail_id as u16)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in detail_header_button_subclass msg={msg:#06x} (recovered)"
            ));
            DefSubclassProc(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn detail_header_button_subclass_impl(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    subclass_id: usize,
    detail_id: u16,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            DETAIL_MOUSE_FOCUS_BUTTON_ID.store(detail_id as u32, Ordering::SeqCst);
        }
        WM_MOUSEMOVE => {
            let previous = DETAIL_HOT_BUTTON_ID.swap(detail_id as u32, Ordering::SeqCst);
            if previous != detail_id as u32 {
                let _ = InvalidateRect(hwnd, None, false);
                if previous != 0 {
                    if let Ok(parent) = GetParent(hwnd) {
                        if let Ok(previous_button) = GetDlgItem(parent, previous as i32) {
                            let _ = InvalidateRect(previous_button, None, false);
                        }
                    }
                }
            }
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
        }
        WM_MOUSELEAVE_MSG => {
            if DETAIL_HOT_BUTTON_ID
                .compare_exchange(detail_id as u32, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
        WM_NCDESTROY => {
            let _ = DETAIL_HOT_BUTTON_ID.compare_exchange(
                detail_id as u32,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            let _ = DETAIL_MOUSE_FOCUS_BUTTON_ID.compare_exchange(
                detail_id as u32,
                0,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            let _ = RemoveWindowSubclass(hwnd, Some(detail_header_button_subclass), subclass_id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

unsafe fn position_detail_header_buttons(hwnd: HWND) {
    let mut client = RECT::default();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let width = client.right - client.left;
    for id in DETAIL_HEADER_BUTTON_IDS {
        let (Some(button), Some(rect)) = (
            detail_header_button(hwnd, id),
            detail_header_button_rect(id, width),
        ) else {
            continue;
        };
        let _ = MoveWindow(
            button,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            true,
        );
    }
}

unsafe fn update_detail_header_buttons(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state
            .as_ref()
            .map(|state| state.language.strings())
            .unwrap_or(LanguageId::English.strings())
    };
    for id in DETAIL_HEADER_BUTTON_IDS {
        let Some(button) = detail_header_button(hwnd, id) else {
            continue;
        };
        let label = detail_header_button_label(id, strings);
        let wide = native_interop::wide_str(label);
        let _ = SetWindowTextW(button, PCWSTR::from_raw(wide.as_ptr()));
        if id == IDC_DETAIL_REFRESH {
            let _ = EnableWindow(button, !DETAIL_REFRESHING.load(Ordering::SeqCst));
        }
        let _ = InvalidateRect(button, None, false);
    }
}

unsafe fn create_detail_header_buttons(hwnd: HWND) {
    let hinstance = match GetModuleHandleW(PCWSTR::null()) {
        Ok(handle) => handle,
        Err(error) => {
            diagnose::log_error("detail controls: GetModuleHandleW failed", error);
            return;
        }
    };
    let strings = {
        let state = lock_state();
        state
            .as_ref()
            .map(|state| state.language.strings())
            .unwrap_or(LanguageId::English.strings())
    };
    let button_class = native_interop::wide_str("BUTTON");
    for id in DETAIL_HEADER_BUTTON_IDS {
        let label = native_interop::wide_str(detail_header_button_label(id, strings));
        match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(button_class.as_ptr()),
            PCWSTR::from_raw(label.as_ptr()),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE(BS_OWNERDRAW as u32)
                | WINDOW_STYLE(BS_NOTIFY as u32),
            0,
            0,
            sc(DETAIL_HEADER_BUTTON_SIZE),
            sc(DETAIL_HEADER_BUTTON_SIZE),
            hwnd,
            HMENU(id as usize as *mut std::ffi::c_void),
            hinstance,
            None,
        ) {
            Ok(button) => {
                if !SetWindowSubclass(
                    button,
                    Some(detail_header_button_subclass),
                    id as usize,
                    id as usize,
                )
                .as_bool()
                {
                    diagnose::log(format!(
                        "detail controls: button {id} hover subclass failed"
                    ));
                }
            }
            Err(error) => diagnose::log_error(
                &format!("detail controls: button {id} creation failed"),
                error,
            ),
        }
    }
    position_detail_header_buttons(hwnd);
    update_detail_header_buttons(hwnd);
}

unsafe fn handle_detail_keyboard_input(detail: HWND, msg: &MSG) -> bool {
    if !matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN)
        || (msg.hwnd != detail && !IsChild(detail, msg.hwnd).as_bool())
    {
        return false;
    }

    let previous = DETAIL_MOUSE_FOCUS_BUTTON_ID.swap(0, Ordering::SeqCst);
    if previous != 0 {
        if let Some(button) = detail_header_button(detail, previous as u16) {
            let _ = InvalidateRect(button, None, false);
        }
    }

    msg.message == WM_KEYDOWN && scroll_detail_for_key(detail, msg.wParam.0 as u32)
}

fn detail_header_is_draggable(x: i32, y: i32, width: i32) -> bool {
    point_in_rect(
        x,
        y,
        &RECT {
            left: sc(4),
            top: sc(4),
            right: detail_refresh_rect(width).left - sc(4),
            bottom: sc(DETAIL_HEADER_H - 4),
        },
    )
}

fn paint_detail_popup(hdc: HDC, hwnd: HWND) {
    let _dpi_scope = DpiScope::for_window(hwnd);
    let snapshot = lock_detail_state()
        .clone()
        .unwrap_or_else(detail_fallback_snapshot);

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;
        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);
        let scroll_offset = sync_detail_scroll_state(detail_scroll_metrics(&snapshot, height));

        paint_detail_content(
            mem_dc,
            width,
            height,
            &snapshot,
            theme::is_dark_mode(),
            theme::is_high_contrast(),
            scroll_offset,
        );
        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

/// Popup colours follow the system theme (like the widget) and reuse the
/// widget's per-provider accents so the colour language stays consistent
/// across widget, tray icons and popup.
struct DetailPalette {
    bg: Color,
    card: Color,
    border: Color,
    divider: Color,
    text: Color,
    muted: Color,
    degraded: Color,
    warn: Color,
    warn_bg: Color,
    warn_on_bg: Color,
    track: Color,
    button_hot_bg: Color,
    button_hot_text: Color,
}

fn detail_palette(is_dark: bool, high_contrast: bool) -> DetailPalette {
    if high_contrast {
        DetailPalette {
            bg: theme::system_color(COLOR_WINDOW),
            card: theme::system_color(COLOR_WINDOW),
            border: theme::system_color(COLOR_WINDOWFRAME),
            divider: theme::system_color(COLOR_GRAYTEXT),
            text: theme::system_color(COLOR_WINDOWTEXT),
            muted: theme::system_color(COLOR_GRAYTEXT),
            degraded: theme::system_color(COLOR_WINDOWTEXT),
            warn: theme::system_color(COLOR_HIGHLIGHT),
            warn_bg: theme::system_color(COLOR_HIGHLIGHT),
            warn_on_bg: theme::system_color(COLOR_HIGHLIGHTTEXT),
            track: theme::system_color(COLOR_GRAYTEXT),
            button_hot_bg: theme::system_color(COLOR_HIGHLIGHT),
            button_hot_text: theme::system_color(COLOR_HIGHLIGHTTEXT),
        }
    } else if is_dark {
        DetailPalette {
            bg: Color::from_hex("#1F1F1F"),
            card: Color::from_hex("#242424"),
            border: Color::from_hex("#353535"),
            divider: Color::from_hex("#303030"),
            text: Color::from_hex("#F3F4F6"),
            muted: Color::from_hex("#9CA3AF"),
            degraded: Color::from_hex("#D8A35D"),
            warn: Color::from_hex("#FF5C66"),
            warn_bg: Color::from_hex("#493033"),
            warn_on_bg: Color::from_hex("#FF747C"),
            track: Color::from_hex("#343434"),
            button_hot_bg: Color::from_hex("#303030"),
            button_hot_text: Color::from_hex("#F3F4F6"),
        }
    } else {
        DetailPalette {
            bg: Color::from_hex("#F9F9F9"),
            card: Color::from_hex("#FFFFFF"),
            border: Color::from_hex("#D4D4D8"),
            divider: Color::from_hex("#E4E4E7"),
            text: Color::from_hex("#1B1B1F"),
            muted: Color::from_hex("#6B7280"),
            degraded: Color::from_hex("#946200"),
            warn: Color::from_hex("#DC2626"),
            warn_bg: Color::from_hex("#FDECEC"),
            warn_on_bg: Color::from_hex("#B91C1C"),
            track: Color::from_hex("#E7E7EA"),
            button_hot_bg: Color::from_hex("#E4E4E7"),
            button_hot_text: Color::from_hex("#1B1B1F"),
        }
    }
}

fn provider_accent(kind: tray_icon::TrayIconKind, is_dark: bool, high_contrast: bool) -> Color {
    match kind {
        tray_icon::TrayIconKind::Claude => claude_accent_color(high_contrast),
        tray_icon::TrayIconKind::Codex => codex_accent_color(is_dark, high_contrast),
        tray_icon::TrayIconKind::Antigravity => antigravity_accent_color(high_contrast),
        tray_icon::TrayIconKind::Grok => grok_accent_color(high_contrast),
    }
}

/// Provider-tinted surface shared by the action badge and recovery hint. The
/// rail keeps the stronger brand accent; these larger, quieter fills group the
/// two action-required elements without making them look like quota warnings.
fn detail_provider_tint(kind: tray_icon::TrayIconKind, is_dark: bool) -> Color {
    match (kind, is_dark) {
        (tray_icon::TrayIconKind::Claude, true) => Color::from_hex("#302824"),
        (tray_icon::TrayIconKind::Claude, false) => Color::from_hex("#FFF1EB"),
        (tray_icon::TrayIconKind::Codex, true) => Color::from_hex("#2C2C2C"),
        (tray_icon::TrayIconKind::Codex, false) => Color::from_hex("#F1F1F3"),
        (tray_icon::TrayIconKind::Antigravity, true) => Color::from_hex("#202B3B"),
        (tray_icon::TrayIconKind::Antigravity, false) => Color::from_hex("#EEF5FF"),
        (tray_icon::TrayIconKind::Grok, true) => Color::from_hex("#26243A"),
        (tray_icon::TrayIconKind::Grok, false) => Color::from_hex("#F1EFFF"),
    }
}

/// Readable brand-relative text for the small action-required badge. Raw
/// provider accents remain on the rail but do not all reach 4.5:1 at 11px.
fn detail_action_badge_foreground(kind: tray_icon::TrayIconKind, is_dark: bool) -> Color {
    match (kind, is_dark) {
        (tray_icon::TrayIconKind::Claude, true) => Color::from_hex("#E58B6F"),
        (tray_icon::TrayIconKind::Claude, false) => Color::from_hex("#AA4D2B"),
        (tray_icon::TrayIconKind::Codex, true) => Color::from_hex("#F5F5F5"),
        (tray_icon::TrayIconKind::Codex, false) => Color::from_hex("#1F1F1F"),
        (tray_icon::TrayIconKind::Antigravity, true) => Color::from_hex("#74A7FF"),
        (tray_icon::TrayIconKind::Antigravity, false) => Color::from_hex("#1A56C4"),
        (tray_icon::TrayIconKind::Grok, true) => Color::from_hex("#A79BFF"),
        (tray_icon::TrayIconKind::Grok, false) => Color::from_hex("#4B3BC7"),
    }
}

fn detail_hint_outcome_foreground(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#9CA3AF")
    } else {
        Color::from_hex("#646C7A")
    }
}

fn detail_hint_colors(
    kind: tray_icon::TrayIconKind,
    is_dark: bool,
    high_contrast: bool,
    palette: &DetailPalette,
) -> (Color, Color) {
    if high_contrast {
        (palette.card, palette.text)
    } else {
        (
            detail_provider_tint(kind, is_dark),
            detail_hint_outcome_foreground(is_dark),
        )
    }
}

/// Each brand mark keeps its real silhouette while the tile supplies a shared
/// optical footprint. Codex deliberately uses the black OpenAI mark on a light
/// tile in both themes, matching the reference and avoiding a generic app icon.
fn provider_chip_style(
    kind: tray_icon::TrayIconKind,
    is_dark: bool,
    high_contrast: bool,
    palette: &DetailPalette,
) -> (Color, Color, bool) {
    if high_contrast {
        return (palette.card, palette.border, is_dark);
    }

    match (kind, is_dark) {
        (tray_icon::TrayIconKind::Claude, true) => {
            (Color::from_hex("#30211E"), Color::from_hex("#70483D"), true)
        }
        (tray_icon::TrayIconKind::Claude, false) => (
            Color::from_hex("#FFF0EA"),
            Color::from_hex("#F1C8BA"),
            false,
        ),
        (tray_icon::TrayIconKind::Codex, _) => (
            Color::from_hex("#F7F7F5"),
            Color::from_hex("#D4D4D0"),
            false,
        ),
        (tray_icon::TrayIconKind::Antigravity, true) => {
            (Color::from_hex("#172B4A"), Color::from_hex("#3C68A4"), true)
        }
        (tray_icon::TrayIconKind::Antigravity, false) => (
            Color::from_hex("#E8F0FF"),
            Color::from_hex("#BFD3FF"),
            false,
        ),
        // One tile for both themes, matching the single Grok brand tile: the
        // mark only exists as white on near-black.
        (tray_icon::TrayIconKind::Grok, _) => {
            (Color::from_hex("#17171C"), Color::from_hex("#41414D"), true)
        }
    }
}

fn draw_detail_header_button_content(
    hdc: HDC,
    rect: RECT,
    id: u16,
    hot: bool,
    pressed: bool,
    focused: bool,
    palette: &DetailPalette,
) {
    let pinned = DETAIL_PINNED.load(Ordering::SeqCst);
    let movement_unlocked = DETAIL_MOVEMENT_UNLOCKED.load(Ordering::SeqCst);
    let refreshing = id == IDC_DETAIL_REFRESH && DETAIL_REFRESHING.load(Ordering::SeqCst);

    fill_rect_color(hdc, &rect, &palette.bg);
    if hot || pressed || refreshing {
        draw_rounded_rect(hdc, &rect, &palette.button_hot_bg, sc(5));
    }

    let glyph = detail_header_button_glyph(id, pinned, movement_unlocked, refreshing);
    draw_detail_text_face(
        hdc,
        glyph,
        rect,
        if hot || pressed || refreshing {
            &palette.button_hot_text
        } else {
            &palette.muted
        },
        "Segoe MDL2 Assets",
        14,
        FW_NORMAL.0 as i32,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );

    if focused {
        let focus = RECT {
            left: rect.left + sc(3),
            top: rect.top + sc(3),
            right: rect.right - sc(3),
            bottom: rect.bottom - sc(3),
        };
        unsafe {
            let _ = DrawFocusRect(hdc, &focus);
        }
    }
}

fn detail_header_button_glyph(
    id: u16,
    pinned: bool,
    movement_unlocked: bool,
    refreshing: bool,
) -> &'static str {
    match id {
        // Stateful controls show the current state.
        IDC_DETAIL_PIN if pinned => "\u{E718}",
        IDC_DETAIL_PIN => "\u{E77A}",
        IDC_DETAIL_MOVE if movement_unlocked => "\u{E785}",
        IDC_DETAIL_MOVE => "\u{E72E}",
        // Sync is a stable, non-animated busy glyph; it remains legible when
        // Windows has client-area animations disabled.
        IDC_DETAIL_REFRESH if refreshing => "\u{E895}",
        IDC_DETAIL_REFRESH => "\u{E72C}",
        IDC_DETAIL_CLOSE => "\u{E711}",
        _ => "",
    }
}

fn detail_focus_cue_visible(item_state: u32, button_id: u16, mouse_focus_button_id: u32) -> bool {
    item_state & ODS_FOCUS.0 != 0
        && item_state & ODS_NOFOCUSRECT.0 == 0
        && mouse_focus_button_id != button_id as u32
}

unsafe fn draw_detail_header_button(item: &DRAWITEMSTRUCT) {
    let palette = detail_palette(theme::is_dark_mode(), theme::is_high_contrast());
    draw_detail_header_button_content(
        item.hDC,
        item.rcItem,
        item.CtlID as u16,
        item.itemState.0 & ODS_HOTLIGHT.0 != 0
            || DETAIL_HOT_BUTTON_ID.load(Ordering::SeqCst) == item.CtlID,
        item.itemState.0 & ODS_SELECTED.0 != 0,
        detail_focus_cue_visible(
            item.itemState.0,
            item.CtlID as u16,
            DETAIL_MOUSE_FOCUS_BUTTON_ID.load(Ordering::SeqCst),
        ),
        &palette,
    );
}

fn paint_detail_content(
    hdc: HDC,
    width: i32,
    height: i32,
    snapshot: &DetailPopupState,
    is_dark: bool,
    high_contrast: bool,
    scroll_offset: i32,
) {
    let palette = detail_palette(is_dark, high_contrast);

    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        fill_rect_color(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            &palette.bg,
        );
        for edge in [
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: sc(1),
            },
            RECT {
                left: 0,
                top: height - sc(1),
                right: width,
                bottom: height,
            },
            RECT {
                left: 0,
                top: 0,
                right: sc(1),
                bottom: height,
            },
            RECT {
                left: width - sc(1),
                top: 0,
                right: width,
                bottom: height,
            },
        ] {
            fill_rect_color(hdc, &edge, &palette.border);
        }

        let margin = sc(18);
        let brand_icon_size = sc(DETAIL_BRAND_ICON_SIZE);
        // The mark is visually top-heavy. Keep its optical centre aligned
        // with the title using a 1.5dp downward correction at every DPI.
        let brand_icon_optical_offset = detail_brand_icon_optical_offset(active_window_dpi());
        let brand_icon_top =
            (sc(DETAIL_HEADER_H) - brand_icon_size) / 2 + brand_icon_optical_offset;
        draw_detail_brand_icon(
            hdc,
            RECT {
                left: margin,
                top: brand_icon_top,
                right: margin + brand_icon_size,
                bottom: brand_icon_top + brand_icon_size,
            },
            &palette,
            high_contrast,
        );
        draw_detail_title(
            hdc,
            &snapshot.title,
            RECT {
                left: margin + brand_icon_size + sc(DETAIL_BRAND_ICON_TEXT_GAP),
                top: sc(14),
                right: detail_refresh_rect(width).left - sc(8),
                bottom: sc(40),
            },
            &palette.text,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        // The live popup overlays accessible owner-drawn BUTTON controls on
        // these exact rectangles. Drawing the same idle state here keeps
        // WM_PRINTCLIENT and the deterministic --dump-detail-popup output
        // complete as well.
        for id in DETAIL_HEADER_BUTTON_IDS {
            if let Some(rect) = detail_header_button_rect(id, width) {
                draw_detail_header_button_content(hdc, rect, id, false, false, false, &palette);
            }
        }

        let metrics = detail_scroll_metrics(snapshot, height);
        let scroll_offset = scroll_offset.clamp(0, metrics.max_offset);
        let saved_dc = SaveDC(hdc);
        let _ = IntersectClipRect(hdc, 0, metrics.viewport_top, width, metrics.viewport_bottom);
        let mut y = metrics.viewport_top - scroll_offset;
        if snapshot.providers.is_empty() {
            let waiting = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.language.strings().detail_waiting)
                    .unwrap_or(LanguageId::English.strings().detail_waiting)
            };
            draw_detail_body_text(
                hdc,
                waiting,
                RECT {
                    left: margin,
                    top: y,
                    right: width - margin,
                    bottom: y + sc(DETAIL_EMPTY_H),
                },
                &palette.muted,
                13,
                FW_NORMAL.0 as i32,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            );
        } else {
            for group in &snapshot.providers {
                draw_detail_group(hdc, width, y, group, &palette, is_dark, high_contrast);
                y += sc(detail_group_height(group)) + sc(DETAIL_GROUP_GAP);
            }
        }
        if saved_dc != 0 {
            let _ = RestoreDC(hdc, saved_dc);
        }

        if let Some(thumb) = detail_scroll_thumb_rect(width, metrics, scroll_offset) {
            draw_rounded_rect(
                hdc,
                &thumb,
                if high_contrast {
                    &palette.text
                } else {
                    &palette.muted
                },
                sc(DETAIL_SCROLL_THUMB_W),
            );
        }

        let footer_top = height - sc(DETAIL_FOOTER_H);
        fill_rect_color(
            hdc,
            &RECT {
                left: margin,
                top: footer_top,
                right: width - margin,
                bottom: footer_top + sc(1),
            },
            &palette.divider,
        );
        draw_detail_body_text(
            hdc,
            &snapshot.status,
            RECT {
                left: margin,
                top: footer_top + sc(8),
                right: width - sc(74),
                bottom: height - sc(6),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_text(
            hdc,
            &format!("v{}", snapshot.version),
            RECT {
                left: width - sc(74),
                top: footer_top + sc(8),
                right: width - margin,
                bottom: height - sc(6),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// One provider card: a compact identity header sits above aligned quota rows.
/// Warning groups add a slim left rail and a status pill without tinting the
/// whole card, so the hierarchy stays readable at a glance.
fn draw_detail_group(
    hdc: HDC,
    width: i32,
    group_y: i32,
    group: &DetailProviderGroup,
    palette: &DetailPalette,
    is_dark: bool,
    high_contrast: bool,
) {
    let card = RECT {
        left: sc(DETAIL_CARD_MARGIN),
        top: group_y,
        right: width - sc(DETAIL_CARD_MARGIN),
        bottom: group_y + sc(detail_group_height(group)),
    };
    let card_radius = sc(8);
    draw_rounded_rect(hdc, &card, &palette.border, card_radius);
    draw_rounded_rect(
        hdc,
        &RECT {
            left: card.left + sc(1),
            top: card.top + sc(1),
            right: card.right - sc(1),
            bottom: card.bottom - sc(1),
        },
        &palette.card,
        (card_radius - sc(1)).max(sc(1)),
    );

    let accent = provider_accent(group.kind, is_dark, high_contrast);
    let action_required = group
        .badge
        .as_ref()
        .is_some_and(|badge| badge.tone == DetailBadgeTone::ActionRequired);
    let data_is_stale = group.data_is_stale;
    let group_warn = !data_is_stale
        && (group.rows.iter().any(|row| row.warn)
            || group
                .badge
                .as_ref()
                .is_some_and(|badge| badge.tone == DetailBadgeTone::Critical));
    let group_attention = group_warn || action_required;
    if group_attention {
        draw_rounded_rect(
            hdc,
            &detail_attention_rail_rect(&card, card_radius),
            if action_required {
                &accent
            } else {
                &palette.warn
            },
            sc(2),
        );
    }

    // The warning rail nudges its card's content by 2px, as in the reference,
    // while every bar/reset pair still shares one strict column grid.
    let content_left = card.left + sc(14 + if group_attention { 2 } else { 0 });
    let content_right = card.right - sc(12);
    let header_top = card.top + sc(DETAIL_GROUP_PAD_V);
    let header_bottom = header_top + sc(DETAIL_GROUP_HEADER_H);
    let rows_y = header_bottom;
    let row_label_left = content_left;
    let bar_left = content_left + sc(30);
    let percent_right = content_right;
    let percent_text_width =
        measure_detail_text_width(hdc, "100%", "Segoe UI", 16, FW_SEMIBOLD.0 as i32);
    let percent_left = percent_right - detail_percent_column_width(percent_text_width);
    let bar_right = percent_left - sc(4);

    let chip = sc(DETAIL_LOGO_CHIP_SIZE);
    let chip_left = content_left;
    let chip_top = header_top + (sc(DETAIL_GROUP_HEADER_H) - chip) / 2;
    let chip_radius = sc(7);
    match provider_tile_icon(
        group.kind,
        active_window_dpi(),
        is_dark,
        high_contrast,
        TileSize::Chip28,
    ) {
        Some((hicon, _asset_size)) => unsafe {
            // Standard DPI buckets are a 1:1 draw. At an uncommon custom DPI,
            // DrawIconEx performs only the small final adjustment needed to
            // keep the tile centered in the logical 28dp slot.
            let _ = DrawIconEx(
                hdc,
                chip_left,
                chip_top,
                hicon,
                chip,
                chip,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
        },
        None => {
            // High Contrast and decode failures stay palette-driven. Normal
            // provider tiles are rendered offline so their rounded border and
            // detailed mark share one supersampled antialiasing grid.
            let (chip_bg, chip_border, _) =
                provider_chip_style(group.kind, is_dark, high_contrast, palette);
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: chip_left,
                    top: chip_top,
                    right: chip_left + chip,
                    bottom: chip_top + chip,
                },
                &chip_border,
                chip_radius,
            );
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: chip_left + sc(1),
                    top: chip_top + sc(1),
                    right: chip_left + chip - sc(1),
                    bottom: chip_top + chip - sc(1),
                },
                &chip_bg,
                (chip_radius - sc(1)).max(sc(1)),
            );
            let dot = sc(10);
            let dot_left = chip_left + (chip - dot) / 2;
            let dot_top = chip_top + (chip - dot) / 2;
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: dot_left,
                    top: dot_top,
                    right: dot_left + dot,
                    bottom: dot_top + dot,
                },
                &accent,
                dot / 2,
            );
        }
    }

    let badge_layout = group.badge.as_ref().map(|badge| {
        let text_width = measure_detail_text_width(
            hdc,
            &badge.text,
            detail_body_face(&badge.text),
            11,
            FW_NORMAL.0 as i32,
        );
        let width = detail_badge_width(text_width, badge.tone);
        let (badge_left, name_right) = detail_badge_horizontal_bounds(content_right, width);
        (badge, width, badge_left, name_right)
    });
    let name_right = badge_layout
        .as_ref()
        .map(|(_, _, _, name_right)| *name_right)
        .unwrap_or(content_right);

    draw_detail_text(
        hdc,
        &group.name,
        RECT {
            left: chip_left + chip + sc(8),
            top: header_top,
            right: name_right,
            bottom: header_bottom,
        },
        &palette.text,
        16,
        FW_SEMIBOLD.0 as i32,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    if let Some((badge, badge_w, badge_left, _)) = badge_layout {
        if matches!(
            badge.tone,
            DetailBadgeTone::ActionRequired | DetailBadgeTone::Critical
        ) {
            let badge_h = sc(22);
            let badge_rect = RECT {
                left: badge_left,
                top: header_top + (sc(DETAIL_GROUP_HEADER_H) - badge_h) / 2,
                right: content_right,
                bottom: header_top + (sc(DETAIL_GROUP_HEADER_H) + badge_h) / 2,
            };
            let badge_bg = if badge.tone == DetailBadgeTone::ActionRequired && !high_contrast {
                detail_provider_tint(group.kind, is_dark)
            } else {
                palette.warn_bg
            };
            let badge_foreground =
                if badge.tone == DetailBadgeTone::ActionRequired && !high_contrast {
                    detail_action_badge_foreground(group.kind, is_dark)
                } else {
                    palette.warn_on_bg
                };
            draw_rounded_rect(hdc, &badge_rect, &badge_bg, badge_h / 2);
            draw_detail_body_text(
                hdc,
                &badge.text,
                badge_rect,
                &badge_foreground,
                11,
                FW_NORMAL.0 as i32,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        } else {
            draw_detail_body_text(
                hdc,
                &badge.text,
                RECT {
                    left: content_right - badge_w,
                    top: header_top,
                    right: content_right,
                    bottom: header_bottom,
                },
                if badge.tone == DetailBadgeTone::Degraded {
                    &palette.degraded
                } else {
                    &palette.muted
                },
                11,
                FW_NORMAL.0 as i32,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
    }

    let mut row_y = rows_y;
    for row in &group.rows {
        let percent_text = match row.percent {
            Some(percent) => compact_view::display_percent_text(percent),
            None => "--".to_string(),
        };
        if !row.window_label.is_empty() {
            draw_detail_text(
                hdc,
                &row.window_label,
                RECT {
                    left: row_label_left,
                    top: row_y,
                    right: bar_left - sc(2),
                    bottom: row_y + sc(DETAIL_PRIMARY_LINE_H),
                },
                &palette.muted,
                12,
                FW_NORMAL.0 as i32,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        draw_detail_text(
            hdc,
            &percent_text,
            RECT {
                left: percent_left,
                top: row_y,
                right: percent_right,
                bottom: row_y + sc(DETAIL_PRIMARY_LINE_H),
            },
            if data_is_stale {
                &palette.muted
            } else if row.warn {
                &palette.warn
            } else if detail_percent_uses_muted_tone(row.percent) {
                &palette.muted
            } else {
                &palette.text
            },
            16,
            FW_SEMIBOLD.0 as i32,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_rect = RECT {
            left: bar_left,
            top: row_y + sc(4),
            right: bar_right,
            bottom: row_y + sc(14),
        };
        let stale_fill = if high_contrast {
            accent
        } else if is_dark {
            Color::from_hex("#747474")
        } else {
            Color::from_hex("#A6A6A6")
        };
        draw_detail_bar(
            hdc,
            &bar_rect,
            row.percent.unwrap_or(0.0),
            if data_is_stale {
                &stale_fill
            } else if row.warn {
                &palette.warn
            } else {
                &accent
            },
            &palette.track,
            &palette.card,
            row.dividers,
        );

        draw_detail_body_text(
            hdc,
            &row.reset_text,
            RECT {
                left: bar_left,
                top: row_y + sc(20),
                right: content_right,
                bottom: row_y + sc(42),
            },
            &palette.muted,
            11,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );

        row_y += sc(DETAIL_WINDOW_ROW_H);
    }

    if let Some(hint) = &group.hint {
        let (hint_rect, action_rect, outcome_rect) =
            detail_hint_rects(content_left, content_right, row_y);
        let (hint_bg, outcome_foreground) =
            detail_hint_colors(group.kind, is_dark, high_contrast, palette);
        // Informational callout only: deliberately no refresh glyph, border,
        // hover treatment, or button-like affordance. High Contrast gets a
        // system-colour outline so the callout remains visibly grouped.
        if high_contrast {
            draw_rounded_rect(hdc, &hint_rect, &palette.border, sc(7));
            draw_rounded_rect(
                hdc,
                &RECT {
                    left: hint_rect.left + sc(1),
                    top: hint_rect.top + sc(1),
                    right: hint_rect.right - sc(1),
                    bottom: hint_rect.bottom - sc(1),
                },
                &hint_bg,
                sc(6),
            );
        } else {
            draw_rounded_rect(hdc, &hint_rect, &hint_bg, sc(7));
        }
        draw_detail_body_text(
            hdc,
            &hint.action,
            action_rect,
            &palette.text,
            11,
            FW_SEMIBOLD.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        draw_detail_body_text(
            hdc,
            &hint.outcome,
            outcome_rect,
            &outcome_foreground,
            11,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    }
}

fn detail_hint_rects(content_left: i32, content_right: i32, row_y: i32) -> (RECT, RECT, RECT) {
    let hint_rect = RECT {
        left: content_left,
        top: row_y + sc(2),
        right: content_right,
        bottom: row_y + sc(40),
    };
    let action_rect = RECT {
        left: hint_rect.left + sc(10),
        top: hint_rect.top + sc(3),
        right: hint_rect.right - sc(10),
        bottom: hint_rect.top + sc(18),
    };
    let outcome_rect = RECT {
        left: hint_rect.left + sc(10),
        top: hint_rect.top + sc(20),
        right: hint_rect.right - sc(10),
        bottom: hint_rect.bottom - sc(3),
    };
    (hint_rect, action_rect, outcome_rect)
}

fn detail_attention_rail_rect(card: &RECT, card_radius: i32) -> RECT {
    RECT {
        left: card.left,
        top: card.top + card_radius,
        right: card.left + sc(3),
        bottom: card.bottom - card_radius,
    }
}

fn detail_bar_cell_rect(rect: &RECT, cell_count: i32, index: i32) -> RECT {
    let cell_count = cell_count.max(1);
    debug_assert!((0..cell_count).contains(&index));

    let gap = if cell_count > 1 {
        sc(DETAIL_BAR_GAP)
    } else {
        0
    };
    let span = rect.right - rect.left;
    let total_gap = gap * (cell_count - 1);
    let drawable_span = span - total_gap;
    debug_assert!(drawable_span >= cell_count);
    let left = rect.left + drawable_span * index / cell_count + gap * index;
    let right = rect.left + drawable_span * (index + 1) / cell_count + gap * index;

    RECT {
        left,
        top: rect.top,
        right,
        bottom: rect.bottom,
    }
}

fn draw_detail_bar(
    hdc: HDC,
    rect: &RECT,
    percent: f64,
    accent: &Color,
    track: &Color,
    background: &Color,
    dividers: i32,
) {
    // The bar is split into `dividers` discrete cells (5 for the 5-hour window,
    // 7 for the 7-day one - see usage_window_dividers), so the segment count
    // echoes the window length and matches the taskbar widget's pip language.
    // The fill stays continuous across the cells and the boundary cell fills
    // proportionally, so a precise percentage still reads at low values.
    unsafe {
        const SUPERSAMPLE: usize = 4;
        let n = dividers.max(1);
        let radius = sc(2);
        let span = rect.right - rect.left;
        let fill_x = rect.left + ((span as f64) * percent.clamp(0.0, 100.0) / 100.0).round() as i32;
        for i in 0..n {
            let cell = detail_bar_cell_rect(rect, n, i);
            let width = cell.right - cell.left;
            let height = cell.bottom - cell.top;
            if width <= 0 || height <= 0 {
                continue;
            }
            let pixels = detail_bar_cell_pixels(
                width,
                height,
                radius,
                cell.left,
                fill_x,
                accent,
                track,
                background,
                SUPERSAMPLE,
            );
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let _ = StretchDIBits(
                hdc,
                cell.left,
                cell.top,
                width,
                height,
                0,
                0,
                width,
                height,
                Some(pixels.as_ptr() as *const std::ffi::c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn detail_bar_cell_pixels(
    width: i32,
    height: i32,
    radius: i32,
    cell_left: i32,
    fill_x: i32,
    accent: &Color,
    track: &Color,
    background: &Color,
    supersample: usize,
) -> Vec<u32> {
    let samples = supersample.max(1);
    let sample_count = (samples * samples) as u32;
    let radius = radius.max(0).min(width.min(height) / 2) as f64;
    let width_f = width as f64;
    let height_f = height as f64;
    let mut pixels = Vec::with_capacity((width * height).max(0) as usize);

    for y in 0..height {
        for x in 0..width {
            let mut red = 0u32;
            let mut green = 0u32;
            let mut blue = 0u32;
            for sample_y in 0..samples {
                for sample_x in 0..samples {
                    let px = x as f64 + (sample_x as f64 + 0.5) / samples as f64;
                    let py = y as f64 + (sample_y as f64 + 0.5) / samples as f64;
                    let corner_x = if px < radius {
                        radius
                    } else if px > width_f - radius {
                        width_f - radius
                    } else {
                        px
                    };
                    let corner_y = if py < radius {
                        radius
                    } else if py > height_f - radius {
                        height_f - radius
                    } else {
                        py
                    };
                    let inside = radius == 0.0
                        || (px - corner_x).powi(2) + (py - corner_y).powi(2) <= radius.powi(2);
                    let color = if !inside {
                        background
                    } else if cell_left as f64 + px < fill_x as f64 {
                        accent
                    } else {
                        track
                    };
                    red += u32::from(color.r);
                    green += u32::from(color.g);
                    blue += u32::from(color.b);
                }
            }
            let red = (red + sample_count / 2) / sample_count;
            let green = (green + sample_count / 2) / sample_count;
            let blue = (blue + sample_count / 2) / sample_count;
            pixels.push(blue | (green << 8) | (red << 16) | 0xFF00_0000);
        }
    }
    pixels
}

/// Cache of fonts keyed by (face, pixel height, weight), shared by the widget
/// and the detail popup. GDI fonts are cheap but both surfaces repaint on
/// every poll refresh; a handful of cached handles (a few per DPI the window
/// has lived at) beats create/destroy per frame.
type FontCacheEntry = ((&'static str, i32, i32), isize);
static FONT_CACHE: Mutex<Vec<FontCacheEntry>> = Mutex::new(Vec::new());

fn cached_font_named(face: &'static str, height_px: i32, weight: i32) -> HFONT {
    let mut cache = FONT_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, handle)) = cache
        .iter()
        .find(|(key, _)| *key == (face, height_px, weight))
    {
        return HFONT(*handle as *mut _);
    }
    let font_name = native_interop::wide_str(face);
    let font = unsafe {
        CreateFontW(
            -height_px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        )
    };
    cache.push(((face, height_px, weight), font.0 as isize));
    font
}

fn cached_font(height_px: i32, weight: i32) -> HFONT {
    cached_font_named("Segoe UI", height_px, weight)
}

fn is_east_asian_character(ch: char) -> bool {
    matches!(
        ch,
        '\u{3040}'..='\u{30FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{AC00}'..='\u{D7AF}'
            | '\u{F900}'..='\u{FAFF}'
    )
}

fn detail_body_face(text: &str) -> &'static str {
    if text.chars().any(|ch| matches!(ch, '\u{AC00}'..='\u{D7AF}')) {
        "Malgun Gothic"
    } else if text.chars().any(|ch| matches!(ch, '\u{3040}'..='\u{30FF}')) {
        "Yu Gothic UI"
    } else if text.chars().any(|ch| {
        matches!(
            ch,
            '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
        )
    }) {
        "Microsoft YaHei UI"
    } else {
        "Segoe UI"
    }
}

/// Complete provider tiles rendered at 8x from the pinned SVGs, then reduced
/// with premultiplied-alpha Lanczos filtering. The final PNG combines the
/// rounded background, border, and 19dp mark on one antialiasing grid.
const PROVIDER_TILE_BUCKET_DPIS: [u32; 10] = provider_tile::BUCKET_DPIS;
const PROVIDER_TILE_SIZES: [i32; 10] = [28, 35, 42, 49, 56, 63, 70, 84, 98, 112];
const PROVIDER_TILE_C20_SIZES: [i32; 10] = [20, 25, 30, 35, 40, 45, 50, 60, 70, 80];

const PROVIDER_TILE_CLAUDE_DARK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-384.png"),
];
const PROVIDER_TILE_CLAUDE_LIGHT: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-384.png"),
];
const PROVIDER_TILE_CLAUDE_DARK_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-dark-c20-384.png"),
];
const PROVIDER_TILE_CLAUDE_LIGHT_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/claude-light-c20-384.png"),
];
const PROVIDER_TILE_OPENAI: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/openai-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-192.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-216.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-240.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-288.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-336.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-384.png"),
];
const PROVIDER_TILE_OPENAI_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/openai-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/openai-c20-384.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_DARK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-384.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_LIGHT: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-384.png"),
];
const PROVIDER_TILE_GROK: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/grok-96.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-120.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-144.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-168.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-192.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-216.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-240.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-288.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-336.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-384.png"),
];
const PROVIDER_TILE_GROK_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/grok-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/grok-c20-384.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_DARK_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-dark-c20-384.png"),
];
const PROVIDER_TILE_ANTIGRAVITY_LIGHT_C20: [&[u8]; 10] = [
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-96.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-120.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-144.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-168.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-192.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-216.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-240.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-288.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-336.png"),
    include_bytes!("icons/providers/rendered/tiles/antigravity-light-c20-384.png"),
];

#[derive(Clone, Copy)]
struct ProviderTileAsset {
    bucket: usize,
    size: i32,
    bytes: &'static [u8],
}

fn nearest_provider_tile_bucket(dpi: u32) -> usize {
    provider_tile::nearest_bucket(dpi)
}

fn provider_tile_asset(
    kind: tray_icon::TrayIconKind,
    dpi: u32,
    is_dark: bool,
    tile_size: TileSize,
) -> ProviderTileAsset {
    if tile_size == TileSize::Chip16 {
        let asset = provider_tile::chip16_asset(kind.brand(), dpi, is_dark);
        return ProviderTileAsset {
            bucket: asset.bucket,
            size: asset.size,
            bytes: asset.bytes,
        };
    }

    let bucket = nearest_provider_tile_bucket(dpi);
    let bytes = match (kind, is_dark, tile_size) {
        (tray_icon::TrayIconKind::Claude, true, TileSize::Chip28) => {
            PROVIDER_TILE_CLAUDE_DARK[bucket]
        }
        (tray_icon::TrayIconKind::Claude, false, TileSize::Chip28) => {
            PROVIDER_TILE_CLAUDE_LIGHT[bucket]
        }
        (tray_icon::TrayIconKind::Claude, true, TileSize::Chip20) => {
            PROVIDER_TILE_CLAUDE_DARK_C20[bucket]
        }
        (tray_icon::TrayIconKind::Claude, false, TileSize::Chip20) => {
            PROVIDER_TILE_CLAUDE_LIGHT_C20[bucket]
        }
        (tray_icon::TrayIconKind::Codex, _, TileSize::Chip28) => PROVIDER_TILE_OPENAI[bucket],
        (tray_icon::TrayIconKind::Codex, _, TileSize::Chip20) => PROVIDER_TILE_OPENAI_C20[bucket],
        (tray_icon::TrayIconKind::Antigravity, true, TileSize::Chip28) => {
            PROVIDER_TILE_ANTIGRAVITY_DARK[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, false, TileSize::Chip28) => {
            PROVIDER_TILE_ANTIGRAVITY_LIGHT[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, true, TileSize::Chip20) => {
            PROVIDER_TILE_ANTIGRAVITY_DARK_C20[bucket]
        }
        (tray_icon::TrayIconKind::Antigravity, false, TileSize::Chip20) => {
            PROVIDER_TILE_ANTIGRAVITY_LIGHT_C20[bucket]
        }
        (tray_icon::TrayIconKind::Grok, _, TileSize::Chip28) => PROVIDER_TILE_GROK[bucket],
        (tray_icon::TrayIconKind::Grok, _, TileSize::Chip20) => PROVIDER_TILE_GROK_C20[bucket],
        (_, _, TileSize::Chip16) => unreachable!("chip16 uses the shared provider tile module"),
    };
    let (logical_size, size) = match tile_size {
        TileSize::Chip16 => unreachable!("chip16 uses the shared provider tile module"),
        TileSize::Chip20 => (20, PROVIDER_TILE_C20_SIZES[bucket]),
        TileSize::Chip28 => (28, PROVIDER_TILE_SIZES[bucket]),
    };
    debug_assert_eq!(
        size,
        scale_px_for_dpi(logical_size, PROVIDER_TILE_BUCKET_DPIS[bucket])
    );
    ProviderTileAsset {
        bucket,
        size,
        bytes,
    }
}

/// HICONs decoded from exact-DPI PNG tiles, keyed by provider, bucket and theme.
/// Like the font cache, the popup repaints on every refresh, so caching a
/// handful of handles beats decoding each frame. Windows Vista+ accepts PNG
/// icon resource bits, so no runtime image dependency is required.
type ProviderTileCacheEntry = ((tray_icon::TrayIconKind, usize, bool, TileSize), isize);
static PROVIDER_TILE_CACHE: Mutex<Vec<ProviderTileCacheEntry>> = Mutex::new(Vec::new());
const DETAIL_BRAND_ICON_PNG: &[u8] = include_bytes!("icons/256x256.png");
type DetailBrandIconCacheEntry = (i32, isize);
static DETAIL_BRAND_ICON_CACHE: Mutex<Vec<DetailBrandIconCacheEntry>> = Mutex::new(Vec::new());

fn provider_tile_icon(
    kind: tray_icon::TrayIconKind,
    dpi: u32,
    is_dark: bool,
    high_contrast: bool,
    tile_size: TileSize,
) -> Option<(HICON, i32)> {
    if tile_size == TileSize::Chip16 {
        return provider_tile::cached_chip16_icon(kind.brand(), dpi, is_dark, high_contrast);
    }
    if high_contrast {
        return None;
    }
    let asset = provider_tile_asset(kind, dpi, is_dark, tile_size);
    let mut cache = PROVIDER_TILE_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (kind, asset.bucket, is_dark, tile_size);
    if let Some((_, handle)) = cache.iter().find(|(cached_key, _)| *cached_key == key) {
        return Some((HICON(*handle as *mut _), asset.size));
    }
    match unsafe {
        CreateIconFromResourceEx(
            asset.bytes,
            TRUE,
            0x0003_0000,
            asset.size,
            asset.size,
            LR_DEFAULTCOLOR,
        )
    } {
        Ok(hicon) => {
            cache.push((key, hicon.0 as isize));
            Some((hicon, asset.size))
        }
        Err(_) => None,
    }
}

fn detail_brand_icon(dpi: u32) -> Option<HICON> {
    let size = scale_px_for_dpi(DETAIL_BRAND_ICON_SIZE, dpi);
    let mut cache = DETAIL_BRAND_ICON_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, handle)) = cache.iter().find(|(cached_size, _)| *cached_size == size) {
        return Some(HICON(*handle as *mut _));
    }
    match unsafe {
        CreateIconFromResourceEx(
            DETAIL_BRAND_ICON_PNG,
            TRUE,
            0x0003_0000,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
    } {
        Ok(icon) => {
            cache.push((size, icon.0 as isize));
            Some(icon)
        }
        Err(_) => None,
    }
}

fn detail_brand_icon_optical_offset(dpi: u32) -> i32 {
    ((normalize_dpi(dpi) as i32 * 3 + 96) / 192).max(1)
}

fn draw_detail_brand_icon(hdc: HDC, rect: RECT, palette: &DetailPalette, high_contrast: bool) {
    if !high_contrast {
        if let Some(icon) = detail_brand_icon(active_window_dpi()) {
            unsafe {
                let _ = DrawIconEx(
                    hdc,
                    rect.left,
                    rect.top,
                    icon,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    0,
                    HBRUSH::default(),
                    DI_NORMAL,
                );
            }
            return;
        }
    }

    // The coloured PNG cannot guarantee contrast against a user-defined
    // High Contrast palette. Preserve the mark's three-bar silhouette with
    // system colours instead.
    let radius = sc(4);
    unsafe {
        let region = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let brush = CreateSolidBrush(COLORREF(palette.text.to_colorref()));
        let _ = FrameRgn(hdc, region, brush, sc(1).max(1), sc(1).max(1));
        let _ = DeleteObject(region);
        let _ = DeleteObject(brush);
    }
    for bar in [
        RECT {
            left: rect.left + sc(4),
            top: rect.top + sc(5),
            right: rect.left + sc(12),
            bottom: rect.top + sc(7),
        },
        RECT {
            left: rect.left + sc(4),
            top: rect.top + sc(9),
            right: rect.left + sc(16),
            bottom: rect.top + sc(12),
        },
        RECT {
            left: rect.left + sc(4),
            top: rect.top + sc(14),
            right: rect.left + sc(10),
            bottom: rect.top + sc(16),
        },
    ] {
        draw_rounded_rect(hdc, &bar, &palette.text, sc(1));
    }
}

fn detail_title_uses_cjk_tracking(title: &str) -> bool {
    matches!(title, "更筹" | "更籌")
}

fn draw_detail_title(hdc: HDC, title: &str, rect: RECT, color: &Color, flags: DRAW_TEXT_FORMAT) {
    draw_detail_text_face_with_tracking(
        hdc,
        title,
        rect,
        color,
        detail_body_face(title),
        18,
        FW_NORMAL.0 as i32,
        flags,
        if detail_title_uses_cjk_tracking(title) {
            sc(1)
        } else {
            0
        },
    );
}

fn draw_detail_text(
    hdc: HDC,
    text: &str,
    rect: RECT,
    color: &Color,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    draw_detail_text_face(hdc, text, rect, color, "Segoe UI", font_size, weight, flags);
}

fn measure_detail_text_width(
    hdc: HDC,
    text: &str,
    face: &'static str,
    font_size: i32,
    weight: i32,
) -> i32 {
    unsafe {
        let font = cached_font_named(face, sc(font_size), weight);
        let old_font = SelectObject(hdc, font);
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let measured = if wide.is_empty() || !GetTextExtentPoint32W(hdc, &wide, &mut size).as_bool()
        {
            0
        } else {
            size.cx
        };
        SelectObject(hdc, old_font);
        measured
    }
}

fn detail_badge_width(measured_text_width: i32, tone: DetailBadgeTone) -> i32 {
    let padding = if matches!(
        tone,
        DetailBadgeTone::ActionRequired | DetailBadgeTone::Critical
    ) {
        sc(20)
    } else {
        sc(2)
    };
    (measured_text_width + padding).clamp(sc(64), sc(160))
}

fn detail_badge_horizontal_bounds(content_right: i32, badge_width: i32) -> (i32, i32) {
    let badge_left = content_right - badge_width;
    (badge_left, badge_left - sc(8))
}

fn detail_percent_column_width(measured_text_width: i32) -> i32 {
    (measured_text_width + sc(2)).clamp(sc(42), sc(48))
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_body_text(
    hdc: HDC,
    text: &str,
    rect: RECT,
    color: &Color,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    draw_detail_text_face(
        hdc,
        text,
        rect,
        color,
        detail_body_face(text),
        font_size,
        weight,
        flags,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_text_face(
    hdc: HDC,
    text: &str,
    rect: RECT,
    color: &Color,
    face: &'static str,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    draw_detail_text_face_with_tracking(hdc, text, rect, color, face, font_size, weight, flags, 0);
}

#[allow(clippy::too_many_arguments)]
fn draw_detail_text_face_with_tracking(
    hdc: HDC,
    text: &str,
    mut rect: RECT,
    color: &Color,
    face: &'static str,
    font_size: i32,
    weight: i32,
    flags: DRAW_TEXT_FORMAT,
    character_extra: i32,
) {
    unsafe {
        let font = cached_font_named(face, sc(font_size), weight);
        let old_font = SelectObject(hdc, font);
        // Paint callers normally prepare a transparent text background once,
        // but owner-drawn child controls receive a fresh HDC whose default is
        // OPAQUE. Without setting this locally, GDI paints a white rectangle
        // behind each header glyph on a dark popup.
        let old_background_mode = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let old_character_extra = SetTextCharacterExtra(hdc, character_extra);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let _ = DrawTextW(hdc, &mut text_wide, &mut rect, flags);
        if old_character_extra != i32::MIN {
            let _ = SetTextCharacterExtra(hdc, old_character_extra);
        }
        if old_background_mode != 0 {
            let _ = SetBkMode(hdc, BACKGROUND_MODE(old_background_mode as u32));
        }
        SelectObject(hdc, old_font);
    }
}

fn fill_rect_color(hdc: HDC, rect: &RECT, color: &Color) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        FillRect(hdc, rect, brush);
        let _ = DeleteObject(brush);
    }
}

fn show_error_message(title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            dialog_owner_hwnd(),
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

fn post_persistence_warning() {
    let hwnd = poll_controller_hwnd();
    if hwnd != HWND::default() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_PERSISTENCE_WARNING, WPARAM(0), LPARAM(0));
        }
    }
}

fn show_pending_persistence_warning_once() {
    if PERSISTENCE_WARNING_SHOWN.load(Ordering::Acquire) {
        return;
    }
    let Some(error) = settings::take_persistence_warning() else {
        return;
    };
    if PERSISTENCE_WARNING_SHOWN.swap(true, Ordering::AcqRel) {
        return;
    }
    let strings = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.language.strings())
            .unwrap_or(LanguageId::English.strings())
    };
    let message = format!("{}\n\n{error}", strings.settings_storage_failed);
    show_error_message(strings.settings, &message);
}

fn show_update_prompt(strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            dialog_owner_hwnd(),
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    // No release channel configured (this project's default): show the plain
    // version instead of a "Check for updates" action that can only fail.
    if !updater::update_channel_configured() {
        return format!("v{current}");
    }
    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Prompting => format!("v{current} - {}", strings.update_in_progress),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        // A remembered update reads exactly like a freshly found one: from the
        // user's side the situation is the same, and the difference (no stored
        // download URL) only shows up as an extra check after they click.
        UpdateStatus::Available(release) => available_version_label(
            strings,
            language,
            install_channel,
            current,
            &release.latest_version,
        ),
        UpdateStatus::AvailableRemembered { version } => {
            available_version_label(strings, language, install_channel, current, version)
        }
    }
}

fn available_version_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    current: &str,
    latest: &str,
) -> String {
    match install_channel {
        InstallChannel::Portable => format!("v{current} - {} v{latest}", strings.update_to),
        InstallChannel::Winget => format!(
            "v{current} - {} v{latest}",
            localization::update_via_winget(language)
        ),
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    if !updater::update_channel_configured() {
        return;
    }
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, install_channel, already_in_progress) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        let strings = app_state.language.strings();
        let already_in_progress = update_status_is_busy(&app_state.update_status);
        if !already_in_progress {
            app_state.update_status = UpdateStatus::Checking;
        }

        (strings, app_state.install_channel, already_in_progress)
    };
    if already_in_progress {
        if interactive {
            show_info_message(strings.updates, strings.update_in_progress);
        }
        return;
    }

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_outcome = Some(settings::LastUpdateOutcome::UpToDate);
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_outcome = Some(settings::LastUpdateOutcome::Available {
                            version: release.latest_version.clone(),
                        });
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.update_status = UpdateStatus::Prompting;
                        }
                    }
                    let accepted = show_update_prompt(strings, &release);
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            if matches!(s.update_status, UpdateStatus::Prompting) {
                                s.update_status = UpdateStatus::Available(release.clone());
                            }
                        }
                    }
                    if accepted {
                        match install_channel {
                            InstallChannel::Portable => begin_update_apply(hwnd, release),
                            InstallChannel::Winget => begin_winget_update(hwnd),
                        }
                    }
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        // The check failed, so the previous answer is no
                        // longer something to stand behind - forget it rather
                        // than keep showing a stale claim.
                        s.last_update_outcome = None;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}\n\n{}", strings.update_failed, error);
                    show_error_message(strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let (strings, already_in_progress) = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        let strings = app_state.language.strings();
        let already_in_progress = update_status_is_busy(&app_state.update_status);
        if !already_in_progress {
            app_state.update_status = UpdateStatus::Applying;
        }

        (strings, already_in_progress)
    };
    if already_in_progress {
        show_info_message(strings.updates, strings.update_in_progress);
        return;
    }

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => request_process_quit(),
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}\n\n{}", strings.update_failed, error);
                show_error_message(strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(_hwnd: HWND) {
    let (strings, expected_version) = {
        let state = lock_state();
        state.as_ref().map(|s| {
            let expected_version = match &s.update_status {
                UpdateStatus::Available(release) => release.latest_version.clone(),
                _ => env!("CARGO_PKG_VERSION").to_string(),
            };
            (s.language.strings(), expected_version)
        })
    }
    .unwrap_or((
        LanguageId::English.strings(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));

    match updater::begin_winget_update(&expected_version) {
        Ok(()) => request_process_quit(),
        Err(error) => {
            let message = format!("{}\n\n{}", strings.update_failed, error);
            show_error_message(strings.updates, &message);
        }
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "Gengchou";

fn quoted_startup_executable(value: &[u16]) -> Option<&[u16]> {
    let mut value = value;
    while value.last() == Some(&0) {
        value = &value[..value.len() - 1];
    }
    if value.len() < 3
        || value.first().copied() != Some(u16::from(b'"'))
        || value.last().copied() != Some(u16::from(b'"'))
    {
        return None;
    }
    let executable = &value[1..value.len() - 1];
    (!executable.is_empty() && !executable.contains(&u16::from(b'"'))).then_some(executable)
}

fn windows_paths_equal(left: &[u16], right: &[u16]) -> bool {
    unsafe { CompareStringOrdinal(left, right, BOOL(1)) == CSTR_EQUAL }
}

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let mut value_type = REG_VALUE_TYPE::default();
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            Some(&mut data_size),
        );
        if result.is_err() || value_type != REG_SZ || data_size < 4 || data_size % 2 != 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            Some(&mut value_type),
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err()
            || value_type != REG_SZ
            || data_size < 4
            || data_size % 2 != 0
            || data_size as usize > buf.len()
        {
            return false;
        }

        let wide_value =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        if wide_value.last() != Some(&0) {
            return false;
        }
        let Some(registered_exe) = quoted_startup_executable(wide_value) else {
            return false;
        };

        let Ok(current_exe) = std::env::current_exe() else {
            return false;
        };
        let current_exe: Vec<u16> = current_exe.as_os_str().encode_wide().collect();
        windows_paths_equal(registered_exe, &current_exe)
    }
}

fn set_startup_enabled(enable: bool) -> Result<(), String> {
    let current_exe = enable
        .then(std::env::current_exe)
        .transpose()
        .map_err(|error| format!("Unable to locate the current executable: {error}"))?;
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return Err(format!(
                "Unable to open the Windows startup registry key: {result:?}"
            ));
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let Some(exe) = current_exe.as_ref() else {
                let _ = RegCloseKey(hkey);
                return Err("Unable to locate the current executable.".to_string());
            };
            let mut quoted = Vec::new();
            quoted.push(u16::from(b'"'));
            quoted.extend(exe.as_os_str().encode_wide());
            quoted.push(u16::from(b'"'));
            quoted.push(0);
            let result = RegSetValueExW(
                hkey,
                PCWSTR::from_raw(key_name.as_ptr()),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    quoted.as_ptr() as *const u8,
                    quoted.len() * 2,
                )),
            );
            if result.is_err() {
                let _ = RegCloseKey(hkey);
                return Err(format!(
                    "Unable to save the Windows startup setting: {result:?}"
                ));
            }
        } else {
            let result = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
            if result.is_err() && result != ERROR_FILE_NOT_FOUND {
                let _ = RegCloseKey(hkey);
                return Err(format!(
                    "Unable to remove the Windows startup setting: {result:?}"
                ));
            }
        }

        let _ = RegCloseKey(hkey);
        Ok(())
    }
}

const LEFT_DIVIDER_W: i32 = 5;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
// Fits the longest compact English forms (such as "100%·59m" and
// "100%·now") at Segoe UI 12px without making short values look sparse.
const WIDGET_HEIGHT: i32 = 46;

fn is_drag_handle_point(client_x: i32, client_y: i32) -> bool {
    let divider_h = sc(25);
    let divider_top = (sc(WIDGET_HEIGHT) - divider_h) / 2;
    client_x >= 0
        && client_x < sc(LEFT_DIVIDER_W)
        && client_y >= divider_top
        && client_y < divider_top + divider_h
}

fn cursor_is_on_drag_handle(hwnd: HWND) -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() || !ScreenToClient(hwnd, &mut pt).as_bool() {
            return false;
        }
        is_drag_handle_point(pt.x, pt.y)
    }
}

fn active_model_count(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
) -> i32 {
    (show_claude_code as i32 + show_codex as i32 + show_antigravity as i32 + show_grok as i32)
        .max(1)
}

fn total_widget_width_for(active_models: i32) -> i32 {
    let metrics = compact_metrics();
    let placeholder_width = sc(12);
    let initial_pill_width =
        metrics.pill_pad_x * 2 + metrics.chip16 + metrics.chip_gap + placeholder_width;
    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + initial_pill_width * active_models
        + metrics.badge_gap * (active_models - 1)
        + metrics.badge_right_pad
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    let scene = compact_scene_for_hwnd(
        state.hwnd.to_hwnd(),
        &state.compact_vm,
        state.is_high_contrast,
        false,
    );
    sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN) + scene.width
}

fn total_widget_width() -> i32 {
    let state = lock_state();
    state
        .as_ref()
        .map(total_widget_width_for_state)
        .unwrap_or_else(|| total_widget_width_for(1))
}

fn sync_widget_tooltip_hits(scene: &Scene) {
    lock_widget_tooltip_runtime().hits = scene.badge_hits.clone();
}

fn widget_tooltip_kind_at(client_x: i32, client_y: i32) -> Option<tray_icon::TrayIconKind> {
    let origin_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
    lock_widget_tooltip_runtime()
        .hits
        .iter()
        .find(|hit| {
            client_x >= origin_x + hit.rect.x
                && client_x < origin_x + hit.rect.x + hit.rect.w
                && client_y >= hit.rect.y
                && client_y < hit.rect.y + hit.rect.h
        })
        .map(|hit| hit.kind)
}

fn widget_tooltip_hwnd() -> Option<HWND> {
    let state = lock_state();
    state
        .as_ref()
        .and_then(|s| s.widget_tooltip_hwnd)
        .map(SendHwnd::to_hwnd)
        .filter(|hwnd| unsafe { IsWindow(*hwnd).as_bool() })
}

fn widget_tooltip_abbrev(kind: tray_icon::TrayIconKind) -> &'static str {
    match kind {
        tray_icon::TrayIconKind::Claude => "CL",
        tray_icon::TrayIconKind::Codex => "CX",
        tray_icon::TrayIconKind::Antigravity => "AG",
        tray_icon::TrayIconKind::Grok => "GK",
    }
}

fn widget_tooltip_aux_text(row: &WidgetTooltipRow) -> String {
    if row.percent_text.is_empty() {
        row.reset_text.clone()
    } else {
        format!("· {}", row.reset_text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WidgetTooltipLayout {
    width: i32,
    height: i32,
    label_width: i32,
    percent_width: i32,
}

fn widget_tooltip_layout(hdc: HDC, snapshot: &WidgetTooltipSnapshot) -> WidgetTooltipLayout {
    let padding = sc(10);
    let chip = sc(20);
    let header_gap = sc(8);
    let column_gap = sc(8);
    let row_height = sc(20);
    let header_width = chip
        + header_gap
        + measure_detail_text_width(
            hdc,
            &snapshot.provider_name,
            detail_body_face(&snapshot.provider_name),
            12,
            FW_SEMIBOLD.0 as i32,
        );
    let label_width = snapshot
        .rows
        .iter()
        .map(|row| {
            measure_detail_text_width(
                hdc,
                &row.window_label,
                detail_body_face(&row.window_label),
                12,
                FW_NORMAL.0 as i32,
            )
        })
        .max()
        .unwrap_or_default();
    let percent_width = snapshot
        .rows
        .iter()
        .map(|row| {
            measure_detail_text_width(hdc, &row.percent_text, "Segoe UI", 12, FW_SEMIBOLD.0 as i32)
        })
        .max()
        .unwrap_or_default();
    let reset_width = snapshot
        .rows
        .iter()
        .map(|row| {
            let text = widget_tooltip_aux_text(row);
            measure_detail_text_width(hdc, &text, detail_body_face(&text), 12, FW_NORMAL.0 as i32)
        })
        .max()
        .unwrap_or_default();
    let body_width = label_width
        + usize::from(percent_width > 0) as i32 * column_gap
        + percent_width
        + column_gap
        + reset_width;
    let width = (padding * 2 + header_width.max(body_width))
        .clamp(sc(WIDGET_TOOLTIP_MIN_WIDTH), sc(WIDGET_TOOLTIP_MAX_WIDTH));
    let height = padding * 2 + chip + sc(6) + row_height * snapshot.rows.len() as i32;
    WidgetTooltipLayout {
        width,
        height,
        label_width,
        percent_width,
    }
}

fn paint_widget_tooltip(hdc: HDC, hwnd: HWND) {
    let snapshot = lock_widget_tooltip_runtime().snapshot.clone();
    let Some(snapshot) = snapshot else {
        return;
    };
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    paint_widget_tooltip_content(
        hdc,
        client.right - client.left,
        client.bottom - client.top,
        &snapshot,
        theme::is_dark_mode(),
        theme::is_high_contrast(),
    );
}

fn paint_widget_tooltip_content(
    hdc: HDC,
    width: i32,
    height: i32,
    snapshot: &WidgetTooltipSnapshot,
    is_dark: bool,
    high_contrast: bool,
) {
    let palette = detail_palette(is_dark, high_contrast);
    let client = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
    }
    draw_rounded_rect(hdc, &client, &palette.border, sc(7));
    draw_rounded_rect(
        hdc,
        &RECT {
            left: sc(1),
            top: sc(1),
            right: client.right - sc(1),
            bottom: client.bottom - sc(1),
        },
        &palette.card,
        sc(6),
    );

    let layout = widget_tooltip_layout(hdc, snapshot);
    let padding = sc(10);
    let chip = sc(20);
    let header_gap = sc(8);
    let column_gap = sc(8);
    let row_height = sc(20);
    if let Some((icon, _asset_size)) = provider_tile_icon(
        snapshot.kind,
        active_window_dpi(),
        is_dark,
        high_contrast,
        TileSize::Chip20,
    ) {
        unsafe {
            let _ = DrawIconEx(
                hdc,
                padding,
                padding,
                icon,
                chip,
                chip,
                0,
                HBRUSH::default(),
                DI_NORMAL,
            );
        }
    } else {
        let chip_rect = RECT {
            left: padding,
            top: padding,
            right: padding + chip,
            bottom: padding + chip,
        };
        draw_rounded_rect(hdc, &chip_rect, &palette.border, sc(5));
        draw_rounded_rect(
            hdc,
            &RECT {
                left: chip_rect.left + sc(1),
                top: chip_rect.top + sc(1),
                right: chip_rect.right - sc(1),
                bottom: chip_rect.bottom - sc(1),
            },
            &palette.card,
            sc(4),
        );
        draw_detail_text(
            hdc,
            widget_tooltip_abbrev(snapshot.kind),
            chip_rect,
            &palette.text,
            9,
            FW_SEMIBOLD.0 as i32,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    draw_detail_body_text(
        hdc,
        &snapshot.provider_name,
        RECT {
            left: padding + chip + header_gap,
            top: padding,
            right: layout.width - padding,
            bottom: padding + chip,
        },
        &palette.text,
        12,
        FW_SEMIBOLD.0 as i32,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    let mut row_top = padding + chip + sc(6);
    for row in &snapshot.rows {
        let row_bottom = row_top + row_height;
        let label_left = padding;
        let percent_left = label_left + layout.label_width + column_gap;
        let percent_right = percent_left + layout.percent_width;
        let aux_left = if layout.percent_width > 0 {
            percent_right + column_gap
        } else {
            label_left + layout.label_width + column_gap
        };
        draw_detail_body_text(
            hdc,
            &row.window_label,
            RECT {
                left: label_left,
                top: row_top,
                right: label_left + layout.label_width,
                bottom: row_bottom,
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        if !row.percent_text.is_empty() {
            draw_detail_text(
                hdc,
                &row.percent_text,
                RECT {
                    left: percent_left,
                    top: row_top,
                    right: percent_right,
                    bottom: row_bottom,
                },
                if row.warn {
                    &palette.warn
                } else {
                    &palette.text
                },
                12,
                FW_SEMIBOLD.0 as i32,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        let aux_text = widget_tooltip_aux_text(row);
        draw_detail_body_text(
            hdc,
            &aux_text,
            RECT {
                left: aux_left,
                top: row_top,
                right: layout.width - padding,
                bottom: row_bottom,
            },
            &palette.muted,
            12,
            FW_NORMAL.0 as i32,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        row_top = row_bottom;
    }
}

fn ensure_widget_tooltip_window_class() -> bool {
    if WIDGET_TOOLTIP_CLASS_REGISTERED.load(Ordering::SeqCst) {
        return true;
    }
    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("widget tooltip: GetModuleHandleW failed", error);
                return false;
            }
        };
        let class_name = native_interop::wide_str(WIDGET_TOOLTIP_WINDOW_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(widget_tooltip_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            diagnose::log("widget tooltip: RegisterClassExW failed");
            return false;
        }
    }
    WIDGET_TOOLTIP_CLASS_REGISTERED.store(true, Ordering::SeqCst);
    true
}

extern "system" fn widget_tooltip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        widget_tooltip_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => unsafe {
            diagnose::log(format!(
                "panic in widget_tooltip_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        },
    }
}

unsafe fn widget_tooltip_wnd_proc_impl(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_widget_tooltip(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            paint_widget_tooltip(HDC(wparam.0 as *mut _), hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DPICHANGED_MSG => {
            let _message_dpi_scope = DpiScope::new(dpi_from_wparam(wparam));
            apply_suggested_dpi_rect(hwnd, lparam, "widget tooltip");
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_DESTROY => {
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                if s.widget_tooltip_hwnd
                    .is_some_and(|stored| stored.to_hwnd() == hwnd)
                {
                    s.widget_tooltip_hwnd = None;
                }
            }
            lock_widget_tooltip_runtime().snapshot = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_widget_tooltip_window(owner: HWND) -> Option<HWND> {
    if let Some(hwnd) = widget_tooltip_hwnd() {
        return Some(hwnd);
    }
    if !ensure_widget_tooltip_window_class() {
        return None;
    }
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).ok()?;
        let class_name = native_interop::wide_str(WIDGET_TOOLTIP_WINDOW_CLASS_NAME);
        let tooltip = match CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("widget tooltip: CreateWindowExW failed", error);
                return None;
            }
        };
        let (is_dark, high_contrast, owner_matches) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (s.is_dark, s.is_high_contrast, s.hwnd.to_hwnd() == owner),
                None => (false, false, false),
            }
        };
        if !owner_matches {
            let _ = DestroyWindow(tooltip);
            return None;
        }
        apply_floating_dwm_style(tooltip, is_dark, high_contrast);
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_tooltip_hwnd = Some(SendHwnd::from_hwnd(tooltip));
        }
        Some(tooltip)
    }
}

fn widget_tooltip_anchor_rect(owner: HWND, kind: tray_icon::TrayIconKind) -> Option<RECT> {
    let hit = lock_widget_tooltip_runtime()
        .hits
        .iter()
        .find(|hit| hit.kind == kind)
        .copied()?;
    let mut owner_rect = RECT::default();
    unsafe {
        GetWindowRect(owner, &mut owner_rect).ok()?;
    }
    let origin_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
    Some(RECT {
        left: owner_rect.left + origin_x + hit.rect.x,
        top: owner_rect.top + hit.rect.y,
        right: owner_rect.left + origin_x + hit.rect.x + hit.rect.w,
        bottom: owner_rect.top + hit.rect.y + hit.rect.h,
    })
}

fn widget_tooltip_position_for_anchor(
    anchor: RECT,
    work: RECT,
    width: i32,
    height: i32,
    gap: i32,
) -> (i32, i32) {
    let centered_x = anchor.left + (anchor.right - anchor.left - width) / 2;
    let centered_y = anchor.top + (anchor.bottom - anchor.top - height) / 2;
    let (x, y) = if anchor.top >= work.bottom {
        (centered_x, anchor.top - gap - height)
    } else if anchor.bottom <= work.top {
        (centered_x, anchor.bottom + gap)
    } else if anchor.left >= work.right {
        (anchor.left - gap - width, centered_y)
    } else if anchor.right <= work.left {
        (anchor.right + gap, centered_y)
    } else if anchor.top - work.top >= height + gap {
        (centered_x, anchor.top - gap - height)
    } else {
        (centered_x, anchor.bottom + gap)
    };
    (
        clamp_i32(x, work.left, work.right - width),
        clamp_i32(y, work.top, work.bottom - height),
    )
}

fn widget_tooltip_position(
    owner: HWND,
    kind: tray_icon::TrayIconKind,
    width: i32,
    height: i32,
) -> Option<(i32, i32)> {
    let anchor = widget_tooltip_anchor_rect(owner, kind)?;
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: anchor.left + (anchor.right - anchor.left) / 2,
                y: anchor.top + (anchor.bottom - anchor.top) / 2,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
    }
    Some(widget_tooltip_position_for_anchor(
        anchor,
        info.rcWork,
        width,
        height,
        sc(WIDGET_TOOLTIP_EDGE_GAP),
    ))
}

unsafe fn hide_widget_tooltip(owner: HWND, clear_hover: bool) {
    let _ = KillTimer(owner, TIMER_WIDGET_TOOLTIP);
    if clear_hover {
        lock_widget_tooltip_runtime().hover_kind = None;
    }
    if let Some(tooltip) = widget_tooltip_hwnd() {
        let _ = ShowWindow(tooltip, SW_HIDE);
    }
}

/// The detail popup shows the same numbers with room to spare, so a hover
/// tooltip beside it is redundant and reads as a stuck window. Clicking the
/// widget hides the tooltip, but the pointer is still over the badge it came
/// from, so the very next mouse move would otherwise count as a new hover and
/// bring it back on top of the popup.
fn detail_popup_is_open() -> bool {
    let detail_hwnd = {
        let state = lock_state();
        state.as_ref().and_then(|s| s.details_hwnd)
    };
    detail_hwnd.is_some_and(|hwnd| unsafe { IsWindow(hwnd).as_bool() })
}

unsafe fn update_widget_tooltip_hover(owner: HWND, client_x: i32, client_y: i32) {
    let next = widget_tooltip_kind_at(client_x, client_y);
    let changed = {
        let mut runtime = lock_widget_tooltip_runtime();
        if runtime.hover_kind == next {
            false
        } else {
            runtime.hover_kind = next;
            true
        }
    };
    if !changed {
        return;
    }
    hide_widget_tooltip(owner, false);
    if next.is_some()
        && !detail_popup_is_open()
        && SetTimer(owner, TIMER_WIDGET_TOOLTIP, WIDGET_TOOLTIP_DELAY_MS, None) == 0
    {
        diagnose::log("widget tooltip: unable to start hover timer");
    }
}

unsafe fn show_widget_tooltip_for_hover(owner: HWND) {
    let _ = KillTimer(owner, TIMER_WIDGET_TOOLTIP);
    if detail_popup_is_open() {
        hide_widget_tooltip(owner, false);
        return;
    }
    let expected = lock_widget_tooltip_runtime().hover_kind;
    let Some(kind) = expected else {
        return;
    };
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() || !ScreenToClient(owner, &mut cursor).as_bool() {
        hide_widget_tooltip(owner, true);
        return;
    }
    if widget_tooltip_kind_at(cursor.x, cursor.y) != Some(kind) {
        hide_widget_tooltip(owner, true);
        return;
    }

    let snapshot = widget_tooltip_snapshot(kind);
    lock_widget_tooltip_runtime().snapshot = Some(snapshot.clone());
    let Some(tooltip) = ensure_widget_tooltip_window(owner) else {
        return;
    };
    let _tooltip_dpi_scope = DpiScope::for_window(tooltip);
    let hdc = GetDC(tooltip);
    let layout = if hdc.0.is_null() {
        WidgetTooltipLayout {
            width: sc(240),
            height: sc(56 + snapshot.rows.len() as i32 * 20),
            label_width: sc(20),
            percent_width: sc(36),
        }
    } else {
        let layout = widget_tooltip_layout(hdc, &snapshot);
        ReleaseDC(tooltip, hdc);
        layout
    };
    let Some((x, y)) = widget_tooltip_position(owner, kind, layout.width, layout.height) else {
        return;
    };
    let (is_dark, high_contrast) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.is_dark, s.is_high_contrast))
            .unwrap_or((false, false))
    };
    apply_floating_dwm_style(tooltip, is_dark, high_contrast);
    let _ = SetWindowPos(
        tooltip,
        HWND_TOPMOST,
        x,
        y,
        layout.width,
        layout.height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = InvalidateRect(tooltip, None, false);
    let _ = UpdateWindow(tooltip);
}

unsafe fn refresh_widget_tooltip_if_visible(owner: HWND) {
    if widget_tooltip_hwnd().is_some_and(|tooltip| IsWindowVisible(tooltip).as_bool()) {
        show_widget_tooltip_for_hover(owner);
    }
}

fn compact_metrics() -> Metrics {
    let logical = Metrics::logical();
    Metrics {
        taskbar_h: sc(logical.taskbar_h),
        floating_h: sc(logical.floating_h),
        pill_h: sc(logical.pill_h),
        pill_pad_x: sc(logical.pill_pad_x),
        chip16: sc(logical.chip16),
        chip_gap: sc(logical.chip_gap),
        badge_gap: sc(logical.badge_gap),
        badge_right_pad: sc(logical.badge_right_pad),
        badge_text_gap: sc(logical.badge_text_gap),
        border_w: sc(logical.border_w).max(1),
        status_gap: sc(logical.status_gap),
        status_content_gap: sc(logical.status_content_gap),
        chip20: sc(logical.chip20),
        group_chip_gap: sc(logical.group_chip_gap),
        label_min_w: sc(logical.label_min_w),
        label_max_w: sc(logical.label_max_w),
        label_gap: sc(logical.label_gap),
        separator_w: sc(logical.separator_w),
        row_text_h: sc(logical.row_text_h),
        gauge_min_w: sc(logical.gauge_min_w),
        gauge_h: sc(logical.gauge_h),
        gauge_top_gap: sc(logical.gauge_top_gap),
        unit_gap: sc(logical.unit_gap),
        sep_margin: sc(logical.sep_margin),
        sep_h: sc(logical.sep_h),
        rows_left_pad: sc(logical.rows_left_pad),
        rows_right_pad: sc(logical.rows_right_pad),
    }
}

fn compact_font(key: FontKey) -> HFONT {
    match key {
        FontKey::Data12 => cached_font(sc(12), FW_MEDIUM.0 as i32),
    }
}

fn measure_compact_text(hdc: HDC, font: FontKey, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    unsafe {
        let old_font = SelectObject(hdc, compact_font(font));
        let wide = text.encode_utf16().collect::<Vec<_>>();
        let mut size = SIZE::default();
        let measured = if GetTextExtentPoint32W(hdc, &wide, &mut size).as_bool() {
            size.cx
        } else {
            0
        };
        SelectObject(hdc, old_font);
        measured
    }
}

fn compact_scene(hdc: HDC, vm: &CompactViewModel, high_contrast: bool, floating: bool) -> Scene {
    let metrics = compact_metrics();
    let measure = |font, text: &str| measure_compact_text(hdc, font, text);
    if floating {
        compact_layout::layout_provider_rows(vm, &metrics, high_contrast, &measure)
    } else {
        compact_layout::layout_badges(vm, &metrics, high_contrast, &measure)
    }
}

fn compact_scene_for_hwnd(
    hwnd: HWND,
    vm: &CompactViewModel,
    high_contrast: bool,
    floating: bool,
) -> Scene {
    unsafe {
        let hdc = GetDC(hwnd);
        if hdc.0.is_null() {
            let metrics = compact_metrics();
            let fallback = |font: FontKey, text: &str| {
                let logical_per_char = match font {
                    FontKey::Data12 => 6,
                };
                sc(logical_per_char * text.chars().count() as i32)
            };
            return if floating {
                compact_layout::layout_provider_rows(vm, &metrics, high_contrast, &fallback)
            } else {
                compact_layout::layout_badges(vm, &metrics, high_contrast, &fallback)
            };
        }
        let scene = compact_scene(hdc, vm, high_contrast, floating);
        ReleaseDC(hwnd, hdc);
        scene
    }
}

fn claude_accent_color(high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else {
        Color::from_hex("#D97757")
    }
}

fn codex_accent_color(is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_accent_color(high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else {
        Color::from_hex("#4285F4")
    }
}

fn grok_accent_color(high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else {
        Color::from_hex("#7C6BF5")
    }
}

struct WidgetPalette {
    bg: Color,
}

fn widget_palette(is_dark: bool, high_contrast: bool) -> WidgetPalette {
    if high_contrast {
        WidgetPalette {
            bg: theme::system_color(COLOR_WINDOW),
        }
    } else {
        WidgetPalette {
            bg: if is_dark {
                Color::from_hex("#1C1C1C")
            } else {
                Color::from_hex("#F3F3F3")
            },
        }
    }
}

fn compact_color(key: ColorKey, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        return match key {
            ColorKey::PillBg | ColorKey::GaugeTrack => theme::system_color(COLOR_WINDOW),
            ColorKey::PillBgWarn | ColorKey::GaugeWarn => theme::system_color(COLOR_HIGHLIGHT),
            ColorKey::PillAlertText => theme::system_color(COLOR_HIGHLIGHTTEXT),
            ColorKey::PillAuxText => theme::system_color(COLOR_WINDOWTEXT),
            ColorKey::CanvasWarnPrimary => theme::system_color(COLOR_WINDOWTEXT),
            ColorKey::GaugeAccent(_) => theme::system_color(COLOR_HIGHLIGHT),
            ColorKey::AuxText | ColorKey::Separator => theme::system_color(COLOR_GRAYTEXT),
            ColorKey::PillText
            | ColorKey::NeutralText
            | ColorKey::HighContrastText
            | ColorKey::StaleText
            | ColorKey::ErrorText => theme::system_color(COLOR_WINDOWTEXT),
        };
    }

    match key {
        ColorKey::PillBg => {
            if is_dark {
                Color::from_hex("#2B2B2B")
            } else {
                Color::from_hex("#E3E3E3")
            }
        }
        ColorKey::PillBgWarn => {
            if is_dark {
                Color::from_hex("#422A2E")
            } else {
                Color::from_hex("#FDECEC")
            }
        }
        ColorKey::PillText => {
            if is_dark {
                Color::from_hex("#E3E3E3")
            } else {
                Color::from_hex("#1F1F1F")
            }
        }
        ColorKey::PillAlertText | ColorKey::CanvasWarnPrimary | ColorKey::ErrorText => {
            if is_dark {
                Color::from_hex("#FF747C")
            } else {
                Color::from_hex("#B91C1C")
            }
        }
        ColorKey::AuxText => {
            if is_dark {
                Color::from_hex("#9A9A9A")
            } else {
                Color::from_hex("#6E6E6E")
            }
        }
        ColorKey::PillAuxText => {
            if is_dark {
                Color::from_hex("#9A9A9A")
            } else {
                Color::from_hex("#5F5F5F")
            }
        }
        ColorKey::NeutralText => {
            if is_dark {
                Color::from_hex("#E8E8E8")
            } else {
                Color::from_hex("#1F1F1F")
            }
        }
        ColorKey::StaleText => {
            if is_dark {
                Color::from_hex("#D6A55C")
            } else {
                Color::from_hex("#7A5A16")
            }
        }
        ColorKey::GaugeTrack => {
            if is_dark {
                Color::from_hex("#3A3A3A")
            } else {
                Color::from_hex("#C9C9C9")
            }
        }
        ColorKey::GaugeAccent(kind) => match kind {
            tray_icon::TrayIconKind::Claude => claude_accent_color(false),
            tray_icon::TrayIconKind::Codex => codex_accent_color(is_dark, false),
            tray_icon::TrayIconKind::Antigravity => antigravity_accent_color(false),
            tray_icon::TrayIconKind::Grok => grok_accent_color(false),
        },
        ColorKey::GaugeWarn => {
            if is_dark {
                Color::from_hex("#FF5C66")
            } else {
                Color::from_hex("#DC2626")
            }
        }
        ColorKey::Separator => {
            if is_dark {
                Color::from_hex("#2E2E2E")
            } else {
                Color::from_hex("#DADADA")
            }
        }
        ColorKey::HighContrastText => theme::system_color(COLOR_WINDOWTEXT),
    }
}

fn offset_compact_rect(rect: compact_layout::Rect, x: i32, y: i32) -> RECT {
    RECT {
        left: rect.x + x,
        top: rect.y + y,
        right: rect.x + x + rect.w,
        bottom: rect.y + y + rect.h,
    }
}

fn render_compact_scene(
    hdc: HDC,
    scene: &Scene,
    origin_x: i32,
    origin_y: i32,
    is_dark: bool,
    high_contrast: bool,
) {
    unsafe {
        let _ = SetBkMode(hdc, TRANSPARENT);
        for command in &scene.cmds {
            match command {
                DrawCmd::RoundRect {
                    rect,
                    color,
                    radius,
                } => {
                    let rect = offset_compact_rect(*rect, origin_x, origin_y);
                    if rect.right > rect.left && rect.bottom > rect.top {
                        if *radius <= 0 {
                            fill_rect_color(
                                hdc,
                                &rect,
                                &compact_color(*color, is_dark, high_contrast),
                            );
                        } else {
                            draw_rounded_rect(
                                hdc,
                                &rect,
                                &compact_color(*color, is_dark, high_contrast),
                                *radius,
                            );
                        }
                    }
                }
                DrawCmd::StrokeRoundRect {
                    rect,
                    color,
                    radius,
                    width,
                } => {
                    let rect = offset_compact_rect(*rect, origin_x, origin_y);
                    let brush = CreateSolidBrush(COLORREF(
                        compact_color(*color, is_dark, high_contrast).to_colorref(),
                    ));
                    let region = CreateRoundRectRgn(
                        rect.left,
                        rect.top,
                        rect.right + 1,
                        rect.bottom + 1,
                        radius * 2,
                        radius * 2,
                    );
                    let _ = FrameRgn(hdc, region, brush, (*width).max(1), (*width).max(1));
                    let _ = DeleteObject(region);
                    let _ = DeleteObject(brush);
                }
                DrawCmd::GaugeFill {
                    track,
                    fraction,
                    color,
                    radius,
                } => {
                    let mut rect = offset_compact_rect(*track, origin_x, origin_y);
                    rect.right = rect.left
                        + ((track.w as f64 * fraction.clamp(0.0, 1.0)).round() as i32)
                            .clamp(1, track.w);
                    draw_rounded_rect(
                        hdc,
                        &rect,
                        &compact_color(*color, is_dark, high_contrast),
                        (*radius).min((rect.right - rect.left) / 2),
                    );
                }
                DrawCmd::Text {
                    rect,
                    text,
                    font,
                    color,
                } => {
                    if text.is_empty() || rect.w <= 0 || rect.h <= 0 {
                        continue;
                    }
                    let old_font = SelectObject(hdc, compact_font(*font));
                    let _ = SetTextColor(
                        hdc,
                        COLORREF(compact_color(*color, is_dark, high_contrast).to_colorref()),
                    );
                    let mut rect = offset_compact_rect(*rect, origin_x, origin_y);
                    let mut wide = text.encode_utf16().collect::<Vec<_>>();
                    let _ = DrawTextW(
                        hdc,
                        &mut wide,
                        &mut rect,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old_font);
                }
                DrawCmd::ProviderTile { rect, kind, size } => {
                    if let Some((icon, _)) = provider_tile_icon(
                        *kind,
                        active_window_dpi(),
                        is_dark,
                        high_contrast,
                        *size,
                    ) {
                        let rect = offset_compact_rect(*rect, origin_x, origin_y);
                        let _ = DrawIconEx(
                            hdc,
                            rect.left,
                            rect.top,
                            icon,
                            rect.right - rect.left,
                            rect.bottom - rect.top,
                            0,
                            HBRUSH::default(),
                            DI_NORMAL,
                        );
                    }
                }
            }
        }
    }
}

fn draw_drag_divider(hdc: HDC, height: i32, is_dark: bool, high_contrast: bool) {
    let divider_h = sc(25);
    let divider_top = (height - divider_h) / 2;
    let divider_bottom = divider_top + divider_h;
    let (left, right) = if high_contrast {
        (
            theme::system_color(COLOR_WINDOWTEXT),
            theme::system_color(COLOR_GRAYTEXT),
        )
    } else if is_dark {
        (Color::new(62, 62, 62), Color::new(34, 34, 34))
    } else {
        (Color::new(176, 176, 176), Color::new(226, 226, 226))
    };
    fill_rect_color(
        hdc,
        &RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        },
        &left,
    );
    fill_rect_color(
        hdc,
        &RECT {
            left: sc(3),
            top: divider_top,
            right: sc(4),
            bottom: divider_bottom,
        },
        &right,
    );
}

fn paint_compact_surface(
    hdc: HDC,
    width: i32,
    height: i32,
    scene: &Scene,
    floating: bool,
    is_dark: bool,
    high_contrast: bool,
) {
    let palette = widget_palette(is_dark, high_contrast);
    fill_rect_color(
        hdc,
        &RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        },
        &palette.bg,
    );
    let origin_x = if floating {
        sc(FLOATING_CONTENT_LEFT_MARGIN)
    } else {
        draw_drag_divider(hdc, height, is_dark, high_contrast);
        sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN)
    };
    render_compact_scene(hdc, scene, origin_x, 0, is_dark, high_contrast);
}

/// Register and create the hidden broadcast helper window (see
/// BROADCAST_WINDOW_CLASS_NAME). Never shown; lives for the whole process, so
/// broadcast handling survives widget destruction and revival.
unsafe fn create_broadcast_helper(hinstance: HINSTANCE) -> Option<HWND> {
    let class_name = native_interop::wide_str(BROADCAST_WINDOW_CLASS_NAME);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(broadcast_wnd_proc),
        hInstance: hinstance,
        lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        ..Default::default()
    };
    if RegisterClassExW(&wc) == 0 {
        diagnose::log("broadcast helper: RegisterClassExW failed");
        return None;
    }
    match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR::from_raw(class_name.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        HWND::default(),
        HMENU::default(),
        hinstance,
        None,
    ) {
        Ok(hwnd) => {
            let taskbar_created = native_interop::wide_str("TaskbarCreated");
            let message = RegisterWindowMessageW(PCWSTR::from_raw(taskbar_created.as_ptr()));
            if message != 0 {
                TASKBAR_CREATED_MSG.store(message, Ordering::Release);
            } else {
                diagnose::log("broadcast helper: unable to register TaskbarCreated");
            }
            if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION).is_err() {
                diagnose::log("broadcast helper: WTS session registration failed");
            }
            diagnose::log(format!("broadcast helper created hwnd={:?}", hwnd));
            Some(hwnd)
        }
        Err(error) => {
            diagnose::log_error("broadcast helper: CreateWindowExW failed", error);
            None
        }
    }
}

unsafe fn handle_poll_timer() {
    let action = {
        // Resolve the controller before locking: `poll_controller_hwnd` falls
        // back to the state's own HWND, and STATE is not reentrant.
        let controller_hwnd = poll_controller_hwnd();
        let mut state = lock_state();
        state.as_mut().map(|s| {
            s.next_poll_deadline = None;
            let now = Instant::now();
            let action = poll_timer_action(
                s.auth_error_paused_polling,
                s.auth_recovery_recheck_deadline,
                now,
            );
            if s.auth_error_paused_polling && action == PollTimerAction::PollNow {
                // Consume the deadline before starting the worker so periodic
                // timer ticks cannot queue duplicate service probes.
                s.auth_recovery_recheck_deadline = None;
            }
            if action == PollTimerAction::CheckCredentials {
                let next_interval = paused_poll_timer_interval_ms(
                    s.poll_interval_ms,
                    s.auth_recovery_recheck_deadline,
                    now,
                );
                arm_poll_timer(s, controller_hwnd, next_interval);
            }
            action
        })
    };
    match action {
        Some(PollTimerAction::PollNow) => request_poll(),
        // Before the bounded service recheck is due, the short credential
        // watch can still trigger an earlier re-poll when a source changes.
        Some(PollTimerAction::CheckCredentials) => spawn_credential_watch_check(),
        None => {}
    }
}

/// Arm the main poll timer and record when the next tick is due.
///
/// Goes through `arm_timer` like every other timer: this one drives the whole
/// monitor, so a `SetTimer` failure here freezes usage indefinitely - nothing
/// re-arms `TIMER_POLL` on its own. `next_poll_deadline` stays `None` on
/// failure because there is no tick to count down to.
unsafe fn arm_poll_timer(state: &mut AppState, hwnd: HWND, interval_ms: u32) {
    let interval_ms = interval_ms.max(1);
    let armed = unsafe { arm_timer(hwnd, TIMER_POLL, interval_ms, "poll") };
    state.next_poll_deadline =
        armed.then(|| Instant::now() + Duration::from_millis(interval_ms as u64));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialWatchOutcome {
    /// Store the new snapshot and poll: either a watched credential changed,
    /// or the watched set itself did, and two snapshots taken over different
    /// sets cannot be compared.
    Repoll,
    /// Same set, same signatures: nothing to do.
    Unchanged,
}

/// A changed watched set must not be read as "nothing happened". A provider
/// that stayed in the set can have had its credential refreshed in the same
/// interval, and swallowing that pushes sign-in recovery out to the next
/// scheduled poll. Polling once is the cheap answer; what the caller must not
/// do is leave `auth_watch_mode` stale, which is what made a narrowed watch
/// repeat this on every tick instead of once.
fn credential_watch_outcome(baseline_matches: bool, changed: bool) -> CredentialWatchOutcome {
    if !baseline_matches || changed {
        CredentialWatchOutcome::Repoll
    } else {
        CredentialWatchOutcome::Unchanged
    }
}

/// Re-poll as soon as the credentials on disk change, so re-authenticating is
/// picked up promptly instead of waiting out the poll interval.
///
/// Runs the comparison on a worker thread: building the snapshot can read WSL
/// credentials via `wsl.exe`, which blocks for up to its timeout and must
/// never stall the UI thread.
fn spawn_credential_watch_check() {
    // One check at a time: the timer keeps firing while a slow probe runs.
    if CREDENTIAL_WATCH_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| hold_credential_watch_busy(run_credential_watch_check));
}

/// Clears `CREDENTIAL_WATCH_BUSY` when the guard goes out of scope.
///
/// That flag is what stops the 15-second timer from stacking checks on top of
/// a slow probe, so a panic that unwound past a plain store at the end of the
/// check left it set for the life of the process: the credential watch never
/// ran again, silently, while polling carried on as if nothing had happened.
struct CredentialWatchBusyGuard;

impl Drop for CredentialWatchBusyGuard {
    fn drop(&mut self) {
        CREDENTIAL_WATCH_BUSY.store(false, Ordering::Release);
    }
}

/// Run `check` and release the busy flag however it ends.
///
/// Taking the check as an argument so the release contract can be asserted
/// with a check that panics, on the function the spawned thread calls rather
/// than on the guard alone: dropping the guard from this call site would leave
/// a guard-only test green.
fn hold_credential_watch_busy(check: impl FnOnce()) {
    let _busy = CredentialWatchBusyGuard;
    check();
}

fn run_credential_watch_check() {
    let watch = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| {
            if !s.auth_watch_active {
                return None;
            }
            let scope = credential_read_scope(s, CredentialReadReason::CredentialWatch);
            let Some(mode) = credential_watch_mode_for_shown(
                scope.claude,
                scope.codex,
                scope.antigravity,
                scope.grok,
            ) else {
                diagnose::log(
                    "credential watch stopped: no provider is in scope for a credential read",
                );
                s.auth_watch_active = false;
                s.auth_watch_snapshot.clear();
                return None;
            };
            Some((
                mode,
                s.auth_watch_mode == mode,
                s.auth_watch_snapshot.clone(),
            ))
        })
    };
    if let Some((watch_mode, baseline_matches, previous_snapshot)) = watch {
        let current_snapshot = poller::credential_watch_snapshot(watch_mode);
        let changed = current_snapshot != previous_snapshot;
        if credential_watch_outcome(baseline_matches, changed) == CredentialWatchOutcome::Repoll {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.auth_watch_active {
                        // Mode and snapshot together: storing the snapshot
                        // under a stale mode is what made the next tick
                        // repeat this pass.
                        s.auth_watch_mode = watch_mode;
                        s.auth_watch_snapshot = current_snapshot;
                    }
                }
            }
            request_poll();
        }
    }
}

unsafe fn handle_auth_watch_timer(hwnd: HWND) {
    let active = {
        let state = lock_state();
        state.as_ref().map(|s| s.auth_watch_active).unwrap_or(false)
    };
    if !active {
        let _ = KillTimer(hwnd, TIMER_AUTH_WATCH);
        return;
    }
    spawn_credential_watch_check();
}

unsafe fn handle_reset_poll_timer() {
    let should_poll = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| !s.auth_error_paused_polling)
            .unwrap_or(false)
    };
    if should_poll {
        request_poll();
    }
}

unsafe fn handle_countdown_timer() {
    update_display();
    let main_hwnd = current_main_hwnd();
    if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
        render_layered();
        refresh_native_provider_tooltips(main_hwnd);
    }
    refresh_floating_monitor();
    refresh_detail_popup_if_open();
    schedule_countdown_timer();
}

unsafe fn handle_usage_updated() {
    check_theme_change();
    check_language_change();

    let main_hwnd = current_main_hwnd();
    if main_hwnd != HWND::default() && IsWindow(main_hwnd).as_bool() {
        render_layered();
        position_at_taskbar();
        suppress_tray_reposition_for(Duration::from_millis(
            TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS,
        ));
        sync_tray_icons(main_hwnd);
    }
    schedule_countdown_timer();
    refresh_floating_monitor();
    refresh_detail_popup_if_open();
}

fn reconcile_surface_topology() -> SurfaceTopologyReset {
    let active_monitors = active_monitor_identities();
    if active_monitors.is_empty() {
        return SurfaceTopologyReset::default();
    }
    let primary_monitor = active_monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .cloned();
    let taskbars = native_interop::find_taskbars();
    let taskbar_monitors = taskbars
        .iter()
        .map(|taskbar| unsafe { monitor_identity_for_taskbar(taskbar) })
        .collect::<Vec<_>>();
    let primary_taskbar_index = taskbar_monitors
        .iter()
        .position(|monitor| monitor.as_ref().is_some_and(|monitor| monitor.is_primary))
        .unwrap_or(0);

    let mut reset = SurfaceTopologyReset::default();
    let mut state = lock_state();
    let Some(s) = state.as_mut() else {
        return reset;
    };

    if !taskbars.is_empty() {
        let desired_index = if s.widget_placement_needs_migration {
            s.preferred_taskbar_index
                .min(taskbars.len().saturating_sub(1))
        } else {
            match &s.widget_placement {
                WidgetPlacement::PrimaryLeft | WidgetPlacement::PrimaryRight => {
                    primary_taskbar_index
                }
                WidgetPlacement::Custom { monitor, .. } => taskbar_monitors
                    .iter()
                    .position(|identity| {
                        identity
                            .as_ref()
                            .is_some_and(|identity| identity.matches_key(monitor))
                    })
                    .unwrap_or(primary_taskbar_index),
            }
        };
        let desired_monitor = taskbar_monitors.get(desired_index).and_then(Clone::clone);
        let already_bound = s.taskbar_index == desired_index
            && s.taskbar_monitor
                .as_ref()
                .zip(desired_monitor.as_ref())
                .is_some_and(|(current, desired)| current.matches(desired));
        if !already_bound {
            s.taskbar_index = desired_index;
            if !s.widget_placement_needs_migration {
                s.preferred_taskbar_index = desired_index;
            }
            s.taskbar_monitor = desired_monitor;
            reset.taskbar = true;
        }
    }
    if secondary_monitor_disappeared(s.details_monitor.as_ref(), &active_monitors) {
        s.details_monitor = primary_monitor;
        reset.details = true;
    }

    reset
}

unsafe fn recover_shell_surfaces(reason: &str) {
    diagnose::log(format!("shell recovery requested: {reason}"));
    native_interop::clear_monitor_device_path_cache();
    check_theme_change();
    check_language_change();
    let topology_reset = reconcile_surface_topology();
    if topology_reset.taskbar || topology_reset.details {
        diagnose::log(format!(
            "surface topology reconciled; reset taskbar={} details={}",
            topology_reset.taskbar, topology_reset.details
        ));
    }

    let (main_hwnd, stored_taskbar, widget_visible) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd.to_hwnd(),
                s.taskbar_hwnd.map(|taskbar| taskbar.0 as isize),
                s.widget_visible,
            ),
            None => return,
        }
    };
    let binding_ok = !topology_reset.taskbar
        && stored_taskbar.is_some_and(|taskbar| {
            native_interop::is_embedded_in_taskbar(main_hwnd, HWND(taskbar as *mut _))
        });

    if binding_ok {
        position_at_taskbar();
        render_layered();
        sync_tray_icons(main_hwnd);
        if widget_visible {
            let _ = ShowWindow(main_hwnd, SW_SHOWNOACTIVATE);
        }
    } else {
        if IsWindow(main_hwnd).as_bool() {
            let _ = ShowWindow(main_hwnd, SW_HIDE);
        }
        revive_request();
    }
    refresh_floating_monitor();
    if topology_reset.details {
        reset_detail_popup_to_primary_default();
    } else {
        refresh_detail_popup_if_open();
    }
}

unsafe fn handle_session_change(code: usize) {
    match code {
        WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOCK => {
            SESSION_INACTIVE.store(true, Ordering::Release);
            diagnose::log(format!(
                "session change {code}: shell re-embedding deferred; provider polling continues"
            ));
        }
        WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => {
            SESSION_INACTIVE.store(false, Ordering::Release);
            recover_shell_surfaces("remote/console session restored");
        }
        WTS_SESSION_UNLOCK => {
            SESSION_INACTIVE.store(false, Ordering::Release);
            recover_shell_surfaces("session unlocked");
        }
        _ => {}
    }
}

unsafe extern "system" fn broadcast_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        broadcast_wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            diagnose::log(format!(
                "panic in broadcast_wnd_proc msg={msg:#06x} (recovered)"
            ));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe fn broadcast_wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED_MSG.load(Ordering::Acquire);
    match msg {
        // Setting/display broadcasts arrive in bursts (an RDP transition
        // fires dozens of WM_SETTINGCHANGE in a row). Trailing-edge debounce:
        // each message re-arms the timer, so the refresh work runs once,
        // shortly after the burst ends, against the final state - a leading
        // -edge throttle would act on an intermediate state and drop the
        // last message.
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED_MSG => {
            arm_timer(
                hwnd,
                TIMER_BROADCAST_DEBOUNCE,
                BROADCAST_DEBOUNCE_MS,
                "broadcast debounce",
            );
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_BROADCAST_DEBOUNCE => {
            let _ = KillTimer(hwnd, TIMER_BROADCAST_DEBOUNCE);
            recover_shell_surfaces("display/settings change");
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_POLL => {
            handle_poll_timer();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_AUTH_WATCH => {
            handle_auth_watch_timer(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_RESET_POLL => {
            handle_reset_poll_timer();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_COUNTDOWN => {
            handle_countdown_timer();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TIMER_PROVIDER_DETECT => {
            handle_provider_detect_timer();
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE_MSG => {
            handle_session_change(wparam.0);
            LRESULT(0)
        }
        _ if taskbar_created != 0 && msg == taskbar_created => {
            recover_shell_surfaces("TaskbarCreated");
            LRESULT(0)
        }
        _ if msg == WM_APP_USAGE_UPDATED => {
            handle_usage_updated();
            LRESULT(0)
        }
        _ if msg == WM_APP_PERSISTENCE_WARNING => {
            show_pending_persistence_warning_once();
            LRESULT(0)
        }
        // Revival ready signal, routed here instead of a thread message so a
        // modal message loop cannot discard it (see post_revive_ready).
        _ if msg == WM_APP_REVIVE_READY => {
            revive_execute();
            LRESULT(0)
        }
        _ if msg == WM_APP_REQUEST_QUIT => {
            let main_hwnd = {
                let state = lock_state();
                state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
            };
            request_quit(main_hwnd);
            LRESULT(0)
        }
        // A second launched instance asks us to surface the detail popup
        // (posted from run()'s single-instance guard).
        _ if msg == WM_APP_TRAY => {
            if lparam.0 as u32 == WM_LBUTTONUP {
                diagnose::log("broadcast helper: show-details request from second instance");
                show_usage_details(hwnd, None);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let _ = WTSUnRegisterSessionNotification(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(crate) struct InstanceGuard {
    handle: HANDLE,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

pub(crate) fn acquire_single_instance() -> Option<InstanceGuard> {
    let is_relaunch = std::env::var(ENV_RELAUNCH).is_ok();
    let handle = acquire_named_mutex(&mutex_name(), is_relaunch)?;
    Some(InstanceGuard { handle })
}

fn mutex_name() -> String {
    #[cfg(debug_assertions)]
    if let Some(suffix) = std::env::var_os("GENGCHOU_TEST_MUTEX_SUFFIX") {
        if !suffix.is_empty() {
            let suffix = suffix.to_string_lossy();
            return format!("{CURRENT_MUTEX_NAME}-{suffix}");
        }
    }
    CURRENT_MUTEX_NAME.to_string()
}

fn acquire_named_mutex(name: &str, is_relaunch: bool) -> Option<HANDLE> {
    let mutex_name = native_interop::wide_str(name);
    unsafe {
        let handle = match CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr())) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return None;
            }
        };
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return Some(handle);
        }
        if !is_relaunch {
            notify_existing_instance();
            diagnose::log("startup aborted: another instance is already running");
            let _ = CloseHandle(handle);
            return None;
        }

        diagnose::log(format!(
            "relaunch: waiting for previous instance mutex {name}"
        ));
        for attempt in 1..=3 {
            let wait_result = WaitForSingleObject(handle, 10_000);
            if wait_result == WAIT_OBJECT_0 || wait_result == WAIT_ABANDONED {
                return Some(handle);
            }
            diagnose::log(format!(
                "relaunch: previous instance still owns {name} after wait {attempt}/3 ({wait_result:?})"
            ));
        }
        let _ = CloseHandle(handle);
        diagnose::log("startup aborted: previous instance never released the mutex");
        None
    }
}

fn notify_existing_instance() {
    unsafe {
        let helper_class = native_interop::wide_str(BROADCAST_WINDOW_CLASS_NAME);
        if let Ok(existing) = FindWindowW(PCWSTR::from_raw(helper_class.as_ptr()), PCWSTR::null()) {
            if existing != HWND::default() {
                let _ = PostMessageW(
                    existing,
                    WM_APP_TRAY,
                    WPARAM(0),
                    LPARAM(WM_LBUTTONUP as isize),
                );
                diagnose::log("asked existing Gengchou instance to show details");
            }
        }
    }
}

pub fn run(_instance_guard: InstanceGuard, startup_notice: Option<String>) {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        set_default_dpi(GetDpiForSystem());
    }
    diagnose::log("window::run started");

    UI_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let class_name = native_interop::wide_str(WINDOW_CLASS_NAME);

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("startup aborted: GetModuleHandleW failed", error);
                return;
            }
        };
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = settings::load();
        DETAIL_PINNED.store(settings.detail_pinned, Ordering::SeqCst);
        DETAIL_MOVEMENT_UNLOCKED.store(DETAIL_DEFAULT_MOVEMENT_UNLOCKED, Ordering::SeqCst);
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count = active_model_count(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
            settings.show_grok,
        );
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(initial_model_count),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("startup aborted: CreateWindowExW failed", error);
                return;
            }
        };

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let is_high_contrast = theme::is_high_contrast();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                is_high_contrast,
                embedded: false,
                language_override,
                language,
                install_channel,
                claude_widget: placeholder_widget(),
                codex_widget: placeholder_widget(),
                antigravity_widget: placeholder_widget(),
                grok_widget: placeholder_widget(),
                compact_vm: compact_view::placeholder_model(
                    "--",
                    &settings.provider_order,
                    settings.show_claude_code,
                    settings.show_codex,
                    settings.show_antigravity,
                    settings.show_grok,
                ),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
                show_grok: settings.show_grok,
                allow_claude_credentials: settings.allow_claude_credentials,
                allow_codex_credentials: settings.allow_codex_credentials,
                allow_antigravity_credentials: settings.allow_antigravity_credentials,
                allow_grok_credentials: settings.allow_grok_credentials,
                credential_consent_granted: settings.credential_consent_granted,
                credential_consent_decided: settings.credential_consent_decided,
                claude_credential_access_decided: settings.claude_credential_access_decided,
                codex_credential_access_decided: settings.codex_credential_access_decided,
                antigravity_credential_access_decided: settings
                    .antigravity_credential_access_decided,
                grok_credential_access_decided: settings.grok_credential_access_decided,
                claude_credential_access_revoked: settings.claude_credential_access_revoked,
                codex_credential_access_revoked: settings.codex_credential_access_revoked,
                antigravity_credential_access_revoked: settings
                    .antigravity_credential_access_revoked,
                grok_credential_access_revoked: settings.grok_credential_access_revoked,
                claude_credential_access_pending: settings.claude_credential_access_pending,
                codex_credential_access_pending: settings.codex_credential_access_pending,
                antigravity_credential_access_pending: settings
                    .antigravity_credential_access_pending,
                grok_credential_access_pending: settings.grok_credential_access_pending,
                provider_order: settings.provider_order.clone(),
                pending_provider_order: None,
                pending_provider_order_samples: 0,
                last_observed_tray_order: None,
                data: None,
                data_is_cached: false,
                manual_refresh_in_progress: false,
                last_error: None,
                provider_refresh_states: ProviderRefreshStates::default(),
                poll_interval_ms: settings.poll_interval_ms,
                next_poll_deadline: None,
                retry_count: 0,
                auth_error_paused_polling: false,
                auth_recovery_recheck_deadline: None,
                auth_watch_active: false,
                auth_watch_mode: poller::CredentialWatchMode::ClaudeSources,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                last_success_unix: None,
                notify_session_reset: settings.notify_session_reset,
                notify_weekly_reset: settings.notify_weekly_reset,
                update_status: remembered_update_status(settings.last_update_outcome.as_ref()),
                last_update_outcome: settings.last_update_outcome.clone(),
                last_update_check_unix: settings.last_update_check_unix,
                details_hwnd: None,
                details_monitor: None,
                floating_hwnd: None,
                floating_monitor: None,
                floating_visible: settings.floating_visible,
                detailed_tray_icons: settings.detailed_tray_icons,
                detail_pinned: settings.detail_pinned,
                floating_x: settings.floating_x,
                floating_y: settings.floating_y,
                floating_default_position: settings.floating_default_position,
                floating_placement: settings.floating_placement.clone().unwrap_or(
                    match settings.floating_default_position {
                        FloatingDefaultPosition::PrimaryBottomLeft => {
                            FloatingPlacement::PrimaryBottomLeft
                        }
                        FloatingDefaultPosition::PrimaryBottomRight => {
                            FloatingPlacement::PrimaryBottomRight
                        }
                    },
                ),
                floating_placement_needs_migration: settings.floating_placement.is_none(),
                widget_tooltip_hwnd: None,
                taskbar_index: settings.taskbar_index,
                taskbar_monitor: None,
                tray_offset: settings.tray_offset,
                preferred_taskbar_index: settings.taskbar_index,
                widget_default_position: settings.widget_default_position,
                widget_placement: settings.widget_placement.clone().unwrap_or(
                    match settings.widget_default_position {
                        WidgetDefaultPosition::PrimaryTaskbarLeft => WidgetPlacement::PrimaryLeft,
                        WidgetDefaultPosition::PrimaryTaskbarRight => WidgetPlacement::PrimaryRight,
                    },
                ),
                widget_placement_needs_migration: settings.widget_placement.is_none(),
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_client_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
            });
        }

        // Broadcast helper: receives the top-level-only broadcast messages,
        // second-instance activation requests, and revival ready signals for
        // the process lifetime.
        if let Some(helper) = create_broadcast_helper(HINSTANCE(hinstance.0)) {
            BROADCAST_HELPER_HWND.store(helper.0 as isize, Ordering::SeqCst);
        } else {
            // Degraded fallback only: polling and WTS handling still work
            // while the main widget HWND survives.
            let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        }

        // Show the previous run's usage numbers immediately (marked as cached
        // in the detail popup) instead of "--" until the first poll lands.
        if let Some((cached_data, saved_unix)) = load_usage_cache() {
            let filtered_cache = {
                let mut state = lock_state();
                state.as_mut().and_then(|s| {
                    s.data = Some(cached_data);
                    // A cached value for a provider whose access is not
                    // granted must not come back from disk. Checks the same
                    // predicate the poll gate uses, so a revoked provider
                    // cannot be restored by the one-time consent alone.
                    for kind in tray_icon::TrayIconKind::ALL {
                        if !provider_has_credential_access(s, kind) {
                            clear_provider_usage(s, kind);
                        }
                    }
                    s.data_is_cached = true;
                    s.last_poll_ok = true;
                    s.last_success_unix = Some(saved_unix);
                    refresh_usage_texts(s);
                    capture_usage_cache_snapshot(s)
                })
            };
            if let Some(snapshot) = filtered_cache.as_ref() {
                save_usage_cache(snapshot);
            }
            diagnose::log("loaded usage snapshot from previous run");
        }

        // Resolve semantic presets against the current primary display and
        // custom placements against their monitor identity. The persisted
        // taskbar ordinal is only a legacy fallback during migration.
        let initial_taskbar_index = {
            let state = lock_state();
            state
                .as_ref()
                .map(|state| {
                    if state.widget_placement_needs_migration {
                        state.preferred_taskbar_index
                    } else {
                        taskbar_index_for_placement(
                            &state.widget_placement,
                            state.preferred_taskbar_index,
                        )
                    }
                })
                .unwrap_or(settings.taskbar_index)
        };
        if attach_to_taskbar(hwnd, initial_taskbar_index) {
            embedded = true;
        }

        // The taskbar widget is not a fallback desktop popup. If Explorer is
        // not ready yet, keep it hidden until verified re-embedding succeeds.
        if !embedded {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        // Registering our icons resizes the notification area asynchronously;
        // wait for its rect to settle so the first visible position is final
        // instead of being corrected (a visible jump) moments after showing.
        wait_for_tray_geometry_stable(Duration::from_secs(3));
        migrate_legacy_placements_if_needed();
        refresh_provider_order_from_tray(hwnd);
        arm_timer(
            hwnd,
            TIMER_TRAY_ORDER,
            TRAY_ORDER_SAMPLE_MS,
            "tray order sample",
        );

        // Position and render first, show last: the widget appears in its
        // final place with real content instead of flashing into view first.
        position_at_taskbar();
        render_layered();
        if settings.floating_visible {
            refresh_floating_monitor();
        }
        if settings.widget_visible && embedded {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log(if embedded {
            "taskbar widget ready"
        } else {
            "taskbar widget hidden pending shell recovery"
        });

        // Confirm the update transaction here: this is exactly the milestone
        // `confirm_update_ready` documents - the single-instance mutex is
        // held, the window and tray icons exist, and the first render is
        // done - and it is the last point before startup can block on the
        // user. The helper gives a new build 30 seconds to report ready and
        // otherwise rolls back to the previous binary, while everything below
        // can put a modal on screen and wait forever: the first-run consent
        // prompt, and the persistence warning that fires whenever the last
        // save failed. An update that landed on either of those was killed and
        // rolled back while the user was still reading the dialog.
        if let Err(error) = updater::confirm_update_ready() {
            diagnose::log(format!(
                "unable to confirm successful update startup; startup stopped: {error}"
            ));
            let strings = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.language.strings())
                    .unwrap_or(LanguageId::English.strings())
            };
            let message = format!("{}\n\n{error}", strings.update_failed);
            show_error_message(strings.updates, &message);
            return;
        }

        // Show the one-time access prompt only after the compact surface and
        // tray icons exist, so an owner with WS_EX_NOACTIVATE cannot strand
        // the modal before any app UI is visible. This remains before
        // credential watches, provider timers, or the initial poll, so no
        // credential-backed operation can run before the user decides.
        // Existing installs are migrated in `settings::normalize` and never
        // reach the prompt.
        // Read before the prompt: an install that has already answered it is
        // a migrated one, and migrated installs are exactly those that may
        // still owe a balloon for a provider added by an update.
        let migrated_install = lock_state()
            .as_ref()
            .is_some_and(|s| s.credential_consent_decided);
        prompt_for_initial_consent(hwnd);
        schedule_provider_detection(migrated_install);

        schedule_countdown_timer();
        show_pending_persistence_warning_once();

        // Provider polling belongs to the process-level helper so it survives
        // taskbar/RDP destruction of the embedded widget HWND.
        {
            let controller_hwnd = poll_controller_hwnd();
            let mut state = lock_state();
            if let Some(state) = state.as_mut() {
                let initial_poll_ms = state.poll_interval_ms;
                arm_poll_timer(state, controller_hwnd, initial_poll_ms);
            } else {
                arm_timer(controller_hwnd, TIMER_POLL, POLL_5_MIN, "poll");
            }
        }

        // Watch for explorer.exe restarts so we can re-embed and re-add the tray
        // icon (the shell discards tray registrations when it restarts). This
        // runs on a dedicated thread, NOT a window timer: once explorer destroys
        // the taskbar, our embedded child window stops receiving all messages
        // (WM_TIMER included), so a timer would never fire again.
        spawn_taskbar_watchdog();

        // Initial poll
        diagnose::log("initial poll requested");
        request_poll();
        if !embedded {
            revive_request();
        }

        // Initial theme check
        check_theme_change();

        if let Some(error) = startup_notice {
            let strings = {
                let state = lock_state();
                state
                    .as_ref()
                    .map(|s| s.language.strings())
                    .unwrap_or(LanguageId::English.strings())
            };
            let message = format!("{}\n\n{error}", strings.update_failed);
            show_error_message(strings.updates, &message);
        }

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            // Thread messages (no window): revive after external destruction.
            // They cannot go through wnd_proc because the window is gone.
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REVIVE {
                revive_request();
                continue;
            }
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REQUEST_QUIT {
                let main_hwnd = {
                    let state = lock_state();
                    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
                };
                request_quit(main_hwnd);
                continue;
            }
            if msg.hwnd == HWND::default() && msg.message == WM_APP_REVIVE_READY {
                revive_execute();
                continue;
            }
            let detail_hwnd = {
                let state = lock_state();
                state.as_ref().and_then(|state| state.details_hwnd)
            };
            if let Some(detail) = detail_hwnd.filter(|detail| IsWindow(*detail).as_bool()) {
                if handle_detail_keyboard_input(detail, &msg) {
                    continue;
                }
                if IsDialogMessageW(detail, &msg).as_bool() {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        diagnose::log("message loop exited");
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    let (hwnd_val, is_dark, high_contrast, embedded, compact_vm) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.is_high_contrast,
                s.embedded,
                s.compact_vm.clone(),
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();
    let _dpi_scope = DpiScope::for_window(hwnd);
    let tooltip_scene = compact_scene_for_hwnd(hwnd, &compact_vm, high_contrast, false);
    sync_widget_tooltip_hits(&tooltip_scene);
    unsafe {
        refresh_widget_tooltip_if_visible(hwnd);
    }

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(Metrics::logical().taskbar_h);

    let palette = widget_palette(is_dark, high_contrast);

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;
        let scene = compact_scene(mem_dc, &compact_vm, high_contrast, false);

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_compact_surface(mem_dc, width, height, &scene, false, is_dark, high_contrast);

        // Background pixels -> alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels -> fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = palette.bg.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

fn live_broadcast_helper_hwnd() -> Option<HWND> {
    let helper = BROADCAST_HELPER_HWND.load(Ordering::Acquire);
    if helper != 0 {
        let hwnd = HWND(helper as *mut _);
        if unsafe { IsWindow(hwnd).as_bool() } {
            return Some(hwnd);
        }
    }

    None
}

/// The window that owns the poll timer: the process-level helper, or the main
/// window when that helper could not be created.
///
/// Takes the state lock on the fallback path, so callers must resolve it
/// *before* they lock; STATE is a plain `Mutex` and would deadlock.
fn poll_controller_hwnd() -> HWND {
    if let Some(hwnd) = live_broadcast_helper_hwnd() {
        return hwnd;
    }

    let state = lock_state();
    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
}

fn current_main_hwnd() -> HWND {
    let state = lock_state();
    state.as_ref().map(|s| s.hwnd.to_hwnd()).unwrap_or_default()
}

fn post_usage_updated() {
    let hwnd = poll_controller_hwnd();
    if hwnd != HWND::default() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
        }
    }
}

/// Report that Gengchou updated the Claude Code CLI in the background.
///
/// Not optional. This is not a preference like the quota-reset notifications
/// next to it used to suggest - it discloses that the app changed something on
/// the user's machine, and an app that can silently upgrade your CLI is worse
/// than one that occasionally tells you it did. The opt-out that matters is
/// `DISABLE_UPDATES=1`, which stops the update itself.
fn notify_claude_cli_update_if_needed(hwnd: HWND) {
    let Some(update) = poller::take_claude_cli_update_notification() else {
        return;
    };
    let strings = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        state.language.strings()
    };
    let before = update.before_version.as_deref().unwrap_or("?");
    let after = update.after_version.as_deref().unwrap_or("?");
    let body = strings
        .claude_cli_updated_body
        .replace("{before}", before)
        .replace("{after}", after);
    tray_icon::notify_balloon(
        hwnd,
        tray_icon::TrayIconKind::Claude,
        tray_icon::BalloonTone::Info,
        strings.claude_cli_updated_title,
        &body,
    );
}

fn request_poll() {
    request_poll_with(false);
}

fn request_poll_with(force_claude_refresh: bool) {
    if QUIT_REQUESTED.load(Ordering::Acquire) {
        return;
    }

    // Synchronize the generation bump with `do_poll` applying a result under
    // the same state lock. Once a worker verifies its generation while holding
    // this lock, no newer request can make that result stale mid-commit.
    let (should_start_worker, has_allowed_provider) = {
        let state = lock_state();
        let has_allowed_provider = state
            .as_ref()
            .map(state_credential_poll_selection)
            .is_some_and(poll_selection_has_target);
        if has_allowed_provider {
            (
                POLL_COORDINATOR.request(force_claude_refresh),
                has_allowed_provider,
            )
        } else {
            POLL_COORDINATOR.invalidate_pending();
            (false, has_allowed_provider)
        }
    };
    if !has_allowed_provider {
        diagnose::log("poll skipped; no shown provider has credential access");
        return;
    }
    if should_start_worker {
        std::thread::spawn(poll_worker);
    }
}

fn poll_worker() {
    run_poll_passes(&POLL_COORDINATOR, do_poll);
}

/// Run coalesced passes until no follow-up is owed.
///
/// Separate from `poll_worker`, and taking the coordinator and the pass as
/// arguments, so the panic contract in `PollPassGuard` can be asserted on the
/// loop the running app enters rather than on a copy of it.
fn run_poll_passes(coordinator: &PollCoordinator, mut pass: impl FnMut(u64, bool)) {
    loop {
        let (generation, force_claude_refresh) = coordinator.begin_pass();
        let guard = PollPassGuard::arm(coordinator);
        pass(generation, force_claude_refresh);
        guard.finished();
        if !coordinator.finish_pass() {
            break;
        }
        diagnose::log("poll request coalesced; starting pending refresh");
    }
}

fn do_poll(generation: u64, force_claude_refresh: bool) {
    let controller_hwnd = poll_controller_hwnd();
    let main_hwnd = current_main_hwnd();
    let plan_now = Instant::now();
    let plan = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| PollPassPlan::from_state(s, plan_now))
            .unwrap_or(PollPassPlan {
                show_claude_code: false,
                show_codex: false,
                show_antigravity: false,
                show_grok: false,
                poll_claude_code: false,
                poll_codex: false,
                poll_antigravity: false,
                poll_grok: false,
                claude_cooldown_ms: None,
                codex_cooldown_ms: None,
                antigravity_cooldown_ms: None,
                grok_cooldown_ms: None,
            })
    };
    let show_claude_code = plan.show_claude_code;
    let show_grok = plan.show_grok;
    let show_codex = plan.show_codex;
    let show_antigravity = plan.show_antigravity;

    if !plan.has_poll_target() {
        let mut state = lock_state();
        if !POLL_COORDINATOR.is_current(generation) {
            return;
        }
        if let Some(s) = state.as_mut() {
            s.manual_refresh_in_progress = false;
            if s.last_poll_ok {
                refresh_usage_texts(s);
            }
            let interval =
                poll_delay_with_provider_cooldowns(s, s.poll_interval_ms, Instant::now());
            unsafe {
                arm_poll_timer(s, controller_hwnd, interval);
            }
        }
        drop(state);
        post_usage_updated();
        return;
    }

    // Sample the credentials the poll is about to use. A refresh landing
    // while the poll is in flight would otherwise be invisible: the result
    // reports a credential issue but a post-poll baseline already carries the
    // refreshed signature, so the watch would compare it against itself and
    // never fire.
    let watch_mode =
        credential_watch_mode_for_shown(show_claude_code, show_codex, show_antigravity, show_grok);
    let pre_poll_snapshot = watch_mode.map(poller::credential_watch_snapshot);

    let poll_result = match poller::poll(
        plan.poll_claude_code,
        plan.poll_codex,
        plan.poll_antigravity,
        plan.poll_grok,
        force_claude_refresh,
    ) {
        Ok(mut data) => {
            plan.apply_skipped_rate_limits(&mut data);
            Ok(data)
        }
        Err(mut failure) => {
            plan.apply_skipped_rate_limits(failure.data.as_mut());
            Err(failure)
        }
    };
    notify_claude_cli_update_if_needed(main_hwnd);

    match poll_result {
        Ok(mut data) => {
            let updated_unix = now_unix_secs();
            stamp_provider_updates(&mut data, updated_unix);

            // Resolve the watch before taking the lock: building a snapshot
            // can shell out to WSL, which must not happen while the state is
            // held (nor on the UI thread - this runs on the poll worker).
            let needs_auth = shown_provider_needs_auth(&data, plan.shown_flags());
            let post_poll_snapshot = watch_mode
                .filter(|_| needs_auth)
                .map(poller::credential_watch_snapshot);
            let watch_decision = auth_watch_decision(
                needs_auth,
                pre_poll_snapshot.as_ref(),
                post_poll_snapshot.as_ref(),
            );

            let mut auth_transition_kind = None;
            // Collected under the lock, sent after it. `Shell_NotifyIconW`
            // reaches Explorer synchronously, and Explorer can call straight
            // back into this app's window procedure, which takes `STATE`.
            // Sending from inside this section - on the poll worker, holding
            // that lock - put the UI thread and this worker on opposite sides
            // of the same wait. The other three balloon sites in this file
            // already release first; this was the one that did not.
            let mut reset_notifications = Vec::new();
            let mut state = lock_state();
            if !POLL_COORDINATOR.is_current(generation) {
                diagnose::log(format!(
                    "discarded stale poll result generation={generation} current={}",
                    POLL_COORDINATOR.generation.load(Ordering::Acquire)
                ));
                return;
            }
            if let Some(s) = state.as_mut() {
                let claude_poll_healthy =
                    plan.poll_claude_code && data.error(tray_icon::TrayIconKind::Claude).is_none();
                let refresh_now = Instant::now();
                let auth_transitions =
                    update_provider_refresh_states(s, plan, &data, updated_unix, refresh_now);
                auth_transition_kind = shown_provider_order(
                    &s.provider_order,
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                )
                .into_iter()
                .find(|kind| auth_transitions.contains(kind));
                let merged = merge_missing_provider_data(
                    s.data.as_ref(),
                    data,
                    credential_read_scope(s, CredentialReadReason::Poll).flags(),
                );
                // A cached previous snapshot is from an earlier run: any
                // reset that elapsed while the app was closed is old news,
                // not an event worth a balloon (pre-cache behavior: the
                // first poll of a run never notified).
                reset_notifications = if s.data_is_cached {
                    Vec::new()
                } else {
                    collect_reset_notifications(
                        s.data.as_ref(),
                        &merged,
                        s.notify_session_reset,
                        s.notify_weekly_reset,
                        s.language.strings(),
                    )
                };

                // Mirror of the arming condition in schedule_countdown_timer:
                // the 5s reset fast poll must stop not only when every window
                // refreshed, but also when the only past-reset windows belong
                // to a failing provider - merge carries its stale section for
                // the whole outage, so app_is_past_reset alone never clears.
                if !healthy_provider_past_reset(&merged) {
                    unsafe {
                        let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(merged);
                s.data_is_cached = false;
                s.manual_refresh_in_progress = false;
                s.last_error = None;
                s.last_poll_ok = true;
                s.last_success_unix = Some(updated_unix);
                refresh_usage_texts(s);

                s.retry_count = 0;
                // Re-arm from completion instead of keeping the old periodic
                // phase, pulled forward to either the Claude cache deadline or
                // the nearest provider-specific 429 cooldown. A limited
                // provider never delays healthy providers.
                let base_interval = if s.show_claude_code && claude_poll_healthy {
                    poller::claude_aligned_poll_delay_ms(s.poll_interval_ms)
                        .unwrap_or(s.poll_interval_ms)
                } else {
                    s.poll_interval_ms
                };
                let interval = poll_delay_with_provider_cooldowns(s, base_interval, Instant::now());
                if interval < s.poll_interval_ms {
                    diagnose::log(format!(
                        "poll timer aligned to the next provider deadline in {}s",
                        interval / 1000
                    ));
                }
                unsafe {
                    arm_poll_timer(s, controller_hwnd, interval);
                }
                s.auth_error_paused_polling = false;
                s.auth_recovery_recheck_deadline = None;

                // The poll succeeded overall, but a provider can still be
                // carrying a credential issue while the others report fine.
                // Watch its credentials so a re-login is picked up in seconds
                // instead of at the next poll interval.
                match watch_decision {
                    AuthWatchDecision::Stop => {
                        s.auth_watch_active = false;
                        s.auth_watch_mode = poller::CredentialWatchMode::ClaudeSources;
                        s.auth_watch_snapshot.clear();
                        unsafe {
                            let _ = KillTimer(controller_hwnd, TIMER_AUTH_WATCH);
                        }
                    }
                    AuthWatchDecision::Watch | AuthWatchDecision::WatchAndPollNow => {
                        if let (Some(mode), Some(snapshot)) = (watch_mode, post_poll_snapshot) {
                            s.auth_watch_active = true;
                            s.auth_watch_mode = mode;
                            s.auth_watch_snapshot = snapshot;
                            unsafe {
                                arm_timer(
                                    controller_hwnd,
                                    TIMER_AUTH_WATCH,
                                    AUTH_WATCH_INTERVAL_MS,
                                    "auth watch",
                                );
                            }
                        }
                    }
                }
            }

            // Persist the snapshot outside the lock so the next start can
            // show these numbers immediately.
            let cache_snapshot = state.as_ref().and_then(capture_usage_cache_snapshot);
            drop(state);
            for notification in reset_notifications {
                diagnose::log(format!("reset notification shown: {}", notification.body));
                tray_icon::notify_balloon(
                    main_hwnd,
                    notification.kind,
                    tray_icon::BalloonTone::Info,
                    &notification.title,
                    &notification.body,
                );
            }
            if let Some(snapshot) = cache_snapshot.as_ref() {
                save_usage_cache(snapshot);
            }

            if let Some(kind) = auth_transition_kind {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        let strings = s.language.strings();
                        // `update_provider_refresh_state` only reports a
                        // transition for `AuthenticationFailed`; a provider
                        // that was never signed in deliberately raises no
                        // balloon, so this status is exact rather than a
                        // fallback. Keep both in step if that ever changes.
                        let (title, body) = credential_notification_text(
                            kind,
                            ProviderStatus::AuthenticationFailed,
                            strings,
                        );
                        (title, body)
                    })
                };
                if let Some((title, body)) = balloon {
                    tray_icon::notify_balloon(
                        main_hwnd,
                        kind,
                        tray_icon::BalloonTone::ActionRequired,
                        &title,
                        &body,
                    );
                }
            }

            // Outside the lock: request_poll takes it. This result was decided
            // against credentials that changed mid-poll, so re-poll rather
            // than leave a "sign in" marker the watch can no longer clear.
            if watch_decision == AuthWatchDecision::WatchAndPollNow {
                diagnose::log("credentials changed while polling; polling again");
                request_poll();
            }

            post_usage_updated();
        }
        Err(failure) => {
            let e = failure.error;
            let failed_data = *failure.data;
            let has_transient_failure = tray_icon::TrayIconKind::ALL.into_iter().any(|kind| {
                plan.polled(kind)
                    && matches!(
                        failed_data.error(kind),
                        Some(ProviderStatus::NetworkUnavailable | ProviderStatus::RequestFailed)
                    )
            });
            // The aggregate error is only the first credential failure when
            // every provider needs user action. Inspect the exact provider
            // errors too, otherwise a remote 401 can be hidden by another
            // provider's local "not signed in" result and never be rechecked.
            let needs_auth_rejection_recheck =
                poll_failure_needs_auth_rejection_recheck(e, &failed_data);
            // Same race as the success path: sampling only now would arm the
            // baseline with credentials that were refreshed while the poll was
            // in flight. Compare against the pre-poll sample instead.
            let needs_watch = shown_provider_needs_auth(&failed_data, plan.shown_flags());
            let pause_for_auth = all_shown_providers_need_auth(&failed_data, plan.shown_flags());
            let post_poll_snapshot = watch_mode
                .filter(|_| needs_watch)
                .map(poller::credential_watch_snapshot);
            let watch_decision = auth_watch_decision(
                needs_watch,
                pre_poll_snapshot.as_ref(),
                post_poll_snapshot.as_ref(),
            );
            let auth_transition_kind = {
                let mut state = lock_state();
                if !POLL_COORDINATOR.is_current(generation) {
                    diagnose::log(format!(
                        "discarded stale poll error generation={generation} current={}",
                        POLL_COORDINATOR.generation.load(Ordering::Acquire)
                    ));
                    return;
                }
                let mut notify_kind = None;
                if let Some(s) = state.as_mut() {
                    let refresh_now = Instant::now();
                    let updated_unix = now_unix_secs();
                    let auth_transitions = update_provider_refresh_states(
                        s,
                        plan,
                        &failed_data,
                        updated_unix,
                        refresh_now,
                    );
                    notify_kind = shown_provider_order(
                        &s.provider_order,
                        s.show_claude_code,
                        s.show_codex,
                        s.show_antigravity,
                        s.show_grok,
                    )
                    .into_iter()
                    .find(|kind| auth_transitions.contains(kind));
                    s.last_error = Some(e);
                    // PollFailure carries this pass's exact per-provider
                    // errors even when every provider failed. Preserve any
                    // previous usage values, but always replace stale error
                    // reasons before either pausing or scheduling a retry.
                    let merged = merge_missing_provider_data(
                        s.data.as_ref(),
                        failed_data,
                        credential_read_scope(s, CredentialReadReason::Poll).flags(),
                    );
                    s.data = Some(merged);
                    s.manual_refresh_in_progress = false;
                    s.last_poll_ok = true;
                    refresh_usage_texts(s);

                    match (pause_for_auth, watch_decision) {
                        (true, AuthWatchDecision::Watch | AuthWatchDecision::WatchAndPollNow) => {
                            s.auth_error_paused_polling = true;
                            let now = Instant::now();
                            s.auth_recovery_recheck_deadline = auth_recovery_recheck_deadline(
                                needs_auth_rejection_recheck,
                                s.poll_interval_ms,
                                now,
                            );
                            s.auth_watch_active = true;
                            if let (Some(mode), Some(snapshot)) =
                                (watch_mode, post_poll_snapshot.clone())
                            {
                                s.auth_watch_mode = mode;
                                s.auth_watch_snapshot = snapshot;
                            }
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(controller_hwnd, TIMER_POLL);
                                let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                                let interval = paused_poll_timer_interval_ms(
                                    s.poll_interval_ms,
                                    s.auth_recovery_recheck_deadline,
                                    now,
                                );
                                arm_poll_timer(s, controller_hwnd, interval);
                                // Watch the credentials on a short cadence so
                                // signing back in is picked up without waiting
                                // out the poll interval.
                                arm_timer(
                                    controller_hwnd,
                                    TIMER_AUTH_WATCH,
                                    AUTH_WATCH_INTERVAL_MS,
                                    "auth watch",
                                );
                            }
                        }
                        (_, watch_decision) => {
                            s.auth_error_paused_polling = false;
                            s.auth_recovery_recheck_deadline = None;

                            match watch_decision {
                                AuthWatchDecision::Stop => {
                                    s.auth_watch_active = false;
                                    s.auth_watch_mode = poller::CredentialWatchMode::ClaudeSources;
                                    s.auth_watch_snapshot.clear();
                                    unsafe {
                                        let _ = KillTimer(controller_hwnd, TIMER_AUTH_WATCH);
                                    }
                                }
                                AuthWatchDecision::Watch | AuthWatchDecision::WatchAndPollNow => {
                                    if let (Some(mode), Some(snapshot)) =
                                        (watch_mode, post_poll_snapshot.clone())
                                    {
                                        s.auth_watch_active = true;
                                        s.auth_watch_mode = mode;
                                        s.auth_watch_snapshot = snapshot;
                                        unsafe {
                                            arm_timer(
                                                controller_hwnd,
                                                TIMER_AUTH_WATCH,
                                                AUTH_WATCH_INTERVAL_MS,
                                                "auth watch",
                                            );
                                        }
                                    }
                                }
                            }

                            let base_retry_ms = if has_transient_failure {
                                s.retry_count = s.retry_count.saturating_add(1);
                                let backoff = RETRY_BASE_MS.saturating_mul(
                                    1u32.checked_shl(s.retry_count.saturating_sub(1))
                                        .unwrap_or(u32::MAX),
                                );
                                backoff.min(s.poll_interval_ms)
                            } else {
                                s.retry_count = 0;
                                s.poll_interval_ms
                            };
                            let retry_ms = poll_delay_with_provider_cooldowns(
                                s,
                                base_retry_ms,
                                Instant::now(),
                            );
                            unsafe {
                                let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
                                arm_poll_timer(s, controller_hwnd, retry_ms);
                            }
                        }
                    }
                }
                notify_kind
            };

            // Outside the lock: request_poll takes it. This failure was
            // decided against credentials that changed mid-poll, so re-poll
            // instead of pausing on a verdict that is already stale.
            if watch_decision == AuthWatchDecision::WatchAndPollNow {
                diagnose::log("credentials changed while polling; polling again");
                request_poll();
            }

            if let Some(kind) = auth_transition_kind {
                let balloon = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        let strings = s.language.strings();
                        // `update_provider_refresh_state` only reports a
                        // transition for `AuthenticationFailed`; a provider
                        // that was never signed in deliberately raises no
                        // balloon, so this status is exact rather than a
                        // fallback. Keep both in step if that ever changes.
                        let (title, body) = credential_notification_text(
                            kind,
                            ProviderStatus::AuthenticationFailed,
                            strings,
                        );
                        (title, body)
                    })
                };
                if let Some((title, body)) = balloon {
                    tray_icon::notify_balloon(
                        main_hwnd,
                        kind,
                        tray_icon::BalloonTone::ActionRequired,
                        &title,
                        &body,
                    );
                }
            }

            post_usage_updated();
        }
    }
}

/// True when some provider that is currently healthy (no per-provider error
/// recorded by the last poll) has a quota window past its reset time - the
/// only case where the 5s reset fast poll actually helps.
fn healthy_provider_past_reset(data: &AppUsageData) -> bool {
    tray_icon::TrayIconKind::ALL.into_iter().any(|kind| {
        let slot = data.provider(kind);
        slot.error.is_none() && slot.usage.as_ref().is_some_and(poller::is_past_reset)
    })
}

fn schedule_countdown_timer() {
    let controller_hwnd = poll_controller_hwnd();
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(controller_hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(controller_hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data - but
    // only when the past-reset provider itself is healthy. A failing
    // provider's carried-forward stale window also looks "past reset" for
    // the whole outage (merge keeps it), and fast-polling then would hammer
    // a broken endpoint (and rewrite the usage cache) at 5s cadence; the
    // retry/backoff timer owns that case.
    if healthy_provider_past_reset(data) && s.last_error.is_none() {
        unsafe {
            arm_timer(controller_hwnd, TIMER_RESET_POLL, 5_000, "reset poll");
        }
    }

    let usage_display_delay = tray_icon::TrayIconKind::ALL
        .into_iter()
        .filter_map(|kind| data.usage(kind))
        .flat_map(|usage| usage.windows.iter())
        .filter_map(|window| poller::time_until_display_change(window.resets_at))
        .min();
    let now_unix = now_unix_secs();
    let stale_transition_delay = tray_icon::TrayIconKind::ALL
        .map(|kind| {
            (
                data.error(kind),
                s.provider_refresh_states.for_kind(kind),
                data.provider(kind).updated_unix,
            )
        })
        .into_iter()
        .filter_map(|(status, refresh_state, updated_unix)| {
            provider_stale_transition_delay(
                status,
                refresh_state,
                updated_unix,
                s.poll_interval_ms,
                now_unix,
            )
        })
        .min();
    let min_delay = usage_display_delay
        .into_iter()
        .chain(stale_transition_delay)
        .min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        arm_timer(controller_hwnd, TIMER_COUNTDOWN, ms, "countdown");
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let new_high_contrast = theme::is_high_contrast();
    let (changed, hwnd, floating_hwnd) = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark || s.is_high_contrast != new_high_contrast {
                s.is_dark = new_dark;
                s.is_high_contrast = new_high_contrast;
                (true, Some(s.hwnd.to_hwnd()), s.floating_hwnd)
            } else {
                (false, None, None)
            }
        } else {
            (false, None, None)
        }
    };
    if changed {
        render_layered();
        if let Some(floating_hwnd) = floating_hwnd {
            unsafe {
                apply_floating_dwm_style(floating_hwnd, new_dark, new_high_contrast);
                let _ = InvalidateRect(floating_hwnd, None, false);
            }
        }
        // The tray icons and the detail popup follow the theme too.
        if let Some(hwnd) = hwnd {
            sync_tray_icons(hwnd);
        }
        refresh_detail_popup_if_open();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
        // Tray tooltips and the popup carry localized text too; without this
        // they would keep the old language until the next poll.
        let hwnd = {
            let state = lock_state();
            state.as_ref().map(|s| s.hwnd.to_hwnd())
        };
        if let Some(hwnd) = hwnd {
            sync_tray_icons(hwnd);
        }
        refresh_detail_popup_if_open();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

/// Wait briefly for the taskbar's notification area to stop moving before
/// the widget is positioned and shown for the first time. Registering our
/// own tray icons (and, right after sign-in, every other startup app's)
/// widens TrayNotifyWnd asynchronously; positioning against a rect that is
/// still changing is what made the widget visibly jump right after launch.
fn wait_for_tray_geometry_stable(max_wait: Duration) {
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
    let deadline = Instant::now() + max_wait;
    let mut last: Option<(i32, i32, i32, i32)> = None;
    loop {
        let taskbar_hwnd = {
            let state = lock_state();
            state.as_ref().and_then(|s| s.taskbar_hwnd)
        };
        // Hidden/unembedded mode: nothing to wait for.
        let Some(taskbar_hwnd) = taskbar_hwnd else {
            return;
        };
        let current = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
            .and_then(native_interop::get_window_rect_safe)
            .or_else(|| native_interop::get_taskbar_rect(taskbar_hwnd))
            .map(|r| (r.left, r.top, r.right, r.bottom));
        if current.is_some() && current == last {
            return;
        }
        last = current;
        if Instant::now() + SAMPLE_INTERVAL > deadline {
            diagnose::log("tray geometry did not stabilize in time; positioning anyway");
            return;
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

fn position_at_taskbar() {
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, taskbar_hwnd, widget_placement) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Don't fight the user's drag.
        if s.dragging {
            return;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            taskbar_hwnd,
            s.widget_placement.clone(),
        )
    };

    if unsafe { !IsWindow(hwnd).as_bool() } {
        diagnose::log(format!(
            "position_at_taskbar skipped: widget hwnd missing hwnd={:?}",
            hwnd
        ));
        let thread_id = UI_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            let _ = unsafe { PostThreadMessageW(thread_id, WM_APP_REVIVE, WPARAM(0), LPARAM(0)) };
        }
        return;
    }
    let _dpi_scope = DpiScope::for_window(hwnd);

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;
    let anchor_top = taskbar_rect.top;
    let anchor_height = taskbar_height;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();
    let widget_height = sc(WIDGET_HEIGHT);
    let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let x = placement::resolve_widget_x(
        &widget_placement,
        taskbar_rect.left,
        tray_left,
        widget_width,
        dpi,
    );
    let tray_offset = (tray_left - taskbar_rect.left - widget_width - x).max(0);
    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.tray_offset = tray_offset;
        }
    }
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        native_interop::move_window(hwnd, x, y - taskbar_rect.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={} w={widget_width} h={widget_height}",
            y - taskbar_rect.top
        ));
    } else {
        // Topmost popup: screen coordinates
        let screen_x = taskbar_rect.left + x;
        native_interop::move_window(hwnd, screen_x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={screen_x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

fn compute_anchor_y(anchor_top: i32, anchor_height: i32, widget_height: i32) -> i32 {
    let anchor_bottom = anchor_top + anchor_height;
    (anchor_bottom - widget_height).max(anchor_top)
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // A panic unwinding across this FFI boundary would abort the process;
    // recover and log instead.
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        on_tray_location_changed_impl(hwnd)
    }))
    .is_err()
    {
        diagnose::log("panic in on_tray_location_changed (recovered)");
    }
}

fn on_tray_location_changed_impl(hwnd: HWND) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd || unsafe { IsChild(h, hwnd).as_bool() })
            .unwrap_or(false)
    };

    if is_tray {
        if tray_reposition_is_suppressed() {
            return;
        }

        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > TRAY_ORDER_EVENT_THROTTLE_MS)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            let main_hwnd = {
                let state = lock_state();
                state.as_ref().map(|s| s.hwnd.to_hwnd())
            };
            if let Some(main_hwnd) = main_hwnd {
                if !refresh_provider_order_from_tray(main_hwnd) {
                    position_at_taskbar();
                    render_layered();
                }
            }
        }
    }
}

/// Main window procedure: panic guard around the real handler. A panic
/// unwinding across this FFI boundary would abort the process; recover, log,
/// and fall back to default handling for the offending message.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        wnd_proc_impl(hwnd, msg, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            diagnose::log(format!("panic in wnd_proc msg={msg:#06x} (recovered)"));
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe fn wnd_proc_impl(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _dpi_scope = DpiScope::for_window(hwnd);
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_WTSSESSION_CHANGE_MSG => {
            handle_session_change(wparam.0);
            LRESULT(0)
        }
        WM_DPICHANGED_MSG => {
            let new_dpi = dpi_from_wparam(wparam);
            let _message_dpi_scope = DpiScope::new(new_dpi);
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            // lParam is a screen-space recommendation for top-level windows.
            // Once embedded, this HWND is a taskbar child and is laid out
            // after WM_DPICHANGED_AFTERPARENT instead.
            if !embedded {
                apply_suggested_dpi_rect(hwnd, lparam, "main widget");
            }
            position_at_taskbar();
            render_layered();
            diagnose::log(format!(
                "main widget: dpi changed dpi={new_dpi} embedded={embedded}"
            ));
            LRESULT(0)
        }
        WM_DPICHANGED_AFTERPARENT => {
            position_at_taskbar();
            render_layered();
            diagnose::log(format!(
                "main widget: parent dpi change applied dpi={}",
                GetDpiForWindow(hwnd)
            ));
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
                // The popup follows the system theme too; repaint if open.
                refresh_detail_popup_if_open();
            }
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    handle_poll_timer();
                }
                TIMER_COUNTDOWN => {
                    handle_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    handle_reset_poll_timer();
                }
                TIMER_AUTH_WATCH => {
                    handle_auth_watch_timer(hwnd);
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                // `poll_controller_hwnd` falls back to this window when the
                // helper is gone, so both procs answer for controller timers.
                TIMER_PROVIDER_DETECT => {
                    handle_provider_detect_timer();
                }
                TIMER_TRAY_ORDER => {
                    migrate_legacy_placements_if_needed();
                    refresh_provider_order_from_tray(hwnd);
                }
                TIMER_TRAY_ORDER_CONFIRM => {
                    let _ = KillTimer(hwnd, TIMER_TRAY_ORDER_CONFIRM);
                    refresh_provider_order_from_tray(hwnd);
                }
                TIMER_WIDGET_TOOLTIP => {
                    show_widget_tooltip_for_hover(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            handle_usage_updated();
            LRESULT(0)
        }
        WM_APP_PERSISTENCE_WARNING => {
            show_pending_persistence_warning_once();
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if cursor_is_on_drag_handle(hwnd) {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            hide_widget_tooltip(hwnd, true);
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            if !is_drag_handle_point(client_x, client_y) {
                return LRESULT(0);
            }

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                s.dragging = true;
                s.drag_start_mouse_x = pt.x;
                s.drag_start_client_x = client_x;
                s.drag_start_offset = s.tray_offset;
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            update_widget_tooltip_hover(hwnd, client_x, client_y);
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let mut new_offset = s.drag_start_offset + delta;

                    // Clamp: offset >= 0 (can't go right of default)
                    if new_offset < 0 {
                        new_offset = 0;
                    }

                    let taskbar_hwnd = s.taskbar_hwnd;
                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();

                    // Clamp: don't go past left edge of taskbar
                    if let Some(taskbar_hwnd) = taskbar_hwnd {
                        if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                            let mut tray_left = taskbar_rect.right;
                            if let Some(tray_hwnd) =
                                native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                            {
                                if let Some(tray_rect) =
                                    native_interop::get_window_rect_safe(tray_hwnd)
                                {
                                    tray_left = tray_rect.left;
                                }
                            }
                            let widget_width = total_widget_width_for_state(s);
                            let max_offset = (tray_left - taskbar_rect.left - widget_width).max(0);
                            if new_offset > max_offset {
                                new_offset = max_offset;
                            }

                            s.tray_offset = new_offset;

                            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                            let anchor_top = taskbar_rect.top;
                            let anchor_height = taskbar_height;
                            let widget_height = sc(WIDGET_HEIGHT);
                            let y = compute_anchor_y(anchor_top, anchor_height, widget_height);
                            let x = if embedded {
                                tray_left - taskbar_rect.left - widget_width - new_offset
                            } else {
                                tray_left - widget_width - new_offset
                            };
                            Some((
                                hwnd_val,
                                embedded,
                                x,
                                y,
                                taskbar_rect.top,
                                widget_width,
                                widget_height,
                            ))
                        } else {
                            s.tray_offset = new_offset;
                            None
                        }
                    } else {
                        s.tray_offset = new_offset;
                        None
                    }
                };

                if let Some((hwnd_val, embedded, x, y, taskbar_top, widget_width, widget_height)) =
                    move_target
                {
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            x,
                            y - taskbar_top,
                            widget_width,
                            widget_height,
                        );
                    } else {
                        native_interop::move_window(hwnd_val, x, y, widget_width, widget_height);
                    }
                }
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            hide_widget_tooltip(hwnd, true);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let drag_result = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        Some((s.taskbar_index, s.drag_start_client_x))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((current_taskbar_index, drag_start_client_x)) = drag_result {
                let _ = ReleaseCapture();
                let target = taskbar_at_point(pt).or_else(|| {
                    native_interop::find_taskbars()
                        .get(current_taskbar_index)
                        .copied()
                        .map(|taskbar| (current_taskbar_index, taskbar))
                });
                if let Some((target_index, target_taskbar)) = target {
                    let _dpi_scope = DpiScope::for_window(target_taskbar.hwnd);
                    let dpi = GetDpiForWindow(target_taskbar.hwnd).max(96);
                    let tray_left = tray_left_for_taskbar(target_taskbar.hwnd, target_taskbar.rect);
                    let widget_width = total_widget_width();
                    let desired_left = pt.x - drag_start_client_x;
                    let (anchor, gap_dip) = placement::custom_widget_anchor(
                        target_taskbar.rect.left,
                        tray_left,
                        desired_left,
                        widget_width,
                        dpi,
                    );
                    let monitor = monitor_identity_for_taskbar(&target_taskbar);
                    if let Some(monitor) = monitor {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.widget_placement = WidgetPlacement::Custom {
                                monitor: monitor.key(),
                                anchor,
                                gap_dip,
                            };
                            s.widget_placement_needs_migration = false;
                            s.preferred_taskbar_index = target_index;
                        }
                    }
                    if target_index == current_taskbar_index
                        || attach_to_taskbar(hwnd, target_index)
                    {
                        position_at_taskbar();
                        render_layered();
                    }
                }
                save_state_settings();
            } else {
                // Plain click on the widget body (not a drag): open the usage
                // detail popup - a far bigger click target than the tray icon.
                show_usage_details(hwnd, None);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            hide_widget_tooltip(hwnd, true);
            show_context_menu(hwnd, None);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_quit(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                IDM_REFRESH_NOW => {
                    trigger_manual_refresh(hwnd);
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => {
                            if release.is_some() {
                                begin_winget_update(hwnd);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd, true);
                            }
                        }
                    }
                }
                2 => {
                    request_quit(hwnd);
                }
                IDM_WIDGET_PRIMARY_LEFT | IDM_WIDGET_PRIMARY_RIGHT => {
                    let default_position = if id == IDM_WIDGET_PRIMARY_LEFT {
                        WidgetDefaultPosition::PrimaryTaskbarLeft
                    } else {
                        WidgetDefaultPosition::PrimaryTaskbarRight
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.widget_default_position = default_position;
                            s.widget_placement = match default_position {
                                WidgetDefaultPosition::PrimaryTaskbarLeft => {
                                    WidgetPlacement::PrimaryLeft
                                }
                                WidgetDefaultPosition::PrimaryTaskbarRight => {
                                    WidgetPlacement::PrimaryRight
                                }
                            };
                            s.widget_placement_needs_migration = false;
                        }
                    }
                    save_state_settings();
                    recover_shell_surfaces("primary taskbar edge selected");
                }
                IDM_START_WITH_WINDOWS => {
                    let enable = !is_startup_enabled();
                    if let Err(error) = set_startup_enabled(enable) {
                        let strings = {
                            let state = lock_state();
                            state
                                .as_ref()
                                .map(|s| s.language.strings())
                                .unwrap_or(LanguageId::English.strings())
                        };
                        show_error_message(strings.settings, &error);
                    }
                }
                IDM_FREQ_1MIN | IDM_FREQ_2MIN | IDM_FREQ_5MIN | IDM_FREQ_10MIN | IDM_FREQ_15MIN
                | IDM_FREQ_30MIN => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_2MIN => POLL_2_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_10MIN => POLL_10_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_30MIN => POLL_30_MIN,
                        _ => POLL_5_MIN,
                    };
                    {
                        let controller_hwnd = poll_controller_hwnd();
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                            let timer_interval = paused_poll_timer_interval_ms(
                                new_interval,
                                s.auth_recovery_recheck_deadline,
                                Instant::now(),
                            );
                            arm_poll_timer(s, controller_hwnd, timer_interval);
                        } else {
                            arm_timer(controller_hwnd, TIMER_POLL, new_interval, "poll");
                        }
                    }
                    save_state_settings();
                }
                IDM_MODEL_CLAUDE_CODE
                | IDM_MODEL_CODEX
                | IDM_MODEL_ANTIGRAVITY
                | IDM_MODEL_GROK => {
                    let kind = match id {
                        IDM_MODEL_CLAUDE_CODE => tray_icon::TrayIconKind::Claude,
                        IDM_MODEL_CODEX => tray_icon::TrayIconKind::Codex,
                        IDM_MODEL_ANTIGRAVITY => tray_icon::TrayIconKind::Antigravity,
                        IDM_MODEL_GROK => tray_icon::TrayIconKind::Grok,
                        _ => unreachable!(),
                    };
                    let (is_shown, pending) = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_MODEL_CLAUDE_CODE => {
                                    if s.show_codex
                                        || s.show_antigravity
                                        || s.show_grok
                                        || !s.show_claude_code
                                    {
                                        s.show_claude_code = !s.show_claude_code;
                                    }
                                }
                                IDM_MODEL_CODEX => {
                                    if s.show_claude_code
                                        || s.show_antigravity
                                        || s.show_grok
                                        || !s.show_codex
                                    {
                                        s.show_codex = !s.show_codex;
                                    }
                                }
                                IDM_MODEL_ANTIGRAVITY => {
                                    if s.show_claude_code
                                        || s.show_codex
                                        || s.show_grok
                                        || !s.show_antigravity
                                    {
                                        s.show_antigravity = !s.show_antigravity;
                                    }
                                }
                                IDM_MODEL_GROK => {
                                    if s.show_claude_code
                                        || s.show_codex
                                        || s.show_antigravity
                                        || !s.show_grok
                                    {
                                        s.show_grok = !s.show_grok;
                                    }
                                }
                                _ => {}
                            }
                            s.provider_refresh_states.reset_hidden(
                                s.show_claude_code,
                                s.show_codex,
                                s.show_antigravity,
                                s.show_grok,
                            );
                            set_widget_placeholders(s, "...");
                            s.pending_provider_order = None;
                            s.pending_provider_order_samples = 0;
                            let is_shown = provider_is_shown(s, kind);
                            let pending = provider_pending_flag(s, kind);
                            match kind {
                                tray_icon::TrayIconKind::Claude => {
                                    s.claude_credential_access_decided = true;
                                }
                                tray_icon::TrayIconKind::Codex => {
                                    s.codex_credential_access_decided = true;
                                }
                                tray_icon::TrayIconKind::Antigravity => {
                                    s.antigravity_credential_access_decided = true;
                                }
                                tray_icon::TrayIconKind::Grok => {
                                    s.grok_credential_access_decided = true;
                                }
                            }
                            (is_shown, pending)
                        } else {
                            (false, false)
                        }
                    };
                    save_state_settings();
                    if is_shown && pending {
                        review_pending_provider(hwnd, kind);
                    }
                    position_at_taskbar();
                    render_layered();
                    refresh_floating_monitor();
                    sync_tray_icons(hwnd);
                    refresh_provider_order_from_tray(hwnd);
                    request_poll();
                }
                IDM_REDETECT_PROVIDERS => {
                    if ensure_credential_consent(hwnd) {
                        if confirm_pending_providers_for_manual(hwnd) {
                            // A provider answered here is already shown and
                            // allowed, so the detection pass below finds
                            // nothing new and returns without touching the
                            // surfaces. Refresh them here instead, the same
                            // way the Provider access entries do.
                            position_at_taskbar();
                            render_layered();
                            refresh_floating_monitor();
                            sync_tray_icons(hwnd);
                            refresh_provider_order_from_tray(hwnd);
                            request_poll();
                        }
                        spawn_provider_detection(DetectionReason::Manual);
                    }
                }
                IDM_ACCESS_CLAUDE_CODE
                | IDM_ACCESS_CODEX
                | IDM_ACCESS_ANTIGRAVITY
                | IDM_ACCESS_GROK => {
                    let kind = match id {
                        IDM_ACCESS_CLAUDE_CODE => tray_icon::TrayIconKind::Claude,
                        IDM_ACCESS_CODEX => tray_icon::TrayIconKind::Codex,
                        IDM_ACCESS_ANTIGRAVITY => tray_icon::TrayIconKind::Antigravity,
                        IDM_ACCESS_GROK => tray_icon::TrayIconKind::Grok,
                        _ => unreachable!(),
                    };
                    let (currently_allowed, pending) = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| {
                                (
                                    provider_has_credential_access(s, kind),
                                    provider_pending_flag(s, kind),
                                )
                            })
                            .unwrap_or((false, false))
                    };
                    if pending {
                        review_pending_provider(hwnd, kind);
                    } else if currently_allowed {
                        set_provider_credential_access(kind, false);
                    } else if ensure_credential_consent(hwnd) {
                        set_provider_credential_access(kind, true);
                    }
                    position_at_taskbar();
                    render_layered();
                    refresh_floating_monitor();
                    sync_tray_icons(hwnd);
                    refresh_provider_order_from_tray(hwnd);
                    request_poll();
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_SIMPLIFIED_CHINESE
                | IDM_LANG_TRADITIONAL_CHINESE
                | IDM_LANG_RUSSIAN
                | IDM_LANG_PORTUGUESE_BRAZIL => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_SIMPLIFIED_CHINESE => Some(LanguageId::SimplifiedChinese),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        IDM_LANG_RUSSIAN => Some(LanguageId::Russian),
                        IDM_LANG_PORTUGUESE_BRAZIL => Some(LanguageId::PortugueseBrazil),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                    refresh_floating_monitor();
                }
                IDM_NOTIFY_SESSION_RESET | IDM_NOTIFY_WEEKLY_RESET => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_NOTIFY_SESSION_RESET => {
                                    s.notify_session_reset = !s.notify_session_reset;
                                }
                                IDM_NOTIFY_WEEKLY_RESET => {
                                    s.notify_weekly_reset = !s.notify_weekly_reset;
                                }
                                _ => {}
                            }
                        }
                    }
                    save_state_settings();
                }
                IDM_DETAILED_TRAY_ICONS => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.detailed_tray_icons = !s.detailed_tray_icons;
                            s.pending_provider_order = None;
                            s.pending_provider_order_samples = 0;
                        }
                    }
                    save_state_settings();
                    sync_tray_icons(hwnd);
                    position_at_taskbar();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                IDM_TOGGLE_FLOATING => {
                    toggle_floating_monitor();
                }
                IDM_FLOATING_DEFAULT_BOTTOM_LEFT | IDM_FLOATING_DEFAULT_BOTTOM_RIGHT => {
                    let default_position = if id == IDM_FLOATING_DEFAULT_BOTTOM_LEFT {
                        FloatingDefaultPosition::PrimaryBottomLeft
                    } else {
                        FloatingDefaultPosition::PrimaryBottomRight
                    };
                    set_floating_default_position(default_position);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ShowDetails { kind, keyboard } => {
                    let anchor = keyboard
                        .then(|| tray_icon_anchor_point(hwnd, kind))
                        .flatten();
                    show_usage_details(hwnd, anchor);
                }
                tray_icon::TrayAction::ShowContextMenu {
                    kind,
                    anchor_to_icon,
                } => {
                    let anchor = anchor_to_icon
                        .then(|| tray_icon_anchor_point(hwnd, kind))
                        .flatten();
                    show_context_menu(hwnd, anchor);
                    match kind {
                        Some(kind) => tray_icon::restore_focus(hwnd, kind),
                        None => tray_icon::restore_app_focus(hwnd),
                    }
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_WIDGET_TOOLTIP);
            {
                let mut runtime = lock_widget_tooltip_runtime();
                runtime.hover_kind = None;
                runtime.hits.clear();
                runtime.snapshot = None;
            }
            let (hook, tooltip) = {
                let mut state = lock_state();
                match state.as_mut() {
                    Some(s) => (s.win_event_hook.take(), s.widget_tooltip_hwnd.take()),
                    None => (None, None),
                }
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            if let Some(tooltip) = tooltip {
                let tooltip = tooltip.to_hwnd();
                if IsWindow(tooltip).as_bool() {
                    let _ = DestroyWindow(tooltip);
                }
            }
            let _ = WTSUnRegisterSessionNotification(hwnd);
            tray_icon::remove_all(hwnd);
            if QUIT_REQUESTED.load(Ordering::SeqCst) {
                PostQuitMessage(0);
            } else {
                // Nothing destroys the main widget window on purpose (the
                // detail popup manages its own DestroyWindow), so reaching
                // here means explorer destroyed our embedded child window
                // (taskbar rebuilt, or the hosting secondary taskbar vanished
                // after an RDP session switch). Upstream quit the process here
                // - the "widget gone until reboot" bug. Revive instead; the
                // thread message keeps the loop alive after this window dies.
                diagnose::log("window destroyed externally; scheduling in-process revival");
                let _ =
                    PostThreadMessageW(GetCurrentThreadId(), WM_APP_REVIVE, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn provider_menu_item_flags(enabled: bool, only_enabled: bool) -> MENU_ITEM_FLAGS {
    let mut flags = MENU_ITEM_FLAGS(0);
    if enabled {
        flags |= MF_CHECKED;
    }
    if enabled && only_enabled {
        flags |= MF_GRAYED;
    }
    flags
}

fn show_context_menu(hwnd: HWND, anchor: Option<POINT>) {
    unsafe {
        let (
            current_interval,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            widget_visible,
            floating_visible,
            detailed_tray_icons,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_grok,
            allow_claude_credentials,
            allow_codex_credentials,
            allow_antigravity_credentials,
            allow_grok_credentials,
            pending_claude_credentials,
            pending_codex_credentials,
            pending_antigravity_credentials,
            pending_grok_credentials,
            notify_session_reset,
            notify_weekly_reset,
            widget_placement,
            floating_placement,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.floating_visible,
                    s.detailed_tray_icons,
                    s.show_claude_code,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_grok,
                    s.allow_claude_credentials,
                    s.allow_codex_credentials,
                    s.allow_antigravity_credentials,
                    s.allow_grok_credentials,
                    s.claude_credential_access_pending,
                    s.codex_credential_access_pending,
                    s.antigravity_credential_access_pending,
                    s.grok_credential_access_pending,
                    s.notify_session_reset,
                    s.notify_weekly_reset,
                    s.widget_placement.clone(),
                    s.floating_placement.clone(),
                ),
                None => (
                    POLL_5_MIN,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    true,
                    false,
                    true,
                    true,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    false,
                    WidgetPlacement::PrimaryRight,
                    FloatingPlacement::PrimaryBottomRight,
                ),
            }
        };

        // Menu creation can fail under GDI/USER handle pressure; skipping the
        // menu for one right-click beats aborting the whole process.
        let Ok(menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            return;
        };

        // Refresh submenu: immediate action first, followed by the interval.
        let Ok(refresh_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
        let refresh_now = native_interop::wide_str(strings.refresh_now);
        let _ = AppendMenuW(
            refresh_menu,
            MENU_ITEM_FLAGS(0),
            IDM_REFRESH_NOW as usize,
            PCWSTR::from_raw(refresh_now.as_ptr()),
        );
        let _ = AppendMenuW(refresh_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let freq_items: [(u16, u32, &str); 6] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_2MIN, POLL_2_MIN, strings.two_minutes),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_10MIN, POLL_10_MIN, strings.ten_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_30MIN, POLL_30_MIN, strings.thirty_minutes),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                refresh_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let refresh_label = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            refresh_menu.0 as usize,
            PCWSTR::from_raw(refresh_label.as_ptr()),
        );

        // Providers submenu. The last enabled provider is visibly disabled,
        // rather than accepting a click and silently ignoring it.
        let Ok(models_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
        let claude_model = native_interop::wide_str(strings.claude_model);
        let only_one_provider = u8::from(show_claude_code)
            + u8::from(show_codex)
            + u8::from(show_antigravity)
            + u8::from(show_grok)
            == 1;
        let claude_flags = provider_menu_item_flags(show_claude_code, only_one_provider);
        let _ = AppendMenuW(
            models_menu,
            claude_flags,
            IDM_MODEL_CLAUDE_CODE as usize,
            PCWSTR::from_raw(claude_model.as_ptr()),
        );

        let codex_model = native_interop::wide_str(strings.codex_model);
        let codex_flags = provider_menu_item_flags(show_codex, only_one_provider);
        let _ = AppendMenuW(
            models_menu,
            codex_flags,
            IDM_MODEL_CODEX as usize,
            PCWSTR::from_raw(codex_model.as_ptr()),
        );

        let antigravity_model = native_interop::wide_str(strings.antigravity_model);
        let antigravity_flags = provider_menu_item_flags(show_antigravity, only_one_provider);
        let _ = AppendMenuW(
            models_menu,
            antigravity_flags,
            IDM_MODEL_ANTIGRAVITY as usize,
            PCWSTR::from_raw(antigravity_model.as_ptr()),
        );

        let grok_model = native_interop::wide_str(strings.grok_model);
        let grok_flags = provider_menu_item_flags(show_grok, only_one_provider);
        let _ = AppendMenuW(
            models_menu,
            grok_flags,
            IDM_MODEL_GROK as usize,
            PCWSTR::from_raw(grok_model.as_ptr()),
        );

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        // Credential access is independent from visibility. A provider can
        // stay visible with no permission, leaving a safe route to re-enable
        // access after the user revokes or declines it.
        let Ok(access_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };
        for (id, name, allowed, pending) in [
            (
                IDM_ACCESS_CLAUDE_CODE,
                strings.claude_model,
                allow_claude_credentials,
                pending_claude_credentials,
            ),
            (
                IDM_ACCESS_CODEX,
                strings.codex_model,
                allow_codex_credentials,
                pending_codex_credentials,
            ),
            (
                IDM_ACCESS_ANTIGRAVITY,
                strings.antigravity_model,
                allow_antigravity_credentials,
                pending_antigravity_credentials,
            ),
            (
                IDM_ACCESS_GROK,
                strings.grok_model,
                allow_grok_credentials,
                pending_grok_credentials,
            ),
        ] {
            let label_owned = if pending {
                strings.access_needs_review.replace("{provider}", name)
            } else {
                name.to_string()
            };
            let label = native_interop::wide_str(&label_owned);
            let flags = if pending {
                MENU_ITEM_FLAGS(0)
            } else if allowed {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                access_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        // Existing installs are never swept automatically, and a user who
        // just signed in somewhere should not have to wait out the detection
        // interval. Both are served by asking for a sweep on demand.
        let _ = AppendMenuW(access_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let redetect_label = native_interop::wide_str(strings.redetect_providers);
        let _ = AppendMenuW(
            access_menu,
            MENU_ITEM_FLAGS(0),
            IDM_REDETECT_PROVIDERS as usize,
            PCWSTR::from_raw(redetect_label.as_ptr()),
        );
        let access_label = native_interop::wide_str(
            localization::credential_consent_copy(language).provider_access,
        );
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            access_menu.0 as usize,
            PCWSTR::from_raw(access_label.as_ptr()),
        );

        // Settings submenu
        let Ok(settings_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(menu);
            return;
        };

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let Ok(widget_position_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let taskbar_left_label = native_interop::wide_str(strings.primary_taskbar_left);
        let _ = AppendMenuW(
            widget_position_menu,
            MENU_ITEM_FLAGS(0),
            IDM_WIDGET_PRIMARY_LEFT as usize,
            PCWSTR::from_raw(taskbar_left_label.as_ptr()),
        );
        let taskbar_right_label = native_interop::wide_str(strings.primary_taskbar_right);
        let _ = AppendMenuW(
            widget_position_menu,
            MENU_ITEM_FLAGS(0),
            IDM_WIDGET_PRIMARY_RIGHT as usize,
            PCWSTR::from_raw(taskbar_right_label.as_ptr()),
        );
        let widget_position_selected = match widget_placement {
            WidgetPlacement::PrimaryLeft => Some(IDM_WIDGET_PRIMARY_LEFT),
            WidgetPlacement::PrimaryRight => Some(IDM_WIDGET_PRIMARY_RIGHT),
            WidgetPlacement::Custom { .. } => None,
        };
        if let Some(widget_position_selected) = widget_position_selected {
            let _ = CheckMenuRadioItem(
                widget_position_menu,
                IDM_WIDGET_PRIMARY_LEFT as u32,
                IDM_WIDGET_PRIMARY_RIGHT as u32,
                widget_position_selected as u32,
                MF_BYCOMMAND.0,
            );
        }
        let widget_position_label = native_interop::wide_str(strings.widget_default_position);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            widget_position_menu.0 as usize,
            PCWSTR::from_raw(widget_position_label.as_ptr()),
        );

        let Ok(floating_position_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let bottom_left_label = native_interop::wide_str(strings.primary_display_bottom_left);
        let _ = AppendMenuW(
            floating_position_menu,
            MENU_ITEM_FLAGS(0),
            IDM_FLOATING_DEFAULT_BOTTOM_LEFT as usize,
            PCWSTR::from_raw(bottom_left_label.as_ptr()),
        );
        let bottom_right_label = native_interop::wide_str(strings.primary_display_bottom_right);
        let _ = AppendMenuW(
            floating_position_menu,
            MENU_ITEM_FLAGS(0),
            IDM_FLOATING_DEFAULT_BOTTOM_RIGHT as usize,
            PCWSTR::from_raw(bottom_right_label.as_ptr()),
        );
        let floating_position_selected = match floating_placement {
            FloatingPlacement::PrimaryBottomLeft => Some(IDM_FLOATING_DEFAULT_BOTTOM_LEFT),
            FloatingPlacement::PrimaryBottomRight => Some(IDM_FLOATING_DEFAULT_BOTTOM_RIGHT),
            FloatingPlacement::Custom { .. } => None,
        };
        if let Some(floating_position_selected) = floating_position_selected {
            let _ = CheckMenuRadioItem(
                floating_position_menu,
                IDM_FLOATING_DEFAULT_BOTTOM_LEFT as u32,
                IDM_FLOATING_DEFAULT_BOTTOM_RIGHT as u32,
                floating_position_selected as u32,
                MF_BYCOMMAND.0,
            );
        }
        let floating_position_label = native_interop::wide_str(strings.floating_default_position);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            floating_position_menu.0 as usize,
            PCWSTR::from_raw(floating_position_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let Ok(notifications_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let session_reset_label = native_interop::wide_str(strings.notify_session_reset);
        let session_reset_flags = if notify_session_reset {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            notifications_menu,
            session_reset_flags,
            IDM_NOTIFY_SESSION_RESET as usize,
            PCWSTR::from_raw(session_reset_label.as_ptr()),
        );
        let weekly_reset_label = native_interop::wide_str(strings.notify_weekly_reset);
        let weekly_reset_flags = if notify_weekly_reset {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            notifications_menu,
            weekly_reset_flags,
            IDM_NOTIFY_WEEKLY_RESET as usize,
            PCWSTR::from_raw(weekly_reset_label.as_ptr()),
        );
        let notifications_label = native_interop::wide_str(strings.notifications);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            notifications_menu.0 as usize,
            PCWSTR::from_raw(notifications_label.as_ptr()),
        );
        let Ok(language_menu) = CreatePopupMenu() else {
            diagnose::log("CreatePopupMenu failed; skipping context menu");
            // settings_menu is not attached to menu yet; destroy it separately.
            let _ = DestroyMenu(settings_menu);
            let _ = DestroyMenu(menu);
            return;
        };
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::SimplifiedChinese => IDM_LANG_SIMPLIFIED_CHINESE,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
                LanguageId::Russian => IDM_LANG_RUSSIAN,
                LanguageId::PortugueseBrazil => IDM_LANG_PORTUGUESE_BRAZIL,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags =
            if !updater::update_channel_configured() || update_status_is_busy(&update_status) {
                MF_GRAYED
            } else {
                MENU_ITEM_FLAGS(0)
            };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let floating_flags = if floating_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let detailed_icons_flags = if detailed_tray_icons {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let detailed_icons_label = native_interop::wide_str(strings.detailed_tray_icons);
        let _ = AppendMenuW(
            menu,
            detailed_icons_flags,
            IDM_DETAILED_TRAY_ICONS as usize,
            PCWSTR::from_raw(detailed_icons_label.as_ptr()),
        );
        let widget_label = native_interop::wide_str(strings.show_widget);
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );
        let floating_label = native_interop::wide_str(strings.show_floating_monitor);
        let _ = AppendMenuW(
            menu,
            floating_flags,
            IDM_TOGGLE_FLOATING as usize,
            PCWSTR::from_raw(floating_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = anchor.unwrap_or_default();
        if anchor.is_none() {
            let _ = GetCursorPos(&mut pt);
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn paint(hdc: HDC, hwnd: HWND) {
    let _dpi_scope = DpiScope::for_window(hwnd);
    let (is_dark, high_contrast, compact_vm, is_floating) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.is_high_contrast,
                s.compact_vm.clone(),
                s.floating_hwnd.is_some_and(|stored| stored.0 == hwnd.0),
            ),
            None => return,
        }
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);
        let scene = compact_scene(mem_dc, &compact_vm, high_contrast, is_floating);

        paint_compact_surface(
            mem_dc,
            width,
            height,
            &scene,
            is_floating,
            is_dark,
            high_contrast,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

#[cfg(test)]
mod reset_notification_tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn persistence_coordinator_drops_an_older_snapshot_that_arrives_last() {
        let coordinator = Arc::new(PersistenceCoordinator::new());
        let older_revision = coordinator.next_revision();
        let newer_revision = coordinator.next_revision();
        let newer_committed = Arc::new(Barrier::new(2));
        let writes = Arc::new(Mutex::new(Vec::new()));

        let older_writer = {
            let coordinator = Arc::clone(&coordinator);
            let newer_committed = Arc::clone(&newer_committed);
            let writes = Arc::clone(&writes);
            std::thread::spawn(move || {
                newer_committed.wait();
                coordinator
                    .write_if_latest(older_revision, || {
                        writes
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push("older");
                        Ok::<(), ()>(())
                    })
                    .expect("older write decision")
            })
        };

        assert!(coordinator
            .write_if_latest(newer_revision, || {
                writes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push("newer");
                Ok::<(), ()>(())
            })
            .expect("newer write"));
        newer_committed.wait();

        assert!(!older_writer.join().expect("older writer thread"));
        assert_eq!(
            *writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec!["newer"]
        );
    }

    #[test]
    fn settings_and_usage_cache_revision_streams_are_independent() {
        let settings = PersistenceCoordinator::new();
        let usage_cache = PersistenceCoordinator::new();
        let stale_settings_revision = settings.next_revision();
        let current_cache_revision = usage_cache.next_revision();
        let _current_settings_revision = settings.next_revision();

        assert!(!settings
            .write_if_latest(stale_settings_revision, || Ok::<(), ()>(()))
            .expect("stale settings decision"));
        assert!(usage_cache
            .write_if_latest(current_cache_revision, || Ok::<(), ()>(()))
            .expect("current cache write"));
    }

    /// A recreated widget must get back everything that died with the old one.
    ///
    /// `WM_TIMER` only reaches the window a timer was armed on, and
    /// `recreate_widget_window` hands back a different `HWND`. When the
    /// broadcast helper could not be created at startup the widget is also the
    /// poll controller, so polling, the credential watch and the periodic
    /// provider sweep were armed on the window Explorer just destroyed: with
    /// nothing to re-arm them the monitor stopped for good at the first
    /// Explorer restart, silently, while the app kept running. The tray-order
    /// sample was the same bug found one round earlier, which is why this set
    /// is data now instead of a run of `arm_timer` calls.
    #[test]
    fn a_recreated_widget_gets_back_every_timer_that_died_with_the_old_one() {
        let ids = |plan: Vec<(usize, u32, &'static str)>| {
            plan.into_iter().map(|(id, _, _)| id).collect::<Vec<_>>()
        };

        // Degraded path: the widget is the poll controller.
        let degraded = revive_timer_plan(true, true, POLL_1_MIN, true);
        assert_eq!(
            degraded
                .iter()
                .find(|(id, _, _)| *id == TIMER_POLL)
                .map(|(_, interval, _)| *interval),
            Some(POLL_1_MIN),
            "the poll timer must come back on the user's own interval"
        );
        let degraded = ids(degraded);
        for timer in [
            TIMER_TRAY_ORDER,
            TIMER_POLL,
            TIMER_PROVIDER_DETECT,
            TIMER_AUTH_WATCH,
        ] {
            assert!(degraded.contains(&timer), "missing timer {timer}");
        }

        // The watch is only re-armed when it was actually running.
        assert!(!ids(revive_timer_plan(true, true, POLL_5_MIN, false)).contains(&TIMER_AUTH_WATCH));

        // Normal path: the helper owns those and outlived the widget, so
        // re-arming them here would move them onto a window that is not the
        // controller.
        assert_eq!(
            ids(revive_timer_plan(true, false, POLL_1_MIN, true)),
            vec![TIMER_TRAY_ORDER]
        );

        // A widget that was never destroyed kept its timers. Re-arming the
        // poll timer would push the next poll out by a full interval on every
        // Explorer restart.
        assert_eq!(
            ids(revive_timer_plan(false, true, POLL_1_MIN, true)),
            vec![TIMER_TRAY_ORDER]
        );
    }

    /// Serializes the tests that install a panic hook. The hook is
    /// process-global, so two of them running at once would leave the wrong
    /// one installed for the rest of the run.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body`, which is expected to panic, without the default hook
    /// printing that panic into the test output.
    fn catch_expected_panic(body: impl FnOnce()) -> bool {
        let _serialized = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        std::panic::set_hook(previous);
        outcome.is_err()
    }

    /// A poll pass that panics must still hand the coordinator back.
    ///
    /// `in_flight` is what keeps a second worker from starting, so a pass that
    /// unwound past `finish_pass` left it set for the life of the process:
    /// every later request was collapsed into a pending pass that nobody ran,
    /// and usage froze while the app itself kept running - silently, because
    /// the panic hook wrote to a log nobody was reading. Asserted on
    /// `run_poll_passes`, the loop the running app enters, rather than on
    /// `PollPassGuard`: a revert that dropped the guard from the loop would
    /// leave a guard-only test green.
    #[test]
    fn a_panicking_poll_pass_releases_the_coordinator() {
        let coordinator = PollCoordinator::new();
        assert!(
            coordinator.request(false),
            "the first request owns the single worker"
        );

        let unwound = catch_expected_panic(|| {
            run_poll_passes(&coordinator, |_, _| panic!("a poll pass panicked"));
        });

        assert!(
            unwound,
            "the panic must still reach the thread boundary so the hook records it"
        );
        assert!(
            coordinator.request(false),
            "a later request must be able to start a new worker"
        );
    }

    /// Same contract for the credential watch, where the damage would be that
    /// a re-login is never picked up: the busy flag stops the 15-second timer
    /// from stacking checks on a slow probe, so a panic that unwound past a
    /// store at the end of the check disabled the watch for good. Asserted on
    /// `hold_credential_watch_busy`, which is what the spawned thread calls.
    #[test]
    fn a_panicking_credential_watch_check_releases_the_busy_flag() {
        CREDENTIAL_WATCH_BUSY.store(true, Ordering::Release);

        let unwound = catch_expected_panic(|| {
            hold_credential_watch_busy(|| panic!("a credential watch check panicked"));
        });

        assert!(unwound, "the panic must still reach the thread boundary");
        assert!(
            !CREDENTIAL_WATCH_BUSY.load(Ordering::Acquire),
            "the busy flag must be released so the next tick can check again"
        );
    }

    #[test]
    fn update_prompt_is_a_busy_state_until_the_modal_choice_returns() {
        assert!(!update_status_is_busy(&UpdateStatus::Idle));
        assert!(update_status_is_busy(&UpdateStatus::Checking));
        assert!(update_status_is_busy(&UpdateStatus::Prompting));
        assert!(update_status_is_busy(&UpdateStatus::Applying));
        assert!(!update_status_is_busy(&UpdateStatus::UpToDate));
    }

    fn relative_luminance(color: Color) -> f64 {
        let linear = |channel: u8| {
            let value = channel as f64 / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast_ratio(first: Color, second: Color) -> f64 {
        let first = relative_luminance(first);
        let second = relative_luminance(second);
        let (lighter, darker) = if first >= second {
            (first, second)
        } else {
            (second, first)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    struct OwnedIconInfoBitmaps {
        color: HBITMAP,
        mask: HBITMAP,
    }

    impl Drop for OwnedIconInfoBitmaps {
        fn drop(&mut self) {
            unsafe {
                if !self.color.is_invalid() {
                    let _ = DeleteObject(self.color);
                }
                if !self.mask.is_invalid() {
                    let _ = DeleteObject(self.mask);
                }
            }
        }
    }

    fn hicon_color_bitmap_metrics(hicon: HICON) -> (i32, i32, u16) {
        unsafe {
            let mut info = ICONINFO::default();
            GetIconInfo(hicon, &mut info).expect("GetIconInfo");
            let bitmaps = OwnedIconInfoBitmaps {
                color: info.hbmColor,
                mask: info.hbmMask,
            };
            assert!(
                !bitmaps.color.is_invalid(),
                "provider PNG should produce a color HBITMAP"
            );

            let mut bitmap = BITMAP::default();
            let copied = GetObjectW(
                bitmaps.color,
                std::mem::size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bitmap).cast()),
            );
            assert_eq!(
                copied,
                std::mem::size_of::<BITMAP>() as i32,
                "GetObjectW(BITMAP)"
            );

            (bitmap.bmWidth, bitmap.bmHeight, bitmap.bmBitsPixel)
        }
    }

    fn png_ihdr(bytes: &[u8]) -> (u32, u32, u8, u8) {
        assert!(bytes.len() >= 26);
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&bytes[12..16], b"IHDR");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
            bytes[24],
            bytes[25],
        )
    }

    #[test]
    fn dpi_scaling_is_window_local_and_restored_after_nested_scope() {
        assert_eq!(scale_px_for_dpi(16, 96), 16);
        assert_eq!(scale_px_for_dpi(16, 120), 20);
        assert_eq!(scale_px_for_dpi(16, 144), 24);
        assert_eq!(scale_px_for_dpi(16, 192), 32);

        let baseline = sc(16);
        {
            let _outer = DpiScope::new(144);
            assert_eq!(sc(16), 24);
            {
                let _inner = DpiScope::new(192);
                assert_eq!(sc(16), 32);
            }
            assert_eq!(sc(16), 24);
        }
        assert_eq!(sc(16), baseline);
    }

    #[test]
    fn detail_brand_icon_scales_and_keeps_its_optical_offset_across_dpis() {
        for (dpi, expected_offset) in [(96, 2), (120, 2), (144, 2), (168, 3), (192, 3)] {
            let expected_size = scale_px_for_dpi(DETAIL_BRAND_ICON_SIZE, dpi);
            let icon = detail_brand_icon(dpi).expect("detail brand icon");
            let (width, height, bits_per_pixel) = hicon_color_bitmap_metrics(icon);
            assert_eq!((width, height), (expected_size, expected_size));
            assert_eq!(bits_per_pixel, 32);
            assert_eq!(detail_brand_icon_optical_offset(dpi), expected_offset);
        }
    }

    #[test]
    fn compact_high_contrast_text_roles_match_their_backdrops() {
        let system = |index| theme::system_color(index).to_colorref();
        let resolved = |key| compact_color(key, true, true).to_colorref();

        assert_eq!(
            resolved(ColorKey::PillAlertText),
            system(COLOR_HIGHLIGHTTEXT)
        );
        assert_eq!(
            resolved(ColorKey::CanvasWarnPrimary),
            system(COLOR_WINDOWTEXT)
        );
        assert_eq!(
            resolved(ColorKey::HighContrastText),
            system(COLOR_WINDOWTEXT)
        );
        assert_eq!(resolved(ColorKey::ErrorText), system(COLOR_WINDOWTEXT));
        assert_eq!(resolved(ColorKey::StaleText), system(COLOR_WINDOWTEXT));
        assert_eq!(resolved(ColorKey::PillBgWarn), system(COLOR_HIGHLIGHT));
        assert_eq!(resolved(ColorKey::PillBg), system(COLOR_WINDOW));

        assert_ne!(
            resolved(ColorKey::PillAlertText),
            resolved(ColorKey::PillBgWarn)
        );
        assert_ne!(
            resolved(ColorKey::CanvasWarnPrimary),
            resolved(ColorKey::PillBg)
        );
        assert_ne!(resolved(ColorKey::ErrorText), resolved(ColorKey::PillBg));
    }

    #[test]
    fn suggested_dpi_rectangle_preserves_negative_monitor_coordinates() {
        let rect = RECT {
            left: -1920,
            top: -240,
            right: -1280,
            bottom: 480,
        };
        let parsed = suggested_dpi_rect(LPARAM(&rect as *const RECT as isize)).unwrap();

        assert_eq!(parsed.left, -1920);
        assert_eq!(parsed.top, -240);
        assert_eq!(parsed.right - parsed.left, 640);
        assert_eq!(parsed.bottom - parsed.top, 720);
    }

    #[test]
    fn provider_tray_tooltip_uses_one_quota_window_per_line() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(85.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(66.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(78.0, None, Some(ONE_WEEK_SECONDS)),
        ]);

        assert_eq!(
            provider_tooltip(
                "Claude Code",
                Some(&usage),
                None,
                LanguageId::English.strings(),
            ),
            "Claude Code\n5h: 85%\n1d: 66%\n7d: 78%"
        );
        assert_eq!(
            app_tooltip_provider_line(
                "Claude Code",
                Some(&usage),
                None,
                LanguageId::English.strings(),
            ),
            "Claude Code: 5h 85% · 1d 66% · 7d 78%"
        );
        assert_eq!(
            provider_tooltip(
                "Claude Code",
                Some(&usage),
                Some(ProviderStatus::AuthenticationFailed),
                LanguageId::English.strings(),
            ),
            "Claude Code · Authentication failed\n5h: 85%\n1d: 66%\n7d: 78%"
        );
    }

    #[test]
    fn native_provider_surfaces_use_product_names_not_window_titles() {
        let strings = LanguageId::SimplifiedChinese.strings();
        assert_eq!(
            localized_provider_name(tray_icon::TrayIconKind::Claude, strings),
            strings.claude_model
        );
        assert_eq!(
            localized_provider_name(tray_icon::TrayIconKind::Codex, strings),
            strings.codex_model
        );
        assert_eq!(
            localized_provider_name(tray_icon::TrayIconKind::Antigravity, strings),
            strings.antigravity_model
        );
    }

    #[test]
    fn native_tooltips_use_the_canonical_percentage_rounding() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(90.5, None, Some(FIVE_HOURS_SECONDS))]);

        assert_eq!(
            provider_tooltip(
                "Claude Code",
                Some(&usage),
                None,
                LanguageId::English.strings(),
            ),
            "Claude Code\n5h: 91%"
        );
        assert_eq!(
            app_tooltip_provider_line(
                "Claude Code",
                Some(&usage),
                None,
                LanguageId::English.strings(),
            ),
            "Claude Code: 5h 91%"
        );
    }

    #[test]
    fn widget_tooltip_lines_keep_every_reported_window() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(53.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(20.0, None, Some(ONE_WEEK_SECONDS)),
            UsageWindow::new(7.0, None, Some(24 * 60 * 60)),
        ]);
        let lines = provider_tooltip_lines(
            "Claude Code",
            usage.windows.iter(),
            LanguageId::English.strings(),
        );

        assert_eq!(lines, ["Claude Code", "5h: 53%", "1d: 7%", "7d: 20%"]);
    }

    #[test]
    fn widget_tooltip_reset_copy_is_relative_and_unwrapped() {
        let resets_at = SystemTime::now()
            .checked_add(Duration::from_secs(6 * 60 * 60 + 11 * 60))
            .unwrap();
        let text = widget_tooltip_reset_text(resets_at, LanguageId::English.strings());

        assert!(text.starts_with("Resets in "));
        assert!(!text.contains('('));
        assert!(!text.contains(')'));
    }

    #[test]
    fn widget_tooltip_position_tracks_all_taskbar_edges() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1_000,
            bottom: 700,
        };
        let size = (200, 80);

        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 400,
                    top: 700,
                    right: 500,
                    bottom: 740,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (350, 613)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 400,
                    top: -40,
                    right: 500,
                    bottom: 0,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (350, 7)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: 1_000,
                    top: 300,
                    right: 1_040,
                    bottom: 400,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (793, 310)
        );
        assert_eq!(
            widget_tooltip_position_for_anchor(
                RECT {
                    left: -40,
                    top: 300,
                    right: 0,
                    bottom: 400,
                },
                work,
                size.0,
                size.1,
                7,
            ),
            (7, 310)
        );
    }

    #[test]
    fn provider_tray_tooltip_puts_reset_details_in_parentheses() {
        let usage = UsageData::from_windows(vec![UsageWindow::new(
            85.0,
            SystemTime::now().checked_add(Duration::from_secs(23 * 60)),
            Some(FIVE_HOURS_SECONDS),
        )]);
        let tooltip = provider_tooltip(
            "Claude Code",
            Some(&usage),
            None,
            LanguageId::English.strings(),
        );
        let lines = tooltip.lines().collect::<Vec<_>>();

        assert_eq!(lines[0], "Claude Code");
        assert!(lines[1].starts_with("5h: 85% (Resets in "));
        assert!(lines[1].ends_with("))"));
    }

    #[test]
    fn tray_tooltip_truncates_at_a_complete_utf16_character() {
        let long = format!("Status {}", "😀".repeat(100));
        let tooltip = tray_tooltip_from_lines([long.as_str()]);

        assert!(tooltip.encode_utf16().count() <= TRAY_TOOLTIP_MAX_UTF16);
        assert!(tooltip.ends_with('…'));
    }

    #[test]
    fn tray_tooltip_reports_omitted_complete_lines() {
        let long_complete_line = "x".repeat(110);
        let tooltip =
            tray_tooltip_from_lines(["Gengchou", long_complete_line.as_str(), "Codex: 7d 46%"]);
        let lines = tooltip.lines().collect::<Vec<_>>();

        assert!(tooltip.encode_utf16().count() <= TRAY_TOOLTIP_MAX_UTF16);
        assert_eq!(lines[0], "Gengchou");
        assert_eq!(lines[1], long_complete_line);
        assert_eq!(lines[2], "… (+1)");
    }

    #[test]
    fn floating_drag_threshold_distinguishes_click_from_move() {
        let threshold = sc(FLOATING_DRAG_THRESHOLD);
        assert!(!floating_drag_distance_exceeded(
            threshold.saturating_sub(1),
            threshold.saturating_sub(1)
        ));
        assert!(floating_drag_distance_exceeded(threshold, 0));
        assert!(floating_drag_distance_exceeded(0, -threshold));
    }

    #[test]
    fn compact_gdi_data_font_fits_supported_dpis() {
        for dpi in PROVIDER_TILE_BUCKET_DPIS {
            let _dpi = DpiScope::new(dpi);
            unsafe {
                let hdc = GetDC(HWND::default());
                assert!(!hdc.0.is_null());
                let metrics = compact_metrics();
                assert!(
                    measure_compact_text(hdc, FontKey::Data12, "5h 100% ·4d") > 0,
                    "compact data font did not measure at {dpi} DPI"
                );
                assert!(
                    measure_compact_text(hdc, FontKey::Data12, "365d") <= metrics.label_max_w,
                    "floating label exceeds its measured slot at {dpi} DPI"
                );
                ReleaseDC(HWND::default(), hdc);
            }
        }
    }

    #[test]
    fn poll_coordinator_coalesces_requests_and_marks_old_generation_stale() {
        let coordinator = PollCoordinator::new();

        assert!(coordinator.request(false));
        let (first_generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
        assert!(coordinator.is_current(first_generation));

        assert!(!coordinator.request(false));
        assert!(!coordinator.request(true));
        assert!(!coordinator.is_current(first_generation));
        assert!(coordinator.finish_pass());

        let (latest_generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(force_claude_refresh);
        assert!(coordinator.is_current(latest_generation));
        assert!(!coordinator.finish_pass());

        assert!(coordinator.request(false));
    }

    /// Never signed in must keep parking the poll and arming the credential
    /// watch exactly like a rejected credential does. Losing this would send
    /// the widget back to polling an endpoint it has no token for, and would
    /// stop it recovering on its own when the user finally signs in.
    #[test]
    fn never_signed_in_still_pauses_polling_and_arms_the_watch() {
        let data = AppUsageData::default()
            .with_error(tray_icon::TrayIconKind::Codex, ProviderStatus::NotSignedIn);
        assert!(all_shown_providers_need_auth(
            &data,
            [false, true, false, false]
        ));
        assert!(shown_provider_needs_auth(
            &data,
            [false, true, false, false]
        ));

        let mixed = AppUsageData::default()
            .with_error(tray_icon::TrayIconKind::Claude, ProviderStatus::NotSignedIn)
            .with_error(
                tray_icon::TrayIconKind::Codex,
                ProviderStatus::AuthenticationFailed,
            );
        assert!(all_shown_providers_need_auth(
            &mixed,
            [true, true, false, false]
        ));

        // A provider that is merely rate limited is not waiting on the user.
        let transient = AppUsageData::default()
            .with_error(tray_icon::TrayIconKind::Codex, ProviderStatus::RateLimited);
        assert!(!all_shown_providers_need_auth(
            &transient,
            [false, true, false, false]
        ));
    }

    /// Only a rejected credential earns a balloon, and the flag it leaves
    /// behind decides whether a later rejection can still raise one.
    /// Every provider must reach the failure-recovery state machine, not just
    /// the ones that were wired when it was written. A provider left out of it
    /// still polls and still shows an error, but never records a rate-limit
    /// cooldown, never counts consecutive failures, never goes stale, and never
    /// raises the one authentication balloon it owes the user - all silently.
    #[test]
    fn every_provider_reaches_the_refresh_state_machine() {
        let plan = PollPassPlan {
            show_claude_code: true,
            show_codex: true,
            show_antigravity: true,
            show_grok: true,
            poll_claude_code: true,
            poll_codex: true,
            poll_antigravity: true,
            poll_grok: true,
            claude_cooldown_ms: None,
            codex_cooldown_ms: None,
            antigravity_cooldown_ms: None,
            grok_cooldown_ms: None,
        };
        let now = Instant::now();
        let mut states = ProviderRefreshStates::default();

        let mut rejected = AppUsageData::default();
        for kind in tray_icon::TrayIconKind::ALL {
            rejected.provider_mut(kind).error = Some(ProviderStatus::AuthenticationFailed);
        }

        let transitions =
            update_refresh_states_for_pass(&mut states, None, POLL_5_MIN, plan, &rejected, 0, now);
        assert_eq!(
            transitions,
            tray_icon::TrayIconKind::ALL.to_vec(),
            "a rejected credential owes every provider exactly one balloon"
        );
        for kind in tray_icon::TrayIconKind::ALL {
            assert!(
                states.for_kind(kind).auth_failure_active,
                "{kind:?} did not reach the state machine"
            );
        }

        // A rate limit must land in the same per-provider cooldown for all of
        // them, which is what keeps the next pass from re-polling immediately.
        let mut limited = AppUsageData::default();
        for kind in tray_icon::TrayIconKind::ALL {
            let slot = limited.provider_mut(kind);
            slot.error = Some(ProviderStatus::RateLimited);
            slot.retry_after_ms = Some(60_000);
        }
        let mut states = ProviderRefreshStates::default();
        update_refresh_states_for_pass(&mut states, None, POLL_5_MIN, plan, &limited, 0, now);
        for kind in tray_icon::TrayIconKind::ALL {
            assert!(
                provider_cooldown_remaining_ms(states.for_kind(kind), now).is_some(),
                "{kind:?} recorded no rate-limit cooldown"
            );
        }
    }

    #[test]
    fn never_signed_in_raises_no_balloon_but_leaves_the_next_one_possible() {
        let mut state = ProviderRefreshState::default();
        let now = Instant::now();

        let announced = update_provider_refresh_state(
            &mut state,
            "Codex",
            true,
            true,
            Some(ProviderStatus::NotSignedIn),
            false,
            None,
            POLL_5_MIN,
            0,
            now,
        );
        assert!(!announced, "never signed in must not notify");
        assert!(!state.auth_failure_active);

        // The user signs in and the credential is rejected: that is news.
        let announced = update_provider_refresh_state(
            &mut state,
            "Codex",
            true,
            true,
            Some(ProviderStatus::AuthenticationFailed),
            false,
            None,
            POLL_5_MIN,
            0,
            now,
        );
        assert!(announced, "a rejected credential must notify");
        assert!(state.auth_failure_active);

        // Still rejected on the next pass: no second balloon.
        let announced = update_provider_refresh_state(
            &mut state,
            "Codex",
            true,
            true,
            Some(ProviderStatus::AuthenticationFailed),
            false,
            None,
            POLL_5_MIN,
            0,
            now,
        );
        assert!(!announced);
    }

    /// Build the visibility projection the scope is really derived from, so a
    /// case states only which providers are on and in what state.
    fn visibility(
        shown: [bool; tray_icon::TrayIconKind::COUNT],
        allowed: [bool; tray_icon::TrayIconKind::COUNT],
    ) -> settings::ProviderVisibility {
        settings::ProviderVisibility {
            show_claude_code: shown[tray_icon::TrayIconKind::Claude.index()],
            show_codex: shown[tray_icon::TrayIconKind::Codex.index()],
            show_antigravity: shown[tray_icon::TrayIconKind::Antigravity.index()],
            show_grok: shown[tray_icon::TrayIconKind::Grok.index()],
            allow_claude_credentials: allowed[tray_icon::TrayIconKind::Claude.index()],
            allow_codex_credentials: allowed[tray_icon::TrayIconKind::Codex.index()],
            allow_antigravity_credentials: allowed[tray_icon::TrayIconKind::Antigravity.index()],
            allow_grok_credentials: allowed[tray_icon::TrayIconKind::Grok.index()],
            claude_announced: true,
            codex_announced: true,
            antigravity_announced: true,
            grok_announced: true,
            ..Default::default()
        }
    }

    fn poll_scope(
        shown: [bool; tray_icon::TrayIconKind::COUNT],
        allowed: [bool; tray_icon::TrayIconKind::COUNT],
    ) -> [bool; tray_icon::TrayIconKind::COUNT] {
        credential_read_scope_for(
            &visibility(shown, allowed),
            true,
            CredentialReadReason::Poll,
        )
        .flags()
    }

    #[test]
    fn the_poll_scope_requires_visibility_and_explicit_consent() {
        assert_eq!(
            poll_scope([true, true, true, true], [false, false, false, false]),
            [false, false, false, false]
        );
        assert_eq!(
            poll_scope([false, false, false, false], [true, true, true, true]),
            [false, false, false, false]
        );
        assert_eq!(
            poll_scope([true, false, true, true], [true, true, false, true]),
            [true, false, false, true]
        );
        // Global consent gates everything, whatever the per-provider state.
        assert_eq!(
            credential_read_scope_for(
                &visibility([true, true, true, true], [true, true, true, true]),
                false,
                CredentialReadReason::Poll,
            )
            .flags(),
            [false, false, false, false]
        );
    }

    /// The worker gate used to read the selection as `.0 || .1 || .2`, so a
    /// profile whose only usable provider was the fourth one never started a
    /// poll at all - a fresh install that detects Grok alone lands exactly
    /// there. Asserted on the scope the running app builds, not on a test-only
    /// copy of it: the copy stayed correct while the gate was wrong.
    #[test]
    fn a_profile_whose_only_usable_provider_is_last_still_polls() {
        let grok_only = poll_scope([false, false, false, true], [false, false, false, true]);
        assert_eq!(grok_only, [false, false, false, true]);
        assert!(poll_selection_has_target(grok_only));
        assert!(!poll_selection_has_target(poll_scope(
            [false, false, false, false],
            [true, true, true, true]
        )));
    }

    /// Each provider's five flags must reach the predicate for that same
    /// provider. A test that only pins `credential_read_allowed` cannot see a
    /// crossed field here, which is where this project's worst defects live.
    #[test]
    fn every_provider_reads_its_own_flags() {
        for kind in tray_icon::TrayIconKind::ALL {
            let mut shown = [false; tray_icon::TrayIconKind::COUNT];
            shown[kind.index()] = true;
            let mut expected = [false; tray_icon::TrayIconKind::COUNT];
            expected[kind.index()] = true;
            let only_this_one = visibility(shown, shown);
            for reason in [
                CredentialReadReason::Poll,
                CredentialReadReason::CredentialWatch,
                CredentialReadReason::Manual,
            ] {
                assert_eq!(
                    credential_read_scope_for(&only_this_one, true, reason).flags(),
                    expected,
                    "{kind:?} under {reason:?}"
                );
            }

            // A revocation must silence exactly the provider it names.
            let all_on = [true; tray_icon::TrayIconKind::COUNT];
            let mut revoked_one = visibility(all_on, all_on);
            match kind {
                tray_icon::TrayIconKind::Claude => {
                    revoked_one.claude_credential_access_revoked = true
                }
                tray_icon::TrayIconKind::Codex => {
                    revoked_one.codex_credential_access_revoked = true
                }
                tray_icon::TrayIconKind::Antigravity => {
                    revoked_one.antigravity_credential_access_revoked = true
                }
                tray_icon::TrayIconKind::Grok => revoked_one.grok_credential_access_revoked = true,
            }
            let mut expected_after_revocation = all_on;
            expected_after_revocation[kind.index()] = false;
            assert_eq!(
                credential_read_scope_for(&revoked_one, true, CredentialReadReason::Poll).flags(),
                expected_after_revocation,
                "revoking {kind:?} must not silence anyone else"
            );

            // Same for a pending review, which is a different field.
            let mut pending_one = visibility(all_on, all_on);
            match kind {
                tray_icon::TrayIconKind::Claude => {
                    pending_one.claude_credential_access_pending = true
                }
                tray_icon::TrayIconKind::Codex => {
                    pending_one.codex_credential_access_pending = true
                }
                tray_icon::TrayIconKind::Antigravity => {
                    pending_one.antigravity_credential_access_pending = true
                }
                tray_icon::TrayIconKind::Grok => pending_one.grok_credential_access_pending = true,
            }
            assert_eq!(
                credential_read_scope_for(&pending_one, true, CredentialReadReason::Poll).flags(),
                expected_after_revocation,
                "a pending {kind:?} must not silence anyone else"
            );
        }
    }

    #[test]
    fn a_narrowed_credential_watch_polls_once_and_does_not_repeat() {
        // Same watched set, a credential changed: that is what the watch is for.
        assert_eq!(
            credential_watch_outcome(true, true),
            CredentialWatchOutcome::Repoll
        );
        assert_eq!(
            credential_watch_outcome(true, false),
            CredentialWatchOutcome::Unchanged
        );
        // The watched set changed, so the snapshots are not comparable. A
        // provider that stayed in the set can have been refreshed in the same
        // interval, so this must not be swallowed; the repeat it used to cause
        // came from leaving `auth_watch_mode` stale, not from the poll.
        assert_eq!(
            credential_watch_outcome(false, true),
            CredentialWatchOutcome::Repoll
        );
        assert_eq!(
            credential_watch_outcome(false, false),
            CredentialWatchOutcome::Repoll
        );
    }

    /// The watchdog relaunch is the only child that inherits this process's
    /// whole environment, so an update transaction's readiness marker has to be
    /// cleared on its command. A relaunched process that inherited it would
    /// treat a finished transaction as an active one and refuse to start - the
    /// shape of the v2.4.0 startup failure. This used to be handled by removing
    /// the variable from the running process, which Rust 2024 turns into an
    /// `unsafe` operation because worker threads are already reading the
    /// environment by then.
    #[test]
    fn a_relaunch_does_not_inherit_an_update_readiness_marker() {
        let command = relaunch_command(
            std::path::Path::new(r"C:\gengchou.exe"),
            &["--flag".to_string()],
            1_700_000_000,
        );
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.contains(&(updater::UPDATE_READY_ENV.to_string(), None)),
            "the readiness marker must be removed, got {envs:?}"
        );
        assert!(envs.contains(&(ENV_RELAUNCH.to_string(), Some("1".to_string()))));
        assert!(envs
            .iter()
            .any(|(key, value)| key == ENV_LAST_RELAUNCH_UNIX && value.is_some()));
    }

    /// The version entry should state what is known, not offer a chore. Before
    /// the last result was persisted it reset to "check for updates" on every
    /// launch, even though the answer from yesterday was still valid.
    #[test]
    fn the_last_update_result_survives_a_restart() {
        assert!(matches!(remembered_update_status(None), UpdateStatus::Idle));
        assert!(matches!(
            remembered_update_status(Some(&settings::LastUpdateOutcome::UpToDate)),
            UpdateStatus::UpToDate
        ));
        let remembered = remembered_update_status(Some(&settings::LastUpdateOutcome::Available {
            version: "9.9.9".to_string(),
        }));
        match &remembered {
            UpdateStatus::AvailableRemembered { version } => assert_eq!(version, "9.9.9"),
            other => panic!("expected a remembered update, got {other:?}"),
        }
    }

    /// A remembered update must be indistinguishable from a freshly found one
    /// in the menu; only the click path differs.
    #[test]
    fn a_remembered_update_is_labelled_like_a_fresh_one() {
        let strings = LanguageId::English.strings();
        let remembered = version_action_label(
            strings,
            LanguageId::English,
            InstallChannel::Portable,
            &UpdateStatus::AvailableRemembered {
                version: "9.9.9".to_string(),
            },
        );

        if updater::update_channel_configured() {
            assert!(remembered.contains("9.9.9"), "{remembered}");
            assert!(remembered.contains(strings.update_to), "{remembered}");
            assert!(
                !remembered.contains(strings.check_for_updates),
                "a known update must not read as an unanswered question: {remembered}"
            );
        } else {
            // Without a release channel the entry is only ever the version.
            assert_eq!(remembered, format!("v{}", env!("CARGO_PKG_VERSION")));
        }
    }

    /// Declining the one-time prompt must read nothing, no matter what the
    /// per-provider switches say - including the switches an existing install
    /// carried over.
    #[test]
    fn credential_access_needs_both_the_global_and_the_per_provider_switch() {
        assert!(credential_access_granted(true, true));
        assert!(!credential_access_granted(true, false));
        assert!(!credential_access_granted(false, true));
        assert!(!credential_access_granted(false, false));
    }

    /// A provider left visible without access would otherwise sit on an
    /// unexplained placeholder with no route back.
    #[test]
    fn revoked_access_is_reported_only_for_visible_providers() {
        assert!(access_is_revoked(true, true, false));
        assert!(!access_is_revoked(true, true, true));
        assert!(!access_is_revoked(false, true, false));
        // The prompt is still on screen: nothing has been declined yet.
        assert!(!access_is_revoked(true, false, false));
    }

    #[test]
    fn credential_consent_dialog_is_minimizable_and_defaults_to_no() {
        let flags = credential_consent_task_dialog_flags();
        let buttons = credential_consent_task_dialog_buttons();
        let fallback_style = credential_consent_fallback_message_box_style();

        assert_ne!(flags.0 & TDF_CAN_BE_MINIMIZED.0, 0);
        assert_ne!(flags.0 & TDF_ALLOW_DIALOG_CANCELLATION.0, 0);
        assert_ne!(buttons.0 & TDCBF_YES_BUTTON.0, 0);
        assert_ne!(buttons.0 & TDCBF_NO_BUTTON.0, 0);
        assert_eq!(credential_consent_default_button(), IDNO.0);
        assert_ne!(fallback_style.0 & MB_DEFBUTTON2.0, 0);
        assert_ne!(fallback_style.0 & MB_SETFOREGROUND.0, 0);
    }

    #[test]
    fn pending_review_buttons_map_to_their_answers() {
        assert_eq!(pending_access_answer(IDYES.0), Some(true));
        assert_eq!(pending_access_answer(IDNO.0), Some(false));
        assert_eq!(pending_access_answer(IDCANCEL.0), None);
        // Esc, the title-bar close button and an id we never registered all
        // land here; none of them may answer for the user.
        assert_eq!(pending_access_answer(0), None);
    }

    #[test]
    fn pending_review_defaults_to_deciding_later() {
        // The regression this guards: with "keep closed" as the default, one
        // Enter recorded a revocation the user never chose, and the only way
        // to stay pending was the title-bar close button.
        assert_eq!(
            pending_access_answer(PENDING_ACCESS_DEFAULT_BUTTON),
            None,
            "the default button must change nothing"
        );
        assert_eq!(PENDING_ACCESS_DEFAULT_BUTTON, IDCANCEL.0);
    }

    #[test]
    fn every_language_offers_the_three_pending_review_answers() {
        for language in LanguageId::ALL {
            let strings = language.strings();
            for (label, name) in [
                (strings.access_allow, "access_allow"),
                (strings.access_keep_closed, "access_keep_closed"),
                (strings.access_decide_later, "access_decide_later"),
            ] {
                assert!(
                    !label.trim().is_empty(),
                    "missing {name} for {}",
                    language.code()
                );
            }
            assert_ne!(strings.access_decide_later, strings.access_keep_closed);
            assert_ne!(strings.access_decide_later, strings.access_allow);
        }
    }

    #[test]
    fn shown_provider_needs_auth_spots_a_signed_out_provider_beside_healthy_ones() {
        // The regression: a healthy provider keeps the poll "successful", so
        // the signed-out one must still be noticed - otherwise its "sign in"
        // marker survives until the next poll interval (up to an hour).
        let claude_rejected = AppUsageData::default().with_error(
            tray_icon::TrayIconKind::Claude,
            ProviderStatus::AuthenticationFailed,
        );
        let codex_rejected = AppUsageData::default().with_error(
            tray_icon::TrayIconKind::Codex,
            ProviderStatus::AuthenticationFailed,
        );
        assert!(shown_provider_needs_auth(
            &claude_rejected,
            [true, true, false, false]
        ));
        assert!(shown_provider_needs_auth(
            &codex_rejected,
            [true, true, false, false]
        ));
        // A provider the user turned off must not arm a watch...
        assert!(!shown_provider_needs_auth(
            &claude_rejected,
            [false, true, false, false]
        ));
        // ...and transient failures are not something signing in would fix.
        for status in [ProviderStatus::RateLimited, ProviderStatus::RequestFailed] {
            let transient =
                AppUsageData::default().with_error(tray_icon::TrayIconKind::Claude, status);
            assert!(!shown_provider_needs_auth(
                &transient,
                [true, false, false, false]
            ));
        }
        assert!(!shown_provider_needs_auth(
            &AppUsageData::default(),
            [true, true, true, true]
        ));

        // The fourth provider goes through the same predicate rather than a
        // branch that stops at the third.
        let grok_rejected = AppUsageData::default().with_error(
            tray_icon::TrayIconKind::Grok,
            ProviderStatus::AuthenticationFailed,
        );
        assert!(shown_provider_needs_auth(
            &grok_rejected,
            [true, true, true, true]
        ));
        assert!(!shown_provider_needs_auth(
            &grok_rejected,
            [true, true, true, false]
        ));
        assert!(all_shown_providers_need_auth(
            &grok_rejected,
            [false, false, false, true]
        ));
    }

    #[test]
    fn credential_watch_mode_for_shown_is_sampleable_before_a_poll() {
        use poller::CredentialWatchMode;

        // Derived from what is shown (not from what failed) so the same mode
        // can be sampled either side of a poll and the snapshots compared.
        assert_eq!(
            credential_watch_mode_for_shown(true, true, false, false),
            Some(CredentialWatchMode::Providers(watched([
                true, true, false, false
            ])))
        );
        assert_eq!(
            credential_watch_mode_for_shown(true, false, false, false),
            Some(CredentialWatchMode::ClaudeSources)
        );
        assert_eq!(
            credential_watch_mode_for_shown(false, true, false, false),
            Some(CredentialWatchMode::Codex)
        );
        assert_eq!(
            credential_watch_mode_for_shown(false, false, true, false),
            Some(CredentialWatchMode::Antigravity)
        );
        assert_eq!(
            credential_watch_mode_for_shown(false, false, false, true),
            Some(CredentialWatchMode::Grok)
        );
        assert_eq!(
            credential_watch_mode_for_shown(false, false, false, false),
            None
        );
    }

    #[test]
    fn auth_watch_polls_again_when_credentials_change_while_a_poll_is_in_flight() {
        let before: poller::CredentialWatchSnapshot = vec!["win:claude|present|10|1".to_string()];
        let after: poller::CredentialWatchSnapshot = vec!["win:claude|present|10|2".to_string()];

        // Steady state: the credentials the poll used are still on disk, so
        // watching from this baseline will see the next sign-in.
        assert_eq!(
            auth_watch_decision(true, Some(&before), Some(&before)),
            AuthWatchDecision::Watch
        );

        // The race: the token was refreshed between the poll reading it and
        // the result being handled. The post-poll baseline is already the
        // refreshed signature, so a plain watch would compare it against
        // itself and never fire - poll again instead.
        assert_eq!(
            auth_watch_decision(true, Some(&before), Some(&after)),
            AuthWatchDecision::WatchAndPollNow
        );

        // Nothing needs auth: stop watching regardless of any change.
        assert_eq!(
            auth_watch_decision(false, Some(&before), Some(&after)),
            AuthWatchDecision::Stop
        );

        // Without both samples there is nothing to compare; watch normally.
        assert_eq!(
            auth_watch_decision(true, None, Some(&after)),
            AuthWatchDecision::Watch
        );
    }

    #[test]
    fn paused_auth_watch_rechecks_remote_rejections_at_the_deadline() {
        let now = Instant::now();
        let deadline = auth_recovery_recheck_deadline(true, POLL_30_MIN, now)
            .expect("remote auth rejection should get a service recheck deadline");

        assert_eq!(
            poll_timer_action(true, Some(deadline), deadline - Duration::from_millis(1)),
            PollTimerAction::CheckCredentials
        );
        assert_eq!(
            poll_timer_action(true, Some(deadline), deadline),
            PollTimerAction::PollNow
        );
        assert_eq!(
            paused_poll_timer_interval_ms(POLL_30_MIN, Some(deadline), now),
            poller::AUTH_REJECTION_RECHECK_MS
        );
        assert_eq!(
            paused_poll_timer_interval_ms(POLL_1_MIN, Some(deadline), now),
            POLL_1_MIN
        );
        let ten_minutes_later = now + Duration::from_secs(10 * 60);
        assert_eq!(
            paused_poll_timer_interval_ms(POLL_30_MIN, Some(deadline), ten_minutes_later),
            POLL_5_MIN
        );
    }

    #[test]
    fn paused_local_credential_failures_recheck_at_the_normal_interval() {
        let now = Instant::now();
        let deadline = auth_recovery_recheck_deadline(false, POLL_30_MIN, now)
            .expect("local credential failures need a bounded fallback recheck");
        assert_eq!(
            poll_timer_action(true, Some(deadline), now),
            PollTimerAction::CheckCredentials
        );
        assert_eq!(
            paused_poll_timer_interval_ms(POLL_30_MIN, Some(deadline), now),
            POLL_30_MIN
        );
        assert_eq!(
            poll_timer_action(true, Some(deadline), deadline),
            PollTimerAction::PollNow
        );
        assert_eq!(
            poll_timer_action(false, None, now),
            PollTimerAction::PollNow
        );
    }

    #[test]
    fn mixed_credential_failures_recheck_any_remote_rejection() {
        let data = {
            let mut data = AppUsageData::default()
                .with_error(
                    tray_icon::TrayIconKind::Claude,
                    ProviderStatus::AuthenticationFailed,
                )
                .with_error(
                    tray_icon::TrayIconKind::Codex,
                    ProviderStatus::AuthenticationFailed,
                );
            data.remote_auth_rejection = true;
            data
        };

        assert!(poll_failure_needs_auth_rejection_recheck(
            poller::PollError::NoCredentials,
            &data,
        ));
        assert!(!poll_failure_needs_auth_rejection_recheck(
            poller::PollError::NoCredentials,
            &{
                let mut data = AppUsageData::default();
                data.remote_auth_rejection = false;
                data
            },
        ));
    }

    #[test]
    fn poll_coordinator_releases_worker_when_no_request_is_pending() {
        let coordinator = PollCoordinator::new();

        assert!(coordinator.request(false));
        let (generation, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
        assert!(coordinator.is_current(generation));
        assert!(!coordinator.finish_pass());
        assert!(coordinator.request(false));
    }

    #[test]
    fn poll_coordinator_invalidation_discards_active_and_pending_work() {
        let coordinator = PollCoordinator::new();
        assert!(coordinator.request(false));
        let (generation, _) = coordinator.begin_pass();
        assert!(!coordinator.request(true));

        coordinator.invalidate_pending();

        assert!(!coordinator.is_current(generation));
        assert!(!coordinator.finish_pass());
        assert!(coordinator.request(false));
        let (_, force_claude_refresh) = coordinator.begin_pass();
        assert!(!force_claude_refresh);
    }

    #[test]
    fn watchdog_uses_the_live_parent_binding_not_a_transient_enumeration() {
        assert!(!watchdog_needs_taskbar_recovery(true, true, true));
        assert!(!watchdog_needs_taskbar_recovery(true, true, false));
        assert!(watchdog_needs_taskbar_recovery(true, false, true));
        assert!(watchdog_needs_taskbar_recovery(false, false, true));
        assert!(!watchdog_needs_taskbar_recovery(false, false, false));
    }

    /// A pass samples its scope before the worker starts, so a revocation
    /// landing while it runs must not be undone by the stale result:
    /// `mask_detection` reapplies the current `credential_read_scope` before
    /// the findings are consumed.
    #[test]
    fn a_revocation_during_a_detection_pass_survives_the_result() {
        let found_everything = poller::DetectedProviders {
            claude: true,
            codex: true,
            antigravity: true,
            grok: true,
        };
        let claude_revoked_meanwhile = poller::DetectionScope {
            claude: false,
            codex: true,
            antigravity: true,
            grok: true,
        };

        let applied = mask_detection(found_everything, claude_revoked_meanwhile);

        assert!(!applied.claude);
        assert!(applied.codex && applied.antigravity && applied.grok);
    }

    #[test]
    fn pending_and_revoked_providers_are_excluded_from_every_silent_read() {
        let pending = credential_read_allowed(
            true,
            false,
            false,
            true,
            true,
            true,
            CredentialReadReason::Rescan,
        );
        let revoked = credential_read_allowed(
            true,
            false,
            true,
            false,
            true,
            true,
            CredentialReadReason::Rescan,
        );
        let allowed_hidden = credential_read_allowed(
            true,
            true,
            false,
            false,
            false,
            false,
            CredentialReadReason::Rescan,
        );
        assert!(!pending);
        assert!(!revoked);
        assert!(allowed_hidden);

        assert!(!credential_read_allowed(
            true,
            false,
            false,
            true,
            true,
            true,
            CredentialReadReason::FirstRun
        ));
        assert!(!credential_read_allowed(
            true,
            false,
            false,
            true,
            true,
            true,
            CredentialReadReason::Manual
        ));
        assert!(!credential_read_allowed(
            true,
            true,
            false,
            true,
            true,
            true,
            CredentialReadReason::Poll
        ));
        assert!(!credential_read_allowed(
            true,
            true,
            false,
            true,
            true,
            true,
            CredentialReadReason::CredentialWatch
        ));
        assert!(!credential_read_allowed(
            true,
            false,
            true,
            false,
            true,
            true,
            CredentialReadReason::Poll
        ));
        assert!(!credential_read_allowed(
            false,
            true,
            false,
            false,
            true,
            true,
            CredentialReadReason::Poll
        ));
    }

    #[test]
    fn rescan_only_reads_unshown_unannounced_providers() {
        assert!(!credential_read_allowed(
            true,
            true,
            false,
            false,
            true,
            false,
            CredentialReadReason::Rescan
        ));
        assert!(!credential_read_allowed(
            true,
            true,
            false,
            false,
            false,
            true,
            CredentialReadReason::Rescan
        ));
        assert!(credential_read_allowed(
            true,
            true,
            false,
            false,
            false,
            false,
            CredentialReadReason::Rescan
        ));
    }

    #[test]
    fn manual_reads_only_explicitly_allowed_providers() {
        assert!(credential_read_allowed(
            true,
            true,
            false,
            false,
            false,
            true,
            CredentialReadReason::Manual
        ));
        assert!(!credential_read_allowed(
            true,
            false,
            false,
            true,
            false,
            true,
            CredentialReadReason::Manual
        ));
        assert!(!credential_read_allowed(
            true,
            false,
            false,
            false,
            false,
            false,
            CredentialReadReason::Manual
        ));
    }

    #[test]
    fn poll_and_watch_require_shown_allow_and_no_pending() {
        assert!(credential_read_allowed(
            true,
            true,
            false,
            false,
            true,
            true,
            CredentialReadReason::Poll
        ));
        assert!(!credential_read_allowed(
            true,
            true,
            false,
            false,
            false,
            true,
            CredentialReadReason::Poll
        ));
        assert!(!credential_read_allowed(
            true,
            false,
            false,
            false,
            true,
            true,
            CredentialReadReason::Poll
        ));
        assert_eq!(
            credential_read_allowed(
                true,
                true,
                false,
                false,
                true,
                true,
                CredentialReadReason::Poll
            ),
            credential_read_allowed(
                true,
                true,
                false,
                false,
                true,
                true,
                CredentialReadReason::CredentialWatch
            )
        );
    }

    #[test]
    fn merge_drops_providers_outside_the_current_poll_scope() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let previous = AppUsageData::default()
            .with_usage(tray_icon::TrayIconKind::Claude, usage.clone())
            .with_usage(tray_icon::TrayIconKind::Codex, usage.clone());
        let next = AppUsageData::default()
            .with_usage(tray_icon::TrayIconKind::Claude, usage.clone())
            .with_usage(tray_icon::TrayIconKind::Codex, usage);
        let merged =
            merge_missing_provider_data(Some(&previous), next, [true, false, false, false]);
        assert!(merged.usage(tray_icon::TrayIconKind::Claude).is_some());
        assert!(merged.usage(tray_icon::TrayIconKind::Codex).is_none());
    }

    fn found_everything() -> poller::DetectedProviders {
        poller::DetectedProviders {
            claude: true,
            codex: true,
            antigravity: true,
            grok: true,
        }
    }

    /// Everything in scope except Claude, which the user just revoked or left
    /// pending while the pass was out reading credentials.
    fn scope_without_claude() -> poller::DetectionScope {
        poller::DetectionScope {
            claude: false,
            codex: true,
            antigravity: true,
            grok: true,
        }
    }

    /// The race this guards: the user revokes a provider while a detection
    /// worker is still out - a WSL probe alone can take seconds - and the
    /// worker then reports that provider as found. Asserted on
    /// `resolve_detection`, which is what the running app calls, rather than
    /// on `mask_detection`, which is only the helper it uses: a revert that
    /// dropped the mask from the decision would leave the helper untouched.
    #[test]
    fn a_result_that_arrives_after_a_revocation_cannot_reenable_a_provider() {
        // Manual is the dangerous one: it assigns visibility from what it
        // found, so an unmasked result puts the provider back on screen and
        // back in the poll.
        let before = settings::ProviderVisibility {
            show_codex: true,
            allow_codex_credentials: true,
            codex_announced: true,
            claude_announced: true,
            ..Default::default()
        };
        let outcome = resolve_detection(
            DetectionReason::Manual,
            found_everything(),
            before,
            scope_without_claude(),
        );
        assert!(
            !outcome.after.show_claude_code,
            "a revoked provider must not be shown again by a stale result"
        );
        assert!(
            !outcome.after.allow_claude_credentials,
            "and it must not regain credential access"
        );
        assert!(outcome.after.show_antigravity && outcome.after.show_grok);
    }

    /// Same for the periodic sweep, where the damage would be a balloon for a
    /// provider the user just closed rather than a re-enable.
    #[test]
    fn a_revoked_provider_is_not_announced_by_a_stale_sweep() {
        let before = settings::ProviderVisibility {
            claude_announced: false,
            codex_announced: true,
            antigravity_announced: true,
            grok_announced: true,
            ..Default::default()
        };
        let outcome = resolve_detection(
            DetectionReason::Rescan,
            found_everything(),
            before,
            scope_without_claude(),
        );
        assert!(
            !outcome
                .announcements
                .contains(&tray_icon::TrayIconKind::Claude),
            "a provider outside the scope owes no balloon"
        );
        assert!(!outcome.after.claude_announced);
    }

    /// First run assigns from what it found, so an out-of-scope provider must
    /// not be assigned either - and the pass must still report no change when
    /// the scope leaves nothing to apply.
    #[test]
    fn first_run_assigns_only_what_the_scope_allows() {
        let outcome = resolve_detection(
            DetectionReason::FirstRun,
            found_everything(),
            settings::ProviderVisibility::default(),
            scope_without_claude(),
        );
        assert!(!outcome.after.show_claude_code && !outcome.after.allow_claude_credentials);
        assert!(outcome.after.show_codex && outcome.after.allow_codex_credentials);
        assert!(outcome.changed);

        let nothing_in_scope = resolve_detection(
            DetectionReason::Manual,
            found_everything(),
            settings::ProviderVisibility::default(),
            poller::DetectionScope::default(),
        );
        assert!(!nothing_in_scope.changed);
        assert!(nothing_in_scope.announcements.is_empty());
    }

    /// Build a watch set from the fixed Claude/Codex/Antigravity/Grok order
    /// these cases read in, so a test states only which providers are on.
    fn watched(
        order: [bool; tray_icon::TrayIconKind::COUNT],
    ) -> [bool; tray_icon::TrayIconKind::COUNT] {
        let mut set = [false; tray_icon::TrayIconKind::COUNT];
        for (slot, kind) in order.into_iter().zip(tray_icon::TrayIconKind::ALL) {
            set[kind.index()] = slot;
        }
        set
    }

    #[test]
    fn credential_watch_mode_tracks_the_only_enabled_provider() {
        assert_eq!(
            credential_watch_mode_for_shown(false, true, false, false),
            Some(poller::CredentialWatchMode::Codex)
        );
        assert_eq!(
            credential_watch_mode_for_shown(false, false, true, false),
            Some(poller::CredentialWatchMode::Antigravity)
        );
        // Claude credentials can live in Windows or any WSL distribution, so
        // every source remains watched even when one file supplied the poll.
        assert_eq!(
            credential_watch_mode_for_shown(true, false, false, false),
            Some(poller::CredentialWatchMode::ClaudeSources)
        );
        assert!(poll_error_needs_credential_watch(
            poller::PollError::NoCredentials
        ));
    }

    #[test]
    fn credential_watch_mode_names_every_provider_of_a_combined_auth_failure() {
        assert_eq!(
            credential_watch_mode_for_shown(true, true, true, false),
            Some(poller::CredentialWatchMode::Providers(watched([
                true, true, true, false
            ])))
        );
        // A provider the caller excluded is not watched either, so its
        // credential is never re-read on the watch's schedule.
        assert_eq!(
            credential_watch_mode_for_shown(true, true, false, false),
            Some(poller::CredentialWatchMode::Providers(watched([
                true, true, false, false
            ])))
        );
        assert!(poll_error_needs_credential_watch(
            poller::PollError::AuthRequired
        ));
        // A network blip is not something signing in would fix.
        assert!(!poll_error_needs_credential_watch(
            poller::PollError::RequestFailed
        ));
    }

    #[test]
    fn failure_branch_polls_again_when_credentials_change_while_a_poll_is_in_flight() {
        let before: poller::CredentialWatchSnapshot = vec!["win:claude|present|10|1".to_string()];
        let after: poller::CredentialWatchSnapshot = vec!["win:claude|present|10|2".to_string()];

        // The every-provider-failed branch pauses polling, so the watch is
        // its only way back. Drive that branch's inputs the way do_poll does:
        // the error decides whether to watch, and the pre/post samples decide
        // whether this verdict is already stale.
        for error in [
            poller::PollError::AuthRequired,
            poller::PollError::NoCredentials,
        ] {
            let needs_watch = poll_error_needs_credential_watch(error);

            // Steady state: pause and watch from this baseline.
            assert_eq!(
                auth_watch_decision(needs_watch, Some(&before), Some(&before)),
                AuthWatchDecision::Watch,
                "{error:?}"
            );

            // Refreshed while the poll was in flight: pausing on this verdict
            // would strand the widget until the next interval, because the
            // baseline already holds the refreshed signature.
            assert_eq!(
                auth_watch_decision(needs_watch, Some(&before), Some(&after)),
                AuthWatchDecision::WatchAndPollNow,
                "{error:?}"
            );
        }

        // Transient failures neither pause nor watch, however the
        // credentials moved.
        let needs_watch = poll_error_needs_credential_watch(poller::PollError::RequestFailed);
        assert_eq!(
            auth_watch_decision(needs_watch, Some(&before), Some(&after)),
            AuthWatchDecision::Stop
        );
    }

    #[test]
    fn visible_reorder_preserves_hidden_provider_slot() {
        let full = vec![
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let visible = vec![
            tray_icon::TrayIconKind::Antigravity,
            tray_icon::TrayIconKind::Claude,
        ];

        assert_eq!(
            merge_visible_provider_order(&full, &visible),
            vec![
                tray_icon::TrayIconKind::Antigravity,
                tray_icon::TrayIconKind::Codex,
                tray_icon::TrayIconKind::Claude,
            ]
        );
    }

    #[test]
    fn detail_tray_and_auth_priority_follow_the_user_provider_order() {
        let order = vec![
            tray_icon::TrayIconKind::Antigravity,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Claude,
        ];
        // Grok stays hidden here so the assertion is about the user's order
        // being respected, not about a hidden provider being appended.
        assert_eq!(shown_provider_order(&order, true, true, true, false), order);
        assert_eq!(
            tray_surface_provider_order(&order, true, true, true, false),
            order
        );

        let data = AppUsageData::default()
            .with_error(
                tray_icon::TrayIconKind::Claude,
                ProviderStatus::AuthenticationFailed,
            )
            .with_error(
                tray_icon::TrayIconKind::Codex,
                ProviderStatus::AuthenticationFailed,
            );
        assert_eq!(
            first_provider_credential_issue(Some(&data), None, &order, true, true, false, false),
            Some((
                tray_icon::TrayIconKind::Codex,
                ProviderStatus::AuthenticationFailed
            ))
        );
    }

    #[test]
    fn final_enabled_provider_is_visibly_disabled_in_the_menu() {
        let only = provider_menu_item_flags(true, true).0;
        assert_ne!(only & MF_CHECKED.0, 0);
        assert_ne!(only & MF_GRAYED.0, 0);

        let one_of_many = provider_menu_item_flags(true, false).0;
        assert_ne!(one_of_many & MF_CHECKED.0, 0);
        assert_eq!(one_of_many & MF_GRAYED.0, 0);

        let disabled_provider = provider_menu_item_flags(false, true).0;
        assert_eq!(disabled_provider & MF_GRAYED.0, 0);
    }

    #[test]
    fn provider_order_requires_a_fast_stable_confirmation() {
        let current = vec![
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let candidate = vec![
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Antigravity,
        ];
        let mut pending = None;
        let mut samples = 0;

        assert_eq!(
            observe_provider_order_candidate(&current, &candidate, &mut pending, &mut samples),
            ProviderOrderObservation::Pending
        );
        assert_eq!(samples, 1);
        assert_eq!(pending.as_deref(), Some(candidate.as_slice()));
        assert_eq!(
            observe_provider_order_candidate(&current, &candidate, &mut pending, &mut samples),
            ProviderOrderObservation::Apply
        );
        assert_eq!(samples, 0);
        assert!(pending.is_none());
    }

    fn window(resets_at: SystemTime) -> UsageWindow {
        UsageWindow::new(0.0, Some(resets_at), Some(FIVE_HOURS_SECONDS))
    }

    #[test]
    fn reset_window_refreshed_requires_elapsed_and_advanced_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_sub(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(reset_window_refreshed(
            &window(previous_reset),
            &window(next_reset)
        ));
    }

    #[test]
    fn reset_window_refreshed_ignores_predicted_future_reset() {
        let now = SystemTime::now();
        let previous_reset = now.checked_add(Duration::from_secs(60)).unwrap();
        let next_reset = now.checked_add(Duration::from_secs(5 * 60 * 60)).unwrap();

        assert!(!reset_window_refreshed(
            &window(previous_reset),
            &window(next_reset)
        ));
    }

    #[test]
    fn weekly_only_codex_usage_feeds_the_tray_percent() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(1.0, None, Some(ONE_WEEK_SECONDS))]);
        let widget = provider_widget_from_usage(Some(&usage));

        assert_eq!(widget.windows.len(), 1);
        assert_eq!(widget.windows[0].percent, Some(1.0));
    }

    #[test]
    fn widget_puts_the_shared_headline_before_the_secondary_window() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(91.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(92.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(10.0, None, Some(ONE_WEEK_SECONDS)),
        ]);
        let widget = provider_widget_from_usage(Some(&usage));

        assert_eq!(widget.windows.len(), 2);
        assert_eq!(widget.windows[0].percent, Some(92.0));
        assert_eq!(widget.windows[1].percent, Some(91.0));
    }

    #[test]
    fn quota_window_labels_stay_compact_in_every_ui_language() {
        let five_hours = UsageWindow::new(0.0, None, Some(FIVE_HOURS_SECONDS));
        let seven_days = UsageWindow::new(0.0, None, Some(ONE_WEEK_SECONDS));
        let thirty_days = UsageWindow::new(0.0, None, Some(30 * 24 * 60 * 60));
        let thirty_minutes = UsageWindow::new(0.0, None, Some(30 * 60));
        let unknown =
            UsageWindow::new(0.0, None, None).with_source_label(Some("Primary".to_string()));
        let strings = LanguageId::Korean.strings();

        assert_eq!(
            compact_view::compact_usage_window_label(&five_hours, strings),
            "5h"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&seven_days, strings),
            "7d"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&thirty_days, strings),
            "30d"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&thirty_minutes, strings),
            "30m"
        );
        assert_eq!(
            compact_view::compact_usage_window_label(&unknown, strings),
            "Primary"
        );
        assert_eq!(usage_window_label(&five_hours, strings), "5h");
        assert_eq!(usage_window_label(&thirty_days, strings), "30d");
        assert_eq!(usage_window_label(&unknown, strings), "Primary");
    }

    #[test]
    fn detail_popup_keeps_all_provider_windows() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(10.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(20.0, None, Some(24 * 60 * 60)),
            UsageWindow::new(30.0, None, Some(ONE_WEEK_SECONDS)),
        ]);
        let group = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            Some(&usage),
            None,
            false,
            false,
            LanguageId::English.strings(),
        );

        assert_eq!(group.rows.len(), 3);
        assert_eq!(group.rows[0].window_label, "5h");
        assert_eq!(group.rows[1].window_label, "1d");
        assert_eq!(group.rows[2].window_label, "7d");
    }

    #[test]
    fn detail_percent_rounding_and_warning_share_one_value() {
        for (percent, displayed, warns) in [
            (-1.0, 0, false),
            (89.4, 89, false),
            (89.6, 90, true),
            (100.4, 100, true),
        ] {
            assert_eq!(compact_view::display_percent(percent), displayed);
            let window = UsageWindow::new(percent, None, Some(FIVE_HOURS_SECONDS));
            let row = detail_usage_row(
                "5h".to_string(),
                Some(&window),
                5,
                LanguageId::English.strings(),
            );
            assert_eq!(row.warn, warns);
        }
    }

    #[test]
    fn detail_muted_tone_covers_missing_and_displayed_zero_percentages() {
        assert!(detail_percent_uses_muted_tone(None));
        assert!(detail_percent_uses_muted_tone(Some(0.0)));
        assert!(detail_percent_uses_muted_tone(Some(0.4)));
        assert!(!detail_percent_uses_muted_tone(Some(0.5)));
        assert!(!detail_percent_reached_limit(Some(99.4)));
        assert!(detail_percent_reached_limit(Some(99.6)));
        assert!(detail_percent_reached_limit(Some(100.0)));
    }

    #[test]
    fn action_required_tints_keep_small_text_readable() {
        let providers = [
            tray_icon::TrayIconKind::Claude,
            tray_icon::TrayIconKind::Codex,
            tray_icon::TrayIconKind::Antigravity,
        ];

        for is_dark in [false, true] {
            let palette = detail_palette(is_dark, false);
            for kind in providers {
                let background = detail_provider_tint(kind, is_dark);
                let badge_text = detail_action_badge_foreground(kind, is_dark);
                let (hint_background, hint_outcome) =
                    detail_hint_colors(kind, is_dark, false, &palette);

                assert_eq!(background.to_colorref(), hint_background.to_colorref());
                assert!(
                    contrast_ratio(badge_text, background) >= 4.5,
                    "action badge contrast below 4.5 for {kind:?}, dark={is_dark}"
                );
                assert!(
                    contrast_ratio(hint_outcome, hint_background) >= 4.5,
                    "hint outcome contrast below 4.5 for {kind:?}, dark={is_dark}"
                );
            }
        }
    }

    #[test]
    fn taskbar_pill_secondary_text_meets_small_text_contrast() {
        for is_dark in [false, true] {
            let foreground = compact_color(ColorKey::PillAuxText, is_dark, false);
            let background = compact_color(ColorKey::PillBg, is_dark, false);
            assert!(
                contrast_ratio(foreground, background) >= 4.5,
                "pill secondary text contrast below 4.5, dark={is_dark}"
            );
        }
    }

    #[test]
    fn stale_marker_is_distinct_from_action_red_and_meets_small_text_contrast() {
        for is_dark in [false, true] {
            let foreground = compact_color(ColorKey::StaleText, is_dark, false);
            let pill_background = compact_color(ColorKey::PillBg, is_dark, false);
            let warning_background = compact_color(ColorKey::PillBgWarn, is_dark, false);
            let canvas_background = widget_palette(is_dark, false).bg;
            assert!(
                contrast_ratio(foreground, pill_background) >= 4.5,
                "stale marker contrast on pill below 4.5, dark={is_dark}"
            );
            assert!(
                contrast_ratio(foreground, warning_background) >= 4.5,
                "stale marker contrast on warning pill below 4.5, dark={is_dark}"
            );
            assert!(
                contrast_ratio(foreground, canvas_background) >= 4.5,
                "stale marker contrast on canvas below 4.5, dark={is_dark}"
            );
            assert_ne!(
                foreground.to_colorref(),
                compact_color(ColorKey::ErrorText, is_dark, false).to_colorref()
            );
        }
    }

    #[test]
    fn credential_hint_text_rows_are_separated_and_inside_the_callout() {
        let (hint, action, outcome) = detail_hint_rects(sc(18), sc(492), sc(100));

        assert!(action.top >= hint.top);
        assert!(action.bottom <= outcome.top);
        assert!(outcome.bottom <= hint.bottom);
        assert!(action.left == outcome.left && action.right == outcome.right);
    }

    #[test]
    fn tray_keyboard_anchor_uses_the_notification_icon_center() {
        let point = rect_center_point(RECT {
            left: -42,
            top: 1032,
            right: -22,
            bottom: 1052,
        });

        assert_eq!(point.x, -32);
        assert_eq!(point.y, 1042);
    }

    #[test]
    fn high_contrast_hint_uses_window_surface_and_window_text() {
        let palette = detail_palette(false, true);
        let (background, outcome) =
            detail_hint_colors(tray_icon::TrayIconKind::Claude, false, true, &palette);

        assert_eq!(background.to_colorref(), palette.card.to_colorref());
        assert_eq!(outcome.to_colorref(), palette.text.to_colorref());
    }

    #[test]
    fn detail_provider_badges_reflect_freshness_usage_and_errors() {
        let strings = LanguageId::English.strings();
        let group_for = |percent: f64, cached: bool| {
            let usage = UsageData::from_windows(vec![UsageWindow::new(
                percent,
                None,
                Some(FIVE_HOURS_SECONDS),
            )]);
            detail_provider_group(
                tray_icon::TrayIconKind::Claude,
                "Claude Code",
                Some(&usage),
                None,
                false,
                cached,
                strings,
            )
        };
        let assert_badge = |group: &DetailProviderGroup, text: &str, tone: DetailBadgeTone| {
            let badge = group.badge.as_ref().expect("provider badge");
            assert_eq!(badge.text, text);
            assert_eq!(badge.tone, tone);
        };

        assert!(group_for(0.0, false).badge.is_none());
        assert!(group_for(0.4, false).badge.is_none());
        assert!(group_for(1.0, false).badge.is_none());
        assert!(group_for(51.0, true).badge.is_none());
        assert!(group_for(51.0, true).data_is_stale);
        assert!(!group_for(51.0, false).data_is_stale);

        let cached_warning = group_for(92.0, true);
        assert_badge(&cached_warning, "Near limit", DetailBadgeTone::Critical);
        assert!(cached_warning.rows[0].warn);

        let near_limit = group_for(89.6, false);
        assert_badge(&near_limit, "Near limit", DetailBadgeTone::Critical);

        let limit_reached = group_for(99.6, false);
        assert_badge(&limit_reached, "Limit reached", DetailBadgeTone::Critical);

        let usage =
            UsageData::from_windows(vec![UsageWindow::new(92.0, None, Some(FIVE_HOURS_SECONDS))]);
        let authentication_failure = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::AuthenticationFailed),
            false,
            true,
            strings,
        );
        assert_badge(
            &authentication_failure,
            "Authentication failed",
            DetailBadgeTone::ActionRequired,
        );
        assert!(authentication_failure.data_is_stale);
        let authentication_failure_with_age = detail_provider_group_with_freshness(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::AuthenticationFailed),
            DetailDataFreshness {
                persistent_refresh_issue: false,
                updated_unix: Some(now_unix_secs().saturating_sub(600)),
                data_is_cached: false,
            },
            strings,
        );
        assert_eq!(
            authentication_failure_with_age
                .hint
                .as_ref()
                .map(|hint| hint.outcome.as_str()),
            Some("Last updated 10m ago")
        );

        let network_failure = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::NetworkUnavailable),
            true,
            false,
            strings,
        );
        assert_eq!(
            network_failure.hint,
            Some(DetailHint {
                action: "Check your connection".to_string(),
                outcome: "Retrying automatically".to_string(),
            })
        );
        assert_badge(
            &network_failure,
            "Refresh failed",
            DetailBadgeTone::Degraded,
        );

        let transient_failure = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::RequestFailed),
            false,
            false,
            strings,
        );
        assert_badge(&transient_failure, "Near limit", DetailBadgeTone::Critical);
        assert!(transient_failure.hint.is_none());

        let empty = UsageData::default();
        let loading = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&empty),
            None,
            false,
            false,
            strings,
        );
        assert!(loading.badge.is_none());

        let needs_update = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::AuthenticationFailed),
            false,
            true,
            strings,
        );
        let hint = needs_update.hint.as_ref().expect("Claude login hint");
        assert_eq!(hint.action, "Sign in to Claude again");
        assert_eq!(hint.outcome, "Automatically resumes after sign-in");

        let auth_failed = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::AuthenticationFailed),
            false,
            true,
            strings,
        );
        let auth_hint = auth_failed.hint.as_ref().expect("authentication hint");
        assert_eq!(auth_hint.action, "Sign in to Claude again");
        assert_eq!(auth_hint.outcome, "Automatically resumes after sign-in");

        let authentication_failed = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::AuthenticationFailed),
            false,
            true,
            strings,
        );
        assert_eq!(
            authentication_failed
                .hint
                .as_ref()
                .map(|hint| hint.action.as_str()),
            Some("Sign in to Claude again")
        );

        let refresh_failed = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            Some(&usage),
            Some(ProviderStatus::NetworkUnavailable),
            true,
            true,
            strings,
        );
        let refresh_hint = refresh_failed.hint.as_ref().expect("network failure hint");
        assert_eq!(refresh_hint.action, "Check your connection");
        assert_eq!(refresh_hint.outcome, "Retrying automatically");
        assert_eq!(
            detail_group_height(&needs_update),
            detail_group_height(&refresh_failed)
        );
    }

    #[test]
    fn detail_rows_use_compact_labels_and_short_placeholder() {
        let usage = UsageData::from_windows(vec![
            UsageWindow::new(1.0, None, Some(30 * 60)),
            UsageWindow::new(2.0, None, Some(365 * 24 * 60 * 60)),
        ]);
        let group = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            Some(&usage),
            None,
            false,
            false,
            LanguageId::Korean.strings(),
        );
        assert_eq!(group.rows[0].window_label, "30m");
        assert_eq!(group.rows[1].window_label, "365d");

        let loading = detail_provider_group(
            tray_icon::TrayIconKind::Codex,
            "Codex",
            None,
            None,
            false,
            false,
            LanguageId::English.strings(),
        );
        assert!(loading.rows[0].window_label.is_empty());
        assert!(loading.rows[0].percent.is_none());
    }

    #[test]
    fn detail_dynamic_columns_clamp_and_keep_the_name_gap() {
        for dpi in [96, 120, 144, 168, 192] {
            let _dpi = DpiScope::new(dpi);
            assert_eq!(detail_badge_width(0, DetailBadgeTone::Critical), sc(64));
            assert_eq!(
                detail_badge_width(sc(500), DetailBadgeTone::Critical),
                sc(160)
            );
            let badge_width = detail_badge_width(sc(80), DetailBadgeTone::Critical);
            let (badge_left, name_right) = detail_badge_horizontal_bounds(sc(378), badge_width);
            assert_eq!(badge_left - name_right, sc(8));
            assert!(name_right < badge_left);

            assert_eq!(detail_percent_column_width(0), sc(42));
            assert_eq!(detail_percent_column_width(sc(500)), sc(48));
        }

        unsafe {
            let hdc = GetDC(HWND::default());
            for dpi in [96, 120, 144, 168, 192] {
                let _dpi = DpiScope::new(dpi);
                let auth_width = measure_detail_text_width(
                    hdc,
                    "Authentication failed",
                    "Segoe UI",
                    11,
                    FW_NORMAL.0 as i32,
                );
                let percent_width =
                    measure_detail_text_width(hdc, "100%", "Segoe UI", 16, FW_SEMIBOLD.0 as i32);

                assert!(detail_badge_width(auth_width, DetailBadgeTone::ActionRequired) > sc(64));
                assert!(detail_percent_column_width(percent_width) >= percent_width);

                for language in LanguageId::ALL {
                    let strings = language.strings();
                    let label = strings.detail_badge_auth_failed;
                    let width = measure_detail_text_width(
                        hdc,
                        label,
                        detail_body_face(label),
                        11,
                        FW_NORMAL.0 as i32,
                    );
                    assert!(
                        width + sc(20) <= sc(160),
                        "localized auth badge loses padding at {dpi} DPI: {} {label}",
                        strings.locale_name
                    );
                    let label = strings.detail_badge_stale;
                    let width = measure_detail_text_width(
                        hdc,
                        label,
                        detail_body_face(label),
                        11,
                        FW_NORMAL.0 as i32,
                    );
                    assert!(
                        width + sc(2) <= sc(160),
                        "localized refresh badge is truncated at {dpi} DPI: {} {label}",
                        strings.locale_name
                    );
                }
            }
            ReleaseDC(HWND::default(), hdc);
        }
    }

    #[test]
    fn reference_card_height_and_popup_size_scale_as_one_layout() {
        let row = DetailUsageRow {
            window_label: "5h".to_string(),
            percent: Some(42.0),
            reset_text: "Resets in 2h".to_string(),
            dividers: 5,
            warn: false,
        };
        let group = DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Claude,
            name: "Claude Code".to_string(),
            badge: None,
            rows: vec![row.clone(), row],
            data_is_stale: false,
            hint: None,
        };
        let snapshot = DetailPopupState {
            title: "Gengchou".to_string(),
            providers: vec![group.clone()],
            status: "Updated now".to_string(),
            version: "test".to_string(),
            refreshing: false,
        };

        assert_eq!(detail_group_height(&group), 152);
        let mut badged = group.clone();
        badged.badge = Some(DetailBadge {
            text: "Authentication failed".to_string(),
            tone: DetailBadgeTone::ActionRequired,
        });
        assert_eq!(detail_group_height(&badged), 152);

        for (dpi, expected) in [
            (96, (408, 258)),
            (120, (510, 323)),
            (144, (612, 387)),
            (192, (816, 516)),
        ] {
            let _dpi = DpiScope::new(dpi);
            assert_eq!(detail_popup_size(&snapshot), expected);
        }

        let full_snapshot = DetailPopupState {
            title: snapshot.title.clone(),
            providers: vec![
                group.clone(),
                DetailProviderGroup {
                    rows: vec![group.rows[0].clone()],
                    ..group.clone()
                },
                group,
            ],
            status: snapshot.status,
            version: snapshot.version,
            refreshing: false,
        };
        let _dpi = DpiScope::new(120);
        assert_eq!(detail_popup_size(&full_snapshot), (510, 668));
    }

    #[test]
    fn naturally_sized_detail_popup_never_scrolls_from_dpi_rounding() {
        let row = DetailUsageRow {
            window_label: "5h".to_string(),
            percent: Some(42.0),
            reset_text: "Resets in 2h".to_string(),
            dividers: 5,
            warn: false,
        };
        let two_row_group = DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Claude,
            name: "Claude Code".to_string(),
            badge: None,
            rows: vec![row.clone(), row.clone()],
            data_is_stale: false,
            hint: None,
        };
        let one_row_group = DetailProviderGroup {
            rows: vec![row],
            ..two_row_group.clone()
        };
        let hinted_one_row_group = DetailProviderGroup {
            hint: Some(DetailHint {
                action: "Sign in".to_string(),
                outcome: "Monitoring resumes".to_string(),
            }),
            ..one_row_group.clone()
        };
        let snapshots = [
            DetailPopupState {
                title: "Gengchou".to_string(),
                providers: vec![two_row_group.clone(), two_row_group.clone()],
                status: "Every 1m".to_string(),
                version: "test".to_string(),
                refreshing: false,
            },
            DetailPopupState {
                title: "Gengchou".to_string(),
                providers: vec![hinted_one_row_group, one_row_group, two_row_group],
                status: "Authentication failed".to_string(),
                version: "test".to_string(),
                refreshing: false,
            },
        ];
        assert_eq!(detail_popup_body_height(&snapshots[0]), 326);
        assert_eq!(detail_popup_body_height(&snapshots[1]), 434);

        for dpi in [96, 120, 144, 168, 192] {
            let _dpi = DpiScope::new(dpi);
            for snapshot in &snapshots {
                let (_, height) = detail_popup_size(snapshot);
                let metrics = detail_scroll_metrics(snapshot, height);
                assert_eq!(metrics.viewport_height, metrics.content_height);
                assert_eq!(metrics.max_offset, 0);
            }
        }
    }

    #[test]
    fn detail_popup_caps_height_and_scrolls_only_the_body() {
        let row = DetailUsageRow {
            window_label: "5h".to_string(),
            percent: Some(42.0),
            reset_text: "Resets in 2h".to_string(),
            dividers: 5,
            warn: false,
        };
        let group = DetailProviderGroup {
            kind: tray_icon::TrayIconKind::Claude,
            name: "Claude Code".to_string(),
            badge: None,
            rows: vec![row.clone(), row],
            data_is_stale: false,
            hint: Some(DetailHint {
                action: "Sign in".to_string(),
                outcome: "Monitoring resumes".to_string(),
            }),
        };
        let snapshot = DetailPopupState {
            title: "Gengchou".to_string(),
            providers: vec![group.clone(), group.clone(), group],
            status: "Every 1m".to_string(),
            version: "test".to_string(),
            refreshing: false,
        };
        let _dpi = DpiScope::new(144);
        let work = RECT {
            left: 0,
            top: 0,
            right: 1_366,
            bottom: 720,
        };
        let (width, height) = detail_popup_fitted_size(&snapshot, work);
        assert_eq!(width, sc(DETAIL_POPUP_WIDTH));
        assert_eq!(height, 720 - 2 * sc(DETAIL_WORK_AREA_MARGIN));

        let metrics = detail_scroll_metrics(&snapshot, height);
        assert_eq!(metrics.viewport_top, sc(DETAIL_HEADER_H));
        assert_eq!(metrics.viewport_bottom, height - sc(DETAIL_FOOTER_H));
        assert!(metrics.max_offset > 0);
        let thumb = detail_scroll_thumb_rect(width, metrics, metrics.max_offset)
            .expect("overflowing body has a scrollbar thumb");
        assert!(thumb.top >= metrics.viewport_top);
        assert!(thumb.bottom <= metrics.viewport_bottom);
        assert_eq!(
            detail_scroll_offset_from_drag(
                0,
                metrics.viewport_height,
                metrics,
                thumb.bottom - thumb.top
            ),
            metrics.max_offset
        );
    }

    #[test]
    fn detail_attention_rail_stays_out_of_rounded_corners() {
        let _dpi = DpiScope::new(96);
        let card = RECT {
            left: 18,
            top: 52,
            right: 390,
            bottom: 204,
        };
        let radius = sc(8);
        let rail = detail_attention_rail_rect(&card, radius);
        assert_eq!(rail.left, card.left);
        assert_eq!(rail.right - rail.left, sc(3));
        assert_eq!(rail.top, card.top + radius);
        assert_eq!(rail.bottom, card.bottom - radius);
    }

    #[test]
    fn detail_fixture_copy_uses_live_localization_and_weekday_rules() {
        let now = SYSTEMTIME {
            wYear: 2030,
            wMonth: 1,
            wDayOfWeek: 0,
            wDay: 6,
            wHour: 21,
            wMinute: 55,
            ..Default::default()
        };
        let target = SYSTEMTIME {
            wYear: 2030,
            wMonth: 1,
            wDayOfWeek: 1,
            wDay: 7,
            wHour: 1,
            ..Default::default()
        };
        let duration = Duration::from_secs(3 * 3_600 + 5 * 60);

        let english = LanguageId::English.strings();
        let english_time = format_local_time_components(&target, &now, duration.as_secs(), english);
        assert_eq!(english_time, "Mon 01:00");
        assert_eq!(
            detail_reset_line_from_parts(duration, Some(english_time), english, true),
            "Resets in 3h 5m · Mon 01:00"
        );

        let chinese = LanguageId::SimplifiedChinese.strings();
        let chinese_time = format_local_time_components(&target, &now, duration.as_secs(), chinese);
        assert_eq!(chinese_time, "周一 01:00");
        assert_eq!(
            detail_reset_line_from_parts(duration, Some(chinese_time), chinese, true),
            "3小时5分钟后重置 · 周一 01:00"
        );
        assert_eq!(
            detail_poll_timing_status(1_000, false, POLL_1_MIN, Some(44), chinese, 1_000),
            "每1分钟 · 44秒后刷新"
        );
    }

    #[test]
    fn empty_detail_rows_distinguish_auth_failure_from_loading() {
        let strings = LanguageId::SimplifiedChinese.strings();
        let credential_group = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            None,
            Some(ProviderStatus::AuthenticationFailed),
            false,
            false,
            strings,
        );
        assert_eq!(
            credential_group.rows[0].reset_text,
            strings.detail_unavailable
        );

        let loading_group = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            None,
            None,
            false,
            false,
            strings,
        );
        assert_eq!(loading_group.rows[0].reset_text, strings.detail_waiting);

        let unavailable_group = detail_provider_group(
            tray_icon::TrayIconKind::Claude,
            "Claude Code",
            None,
            Some(ProviderStatus::RequestFailed),
            true,
            false,
            strings,
        );
        assert_eq!(
            unavailable_group.rows[0].reset_text,
            strings.detail_temporarily_unavailable
        );
    }

    #[test]
    fn detail_motion_respects_windows_and_high_contrast_preferences() {
        assert!(detail_should_animate(true, false));
        assert!(!detail_should_animate(false, false));
        assert!(!detail_should_animate(true, true));
        assert!(!detail_should_animate(false, true));
    }

    #[test]
    fn detail_bar_cells_leave_equal_segments_and_gaps_only_between_them() {
        for dpi in [96, 120, 144, 168, 192] {
            let _dpi = DpiScope::new(dpi);
            let rect = RECT {
                left: sc(13),
                top: sc(4),
                right: sc(213),
                bottom: sc(16),
            };

            for cell_count in [5, 7] {
                let cells: Vec<_> = (0..cell_count)
                    .map(|index| detail_bar_cell_rect(&rect, cell_count, index))
                    .collect();
                assert_eq!(cells.first().unwrap().left, rect.left);
                assert_eq!(cells.last().unwrap().right, rect.right);
                assert!(cells.iter().all(|cell| {
                    cell.top == rect.top && cell.bottom == rect.bottom && cell.right > cell.left
                }));
                assert!(cells
                    .windows(2)
                    .all(|pair| pair[1].left - pair[0].right == sc(DETAIL_BAR_GAP)));
                let widths = cells
                    .iter()
                    .map(|cell| cell.right - cell.left)
                    .collect::<Vec<_>>();
                assert!(widths.iter().max().unwrap() - widths.iter().min().unwrap() <= 1);
            }
        }
    }

    #[test]
    fn detail_bar_supersampling_blends_only_the_rounded_edge() {
        let accent = Color::new(220, 20, 60);
        let track = Color::new(40, 40, 40);
        let background = Color::new(255, 255, 255);
        let pixels = detail_bar_cell_pixels(20, 10, 2, 0, 10, &accent, &track, &background, 4);
        let packed = |color: Color| {
            u32::from(color.b)
                | (u32::from(color.g) << 8)
                | (u32::from(color.r) << 16)
                | 0xFF00_0000
        };

        let corner = pixels[0];
        assert_ne!(corner, packed(background));
        assert_ne!(corner, packed(accent));
        assert_eq!(pixels[5 * 20 + 3], packed(accent));
        assert_eq!(pixels[5 * 20 + 15], packed(track));
    }

    #[test]
    fn detail_text_never_paints_an_opaque_glyph_background() {
        unsafe {
            let screen = GetDC(HWND::default());
            assert_ne!(screen, HDC::default());
            let hdc = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, 32, 32);
            let old_bitmap = SelectObject(hdc, bitmap);

            fill_rect_color(
                hdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 32,
                    bottom: 32,
                },
                &Color::new(31, 31, 31),
            );
            let opaque_marker = COLORREF(Color::new(0, 255, 0).to_colorref());
            let _ = SetBkColor(hdc, opaque_marker);
            let _ = SetBkMode(hdc, OPAQUE);

            draw_detail_text_face(
                hdc,
                "\u{E711}",
                RECT {
                    left: 0,
                    top: 0,
                    right: 32,
                    bottom: 32,
                },
                &Color::new(220, 40, 40),
                "Segoe MDL2 Assets",
                14,
                FW_NORMAL.0 as i32,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );

            for y in 0..32 {
                for x in 0..32 {
                    assert_ne!(
                        GetPixel(hdc, x, y),
                        opaque_marker,
                        "opaque text background leaked at ({x}, {y})"
                    );
                }
            }
            // The helper must not unexpectedly change the caller's HDC state.
            assert_eq!(SetBkMode(hdc, TRANSPARENT), OPAQUE.0 as i32);

            SelectObject(hdc, old_bitmap);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(hdc);
            ReleaseDC(HWND::default(), screen);
        }
    }

    #[test]
    fn bundled_provider_tiles_use_exact_png_and_hicon_sizes() {
        for dpi in PROVIDER_TILE_BUCKET_DPIS {
            let _dpi = DpiScope::new(dpi);
            for kind in tray_icon::TrayIconKind::ALL {
                for (tile_size, logical_size) in [
                    (TileSize::Chip16, 16),
                    (TileSize::Chip20, 20),
                    (TileSize::Chip28, DETAIL_LOGO_CHIP_SIZE),
                ] {
                    let asset = provider_tile_asset(kind, dpi, false, tile_size);
                    assert_eq!(asset.size, sc(logical_size));
                    assert_eq!(
                        png_ihdr(asset.bytes),
                        (asset.size as u32, asset.size as u32, 8, 6)
                    );

                    let (hicon, size) = provider_tile_icon(kind, dpi, false, false, tile_size)
                        .expect("provider tile");
                    assert_eq!(size, asset.size);
                    let (width, height, bits_per_pixel) = hicon_color_bitmap_metrics(hicon);
                    assert_eq!((width, height), (size, size));
                    assert_eq!(bits_per_pixel, 32);

                    let dark_asset = provider_tile_asset(kind, dpi, true, tile_size);
                    assert_eq!(
                        png_ihdr(dark_asset.bytes),
                        (dark_asset.size as u32, dark_asset.size as u32, 8, 6)
                    );
                    let (dark_hicon, dark_size) =
                        provider_tile_icon(kind, dpi, true, false, tile_size)
                            .expect("dark provider tile");
                    assert_eq!(dark_size, dark_asset.size);
                    let (dark_width, dark_height, dark_bits_per_pixel) =
                        hicon_color_bitmap_metrics(dark_hicon);
                    assert_eq!((dark_width, dark_height), (dark_size, dark_size));
                    assert_eq!(dark_bits_per_pixel, 32);
                    assert!(provider_tile_icon(kind, dpi, true, true, tile_size).is_none());
                }
            }
        }
    }

    #[test]
    fn provider_tile_bucket_selection_clamps_and_breaks_ties_downward() {
        for (index, dpi) in PROVIDER_TILE_BUCKET_DPIS.iter().enumerate() {
            assert_eq!(nearest_provider_tile_bucket(*dpi), index);
        }
        assert_eq!(nearest_provider_tile_bucket(0), 0);
        assert_eq!(nearest_provider_tile_bucket(72), 0);
        for (index, pair) in PROVIDER_TILE_BUCKET_DPIS.windows(2).enumerate() {
            let midpoint = (pair[0] + pair[1]) / 2;
            assert_eq!(nearest_provider_tile_bucket(midpoint), index);
            assert_eq!(nearest_provider_tile_bucket(midpoint + 1), index + 1);
        }
        assert_eq!(
            nearest_provider_tile_bucket(480),
            PROVIDER_TILE_BUCKET_DPIS.len() - 1
        );
    }

    #[test]
    fn detail_duration_spacing_follows_the_writing_system() {
        let duration = 3 * 60 * 60 + 38 * 60;
        assert_eq!(
            detail_duration_from_secs(duration, LanguageId::English.strings()),
            "3h 38m"
        );
        assert_eq!(
            detail_duration_from_secs(duration, LanguageId::SimplifiedChinese.strings()),
            "3小时38分钟"
        );
        assert_eq!(detail_body_face("3小时38分钟后重置"), "Microsoft YaHei UI");
    }

    #[test]
    fn detail_drag_region_stays_clear_of_header_buttons() {
        let width = sc(DETAIL_POPUP_WIDTH);
        assert!(detail_header_is_draggable(sc(20), sc(20), width));

        let buttons = [
            detail_refresh_rect(width),
            detail_pin_rect(width),
            detail_move_rect(width),
            detail_close_rect(width),
        ];
        assert_eq!(
            buttons[1].left - buttons[0].right,
            sc(DETAIL_HEADER_REFRESH_GROUP_GAP)
        );
        for pair in buttons[1..].windows(2) {
            assert_eq!(pair[1].left - pair[0].right, sc(DETAIL_HEADER_BUTTON_GAP));
        }
        for button in buttons {
            assert_eq!(button.right - button.left, sc(DETAIL_HEADER_BUTTON_SIZE));
            assert_eq!(button.bottom - button.top, sc(DETAIL_HEADER_BUTTON_SIZE));
            let x = (button.left + button.right) / 2;
            let y = (button.top + button.bottom) / 2;
            assert!(!detail_header_is_draggable(x, y, width));
        }
    }

    #[test]
    fn detail_title_tracking_is_limited_to_localized_chinese_brand_names() {
        assert!(detail_title_uses_cjk_tracking("更筹"));
        assert!(detail_title_uses_cjk_tracking("更籌"));
        assert!(!detail_title_uses_cjk_tracking("Gengchou"));
    }

    #[test]
    fn detail_state_icons_show_current_state() {
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_PIN, true, true, false),
            "\u{E718}"
        );
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_PIN, false, true, false),
            "\u{E77A}"
        );
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_MOVE, false, false, false),
            "\u{E72E}"
        );
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_MOVE, false, true, false),
            "\u{E785}"
        );
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_REFRESH, false, true, false),
            "\u{E72C}"
        );
        assert_eq!(
            detail_header_button_glyph(IDC_DETAIL_REFRESH, false, true, true),
            "\u{E895}"
        );
    }

    #[test]
    fn detail_focus_cue_is_reserved_for_visible_keyboard_focus() {
        assert!(!detail_focus_cue_visible(0, IDC_DETAIL_REFRESH, 0));
        assert!(detail_focus_cue_visible(ODS_FOCUS.0, IDC_DETAIL_REFRESH, 0));
        assert!(!detail_focus_cue_visible(
            ODS_FOCUS.0,
            IDC_DETAIL_REFRESH,
            IDC_DETAIL_REFRESH as u32
        ));
        assert!(detail_focus_cue_visible(
            ODS_FOCUS.0,
            IDC_DETAIL_REFRESH,
            IDC_DETAIL_PIN as u32
        ));
        assert!(!detail_focus_cue_visible(
            ODS_FOCUS.0 | ODS_NOFOCUSRECT.0,
            IDC_DETAIL_REFRESH,
            0
        ));
    }

    #[test]
    fn detail_poll_timing_advances_each_second() {
        let strings = LanguageId::English.strings();
        let first = detail_poll_timing_status(1_000, false, POLL_1_MIN, Some(13), strings, 1_047);
        let second = detail_poll_timing_status(1_000, false, POLL_1_MIN, Some(12), strings, 1_048);

        assert!(first.contains("Every 1m"));
        assert!(first.contains("next in 13s"));
        assert!(!first.contains("Updated"));
        assert!(second.contains("Every 1m"));
        assert!(second.contains("next in 12s"));

        let cached = detail_poll_timing_status(1_000, true, POLL_1_MIN, Some(13), strings, 1_047);
        assert_eq!(cached, "Last updated 47s ago");
        assert!(!cached.contains("Every"));
        assert!(!cached.contains("next in"));

        let backoff =
            detail_poll_timing_status(1_000, false, POLL_30_MIN, Some(30), strings, 1_047);
        assert!(backoff.contains("Every 30m"));
        assert!(backoff.contains("next in 30s"));
    }

    #[test]
    fn legacy_cache_drops_ghost_zero_window_and_preserves_weekly_usage() {
        let provider = UsageCacheProvider {
            updated_unix: None,
            windows: Vec::new(),
            session: Some(UsageCacheWindow::default()),
            weekly: Some(UsageCacheWindow {
                percent: 12.0,
                resets_unix: Some(1_800_000_000),
                ..Default::default()
            }),
        };

        let usage = usage_provider_from_cache(&provider);
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percentage, 12.0);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    /// Every provider must survive the usage cache, not just the ones that
    /// happened to be wired when the file format was last touched. A provider
    /// missing from `UsageCacheFile` fails silently: it polls and displays
    /// normally, and only a restart shows it lost its cached value.
    #[test]
    fn usage_cache_carries_every_provider() {
        let mut data = AppUsageData::default();
        for (index, kind) in tray_icon::TrayIconKind::ALL.into_iter().enumerate() {
            let percent = 10.0 + index as f64;
            let slot = data.provider_mut(kind);
            slot.usage = Some(UsageData::from_windows(vec![UsageWindow::new(
                percent,
                Some(UNIX_EPOCH + Duration::from_secs(1_000 + index as u64)),
                Some(FIVE_HOURS_SECONDS),
            )]));
            slot.updated_unix = Some(500 + index as u64);
        }

        let file = usage_cache_file_from(&data, 900);
        // Round-trip through the serialized form, so a field that exists on the
        // struct but is never written would still be caught.
        let json = serde_json::to_string(&file).expect("cache should serialize");
        let parsed: UsageCacheFile = serde_json::from_str(&json).expect("cache should parse");

        for (index, (kind, section)) in usage_cache_file_sections(&parsed).into_iter().enumerate() {
            let section = section
                .unwrap_or_else(|| panic!("{kind:?} is missing from the serialized usage cache"));
            assert_eq!(section.updated_unix, Some(500 + index as u64), "{kind:?}");
            let restored = usage_provider_from_cache(section);
            assert_eq!(restored.windows.len(), 1, "{kind:?}");
            assert_eq!(
                restored.windows[0].percentage,
                10.0 + index as f64,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn dynamic_cache_round_trip_preserves_window_metadata() {
        let usage = UsageData::from_windows(vec![UsageWindow::new(
            7.0,
            Some(UNIX_EPOCH + Duration::from_secs(42)),
            None,
        )
        .with_source_label(Some("Quota".to_string()))]);

        let cache = usage_provider_to_cache(&usage, Some(123));
        let restored = usage_provider_from_cache(&cache);
        assert_eq!(cache.updated_unix, Some(123));
        assert_eq!(restored.windows.len(), 1);
        assert_eq!(restored.windows[0].percentage, 7.0);
        assert_eq!(restored.windows[0].resets_at, usage.windows[0].resets_at);
        assert_eq!(restored.windows[0].source_label.as_deref(), Some("Quota"));
    }

    #[test]
    fn provider_cache_uses_legacy_file_time_and_drops_stale_sections() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let legacy = usage_provider_to_cache(&usage, None);

        let (_, updated_unix) =
            fresh_cached_provider(Some(&legacy), 1_000, 1_001).expect("fresh legacy cache");
        assert_eq!(updated_unix, 1_000);
        assert!(
            fresh_cached_provider(Some(&legacy), 1_000, 1_000 + USAGE_CACHE_MAX_AGE_SECS + 1,)
                .is_none()
        );
    }

    #[test]
    fn partial_poll_preserves_failed_provider_freshness() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let previous = AppUsageData::default()
            .with_usage(tray_icon::TrayIconKind::Claude, usage.clone())
            .with_usage(tray_icon::TrayIconKind::Codex, usage.clone())
            .with_updated_unix(tray_icon::TrayIconKind::Claude, 100)
            .with_updated_unix(tray_icon::TrayIconKind::Codex, 100);
        let mut next = AppUsageData::default()
            .with_usage(tray_icon::TrayIconKind::Codex, usage)
            .with_error(tray_icon::TrayIconKind::Claude, ProviderStatus::RateLimited);
        stamp_provider_updates(&mut next, 200);

        let merged = merge_missing_provider_data(Some(&previous), next, [true, true, false, false]);
        assert!(merged.usage(tray_icon::TrayIconKind::Claude).is_some());
        assert_eq!(
            merged
                .provider(tray_icon::TrayIconKind::Claude)
                .updated_unix,
            Some(100)
        );
        assert_eq!(
            merged.provider(tray_icon::TrayIconKind::Codex).updated_unix,
            Some(200)
        );
    }

    #[test]
    fn failed_poll_keeps_usage_but_replaces_stale_provider_errors() {
        let usage =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(FIVE_HOURS_SECONDS))]);
        let previous = AppUsageData::default()
            .with_usage(tray_icon::TrayIconKind::Claude, usage.clone())
            .with_usage(tray_icon::TrayIconKind::Codex, usage)
            .with_updated_unix(tray_icon::TrayIconKind::Claude, 100)
            .with_updated_unix(tray_icon::TrayIconKind::Codex, 100)
            .with_error(tray_icon::TrayIconKind::Claude, ProviderStatus::RateLimited)
            .with_error(
                tray_icon::TrayIconKind::Codex,
                ProviderStatus::RequestFailed,
            )
            .with_retry_after_ms(tray_icon::TrayIconKind::Claude, 120_000);
        let next = AppUsageData::default()
            .with_error(
                tray_icon::TrayIconKind::Claude,
                ProviderStatus::AuthenticationFailed,
            )
            .with_error(
                tray_icon::TrayIconKind::Codex,
                ProviderStatus::AuthenticationFailed,
            );

        let merged = merge_missing_provider_data(Some(&previous), next, [true, true, false, false]);

        assert!(merged.usage(tray_icon::TrayIconKind::Claude).is_some());
        assert!(merged.usage(tray_icon::TrayIconKind::Codex).is_some());
        assert_eq!(
            merged
                .provider(tray_icon::TrayIconKind::Claude)
                .updated_unix,
            Some(100)
        );
        assert_eq!(
            merged.provider(tray_icon::TrayIconKind::Codex).updated_unix,
            Some(100)
        );
        assert_eq!(
            merged.error(tray_icon::TrayIconKind::Claude),
            Some(ProviderStatus::AuthenticationFailed)
        );
        assert_eq!(
            merged.error(tray_icon::TrayIconKind::Codex),
            Some(ProviderStatus::AuthenticationFailed)
        );
        assert_eq!(
            merged
                .provider(tray_icon::TrayIconKind::Claude)
                .retry_after_ms,
            None
        );
        assert_eq!(
            merged
                .provider(tray_icon::TrayIconKind::Codex)
                .retry_after_ms,
            None
        );
    }

    #[test]
    fn compact_attention_waits_for_persistent_staleness_and_reserves_red_for_action() {
        let classify = |status, failures, updated_unix, interval, now| {
            compact_attention_for_provider_status(
                compact_view::Attention::Normal,
                status,
                ProviderRefreshState {
                    consecutive_failures: failures,
                    ..Default::default()
                },
                updated_unix,
                interval,
                now,
            )
        };

        for failures in [1, 2] {
            assert_eq!(
                classify(
                    Some(ProviderStatus::NetworkUnavailable),
                    failures,
                    Some(900),
                    POLL_1_MIN,
                    1_000,
                ),
                compact_view::Attention::Normal
            );
        }
        assert_eq!(
            classify(
                Some(ProviderStatus::NetworkUnavailable),
                COMPACT_REQUEST_FAILURE_THRESHOLD,
                None,
                POLL_1_MIN,
                1_000,
            ),
            compact_view::Attention::Stale
        );

        assert_eq!(
            classify(
                Some(ProviderStatus::RateLimited),
                0,
                Some(900),
                POLL_1_MIN,
                1_000,
            ),
            compact_view::Attention::Normal
        );
        assert_eq!(
            classify(
                Some(ProviderStatus::RateLimited),
                0,
                None,
                POLL_1_MIN,
                1_000,
            ),
            compact_view::Attention::Normal
        );
        assert_eq!(
            classify(
                Some(ProviderStatus::RateLimited),
                0,
                Some(700),
                POLL_1_MIN,
                1_000,
            ),
            compact_view::Attention::Stale
        );

        assert_eq!(compact_stale_after_secs(POLL_1_MIN), 5 * 60);
        assert_eq!(compact_stale_after_secs(POLL_30_MIN), 60 * 60);
        assert_eq!(
            classify(
                Some(ProviderStatus::NetworkUnavailable),
                0,
                Some(1_000),
                POLL_30_MIN,
                4_599,
            ),
            compact_view::Attention::Normal
        );
        assert_eq!(
            classify(
                Some(ProviderStatus::NetworkUnavailable),
                0,
                Some(1_000),
                POLL_30_MIN,
                4_600,
            ),
            compact_view::Attention::Stale
        );

        assert_eq!(
            classify(
                Some(ProviderStatus::AuthenticationFailed),
                0,
                Some(999),
                POLL_1_MIN,
                1_000,
            ),
            compact_view::Attention::ActionRequired
        );
    }

    #[test]
    fn provider_refresh_state_keeps_failure_count_across_rate_limit_and_resets_on_success() {
        let mut state = ProviderRefreshState::default();
        let now = Instant::now();
        update_provider_refresh_state(
            &mut state,
            "test",
            true,
            true,
            Some(ProviderStatus::RequestFailed),
            false,
            None,
            POLL_1_MIN,
            1_000,
            now,
        );
        assert_eq!(state.consecutive_failures, 1);
        assert_eq!(state.unavailable_since_unix, Some(1_000));

        update_provider_refresh_state(
            &mut state,
            "test",
            true,
            true,
            Some(ProviderStatus::RateLimited),
            false,
            Some(120_000),
            POLL_1_MIN,
            1_001,
            now,
        );
        assert_eq!(state.consecutive_failures, 1);

        update_provider_refresh_state(
            &mut state,
            "test",
            true,
            true,
            Some(ProviderStatus::RequestFailed),
            false,
            None,
            POLL_1_MIN,
            1_002,
            now,
        );
        assert_eq!(state.consecutive_failures, 2);

        update_provider_refresh_state(
            &mut state, "test", true, true, None, true, None, POLL_1_MIN, 1_003, now,
        );
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.unavailable_since_unix, None);
        assert_eq!(state.rate_limit_until, None);
    }

    #[test]
    fn rate_limit_without_history_becomes_stale_from_first_unavailable_time() {
        let mut state = ProviderRefreshState::default();
        let now = Instant::now();
        update_provider_refresh_state(
            &mut state,
            "test",
            true,
            true,
            Some(ProviderStatus::RateLimited),
            false,
            Some(10 * 60 * 1_000),
            POLL_1_MIN,
            1_000,
            now,
        );

        assert_eq!(state.unavailable_since_unix, Some(1_000));
        assert!(!provider_refresh_is_stale(
            Some(ProviderStatus::RateLimited),
            state,
            None,
            POLL_1_MIN,
            1_299,
        ));
        assert!(provider_refresh_is_stale(
            Some(ProviderStatus::RateLimited),
            state,
            None,
            POLL_1_MIN,
            1_300,
        ));
    }

    #[test]
    fn rate_limit_stale_timer_ignores_retained_transient_failure_count() {
        let state = ProviderRefreshState {
            consecutive_failures: COMPACT_REQUEST_FAILURE_THRESHOLD,
            unavailable_since_unix: Some(1_000),
            ..ProviderRefreshState::default()
        };

        assert_eq!(
            provider_stale_transition_delay(
                Some(ProviderStatus::RateLimited),
                state,
                None,
                POLL_1_MIN,
                1_299,
            ),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            provider_stale_transition_delay(
                Some(ProviderStatus::RequestFailed),
                state,
                None,
                POLL_1_MIN,
                1_299,
            ),
            None
        );
    }

    #[test]
    fn provider_cooldown_skips_only_the_rate_limited_provider() {
        let plan = PollPassPlan {
            show_claude_code: true,
            show_codex: true,
            show_antigravity: false,
            show_grok: false,
            poll_claude_code: false,
            poll_codex: true,
            poll_antigravity: false,
            poll_grok: false,
            claude_cooldown_ms: Some(42_000),
            codex_cooldown_ms: None,
            antigravity_cooldown_ms: None,
            grok_cooldown_ms: None,
        };
        let mut data = AppUsageData::default();

        plan.apply_skipped_rate_limits(&mut data);

        assert_eq!(
            data.error(tray_icon::TrayIconKind::Claude),
            Some(ProviderStatus::RateLimited)
        );
        assert_eq!(
            data.provider(tray_icon::TrayIconKind::Claude)
                .retry_after_ms,
            Some(42_000)
        );
        assert_eq!(data.error(tray_icon::TrayIconKind::Codex), None);
        assert_eq!(
            data.provider(tray_icon::TrayIconKind::Codex).retry_after_ms,
            None
        );
        assert!(plan.has_poll_target());
    }

    #[test]
    fn startup_command_requires_one_exact_quoted_executable() {
        let quoted: Vec<u16> =
            r#""C:\应用\Gengchou.exe""#.encode_utf16().chain(std::iter::once(0)).collect();
        let unquoted: Vec<u16> = r"C:\应用\Gengchou.exe"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let with_args: Vec<u16> =
            r#""C:\应用\Gengchou.exe" --silent"#.encode_utf16().chain(std::iter::once(0)).collect();

        assert_eq!(
            String::from_utf16(quoted_startup_executable(&quoted).unwrap()).unwrap(),
            r"C:\应用\Gengchou.exe"
        );
        assert!(quoted_startup_executable(&unquoted).is_none());
        assert!(quoted_startup_executable(&with_args).is_none());
    }

    #[test]
    fn startup_path_comparison_is_unicode_case_insensitive() {
        let registered: Vec<u16> = r"C:\Ä\Gengchou.exe".encode_utf16().collect();
        let current: Vec<u16> = r"c:\ä\GENGCHOU.EXE".encode_utf16().collect();

        assert!(windows_paths_equal(&registered, &current));
    }

    #[test]
    fn topology_reset_requires_a_disappeared_secondary_monitor() {
        let primary = MonitorIdentity {
            device: "DISPLAY1".to_string(),
            device_path: Some("MONITOR1".to_string()),
            is_primary: true,
        };
        let secondary = MonitorIdentity {
            device: "DISPLAY2".to_string(),
            device_path: Some("MONITOR2".to_string()),
            is_primary: false,
        };

        assert!(secondary_monitor_disappeared(
            Some(&secondary),
            std::slice::from_ref(&primary)
        ));
        assert!(!secondary_monitor_disappeared(
            Some(&secondary),
            &[primary.clone(), secondary.clone()]
        ));
        assert!(!secondary_monitor_disappeared(
            Some(&primary),
            std::slice::from_ref(&secondary)
        ));
        assert!(!secondary_monitor_disappeared(None, &[primary]));
    }

    #[test]
    fn legacy_taskbar_drift_migrates_to_preset_but_manual_positions_stay_custom() {
        let primary = MonitorIdentity {
            device: r"\\.\DISPLAY1".to_string(),
            device_path: Some("MONITOR1".to_string()),
            is_primary: true,
        };
        let taskbar = PlacementRect {
            left: 0,
            top: 1_040,
            right: 2_560,
            bottom: 1_120,
        };

        // The live regression had drifted to x=95 with this derived offset.
        assert_eq!(
            migrate_legacy_widget_placement(
                WidgetDefaultPosition::PrimaryTaskbarLeft,
                1_622,
                taskbar,
                2_132,
                415,
                96,
                &primary,
            ),
            WidgetPlacement::PrimaryLeft
        );

        let manual = migrate_legacy_widget_placement(
            WidgetDefaultPosition::PrimaryTaskbarLeft,
            1_117,
            taskbar,
            2_132,
            415,
            96,
            &primary,
        );
        assert!(matches!(
            manual,
            WidgetPlacement::Custom {
                anchor: crate::settings::WidgetAnchor::TaskbarLeft,
                gap_dip: 600,
                ..
            }
        ));
    }

    #[test]
    fn legacy_secondary_taskbar_position_never_collapses_to_a_primary_preset() {
        let secondary = MonitorIdentity {
            device: r"\\.\DISPLAY2".to_string(),
            device_path: Some("MONITOR2".to_string()),
            is_primary: false,
        };
        let migrated = migrate_legacy_widget_placement(
            WidgetDefaultPosition::PrimaryTaskbarLeft,
            1_709,
            PlacementRect {
                left: 0,
                top: 1_040,
                right: 2_560,
                bottom: 1_120,
            },
            2_132,
            415,
            96,
            &secondary,
        );
        assert!(matches!(migrated, WidgetPlacement::Custom { .. }));
    }

    #[test]
    fn legacy_floating_size_drift_migrates_to_preset() {
        let primary = MonitorIdentity {
            device: r"\\.\DISPLAY1".to_string(),
            device_path: Some("MONITOR1".to_string()),
            is_primary: true,
        };
        let work = PlacementRect {
            left: 0,
            top: 0,
            right: 1_920,
            bottom: 1_040,
        };
        // Coordinates were correct for an older 180x52 surface; the current
        // surface is 260x88, so both stored axes appear to have drifted.
        let migrated = migrate_legacy_floating_placement(
            FloatingDefaultPosition::PrimaryBottomRight,
            PlacementRect {
                left: 1_732,
                top: 980,
                right: 1_992,
                bottom: 1_068,
            },
            work,
            260,
            88,
            96,
            &primary,
        );
        assert_eq!(migrated, FloatingPlacement::PrimaryBottomRight);

        let manual = migrate_legacy_floating_placement(
            FloatingDefaultPosition::PrimaryBottomRight,
            PlacementRect {
                left: 800,
                top: 500,
                right: 1_060,
                bottom: 588,
            },
            work,
            260,
            88,
            96,
            &primary,
        );
        assert!(matches!(manual, FloatingPlacement::Custom { .. }));
    }

    #[test]
    fn primary_default_position_is_bottom_right_inside_the_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1_920,
            bottom: 1_040,
        };
        assert_eq!(
            bottom_right_default_position(work, 180, 52, 8),
            (1_732, 980)
        );
        assert_eq!(bottom_right_default_position(work, 2_000, 1_100, 8), (8, 8));
    }
}
