mod dutch;
mod english;
mod french;
mod german;
mod japanese;
mod korean;
mod portuguese_brazil;
mod russian;
mod simplified_chinese;
mod spanish;
mod traditional_chinese;

use windows::core::PWSTR;
use windows::Win32::Globalization::{
    GetUserDefaultLocaleName, GetUserDefaultUILanguage, GetUserPreferredUILanguages,
    LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME, MUI_LANGUAGE_NAME,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanguageId {
    English,
    Dutch,
    Spanish,
    French,
    German,
    Japanese,
    Korean,
    SimplifiedChinese,
    TraditionalChinese,
    Russian,
    PortugueseBrazil,
}

impl LanguageId {
    pub const ALL: [LanguageId; 11] = [
        LanguageId::English,
        LanguageId::Dutch,
        LanguageId::Spanish,
        LanguageId::French,
        LanguageId::German,
        LanguageId::Japanese,
        LanguageId::Korean,
        LanguageId::SimplifiedChinese,
        LanguageId::TraditionalChinese,
        LanguageId::Russian,
        LanguageId::PortugueseBrazil,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Dutch => "nl",
            Self::Spanish => "es",
            Self::French => "fr",
            Self::German => "de",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::SimplifiedChinese => "zh-CN",
            Self::TraditionalChinese => "zh-TW",
            Self::Russian => "ru",
            Self::PortugueseBrazil => "pt-BR",
        }
    }
    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Dutch => "Nederlands",
            Self::Spanish => "Español",
            Self::French => "Français",
            Self::German => "Deutsch",
            Self::Japanese => "日本語",
            Self::Korean => "한국어",
            Self::SimplifiedChinese => "简体中文",
            Self::TraditionalChinese => "繁體中文",
            Self::Russian => "Русский",
            Self::PortugueseBrazil => "Português (Brasil)",
        }
    }
    pub fn strings(self) -> Strings {
        match self {
            Self::English => english::STRINGS,
            Self::Dutch => dutch::STRINGS,
            Self::Spanish => spanish::STRINGS,
            Self::French => french::STRINGS,
            Self::German => german::STRINGS,
            Self::Japanese => japanese::STRINGS,
            Self::Korean => korean::STRINGS,
            Self::SimplifiedChinese => simplified_chinese::STRINGS,
            Self::TraditionalChinese => traditional_chinese::STRINGS,
            Self::Russian => russian::STRINGS,
            Self::PortugueseBrazil => portuguese_brazil::STRINGS,
        }
    }

    pub fn update_via_winget_label(self) -> &'static str {
        match self {
            Self::English => english::UPDATE_VIA_WINGET_LABEL,
            Self::Dutch => dutch::UPDATE_VIA_WINGET_LABEL,
            Self::Spanish => spanish::UPDATE_VIA_WINGET_LABEL,
            Self::French => french::UPDATE_VIA_WINGET_LABEL,
            Self::German => german::UPDATE_VIA_WINGET_LABEL,
            Self::Japanese => japanese::UPDATE_VIA_WINGET_LABEL,
            Self::Korean => korean::UPDATE_VIA_WINGET_LABEL,
            Self::SimplifiedChinese => simplified_chinese::UPDATE_VIA_WINGET_LABEL,
            Self::TraditionalChinese => traditional_chinese::UPDATE_VIA_WINGET_LABEL,
            Self::Russian => russian::UPDATE_VIA_WINGET_LABEL,
            Self::PortugueseBrazil => portuguese_brazil::UPDATE_VIA_WINGET_LABEL,
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        let normalized = code.trim().replace('_', "-").to_ascii_lowercase();
        if normalized.is_empty() || normalized == "system" {
            return None;
        }

        let prefix = normalized.split('-').next().unwrap_or_default();
        match prefix {
            "en" => Some(Self::English),
            "nl" => Some(Self::Dutch),
            "es" => Some(Self::Spanish),
            "fr" => Some(Self::French),
            "de" => Some(Self::German),
            "ja" => Some(Self::Japanese),
            "ko" => Some(Self::Korean),
            "zh" => {
                if normalized.contains("tw")
                    || normalized.contains("hk")
                    || normalized.contains("hant")
                {
                    Some(Self::TraditionalChinese)
                } else {
                    Some(Self::SimplifiedChinese)
                }
            }
            "ru" => Some(Self::Russian),
            "pt" => Some(Self::PortugueseBrazil),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CredentialConsentCopy {
    pub provider_access: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

pub fn credential_consent_copy(language: LanguageId) -> CredentialConsentCopy {
    let (provider_access, title, body) = match language {
        LanguageId::English => (
            "Provider access",
            "Allow Gengchou to check your AI usage?",
            "Gengchou uses the sign-in state already on this machine to check quota usage. It does not consume model allowance and does not store your sign-in information.\n\nYou can revoke access at any time from Provider access in the context menu.",
        ),
        LanguageId::Dutch => (
            "Providertoegang",
            "Gengchou toegang geven tot uw AI-verbruik?",
            "Gengchou gebruikt de aanmeldstatus die al op deze computer aanwezig is om het verbruik op te vragen. Dit verbruikt geen modeltegoed en uw aanmeldgegevens worden niet opgeslagen.\n\nU kunt de toegang op elk moment intrekken via Providertoegang in het contextmenu.",
        ),
        LanguageId::Spanish => (
            "Acceso a proveedores",
            "¿Permitir que Gengchou consulte tu uso de IA?",
            "Gengchou usa la sesión ya iniciada en este equipo para consultar el uso. No consume cuota de modelo ni guarda tus datos de inicio de sesión.\n\nPuedes revocar el acceso en cualquier momento desde Acceso a proveedores en el menú contextual.",
        ),
        LanguageId::French => (
            "Accès aux fournisseurs",
            "Autoriser Gengchou à consulter votre utilisation d'IA ?",
            "Gengchou utilise la session déjà présente sur cet ordinateur pour consulter votre utilisation. Cela ne consomme aucun quota de modèle et vos informations de connexion ne sont pas enregistrées.\n\nVous pouvez révoquer l'accès à tout moment depuis Accès aux fournisseurs dans le menu contextuel.",
        ),
        LanguageId::German => (
            "Anbieterzugriff",
            "Gengchou erlauben, Ihre KI-Nutzung abzufragen?",
            "Gengchou verwendet die auf diesem Computer bereits vorhandene Anmeldung, um die Nutzung abzufragen. Dabei wird kein Modellkontingent verbraucht und Ihre Anmeldedaten werden nicht gespeichert.\n\nSie können den Zugriff jederzeit im Kontextmenü unter „Anbieterzugriff“ widerrufen.",
        ),
        LanguageId::Japanese => (
            "プロバイダーへのアクセス",
            "Gengchou に AI 使用量の確認を許可しますか？",
            "Gengchou はこのコンピューターに既にあるサインイン状態を使用して使用量を確認します。モデルの利用枠は消費せず、サインイン情報も保存しません。\n\nコンテキスト メニューの「プロバイダーへのアクセス」からいつでも許可を取り消せます。",
        ),
        LanguageId::Korean => (
            "공급자 액세스",
            "Gengchou가 AI 사용량을 조회하도록 허용하시겠습니까?",
            "Gengchou는 이 컴퓨터에 이미 있는 로그인 상태를 사용하여 사용량을 조회합니다. 모델 할당량을 소비하지 않으며 로그인 정보를 저장하지도 않습니다.\n\n상황에 맞는 메뉴의 '공급자 액세스'에서 언제든지 권한을 철회할 수 있습니다.",
        ),
        LanguageId::SimplifiedChinese => (
            "服务商访问权限",
            "允许更筹查询本机 AI 用量？",
            "更筹会使用本机已有的登录状态查询用量，不会消耗模型额度，也不会保存登录信息。\n\n可随时在右键菜单的“服务商访问权限”中撤销。",
        ),
        LanguageId::TraditionalChinese => (
            "服務商存取權限",
            "允許更籌查詢本機 AI 用量？",
            "更籌會使用本機已有的登入狀態查詢用量，不會消耗模型額度，也不會儲存登入資訊。\n\n可隨時在右鍵選單的「服務商存取權限」中撤銷。",
        ),
        LanguageId::Russian => (
            "Доступ к провайдерам",
            "Разрешить Gengchou запрашивать использование ИИ?",
            "Gengchou использует вход, уже выполненный на этом компьютере, чтобы запрашивать использование. Это не расходует квоту модели, а данные для входа не сохраняются.\n\nВы можете отозвать доступ в любое время в меню «Доступ к провайдерам».",
        ),
        LanguageId::PortugueseBrazil => (
            "Acesso aos provedores",
            "Permitir que o Gengchou consulte seu uso de IA?",
            "O Gengchou usa o login já existente neste computador para consultar o uso. Isso não consome cota de modelo e seus dados de login não são armazenados.\n\nVocê pode revogar o acesso a qualquer momento em Acesso aos provedores no menu de contexto.",
        ),
    };
    CredentialConsentCopy {
        provider_access,
        title,
        body,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Strings {
    pub locale_name: &'static str,
    pub window_title: &'static str,
    /// Shown when a launch cannot become the running instance and there is no
    /// existing window on this desktop to bring forward - a second session of
    /// the same account. Without it that launch simply exited.
    pub instance_already_running: &'static str,
    /// Shown when the single-instance guard itself could not be created. The
    /// Win32 error is appended untranslated.
    pub instance_lock_failed: &'static str,
    pub refresh: &'static str,
    pub refresh_now: &'static str,
    pub one_minute: &'static str,
    pub two_minutes: &'static str,
    pub five_minutes: &'static str,
    pub ten_minutes: &'static str,
    pub fifteen_minutes: &'static str,
    pub thirty_minutes: &'static str,
    pub models: &'static str,
    pub claude_model: &'static str,
    pub codex_model: &'static str,
    pub antigravity_model: &'static str,
    pub grok_model: &'static str,
    pub settings: &'static str,
    pub settings_storage_failed: &'static str,
    pub start_with_windows: &'static str,
    pub widget_default_position: &'static str,
    pub floating_default_position: &'static str,
    pub primary_taskbar_left: &'static str,
    pub primary_taskbar_right: &'static str,
    pub primary_display_bottom_left: &'static str,
    pub primary_display_bottom_right: &'static str,
    pub language: &'static str,
    pub system_default: &'static str,
    pub check_for_updates: &'static str,
    pub checking_for_updates: &'static str,
    pub updates: &'static str,
    pub update_in_progress: &'static str,
    pub up_to_date: &'static str,
    pub up_to_date_short: &'static str,
    pub update_failed: &'static str,
    pub applying_update: &'static str,
    pub update_to: &'static str,
    pub update_available: &'static str,
    pub update_prompt_now: &'static str,
    pub exit: &'static str,
    pub show_widget: &'static str,
    pub show_floating_monitor: &'static str,
    pub detailed_tray_icons: &'static str,
    pub notifications: &'static str,
    pub notify_session_reset: &'static str,
    pub notify_weekly_reset: &'static str,
    pub claude_cli_updated_title: &'static str,
    pub claude_cli_updated_body: &'static str,
    pub reset_notification_title: &'static str,
    pub reset_notification_body: &'static str,
    pub session_window: &'static str,
    pub weekly_window: &'static str,
    pub quota_window: &'static str,
    pub day_suffix: &'static str,
    pub hour_suffix: &'static str,
    pub minute_suffix: &'static str,
    pub second_suffix: &'static str,
    // Detail popup (tray left-click) strings.
    pub detail_waiting: &'static str,
    pub detail_unavailable: &'static str,
    pub detail_temporarily_unavailable: &'static str,
    pub detail_pin_action: &'static str,
    pub detail_unpin_action: &'static str,
    pub detail_lock_position_action: &'static str,
    pub detail_unlock_position_action: &'static str,
    pub detail_close_action: &'static str,
    pub detail_refreshing: &'static str,
    pub detail_network_action: &'static str,
    pub detail_network_outcome: &'static str,
    pub detail_badge_auth_failed: &'static str,
    pub detail_claude_login_action: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub detail_sign_in_again_action: &'static str,
    pub detail_monitoring_resumes: &'static str,
    pub detail_reset_unavailable: &'static str,
    /// "{duration}" is replaced with a relative countdown like "2h 13m".
    pub detail_resets_in: &'static str,
    pub detail_resets_now: &'static str,
    /// "{ago}" is replaced with an elapsed duration like "2m".
    pub detail_updated_ago: &'static str,
    /// "{next}" is replaced with the time until the next poll.
    pub detail_next_in: &'static str,
    /// "{interval}" is replaced with the poll interval like "15m".
    pub detail_poll_every: &'static str,
    pub detail_some_not_updated: &'static str,
    pub detail_all_not_updated: &'static str,
    pub detail_badge_stale: &'static str,
    pub detail_badge_near_limit: &'static str,
    pub detail_badge_limit_reached: &'static str,
    /// Shown when no credential for a provider exists on this machine. Kept
    /// distinct from `detail_badge_auth_failed`: nothing has failed, the
    /// provider was simply never signed in.
    pub detail_badge_not_signed_in: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub detail_not_signed_in_action: &'static str,
    /// "{provider}" is replaced with the localized provider name. Rendered as
    /// the hint's single-line action, so it stays short; the route back is in
    /// `detail_access_revoked_outcome`.
    ///
    /// Covers a provider that was never granted access as well as one the user
    /// revoked: the surfaces cannot tell those apart on an upgraded install,
    /// so the wording states the current fact rather than guessing a past
    /// decision.
    pub detail_access_revoked_hint: &'static str,
    pub detail_access_revoked_outcome: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub access_needs_review: &'static str,
    pub access_allow: &'static str,
    pub access_keep_closed: &'static str,
    /// Third choice in the pending review dialog, and its default: leaves the
    /// provider pending and reads nothing. Without it the only way to answer
    /// "not now" is the title-bar close button, and users pick "keep closed"
    /// instead - recording a revocation they did not intend.
    pub access_decide_later: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub pending_access_title: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub pending_access_body: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub detail_access_pending_hint: &'static str,
    pub detail_access_pending_outcome: &'static str,
    /// "{provider}" is replaced with the localized provider name.
    pub provider_detected_title: &'static str,
    pub provider_detected_body: &'static str,
    pub redetect_providers: &'static str,
    /// Short weekday names, Sunday first (SYSTEMTIME::wDayOfWeek order).
    pub weekdays: [&'static str; 7],
}

pub fn resolve_language(language_override: Option<LanguageId>) -> LanguageId {
    language_override.unwrap_or_else(detect_system_language)
}

pub fn detect_system_language() -> LanguageId {
    preferred_ui_languages()
        .into_iter()
        .find_map(|locale| LanguageId::from_code(&locale))
        .or_else(default_ui_locale)
        .or_else(default_locale_name)
        .unwrap_or(LanguageId::English)
}

pub fn update_via_winget(language: LanguageId) -> &'static str {
    language.update_via_winget_label()
}

fn preferred_ui_languages() -> Vec<String> {
    unsafe {
        let mut num_languages = 0u32;
        let mut buffer_len = 0u32;
        if GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut num_languages,
            PWSTR::null(),
            &mut buffer_len,
        )
        .is_err()
            || buffer_len == 0
        {
            return Vec::new();
        }

        let mut buffer = vec![0u16; buffer_len as usize];
        if GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut num_languages,
            PWSTR(buffer.as_mut_ptr()),
            &mut buffer_len,
        )
        .is_err()
        {
            return Vec::new();
        }

        buffer
            .split(|unit| *unit == 0)
            .filter(|part| !part.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}

fn default_ui_locale() -> Option<LanguageId> {
    unsafe {
        let lang_id = GetUserDefaultUILanguage();
        let mut buffer = [0u16; MAX_LOCALE_NAME as usize];
        let len = LCIDToLocaleName(
            lang_id as u32,
            Some(&mut buffer),
            LOCALE_ALLOW_NEUTRAL_NAMES,
        );
        if len <= 1 {
            return None;
        }
        let locale = String::from_utf16_lossy(&buffer[..(len as usize - 1)]);
        LanguageId::from_code(&locale)
    }
}

fn default_locale_name() -> Option<LanguageId> {
    unsafe {
        let mut buffer = [0u16; MAX_LOCALE_NAME as usize];
        let len = GetUserDefaultLocaleName(&mut buffer);
        if len <= 1 {
            return None;
        }
        let locale = String::from_utf16_lossy(&buffer[..(len as usize - 1)]);
        LanguageId::from_code(&locale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_status_copy_is_complete_in_every_language() {
        for language in LanguageId::ALL {
            let strings = language.strings();
            for value in [
                strings.detail_badge_auth_failed,
                strings.detail_badge_limit_reached,
                strings.detail_badge_stale,
                strings.detail_unavailable,
                strings.detail_temporarily_unavailable,
                strings.detail_network_outcome,
                strings.detail_some_not_updated,
                strings.detail_all_not_updated,
                strings.detail_claude_login_action,
                strings.detail_monitoring_resumes,
                strings.claude_cli_updated_title,
                strings.claude_cli_updated_body,
                strings.detail_badge_not_signed_in,
                strings.detail_not_signed_in_action,
                strings.detail_access_revoked_hint,
                strings.detail_access_revoked_outcome,
                strings.access_needs_review,
                strings.access_allow,
                strings.access_keep_closed,
                strings.access_decide_later,
                strings.pending_access_title,
                strings.pending_access_body,
                strings.detail_access_pending_hint,
                strings.detail_access_pending_outcome,
                strings.provider_detected_title,
                strings.provider_detected_body,
                strings.redetect_providers,
            ] {
                assert!(
                    !value.trim().is_empty(),
                    "missing copy for {}",
                    language.code()
                );
            }
            assert!(strings.detail_claude_login_action.contains("Claude"));
            for value in [
                strings.detail_sign_in_again_action,
                strings.detail_not_signed_in_action,
                strings.detail_access_revoked_hint,
                strings.access_needs_review,
                strings.pending_access_title,
                strings.pending_access_body,
                strings.detail_access_pending_hint,
                strings.provider_detected_title,
            ] {
                assert!(
                    value.contains("{provider}"),
                    "missing {{provider}} placeholder for {}",
                    language.code()
                );
            }
        }
    }

    /// The first-run prompt now covers every provider at once, so naming one
    /// would be wrong. It also no longer spells out credential paths - those
    /// live in the README privacy section.
    #[test]
    fn consent_copy_is_provider_neutral_in_every_language() {
        for language in LanguageId::ALL {
            let copy = credential_consent_copy(language);
            for value in [copy.title, copy.body] {
                assert!(
                    !value.contains("{provider}"),
                    "{} consent copy still names a provider",
                    language.code()
                );
                assert!(
                    !value.contains("{source}"),
                    "{} consent copy still names a credential source",
                    language.code()
                );
            }
            assert!(!copy.provider_access.trim().is_empty());
        }
    }

    #[test]
    fn compact_quota_window_labels_are_identical_in_every_language() {
        for language in LanguageId::ALL {
            let strings = language.strings();
            assert_eq!(strings.session_window, "5h", "{} session", language.code());
            assert_eq!(strings.weekly_window, "7d", "{} weekly", language.code());
        }
    }

    /// The balloon names the CLI it changed, so the user knows what was
    /// touched without opening anything.
    #[test]
    fn english_claude_update_notification_copy_is_complete() {
        let strings = LanguageId::English.strings();
        assert!(strings.claude_cli_updated_title.contains("Claude Code"));
        assert!(strings.claude_cli_updated_body.contains("{before}"));
        assert!(strings.claude_cli_updated_body.contains("{after}"));
    }

    #[test]
    fn quota_provider_is_named_claude_in_every_language() {
        for language in LanguageId::ALL {
            assert_eq!(
                language.strings().claude_model,
                "Claude",
                "{} provider label",
                language.code()
            );
        }
    }

    #[test]
    fn simplified_chinese_consent_copy_matches_the_approved_short_form() {
        let copy = credential_consent_copy(LanguageId::SimplifiedChinese);
        assert_eq!(copy.provider_access, "服务商访问权限");
        assert_eq!(copy.title, "允许更筹查询本机 AI 用量？");
        assert_eq!(
            copy.body,
            "更筹会使用本机已有的登录状态查询用量，不会消耗模型额度，也不会保存登录信息。\n\n可随时在右键菜单的“服务商访问权限”中撤销。"
        );
    }

    #[test]
    fn simplified_chinese_claude_recovery_copy_matches_the_approved_short_form() {
        let strings = LanguageId::SimplifiedChinese.strings();
        assert_eq!(strings.detail_badge_auth_failed, "认证失败");
        assert_eq!(strings.detail_badge_stale, "刷新失败");
        assert_eq!(strings.detail_network_action, "请检查网络连接");
        assert_eq!(strings.detail_claude_login_action, "请重新登录 Claude");
        assert_eq!(strings.detail_sign_in_again_action, "请重新登录 {provider}");
        assert_eq!(strings.detail_monitoring_resumes, "登录后自动恢复");
    }

    #[test]
    fn app_display_name_is_localized_only_for_chinese() {
        assert_eq!(LanguageId::SimplifiedChinese.strings().window_title, "更筹");
        assert_eq!(
            LanguageId::TraditionalChinese.strings().window_title,
            "更籌"
        );
        assert!(credential_consent_copy(LanguageId::SimplifiedChinese)
            .body
            .contains("更筹"));
        assert!(credential_consent_copy(LanguageId::TraditionalChinese)
            .body
            .contains("更籌"));

        for language in LanguageId::ALL {
            if !matches!(
                language,
                LanguageId::SimplifiedChinese | LanguageId::TraditionalChinese
            ) {
                assert_eq!(language.strings().window_title, "Gengchou");
            }
        }
    }

    #[test]
    fn compact_surface_names_match_the_approved_menu_system_in_every_language() {
        let cases = [
            (
                LanguageId::English,
                [
                    "Provider tray icons",
                    "Taskbar widget",
                    "Floating window",
                    "Taskbar widget position",
                    "Floating window position",
                ],
            ),
            (
                LanguageId::Dutch,
                [
                    "Systeemvakpictogrammen per aanbieder",
                    "Taakbalkwidget",
                    "Zwevend venster",
                    "Positie taakbalkwidget",
                    "Positie zwevend venster",
                ],
            ),
            (
                LanguageId::Spanish,
                [
                    "Iconos de proveedores en la bandeja",
                    "Widget de la barra de tareas",
                    "Ventana flotante",
                    "Posición del widget de la barra de tareas",
                    "Posición de la ventana flotante",
                ],
            ),
            (
                LanguageId::French,
                [
                    "Icônes des fournisseurs dans la zone de notification",
                    "Widget de la barre des tâches",
                    "Fenêtre flottante",
                    "Position du widget de la barre des tâches",
                    "Position de la fenêtre flottante",
                ],
            ),
            (
                LanguageId::German,
                [
                    "Anbietersymbole im Infobereich",
                    "Taskleisten-Widget",
                    "Schwebendes Fenster",
                    "Position des Taskleisten-Widgets",
                    "Position des schwebenden Fensters",
                ],
            ),
            (
                LanguageId::Japanese,
                [
                    "プロバイダー別の通知領域アイコン",
                    "タスクバー ウィジェット",
                    "フローティングウィンドウ",
                    "タスクバー ウィジェットの位置",
                    "フローティングウィンドウの位置",
                ],
            ),
            (
                LanguageId::Korean,
                [
                    "서비스별 알림 영역 아이콘",
                    "작업 표시줄 위젯",
                    "플로팅 창",
                    "작업 표시줄 위젯 위치",
                    "플로팅 창 위치",
                ],
            ),
            (
                LanguageId::SimplifiedChinese,
                [
                    "服务商托盘图标",
                    "任务栏小组件",
                    "桌面浮窗",
                    "任务栏小组件位置",
                    "桌面浮窗位置",
                ],
            ),
            (
                LanguageId::TraditionalChinese,
                [
                    "服務商系統匣圖示",
                    "工作列小工具",
                    "桌面浮窗",
                    "工作列小工具位置",
                    "桌面浮窗位置",
                ],
            ),
            (
                LanguageId::Russian,
                [
                    "Значки провайдеров в области уведомлений",
                    "Виджет панели задач",
                    "Плавающее окно",
                    "Положение виджета панели задач",
                    "Положение плавающего окна",
                ],
            ),
            (
                LanguageId::PortugueseBrazil,
                [
                    "Ícones dos provedores na bandeja",
                    "Widget da barra de tarefas",
                    "Janela flutuante",
                    "Posição do widget da barra de tarefas",
                    "Posição da janela flutuante",
                ],
            ),
        ];

        for (language, expected) in cases {
            let strings = language.strings();
            assert_eq!(
                [
                    strings.detailed_tray_icons,
                    strings.show_widget,
                    strings.show_floating_monitor,
                    strings.widget_default_position,
                    strings.floating_default_position,
                ],
                expected,
                "{}",
                strings.locale_name
            );
        }
    }

    #[test]
    fn taskbar_default_labels_name_primary_display_edges() {
        let english = LanguageId::English.strings();
        assert_eq!(
            english.primary_taskbar_left,
            "Left Side of Primary Display Taskbar"
        );
        assert_eq!(
            english.primary_taskbar_right,
            "Right Side of Primary Display Taskbar"
        );

        let simplified = LanguageId::SimplifiedChinese.strings();
        assert_eq!(simplified.primary_taskbar_left, "主屏幕任务栏左侧");
        assert_eq!(simplified.primary_taskbar_right, "主屏幕任务栏右侧");
    }
}
