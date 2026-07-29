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
            "Allow access to {provider}?",
            "Before reading {source}, Gengchou needs your permission. If allowed, it re-reads that source as needed and sends the session only to {provider} for read-only quota requests.\n\nGengchou never stores the token. You can revoke access from the Provider access menu at any time.\n\nAllow access?",
        ),
        LanguageId::Dutch => (
            "Providertoegang",
            "Toegang tot {provider} toestaan?",
            "Gengchou heeft uw toestemming nodig voordat {source} wordt gelezen. Na toestemming leest Gengchou deze bron opnieuw wanneer dat nodig is en stuurt de sessie alleen naar {provider} voor alleen-lezen quota-aanvragen.\n\nGengchou slaat het token nooit op. U kunt de toegang op elk moment intrekken via het menu Providertoegang.\n\nToegang toestaan?",
        ),
        LanguageId::Spanish => (
            "Acceso a proveedores",
            "¿Permitir el acceso a {provider}?",
            "Gengchou necesita su permiso antes de leer {source}. Si lo permite, vuelve a leer esa fuente cuando sea necesario y envía la sesión únicamente a {provider} para consultar la cuota en modo de solo lectura.\n\nGengchou nunca guarda el token. Puede revocar el acceso en cualquier momento desde el menú Acceso a proveedores.\n\n¿Permitir el acceso?",
        ),
        LanguageId::French => (
            "Accès aux fournisseurs",
            "Autoriser l'accès à {provider} ?",
            "Gengchou a besoin de votre autorisation avant de lire {source}. Une fois autorisé, il relit cette source lorsque nécessaire et envoie la session uniquement à {provider} pour des requêtes de quota en lecture seule.\n\nGengchou ne stocke jamais le jeton. Vous pouvez révoquer l'accès à tout moment depuis le menu Accès aux fournisseurs.\n\nAutoriser l'accès ?",
        ),
        LanguageId::German => (
            "Anbieterzugriff",
            "Zugriff auf {provider} erlauben?",
            "Gengchou benötigt Ihre Zustimmung, bevor {source} gelesen wird. Nach der Zustimmung liest Gengchou diese Quelle bei Bedarf erneut und sendet die Sitzung nur für schreibgeschützte Kontingentabfragen an {provider}.\n\nGengchou speichert das Token niemals. Sie können den Zugriff jederzeit im Menü Anbieterzugriff widerrufen.\n\nZugriff erlauben?",
        ),
        LanguageId::Japanese => (
            "プロバイダーへのアクセス",
            "{provider} へのアクセスを許可しますか？",
            "{source} を読み取る前に、Gengchou は許可を必要とします。許可すると、必要に応じてこのソースを再度読み取り、読み取り専用のクォータ照会のためだけにセッションを {provider} へ送信します。\n\nGengchou はトークンを保存しません。プロバイダーへのアクセス メニューからいつでも許可を取り消せます。\n\nアクセスを許可しますか？",
        ),
        LanguageId::Korean => (
            "공급자 액세스",
            "{provider} 액세스를 허용하시겠습니까?",
            "{source}을(를) 읽기 전에 Gengchou에 사용자의 허가가 필요합니다. 허용하면 필요할 때 이 원본을 다시 읽고 읽기 전용 할당량 요청을 위해서만 세션을 {provider}에 보냅니다.\n\nGengchou는 토큰을 저장하지 않습니다. 공급자 액세스 메뉴에서 언제든지 권한을 철회할 수 있습니다.\n\n액세스를 허용하시겠습니까?",
        ),
        LanguageId::SimplifiedChinese => (
            "服务商访问权限",
            "允许访问 {provider}？",
            "读取 {source} 前，Gengchou 需要获得你的明确授权。授权后，Gengchou 会按需重新读取该来源，并且只把会话用于向 {provider} 发起只读的额度查询。\n\nGengchou 不会保存令牌。你可以随时在“服务商访问权限”菜单中撤销授权。\n\n是否允许访问？",
        ),
        LanguageId::TraditionalChinese => (
            "服務商存取權限",
            "允許存取 {provider}？",
            "讀取 {source} 前，Gengchou 需要取得你的明確授權。授權後，Gengchou 會按需重新讀取該來源，並且只把工作階段用於向 {provider} 發起唯讀的配額查詢。\n\nGengchou 不會儲存權杖。你可以隨時在「服務商存取權限」選單中撤銷授權。\n\n是否允許存取？",
        ),
        LanguageId::Russian => (
            "Доступ к провайдерам",
            "Разрешить доступ к {provider}?",
            "Перед чтением {source} Gengchou требуется ваше разрешение. После разрешения источник перечитывается по мере необходимости, а сеанс отправляется только в {provider} для запросов квоты без изменения данных.\n\nGengchou никогда не сохраняет токен. Доступ можно отозвать в любое время в меню доступа к провайдерам.\n\nРазрешить доступ?",
        ),
        LanguageId::PortugueseBrazil => (
            "Acesso aos provedores",
            "Permitir acesso ao {provider}?",
            "O Gengchou precisa da sua permissão antes de ler {source}. Se permitido, ele relê essa fonte quando necessário e envia a sessão somente ao {provider} para consultas de cota somente leitura.\n\nO Gengchou nunca armazena o token. Você pode revogar o acesso a qualquer momento no menu Acesso aos provedores.\n\nPermitir acesso?",
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
    pub window_title: &'static str,
    pub refresh: &'static str,
    pub refresh_now: &'static str,
    pub one_minute: &'static str,
    pub two_minutes: &'static str,
    pub five_minutes: &'static str,
    pub ten_minutes: &'static str,
    pub fifteen_minutes: &'static str,
    pub thirty_minutes: &'static str,
    pub models: &'static str,
    pub claude_code_model: &'static str,
    pub codex_model: &'static str,
    pub antigravity_model: &'static str,
    pub settings: &'static str,
    pub start_with_windows: &'static str,
    pub reset_widget_position: &'static str,
    pub reset_floating_position: &'static str,
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
    pub reset_notification_title: &'static str,
    pub reset_notification_body: &'static str,
    pub session_window: &'static str,
    pub weekly_window: &'static str,
    pub quota_window: &'static str,
    pub day_suffix: &'static str,
    pub hour_suffix: &'static str,
    pub minute_suffix: &'static str,
    pub second_suffix: &'static str,
    pub token_expired_title: &'static str,
    pub token_expired_body: &'static str,
    pub codex_token_expired_title: &'static str,
    pub codex_token_expired_body: &'static str,
    pub antigravity_token_expired_title: &'static str,
    pub antigravity_token_expired_body: &'static str,
    pub codex_window_title: &'static str,
    pub antigravity_window_title: &'static str,
    // Detail popup (tray left-click) strings.
    pub detail_waiting: &'static str,
    pub detail_auth_required: &'static str,
    pub detail_sign_in_hint: &'static str,
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
    pub detail_rate_limited: &'static str,
    pub detail_badge_rate_limited: &'static str,
    pub detail_badge_error: &'static str,
    pub detail_badge_loading: &'static str,
    pub detail_badge_near_limit: &'static str,
    pub detail_badge_normal: &'static str,
    pub detail_badge_unused: &'static str,
    pub detail_badge_cached: &'static str,
    pub detail_stale: &'static str,
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
