use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};

use crate::diagnose;
use crate::poller::DetectedProviders;
use crate::tray_icon::TrayIconKind;

pub const APP_DIR_NAME: &str = "Gengchou";

pub const POLL_1_MIN: u32 = 60_000;
pub const POLL_2_MIN: u32 = 120_000;
pub const POLL_5_MIN: u32 = 300_000;
pub const POLL_10_MIN: u32 = 600_000;
pub const POLL_15_MIN: u32 = 900_000;
pub const POLL_30_MIN: u32 = 1_800_000;
const SUPPORTED_POLL_INTERVALS: [u32; 6] = [
    POLL_1_MIN,
    POLL_2_MIN,
    POLL_5_MIN,
    POLL_10_MIN,
    POLL_15_MIN,
    POLL_30_MIN,
];

pub(crate) const PLACEMENT_SCHEMA_VERSION: u8 = 2;
/// Version 1 introduced the one-time, all-provider access prompt that
/// replaced the per-provider prompts.
pub(crate) const CONSENT_SCHEMA_VERSION: u8 = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MonitorKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_path: Option<String>,
    pub gdi_device_name: String,
}

impl MonitorKey {
    pub(crate) fn matches(&self, device_path: Option<&str>, gdi_device_name: &str) -> bool {
        match (self.device_path.as_deref(), device_path) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => self.gdi_device_name.eq_ignore_ascii_case(gdi_device_name),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WidgetAnchor {
    TaskbarLeft,
    NotificationArea,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum WidgetPlacement {
    PrimaryLeft,
    PrimaryRight,
    Custom {
        monitor: MonitorKey,
        anchor: WidgetAnchor,
        gap_dip: i32,
    },
}

impl Default for WidgetPlacement {
    fn default() -> Self {
        Self::PrimaryRight
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HorizontalAnchor {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerticalAnchor {
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum FloatingPlacement {
    PrimaryBottomLeft,
    PrimaryBottomRight,
    Custom {
        monitor: MonitorKey,
        horizontal_anchor: HorizontalAnchor,
        vertical_anchor: VerticalAnchor,
        horizontal_gap_dip: i32,
        vertical_gap_dip: i32,
    },
}

impl Default for FloatingPlacement {
    fn default() -> Self {
        Self::PrimaryBottomRight
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WidgetDefaultPosition {
    #[serde(rename = "taskbar_left")]
    PrimaryTaskbarLeft,
    #[default]
    #[serde(rename = "notification_area")]
    PrimaryTaskbarRight,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FloatingDefaultPosition {
    PrimaryBottomLeft,
    #[default]
    PrimaryBottomRight,
}

/// What the last completed update check found.
///
/// Persisted so the version menu entry can state the current situation right
/// after a restart instead of falling back to a generic "check for updates"
/// prompt. Display only: the stored download URL of a release goes stale, so
/// acting on a remembered update re-checks first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub(crate) enum LastUpdateOutcome {
    UpToDate,
    Available { version: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsFile {
    #[serde(default)]
    pub placement_schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget_placement: Option<WidgetPlacement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_placement: Option<FloatingPlacement>,
    #[serde(default)]
    pub tray_offset: i32,
    #[serde(default)]
    pub taskbar_index: usize,
    #[serde(default)]
    pub widget_default_position: WidgetDefaultPosition,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_outcome: Option<LastUpdateOutcome>,
    #[serde(default = "default_widget_visible")]
    pub widget_visible: bool,
    #[serde(default)]
    pub floating_visible: bool,
    #[serde(default = "default_detailed_tray_icons")]
    pub detailed_tray_icons: bool,
    #[serde(default)]
    pub detail_pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_y: Option<i32>,
    #[serde(default)]
    pub floating_default_position: FloatingDefaultPosition,
    #[serde(default = "legacy_show_claude_code")]
    pub show_claude_code: bool,
    #[serde(default = "legacy_show_codex")]
    pub show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    pub show_antigravity: bool,
    #[serde(default = "default_show_grok")]
    pub show_grok: bool,
    #[serde(default)]
    pub allow_claude_credentials: bool,
    #[serde(default)]
    pub allow_codex_credentials: bool,
    #[serde(default)]
    pub allow_antigravity_credentials: bool,
    #[serde(default)]
    pub allow_grok_credentials: bool,
    /// Whether the user has answered the one-time access prompt, and what
    /// they answered. The prompt covers every provider at once; the
    /// `allow_*_credentials` switches above stay per-provider so access can
    /// still be revoked one at a time.
    #[serde(default)]
    pub credential_consent_granted: bool,
    #[serde(default)]
    pub credential_consent_decided: bool,
    /// Bumped when the consent model changes so existing installs can be
    /// migrated exactly once. Absent (0) means the file predates the
    /// one-time prompt.
    #[serde(default)]
    pub consent_schema_version: u8,
    /// Whether the user has already been told this provider exists - either
    /// by it being enabled for them at first run, by a detection balloon, or
    /// by them toggling it themselves. Detection never announces the same
    /// provider twice.
    #[serde(default)]
    pub claude_credential_access_decided: bool,
    #[serde(default)]
    pub codex_credential_access_decided: bool,
    #[serde(default)]
    pub antigravity_credential_access_decided: bool,
    #[serde(default)]
    pub grok_credential_access_decided: bool,
    #[serde(
        default = "legacy_provider_order",
        deserialize_with = "deserialize_provider_order"
    )]
    pub provider_order: Vec<TrayIconKind>,
    #[serde(default)]
    pub notify_session_reset: bool,
    #[serde(default)]
    pub notify_weekly_reset: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            placement_schema_version: PLACEMENT_SCHEMA_VERSION,
            widget_placement: Some(WidgetPlacement::default()),
            floating_placement: Some(FloatingPlacement::default()),
            tray_offset: 0,
            taskbar_index: 0,
            widget_default_position: WidgetDefaultPosition::default(),
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            last_update_outcome: None,
            widget_visible: true,
            floating_visible: false,
            detailed_tray_icons: true,
            detail_pinned: false,
            floating_x: None,
            floating_y: None,
            floating_default_position: FloatingDefaultPosition::default(),
            show_claude_code: false,
            show_codex: true,
            show_antigravity: false,
            show_grok: false,
            allow_claude_credentials: false,
            allow_codex_credentials: false,
            allow_antigravity_credentials: false,
            allow_grok_credentials: false,
            credential_consent_granted: false,
            credential_consent_decided: false,
            consent_schema_version: CONSENT_SCHEMA_VERSION,
            claude_credential_access_decided: false,
            codex_credential_access_decided: false,
            antigravity_credential_access_decided: false,
            grok_credential_access_decided: false,
            provider_order: default_provider_order(),
            notify_session_reset: false,
            notify_weekly_reset: false,
        }
    }
}

/// Drop provider names this build does not know instead of rejecting the file.
///
/// A settings file written by a newer version can name a provider that does not
/// exist in this one. serde's default for an unknown enum variant is an error,
/// and an error on any field fails the whole file: the loader then falls back
/// to defaults, and the next normalized save overwrites the user's layout,
/// language, and provider selection with them. Dropping the unknown entry is
/// safe because `normalize_provider_order` re-appends every kind missing from a
/// stored order.
fn deserialize_provider_order<'de, D>(deserializer: D) -> Result<Vec<TrayIconKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MaybeKind {
        Known(TrayIconKind),
        Unknown(serde::de::IgnoredAny),
    }

    Ok(Vec::<MaybeKind>::deserialize(deserializer)?
        .into_iter()
        .filter_map(|entry| match entry {
            MaybeKind::Known(kind) => Some(kind),
            MaybeKind::Unknown(_) => None,
        })
        .collect())
}

pub fn default_provider_order() -> Vec<TrayIconKind> {
    vec![
        TrayIconKind::Codex,
        TrayIconKind::Claude,
        TrayIconKind::Antigravity,
        TrayIconKind::Grok,
    ]
}

fn legacy_provider_order() -> Vec<TrayIconKind> {
    vec![
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
        TrayIconKind::Grok,
    ]
}

fn default_poll_interval() -> u32 {
    POLL_5_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_detailed_tray_icons() -> bool {
    true
}

fn legacy_show_claude_code() -> bool {
    true
}

fn legacy_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
}

fn default_show_grok() -> bool {
    false
}

/// The provider-visibility slice of the settings, lifted out so detection can
/// be decided as a pure function and applied to either the persisted file or
/// the live application state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProviderVisibility {
    pub show_claude_code: bool,
    pub show_codex: bool,
    pub show_antigravity: bool,
    pub show_grok: bool,
    pub allow_claude_credentials: bool,
    pub allow_codex_credentials: bool,
    pub allow_antigravity_credentials: bool,
    pub allow_grok_credentials: bool,
    /// Whether the user has already been told this provider exists.
    pub claude_announced: bool,
    pub codex_announced: bool,
    pub antigravity_announced: bool,
    pub grok_announced: bool,
}

/// Turn on exactly the providers detected right after the user granted
/// access.
///
/// Assignment rather than a merge: the default `show_codex` is only a
/// placeholder for machines where nothing is found, so a machine with just
/// Claude must not also show an empty Codex row.
///
/// When nothing is detected the placeholder stays visible *and polled*. That
/// costs one local credential read per pass and nothing on the network - the
/// poll reports "not signed in", which parks polling and arms the existing
/// credential watch, so the first sign-in is picked up without any extra
/// machinery.
pub(crate) fn apply_first_run_detection(
    visibility: &mut ProviderVisibility,
    detected: DetectedProviders,
) {
    visibility.show_claude_code = detected.claude;
    visibility.show_codex = detected.codex;
    visibility.show_antigravity = detected.antigravity;
    visibility.show_grok = detected.grok;
    visibility.allow_claude_credentials = detected.claude;
    visibility.allow_codex_credentials = detected.codex;
    visibility.allow_antigravity_credentials = detected.antigravity;
    visibility.allow_grok_credentials = detected.grok;
    // Enabling a provider is itself the announcement, so it is never also
    // announced by a balloon later.
    visibility.claude_announced |= detected.claude;
    visibility.codex_announced |= detected.codex;
    visibility.antigravity_announced |= detected.antigravity;
    visibility.grok_announced |= detected.grok;
    if !detected.any() {
        visibility.show_codex = true;
        visibility.allow_codex_credentials = true;
    }
}

/// Turn on everything detected, without turning anything off.
///
/// For the explicit "detect providers again" menu action. Additive rather
/// than assigning, because the user asked for a sweep, not a reset - a
/// provider they deliberately keep visible stays visible even if its
/// credential is currently unreadable. Unlike the periodic sweep this does
/// change what is shown: the user asked for it, so it is not a surprise.
pub(crate) fn apply_manual_detection(
    visibility: &mut ProviderVisibility,
    detected: DetectedProviders,
) {
    visibility.show_claude_code |= detected.claude;
    visibility.show_codex |= detected.codex;
    visibility.show_antigravity |= detected.antigravity;
    visibility.show_grok |= detected.grok;
    visibility.allow_claude_credentials |= detected.claude;
    visibility.allow_codex_credentials |= detected.codex;
    visibility.allow_antigravity_credentials |= detected.antigravity;
    visibility.allow_grok_credentials |= detected.grok;
    visibility.claude_announced |= detected.claude;
    visibility.codex_announced |= detected.codex;
    visibility.antigravity_announced |= detected.antigravity;
    visibility.grok_announced |= detected.grok;
}

/// Providers that appeared after first run and have never been announced.
///
/// Deliberately does not enable anything: a later discovery only earns one
/// balloon, because changing what the widget shows while the user is not
/// looking is worse than making them click once.
pub(crate) fn take_detection_announcements(
    visibility: &mut ProviderVisibility,
    detected: DetectedProviders,
) -> Vec<TrayIconKind> {
    let mut announcements = Vec::new();
    for (kind, detected, shown, announced) in [
        (
            TrayIconKind::Claude,
            detected.claude,
            visibility.show_claude_code,
            &mut visibility.claude_announced,
        ),
        (
            TrayIconKind::Codex,
            detected.codex,
            visibility.show_codex,
            &mut visibility.codex_announced,
        ),
        (
            TrayIconKind::Antigravity,
            detected.antigravity,
            visibility.show_antigravity,
            &mut visibility.antigravity_announced,
        ),
        (
            TrayIconKind::Grok,
            detected.grok,
            visibility.show_grok,
            &mut visibility.grok_announced,
        ),
    ] {
        if detected && !shown && !*announced {
            *announced = true;
            announcements.push(kind);
        }
    }
    announcements
}

fn normalize_provider_order(configured: &[TrayIconKind]) -> Vec<TrayIconKind> {
    let mut normalized = Vec::with_capacity(3);
    for kind in configured.iter().chain(default_provider_order().iter()) {
        if !normalized.contains(kind) {
            normalized.push(*kind);
        }
    }
    normalized
}

pub(crate) fn normalize(settings: &mut SettingsFile) -> Vec<&'static str> {
    let mut repaired = Vec::new();
    if (settings.widget_placement.is_some() || settings.floating_placement.is_some())
        && settings.placement_schema_version != PLACEMENT_SCHEMA_VERSION
    {
        settings.placement_schema_version = PLACEMENT_SCHEMA_VERSION;
        repaired.push("placement_schema_version");
    }
    if let Some(WidgetPlacement::Custom { gap_dip, .. }) = settings.widget_placement.as_mut() {
        if *gap_dip < 0 {
            *gap_dip = 0;
            repaired.push("widget_placement");
        }
    }
    if let Some(FloatingPlacement::Custom {
        horizontal_gap_dip,
        vertical_gap_dip,
        ..
    }) = settings.floating_placement.as_mut()
    {
        if *horizontal_gap_dip < 0 || *vertical_gap_dip < 0 {
            *horizontal_gap_dip = (*horizontal_gap_dip).max(0);
            *vertical_gap_dip = (*vertical_gap_dip).max(0);
            repaired.push("floating_placement");
        }
    }
    if settings.tray_offset < 0 {
        settings.tray_offset = 0;
        repaired.push("tray_offset");
    }
    if !SUPPORTED_POLL_INTERVALS.contains(&settings.poll_interval_ms) {
        settings.poll_interval_ms = default_poll_interval();
        repaired.push("poll_interval_ms");
    }
    if !settings.show_claude_code
        && !settings.show_codex
        && !settings.show_antigravity
        && !settings.show_grok
    {
        settings.show_codex = true;
        repaired.push("enabled_providers");
    }
    // Existing installs already made their choices under the per-provider
    // prompts. Carry those choices forward untouched and mark every provider
    // as already announced, so the new one-time prompt and the detector never
    // second-guess a decision the user already made. Runs exactly once: the
    // schema bump below closes the door behind it.
    if settings.consent_schema_version < 1 {
        settings.credential_consent_granted = settings.allow_claude_credentials
            || settings.allow_codex_credentials
            || settings.allow_antigravity_credentials;
        settings.credential_consent_decided = true;
        settings.claude_credential_access_decided = true;
        settings.codex_credential_access_decided = true;
        settings.antigravity_credential_access_decided = true;
    }
    // Grok joined after the one-time prompt shipped, so an install that
    // already answered it never got the chance to say anything about Grok.
    // The prompt asks about reading AI CLI credentials as a whole, so the
    // answer carries over rather than being asked again. Announcement stays
    // unset on purpose: the detector still owes the user one balloon before
    // Grok may appear on any surface.
    if settings.consent_schema_version < 2 {
        settings.allow_grok_credentials = settings.credential_consent_granted;
    }
    if settings.consent_schema_version < CONSENT_SCHEMA_VERSION {
        settings.consent_schema_version = CONSENT_SCHEMA_VERSION;
        repaired.push("credential_consent");
    }
    let provider_order = normalize_provider_order(&settings.provider_order);
    if provider_order != settings.provider_order {
        settings.provider_order = provider_order;
        repaired.push("provider_order");
    }
    if settings
        .language
        .as_deref()
        .is_some_and(|language| language.trim().is_empty())
    {
        settings.language = None;
        repaired.push("language");
    }
    repaired
}

static PERSISTENCE_WARNING: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn settings_path_for(app_dir_name: &str) -> io::Result<PathBuf> {
    Ok(app_data_dir(app_dir_name)?.join("settings.json"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppDataDirSource {
    AppData,
    Config,
    LocalAppData,
}

fn resolve_app_data_dir(
    app_dir_name: &str,
    appdata: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
) -> io::Result<(PathBuf, AppDataDirSource)> {
    if let Some(appdata) = appdata.filter(|value| !value.as_os_str().is_empty()) {
        return Ok((appdata.join(app_dir_name), AppDataDirSource::AppData));
    }
    if let Some(config_dir) = config_dir.filter(|value| !value.as_os_str().is_empty()) {
        return Ok((config_dir.join(app_dir_name), AppDataDirSource::Config));
    }
    if let Some(local_app_data) = local_app_data.filter(|value| !value.as_os_str().is_empty()) {
        return Ok((
            local_app_data.join(app_dir_name),
            AppDataDirSource::LocalAppData,
        ));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "APPDATA, the Windows config directory, and LOCALAPPDATA are unavailable",
    ))
}

fn app_data_dir(app_dir_name: &str) -> io::Result<PathBuf> {
    let (path, source) = resolve_app_data_dir(
        app_dir_name,
        std::env::var_os("APPDATA").map(PathBuf::from),
        dirs::config_dir(),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    )?;
    match source {
        AppDataDirSource::AppData => {}
        AppDataDirSource::Config => diagnose::log(format!(
            "APPDATA unavailable; using config directory fallback {}",
            path.display()
        )),
        AppDataDirSource::LocalAppData => diagnose::log(format!(
            "APPDATA and config directory unavailable; using local fallback {}",
            path.display()
        )),
    }
    Ok(path)
}

pub fn app_data_file(name: &str) -> io::Result<PathBuf> {
    Ok(app_data_dir(APP_DIR_NAME)?.join(name))
}

pub(crate) fn record_persistence_warning(context: &str, error: &dyn std::fmt::Display) {
    let warning = format!("{context}: {error}");
    diagnose::log(format!("persistence warning queued: {warning}"));
    let slot = PERSISTENCE_WARNING.get_or_init(|| Mutex::new(None));
    let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(warning);
    }
}

pub(crate) fn take_persistence_warning() -> Option<String> {
    PERSISTENCE_WARNING.get().and_then(|slot| {
        slot.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    })
}

fn read_settings_content() -> Option<String> {
    let current_path = match settings_path_for(APP_DIR_NAME) {
        Ok(path) => path,
        Err(error) => {
            record_persistence_warning("Unable to locate the settings directory", &error);
            return None;
        }
    };
    match std::fs::read_to_string(&current_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            record_persistence_warning("Unable to read settings", &error);
            diagnose::log_error(
                &format!("settings read failed path={}", current_path.display()),
                &error,
            );
            None
        }
    }
}

pub(crate) fn load() -> SettingsFile {
    let Some(content) = read_settings_content() else {
        return SettingsFile::default();
    };
    let mut settings: SettingsFile = match serde_json::from_str(&content) {
        Ok(settings) => settings,
        Err(error) => {
            diagnose::log(format!(
                "settings parse failed; using defaults without overwriting the file: {error}"
            ));
            return SettingsFile::default();
        }
    };
    let repaired = normalize(&mut settings);
    if !repaired.is_empty() {
        diagnose::log(format!("settings normalized fields={}", repaired.join(",")));
    }
    if !repaired.is_empty() {
        if let Err(error) = save(&settings) {
            record_persistence_warning("Unable to save normalized settings", &error);
            diagnose::log_error("settings normalization save failed", &error);
        }
    }
    settings
}

pub(crate) fn write_file_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let write_result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {}
            Err(cleanup_error) => diagnose::log_error(
                &format!("atomic write cleanup failed path={}", tmp.display()),
                cleanup_error,
            ),
        }
        return Err(error);
    }
    let replace_result = if path.exists() {
        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let tmp_wide: Vec<u16> = tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            ReplaceFileW(
                PCWSTR::from_raw(path_wide.as_ptr()),
                PCWSTR::from_raw(tmp_wide.as_ptr()),
                PCWSTR::null(),
                REPLACE_FILE_FLAGS(0),
                None,
                None,
            )
        }
        .map_err(io::Error::other)
    } else {
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = replace_result {
        if let Err(cleanup_error) = std::fs::remove_file(&tmp) {
            if cleanup_error.kind() != io::ErrorKind::NotFound {
                diagnose::log_error(
                    &format!("atomic write cleanup failed path={}", tmp.display()),
                    cleanup_error,
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(crate) fn save(settings: &SettingsFile) -> io::Result<()> {
    let json = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    let path = settings_path_for(APP_DIR_NAME)?;
    write_file_atomic(&path, &json)
        .map_err(|error| io::Error::new(error.kind(), format!("path={}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider name from a newer build must not take the whole settings file
    /// down with it: the loader falls back to defaults on a parse error, and the
    /// next save would then overwrite the user's layout and provider selection.
    #[test]
    fn unknown_provider_names_are_dropped_instead_of_failing_the_file() {
        let json = r#"{
            "provider_order": ["codex", "not_a_provider", "claude"],
            "poll_interval_ms": 120000,
            "show_antigravity": true
        }"#;
        let mut settings: SettingsFile =
            serde_json::from_str(json).expect("an unknown provider must not fail the whole file");

        // Everything else in the file survives, which is the point.
        assert_eq!(settings.poll_interval_ms, 120_000);
        assert!(settings.show_antigravity);
        assert_eq!(
            settings.provider_order,
            vec![TrayIconKind::Codex, TrayIconKind::Claude]
        );

        // Normalization then restores the providers this build does know.
        normalize(&mut settings);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Antigravity,
                TrayIconKind::Grok,
            ]
        );
    }

    #[test]
    fn an_entirely_unknown_provider_order_still_loads() {
        let json = r#"{"provider_order": ["mystery", 7, null]}"#;
        let mut settings: SettingsFile =
            serde_json::from_str(json).expect("unknown entries must not fail the file");
        assert!(settings.provider_order.is_empty());
        normalize(&mut settings);
        assert_eq!(settings.provider_order, default_provider_order());
    }

    #[test]
    fn provider_order_is_deduplicated_and_completed() {
        assert_eq!(
            normalize_provider_order(&[TrayIconKind::Codex, TrayIconKind::Codex]),
            vec![
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Antigravity,
                TrayIconKind::Grok,
            ]
        );
    }

    #[test]
    fn new_install_starts_with_only_codex_shown() {
        let settings = SettingsFile::default();

        assert!(!settings.show_claude_code);
        assert!(settings.show_codex);
        assert!(!settings.show_antigravity);
        assert!(!settings.show_grok);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Antigravity,
                TrayIconKind::Grok,
            ]
        );
    }

    #[test]
    fn unsafe_or_unsupported_values_are_repaired() {
        let mut settings = SettingsFile {
            tray_offset: -1,
            poll_interval_ms: 0,
            language: Some("  ".to_string()),
            show_claude_code: false,
            show_codex: false,
            show_antigravity: false,
            provider_order: vec![TrayIconKind::Antigravity, TrayIconKind::Antigravity],
            ..SettingsFile::default()
        };

        let repaired = normalize(&mut settings);

        assert_eq!(settings.tray_offset, 0);
        assert_eq!(settings.poll_interval_ms, POLL_5_MIN);
        assert_eq!(settings.language, None);
        assert!(settings.show_codex);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Antigravity,
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Grok,
            ]
        );
        assert_eq!(repaired.len(), 5);
    }

    #[test]
    fn credentials_require_explicit_consent_by_default() {
        let settings = SettingsFile::default();

        assert!(!settings.allow_claude_credentials);
        assert!(!settings.allow_codex_credentials);
        assert!(!settings.allow_antigravity_credentials);
        assert!(!settings.claude_credential_access_decided);
        assert!(!settings.codex_credential_access_decided);
        assert!(!settings.antigravity_credential_access_decided);
    }

    #[test]
    fn all_refresh_menu_intervals_are_preserved() {
        for poll_interval_ms in SUPPORTED_POLL_INTERVALS {
            let mut settings = SettingsFile {
                poll_interval_ms,
                ..SettingsFile::default()
            };

            assert!(!normalize(&mut settings).contains(&"poll_interval_ms"));
            assert_eq!(settings.poll_interval_ms, poll_interval_ms);
        }
    }

    #[test]
    fn removed_one_hour_interval_migrates_to_five_minute_default() {
        let mut settings = SettingsFile {
            poll_interval_ms: 60 * 60 * 1_000,
            ..SettingsFile::default()
        };

        let repaired = normalize(&mut settings);

        assert_eq!(settings.poll_interval_ms, POLL_5_MIN);
        assert_eq!(repaired, vec!["poll_interval_ms"]);
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(serde_json::from_str::<SettingsFile>("{not-json").is_err());
    }

    #[test]
    fn app_data_directory_has_safe_ordered_fallbacks() {
        let appdata = PathBuf::from(r"C:\Roaming");
        let config = PathBuf::from(r"C:\Config");
        let local = PathBuf::from(r"C:\Local");

        assert_eq!(
            resolve_app_data_dir(
                "Gengchou",
                Some(appdata.clone()),
                Some(config.clone()),
                Some(local.clone())
            )
            .unwrap(),
            (appdata.join("Gengchou"), AppDataDirSource::AppData)
        );
        assert_eq!(
            resolve_app_data_dir("Gengchou", None, Some(config.clone()), Some(local.clone()))
                .unwrap(),
            (config.join("Gengchou"), AppDataDirSource::Config)
        );
        assert_eq!(
            resolve_app_data_dir("Gengchou", None, None, Some(local.clone())).unwrap(),
            (local.join("Gengchou"), AppDataDirSource::LocalAppData)
        );
        assert_eq!(
            resolve_app_data_dir("Gengchou", None, Some(PathBuf::new()), Some(local.clone()))
                .unwrap(),
            (local.join("Gengchou"), AppDataDirSource::LocalAppData)
        );
        assert!(resolve_app_data_dir("Gengchou", None, None, None).is_err());
    }

    /// Settings written before the one-time prompt existed must keep the
    /// provider choices their owner made and never see the prompt. Their
    /// `*_credential_access_decided` fields deserialize as false, so keying
    /// the migration on those would misread them as a fresh install and let
    /// detection re-enable providers the user had turned off.
    #[test]
    fn settings_predating_the_one_time_prompt_keep_their_choices() {
        let mut settings: SettingsFile = serde_json::from_str(
            r#"{
                "show_claude_code": true,
                "show_codex": false,
                "allow_claude_credentials": true
            }"#,
        )
        .expect("older settings should remain readable");
        assert_eq!(settings.consent_schema_version, 0);

        let repaired = normalize(&mut settings);

        assert!(repaired.contains(&"credential_consent"));
        assert!(settings.credential_consent_decided);
        assert!(settings.credential_consent_granted);
        assert!(settings.show_claude_code);
        assert!(!settings.show_codex);
        // Every provider counts as already announced, so the detector stays
        // quiet about choices this user already made.
        assert!(settings.claude_credential_access_decided);
        assert!(settings.codex_credential_access_decided);
        assert!(settings.antigravity_credential_access_decided);
    }

    /// An install whose owner declined every provider must not be re-prompted
    /// either, and must not silently gain access.
    #[test]
    fn settings_that_declined_every_provider_migrate_to_a_declined_consent() {
        let mut settings: SettingsFile = serde_json::from_str(r#"{"show_codex": true}"#)
            .expect("older settings should remain readable");

        normalize(&mut settings);

        assert!(settings.credential_consent_decided);
        assert!(!settings.credential_consent_granted);
    }

    /// `load` runs `normalize` unconditionally and rewrites the file whenever
    /// anything was repaired, so a second pass must be a no-op.
    #[test]
    fn consent_migration_runs_exactly_once() {
        let mut settings: SettingsFile =
            serde_json::from_str(r#"{"allow_codex_credentials": true}"#).expect("readable");

        normalize(&mut settings);
        let after_first = settings.clone();
        let repaired = normalize(&mut settings);

        assert!(!repaired.contains(&"credential_consent"));
        assert_eq!(settings, after_first);
    }

    /// A fresh install must reach the prompt, so the default carries the
    /// current schema and an unanswered consent.
    #[test]
    fn fresh_settings_are_not_migrated_and_still_need_an_answer() {
        let mut settings = SettingsFile::default();
        assert_eq!(settings.consent_schema_version, CONSENT_SCHEMA_VERSION);
        assert!(!settings.credential_consent_decided);

        let repaired = normalize(&mut settings);

        assert!(!repaired.contains(&"credential_consent"));
        assert!(!settings.credential_consent_decided);
    }

    #[test]
    fn first_run_detection_enables_exactly_what_was_found() {
        let mut visibility = ProviderVisibility::default();
        apply_first_run_detection(
            &mut visibility,
            DetectedProviders {
                claude: true,
                codex: false,
                antigravity: true,
                grok: false,
            },
        );

        assert!(visibility.show_claude_code && visibility.allow_claude_credentials);
        assert!(visibility.show_antigravity && visibility.allow_antigravity_credentials);
        // The default Codex placeholder must not survive on a machine that
        // has no Codex - it would be an empty row the user cannot explain.
        assert!(!visibility.show_codex && !visibility.allow_codex_credentials);
        assert!(visibility.claude_announced && visibility.antigravity_announced);
        assert!(!visibility.codex_announced);
    }

    /// With nothing installed the widget still needs one row, and that row
    /// has to be polled so the credential watch notices the first sign-in.
    #[test]
    fn first_run_detection_keeps_a_polled_placeholder_when_nothing_is_found() {
        let mut visibility = ProviderVisibility::default();
        apply_first_run_detection(&mut visibility, DetectedProviders::default());

        assert!(visibility.show_codex);
        assert!(visibility.allow_codex_credentials);
        assert!(!visibility.show_claude_code);
        assert!(!visibility.show_antigravity);
        // Nothing was found, so nothing has been announced.
        assert!(!visibility.codex_announced);
    }

    /// The periodic sweep may notify, but must never change what is on screen
    /// while the user is not looking.
    #[test]
    fn periodic_detection_announces_without_changing_visibility() {
        let mut visibility = ProviderVisibility {
            show_codex: true,
            allow_codex_credentials: true,
            codex_announced: true,
            ..Default::default()
        };
        let before = visibility;

        let announced = take_detection_announcements(
            &mut visibility,
            DetectedProviders {
                claude: true,
                codex: true,
                antigravity: false,
                grok: false,
            },
        );

        assert_eq!(announced, vec![TrayIconKind::Claude]);
        assert!(!visibility.show_claude_code);
        assert!(!visibility.allow_claude_credentials);
        assert_eq!(
            ProviderVisibility {
                claude_announced: before.claude_announced,
                ..visibility
            },
            ProviderVisibility {
                claude_announced: before.claude_announced,
                ..before
            }
        );
    }

    #[test]
    fn periodic_detection_announces_each_provider_only_once() {
        let mut visibility = ProviderVisibility::default();
        let detected = DetectedProviders {
            claude: true,
            ..Default::default()
        };

        assert_eq!(
            take_detection_announcements(&mut visibility, detected),
            vec![TrayIconKind::Claude]
        );
        assert!(take_detection_announcements(&mut visibility, detected).is_empty());
    }

    /// The startup sweep exists for exactly this install: it upgraded into a
    /// version that added a provider, so migration granted access but left the
    /// announcement unpaid. Providers it had already decided on stay quiet
    /// even when they are detected too.
    #[test]
    fn a_migrated_install_still_owes_a_balloon_for_a_provider_an_update_added() {
        let mut settings: SettingsFile = serde_json::from_str(
            r#"{
                "consent_schema_version": 1,
                "credential_consent_granted": true,
                "credential_consent_decided": true,
                "claude_credential_access_decided": true,
                "codex_credential_access_decided": true,
                "antigravity_credential_access_decided": true,
                "show_codex": true,
                "allow_codex_credentials": true
            }"#,
        )
        .expect("settings from before Grok shipped should remain readable");

        normalize(&mut settings);

        assert!(settings.allow_grok_credentials);
        assert!(!settings.show_grok);
        assert!(!settings.grok_credential_access_decided);

        let mut visibility = ProviderVisibility {
            show_claude_code: settings.show_claude_code,
            show_codex: settings.show_codex,
            show_antigravity: settings.show_antigravity,
            show_grok: settings.show_grok,
            allow_claude_credentials: settings.allow_claude_credentials,
            allow_codex_credentials: settings.allow_codex_credentials,
            allow_antigravity_credentials: settings.allow_antigravity_credentials,
            allow_grok_credentials: settings.allow_grok_credentials,
            claude_announced: settings.claude_credential_access_decided,
            codex_announced: settings.codex_credential_access_decided,
            antigravity_announced: settings.antigravity_credential_access_decided,
            grok_announced: settings.grok_credential_access_decided,
        };

        let announced = take_detection_announcements(
            &mut visibility,
            DetectedProviders {
                claude: true,
                codex: true,
                antigravity: false,
                grok: true,
            },
        );

        assert_eq!(announced, vec![TrayIconKind::Grok]);
        assert!(!visibility.show_grok);
    }

    /// Why the startup sweep must not run on a fresh install: it is only
    /// silent once first-run detection has finished claiming the providers it
    /// enables, so running the two concurrently would announce providers that
    /// pass is in the middle of turning on.
    #[test]
    fn first_run_detection_leaves_a_following_sweep_nothing_to_announce() {
        let mut visibility = ProviderVisibility::default();
        let detected = DetectedProviders {
            claude: true,
            codex: false,
            antigravity: false,
            grok: true,
        };

        apply_first_run_detection(&mut visibility, detected);

        assert!(take_detection_announcements(&mut visibility, detected).is_empty());
    }

    /// A provider the user hid by hand counts as announced, so the sweep
    /// leaves it alone instead of nagging.
    #[test]
    fn periodic_detection_stays_quiet_about_providers_the_user_turned_off() {
        let mut visibility = ProviderVisibility {
            claude_announced: true,
            ..Default::default()
        };

        let announced = take_detection_announcements(
            &mut visibility,
            DetectedProviders {
                claude: true,
                ..Default::default()
            },
        );

        assert!(announced.is_empty());
    }

    /// The menu action exists for installs the sweep will never announce to
    /// (migrated ones mark every provider as announced), so it has to enable
    /// rather than notify - and must not undo a deliberate choice.
    #[test]
    fn manual_detection_enables_findings_without_hiding_anything() {
        let mut visibility = ProviderVisibility {
            show_antigravity: true,
            allow_antigravity_credentials: true,
            claude_announced: true,
            codex_announced: true,
            antigravity_announced: true,
            ..Default::default()
        };

        apply_manual_detection(
            &mut visibility,
            DetectedProviders {
                claude: true,
                codex: false,
                antigravity: false,
                grok: false,
            },
        );

        assert!(visibility.show_claude_code && visibility.allow_claude_credentials);
        assert!(!visibility.show_codex);
        // Kept, even though this pass could not read its credential.
        assert!(visibility.show_antigravity && visibility.allow_antigravity_credentials);
    }

    #[test]
    fn older_settings_default_floating_monitor_to_hidden() {
        let settings: SettingsFile = serde_json::from_str(
            r#"{
                "widget_visible": true,
                "show_claude_code": true,
                "provider_order": ["claude", "codex", "antigravity"]
            }"#,
        )
        .expect("older settings should remain readable");

        assert!(!settings.floating_visible);
        assert!(settings.detailed_tray_icons);
        assert!(!settings.detail_pinned);
        assert_eq!(settings.floating_x, None);
        assert_eq!(settings.floating_y, None);
        assert!(!settings.allow_claude_credentials);
        assert!(!settings.allow_codex_credentials);
        assert!(!settings.allow_antigravity_credentials);
        assert!(!settings.claude_credential_access_decided);
        assert!(!settings.codex_credential_access_decided);
        assert!(!settings.antigravity_credential_access_decided);
        assert_eq!(
            settings.widget_default_position,
            WidgetDefaultPosition::PrimaryTaskbarRight
        );
        assert_eq!(
            settings.floating_default_position,
            FloatingDefaultPosition::PrimaryBottomRight
        );
        assert!(settings.show_claude_code);
        assert!(!settings.show_codex);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Claude,
                TrayIconKind::Codex,
                TrayIconKind::Antigravity,
            ]
        );
    }

    /// Older files have no recorded outcome, and a file that does must round
    /// trip - the version menu entry reads from it on every launch.
    #[test]
    fn the_last_update_outcome_round_trips_and_defaults_to_unknown() {
        let older: SettingsFile =
            serde_json::from_str(r#"{"last_update_check_unix": 1}"#).expect("readable");
        assert_eq!(older.last_update_outcome, None);
        assert!(!serde_json::to_string(&older)
            .unwrap()
            .contains("last_update_outcome"));

        for outcome in [
            LastUpdateOutcome::UpToDate,
            LastUpdateOutcome::Available {
                version: "9.9.9".to_string(),
            },
        ] {
            let settings = SettingsFile {
                last_update_outcome: Some(outcome.clone()),
                ..SettingsFile::default()
            };
            let json = serde_json::to_string(&settings).unwrap();
            let parsed: SettingsFile = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.last_update_outcome, Some(outcome));
        }
    }

    /// Retired settings must not break older files. `notify_claude_cli_update`
    /// joined them once the CLI-update balloon became a disclosure rather than
    /// a preference: it reports that Gengchou changed the user's CLI, so it is
    /// no longer something to switch off.
    #[test]
    fn retired_settings_are_ignored_and_not_reserialized() {
        let settings: SettingsFile = serde_json::from_str(
            r#"{
                "claude_auto_recovery": false,
                "notify_claude_cli_update": false
            }"#,
        )
        .expect("retired settings fields should remain readable");

        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("claude_auto_recovery"));
        assert!(!serialized.contains("notify_claude_cli_update"));
    }

    #[test]
    fn default_positions_round_trip_with_stable_names() {
        let settings = SettingsFile {
            widget_default_position: WidgetDefaultPosition::PrimaryTaskbarLeft,
            floating_default_position: FloatingDefaultPosition::PrimaryBottomLeft,
            detail_pinned: true,
            ..SettingsFile::default()
        };

        let json = serde_json::to_string(&settings).expect("serialize settings");
        assert!(json.contains("\"widget_default_position\":\"taskbar_left\""));
        assert!(json.contains("\"floating_default_position\":\"primary_bottom_left\""));
        assert!(json.contains("\"detail_pinned\":true"));

        let restored: SettingsFile = serde_json::from_str(&json).expect("deserialize settings");
        assert_eq!(restored, settings);
    }

    #[test]
    fn semantic_placements_round_trip_with_stable_monitor_identity() {
        let monitor = MonitorKey {
            device_path: Some("MONITOR\\ACME123".to_string()),
            gdi_device_name: r"\\.\DISPLAY2".to_string(),
        };
        let settings = SettingsFile {
            widget_placement: Some(WidgetPlacement::Custom {
                monitor: monitor.clone(),
                anchor: WidgetAnchor::TaskbarLeft,
                gap_dip: 24,
            }),
            floating_placement: Some(FloatingPlacement::Custom {
                monitor,
                horizontal_anchor: HorizontalAnchor::Right,
                vertical_anchor: VerticalAnchor::Bottom,
                horizontal_gap_dip: 18,
                vertical_gap_dip: 12,
            }),
            ..SettingsFile::default()
        };

        let json = serde_json::to_string(&settings).expect("serialize settings");
        assert!(json.contains("\"placement_schema_version\":2"));
        assert!(json.contains("\"mode\":\"custom\""));
        assert!(json.contains("\"anchor\":\"taskbar_left\""));
        assert!(json.contains("\"horizontal_anchor\":\"right\""));

        let restored: SettingsFile = serde_json::from_str(&json).expect("deserialize settings");
        assert_eq!(restored, settings);
    }

    #[test]
    fn monitor_key_prefers_device_path_and_falls_back_to_gdi_name() {
        let stable = MonitorKey {
            device_path: Some("MONITOR\\ACME123".to_string()),
            gdi_device_name: r"\\.\DISPLAY2".to_string(),
        };
        assert!(stable.matches(Some("monitor\\acme123"), r"\\.\DISPLAY9"));
        assert!(!stable.matches(Some("MONITOR\\OTHER"), r"\\.\DISPLAY2"));
        assert!(stable.matches(None, r"\\.\display2"));

        let legacy = MonitorKey {
            device_path: None,
            gdi_device_name: r"\\.\DISPLAY3".to_string(),
        };
        assert!(legacy.matches(Some("MONITOR\\ANY"), r"\\.\display3"));
    }

    #[test]
    fn semantic_placement_gaps_are_repaired_without_touching_legacy_migration() {
        let monitor = MonitorKey {
            device_path: None,
            gdi_device_name: "DISPLAY2".to_string(),
        };
        let mut settings = SettingsFile {
            widget_placement: Some(WidgetPlacement::Custom {
                monitor: monitor.clone(),
                anchor: WidgetAnchor::NotificationArea,
                gap_dip: -5,
            }),
            floating_placement: Some(FloatingPlacement::Custom {
                monitor,
                horizontal_anchor: HorizontalAnchor::Left,
                vertical_anchor: VerticalAnchor::Top,
                horizontal_gap_dip: -2,
                vertical_gap_dip: -3,
            }),
            ..SettingsFile::default()
        };

        let repaired = normalize(&mut settings);

        assert!(repaired.contains(&"widget_placement"));
        assert!(repaired.contains(&"floating_placement"));
        assert!(matches!(
            settings.widget_placement,
            Some(WidgetPlacement::Custom { gap_dip: 0, .. })
        ));
        assert!(matches!(
            settings.floating_placement,
            Some(FloatingPlacement::Custom {
                horizontal_gap_dip: 0,
                vertical_gap_dip: 0,
                ..
            })
        ));

        let legacy: SettingsFile = serde_json::from_str(r#"{"tray_offset":42}"#).unwrap();
        assert_eq!(legacy.placement_schema_version, 0);
        assert!(legacy.widget_placement.is_none());
        assert!(legacy.floating_placement.is_none());
    }

    #[test]
    fn credential_consent_round_trips() {
        let settings = SettingsFile {
            allow_claude_credentials: true,
            allow_codex_credentials: false,
            allow_antigravity_credentials: true,
            claude_credential_access_decided: true,
            codex_credential_access_decided: true,
            antigravity_credential_access_decided: true,
            ..SettingsFile::default()
        };

        let json = serde_json::to_string(&settings).expect("settings should serialize");
        let decoded: SettingsFile =
            serde_json::from_str(&json).expect("settings should deserialize");

        assert!(decoded.allow_claude_credentials);
        assert!(!decoded.allow_codex_credentials);
        assert!(decoded.allow_antigravity_credentials);
        assert!(decoded.claude_credential_access_decided);
        assert!(decoded.codex_credential_access_decided);
        assert!(decoded.antigravity_credential_access_decided);
    }

    #[test]
    fn atomic_write_creates_parent_and_replaces_existing_file() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gengchou-settings-test-{}-{nonce}",
            std::process::id(),
        ));
        let path = root.join("nested").join("settings.json");

        write_file_atomic(&path, "first").expect("initial atomic write");
        write_file_atomic(&path, "second").expect("replacement atomic write");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!path.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_write_reports_an_unwritable_parent() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gengchou-settings-error-test-{}-{nonce}",
            std::process::id(),
        ));
        std::fs::write(&root, "not a directory").unwrap();

        let error = write_file_atomic(&root.join("settings.json"), "{}")
            .expect_err("a file cannot be used as the parent directory");

        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_file(root);
    }
}
