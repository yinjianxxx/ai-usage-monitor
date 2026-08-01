use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};

use crate::diagnose;
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
    #[serde(default = "default_show_claude_code")]
    pub show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    pub show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    pub show_antigravity: bool,
    #[serde(default)]
    pub allow_claude_credentials: bool,
    #[serde(default)]
    pub allow_codex_credentials: bool,
    #[serde(default)]
    pub allow_antigravity_credentials: bool,
    #[serde(default)]
    pub claude_credential_access_decided: bool,
    #[serde(default)]
    pub codex_credential_access_decided: bool,
    #[serde(default)]
    pub antigravity_credential_access_decided: bool,
    #[serde(default = "default_provider_order")]
    pub provider_order: Vec<TrayIconKind>,
    #[serde(default)]
    pub notify_session_reset: bool,
    #[serde(default)]
    pub notify_weekly_reset: bool,
    #[serde(default = "default_enabled")]
    pub notify_claude_cli_update: bool,
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
            widget_visible: true,
            floating_visible: false,
            detailed_tray_icons: true,
            detail_pinned: false,
            floating_x: None,
            floating_y: None,
            floating_default_position: FloatingDefaultPosition::default(),
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            allow_claude_credentials: false,
            allow_codex_credentials: false,
            allow_antigravity_credentials: false,
            claude_credential_access_decided: false,
            codex_credential_access_decided: false,
            antigravity_credential_access_decided: false,
            provider_order: default_provider_order(),
            notify_session_reset: false,
            notify_weekly_reset: false,
            notify_claude_cli_update: true,
        }
    }
}

pub fn default_provider_order() -> Vec<TrayIconKind> {
    vec![
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ]
}

fn default_poll_interval() -> u32 {
    POLL_5_MIN
}

fn default_widget_visible() -> bool {
    true
}

fn default_enabled() -> bool {
    true
}

fn default_detailed_tray_icons() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
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
    if !settings.show_claude_code && !settings.show_codex && !settings.show_antigravity {
        settings.show_claude_code = true;
        repaired.push("enabled_providers");
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
    if let Err(error) = std::fs::write(&tmp, contents) {
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

    #[test]
    fn provider_order_is_deduplicated_and_completed() {
        assert_eq!(
            normalize_provider_order(&[TrayIconKind::Codex, TrayIconKind::Codex]),
            vec![
                TrayIconKind::Codex,
                TrayIconKind::Claude,
                TrayIconKind::Antigravity,
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
        assert!(settings.show_claude_code);
        assert_eq!(
            settings.provider_order,
            vec![
                TrayIconKind::Antigravity,
                TrayIconKind::Claude,
                TrayIconKind::Codex,
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
        assert!(settings.notify_claude_cli_update);
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
    }

    #[test]
    fn retired_claude_recovery_setting_is_ignored_and_not_reserialized() {
        let settings: SettingsFile = serde_json::from_str(
            r#"{
                "claude_auto_recovery": false,
                "notify_claude_cli_update": true
            }"#,
        )
        .expect("retired settings fields should remain readable");

        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("claude_auto_recovery"));
        assert!(settings.notify_claude_cli_update);
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
