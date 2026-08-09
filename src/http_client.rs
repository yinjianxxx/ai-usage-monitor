use std::collections::HashSet;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::de::DeserializeOwned;
use windows::core::PWSTR;
use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
use windows::Win32::Networking::WinHttp::{
    WinHttpGetIEProxyConfigForCurrentUser, WINHTTP_CURRENT_USER_IE_PROXY_CONFIG,
};

use crate::diagnose;

const PROXY_ENV_VARS: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// Provider quota payloads and GitHub release metadata are normally well
/// below one MiB. Keep a generous fixed ceiling so a broken endpoint cannot
/// make this long-running desktop process allocate an unbounded JSON body.
pub const MAX_JSON_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

struct WindowsUserProxyConfig {
    auto_detect: bool,
    auto_config_url: Option<String>,
    proxy: Option<String>,
}

struct ProxySelection {
    proxy: Option<ureq::Proxy>,
    state_key: String,
    log_message: String,
}

/// Build the shared HTTPS client used by provider polling and self-updates.
///
/// Explicit proxy environment variables keep their existing precedence. When
/// none is usable, fall back to the current Windows user's static Internet
/// proxy so desktop apps work with the same local proxy as Codex/Claude.
pub fn build_agent(url: &str, timeout: Duration) -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|error| error.to_string())?;
    let selection = proxy_selection(url);
    log_proxy_selection(&selection.state_key, &selection.log_message);

    let mut builder = ureq::AgentBuilder::new()
        .timeout(timeout)
        .tls_connector(std::sync::Arc::new(tls))
        // Proxy selection is handled here so Windows user settings can be the
        // fallback and the chosen source can be diagnosed without secrets.
        .try_proxy_from_env(false);
    if let Some(proxy) = selection.proxy {
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

/// Read and deserialize a JSON response with a hard limit on the decoded body.
///
/// `Content-Length` is only an early rejection hint: proxies and chunked
/// responses can omit or misstate it, so the bounded read remains the source
/// of truth.
pub fn response_json_limited<T: DeserializeOwned>(
    response: ureq::Response,
    description: &str,
) -> Result<T, String> {
    response_json_limited_with(response, MAX_JSON_RESPONSE_BYTES, description)
}

fn response_json_limited_with<T: DeserializeOwned>(
    response: ureq::Response,
    max_bytes: u64,
    description: &str,
) -> Result<T, String> {
    if response
        .header("Content-Length")
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|length| length > max_bytes)
    {
        return Err(format!(
            "{description} exceeds the {} byte safety limit",
            max_bytes
        ));
    }

    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("unable to read {description}: {error}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "{description} exceeds the {} byte safety limit",
            max_bytes
        ));
    }

    serde_json::from_slice(&bytes)
        .map_err(|error| format!("unable to parse {description}: {error}"))
}

fn proxy_selection(url: &str) -> ProxySelection {
    if let Some((name, proxy)) = environment_proxy() {
        return ProxySelection {
            proxy: Some(proxy),
            state_key: format!("environment:{name}"),
            log_message: format!("network proxy source=environment variable={name}"),
        };
    }

    match windows_user_proxy_config() {
        Ok(config) => {
            if let Some(raw_proxy) = config.proxy.as_deref() {
                if let Some(server) = select_windows_proxy(raw_proxy, url) {
                    match ureq::Proxy::new(&server) {
                        Ok(proxy) => {
                            return ProxySelection {
                                proxy: Some(proxy),
                                state_key: "windows-user".to_string(),
                                log_message: "network proxy source=windows-user".to_string(),
                            };
                        }
                        Err(_) => {
                            return ProxySelection {
                                proxy: None,
                                state_key: "windows-user-invalid".to_string(),
                                log_message: "Windows user proxy has an unsupported format; using a direct connection"
                                    .to_string(),
                            };
                        }
                    }
                }
            }

            if config.auto_detect || config.auto_config_url.is_some() {
                return ProxySelection {
                    proxy: None,
                    state_key: "windows-auto-proxy".to_string(),
                    log_message: "Windows automatic proxy configuration detected but no static proxy was resolved; using a direct connection"
                        .to_string(),
                };
            }

            ProxySelection {
                proxy: None,
                state_key: "direct".to_string(),
                log_message: "network proxy source=direct".to_string(),
            }
        }
        Err(error) => ProxySelection {
            proxy: None,
            state_key: "windows-proxy-query-failed".to_string(),
            log_message: format!(
                "unable to read Windows user proxy settings; using a direct connection: {error}"
            ),
        },
    }
}

fn environment_proxy() -> Option<(&'static str, ureq::Proxy)> {
    for &name in PROXY_ENV_VARS {
        let Ok(value) = std::env::var(name) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if let Ok(proxy) = ureq::Proxy::new(value) {
            return Some((name, proxy));
        }
        log_proxy_warning_once(
            &format!("invalid-environment:{name}"),
            &format!("ignored malformed proxy environment variable {name}"),
        );
    }
    None
}

fn windows_user_proxy_config() -> Result<WindowsUserProxyConfig, String> {
    unsafe {
        let mut raw = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
        WinHttpGetIEProxyConfigForCurrentUser(&mut raw)
            .map_err(|error| format!("WinHttpGetIEProxyConfigForCurrentUser failed: {error}"))?;

        let auto_config_url = take_global_wide_string(raw.lpszAutoConfigUrl);
        let proxy = take_global_wide_string(raw.lpszProxy);
        // The bypass list is not needed for Gengchou's external HTTPS
        // endpoints, but it is allocated by the same API and must be freed.
        let _proxy_bypass = take_global_wide_string(raw.lpszProxyBypass);
        Ok(WindowsUserProxyConfig {
            auto_detect: raw.fAutoDetect.as_bool(),
            auto_config_url,
            proxy,
        })
    }
}

unsafe fn take_global_wide_string(value: PWSTR) -> Option<String> {
    if value.0.is_null() {
        return None;
    }
    let result = value
        .to_string()
        .ok()
        .filter(|text| !text.trim().is_empty());
    // WinHttpGetIEProxyConfigForCurrentUser allocates these strings with
    // GlobalAlloc; the API contract requires the caller to release them.
    let _ = GlobalFree(HGLOBAL(value.0.cast()));
    result
}

fn select_windows_proxy(raw: &str, url: &str) -> Option<String> {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("https");
    let mut generic = None;

    for entry in raw
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if let Some((entry_scheme, server)) = entry.split_once('=') {
            if entry_scheme.trim().eq_ignore_ascii_case(scheme) {
                return normalize_windows_proxy(server);
            }
        } else if generic.is_none() {
            generic = normalize_windows_proxy(entry);
        }
    }

    generic
}

fn normalize_windows_proxy(value: &str) -> Option<String> {
    let mut value = value.trim();
    if value
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("PROXY "))
    {
        value = value[6..].trim();
    }
    if value.is_empty() || value.eq_ignore_ascii_case("DIRECT") {
        None
    } else {
        Some(value.to_string())
    }
}

fn log_proxy_selection(state_key: &str, message: &str) {
    static LAST_PROXY_STATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let state = LAST_PROXY_STATE.get_or_init(|| Mutex::new(None));
    let mut last = match state.lock() {
        Ok(last) => last,
        Err(poisoned) => poisoned.into_inner(),
    };
    if last.as_deref() == Some(state_key) {
        return;
    }
    *last = Some(state_key.to_string());
    diagnose::log(message);
}

fn log_proxy_warning_once(key: &str, message: &str) {
    static LOGGED_PROXY_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let warnings = LOGGED_PROXY_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut warnings = match warnings.lock() {
        Ok(warnings) => warnings,
        Err(poisoned) => poisoned.into_inner(),
    };
    if warnings.insert(key.to_string()) {
        diagnose::log(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct TestPayload {
        ok: bool,
    }

    fn response(body: &str) -> ureq::Response {
        ureq::Response::new(200, "OK", body).expect("synthetic response")
    }

    #[test]
    fn selects_generic_windows_proxy() {
        assert_eq!(
            select_windows_proxy("127.0.0.1:7897", "https://chatgpt.com/test"),
            Some("127.0.0.1:7897".to_string())
        );
    }

    #[test]
    fn selects_proxy_matching_request_scheme() {
        let raw = "http=127.0.0.1:8080;https=127.0.0.1:7897;socks=127.0.0.1:1080";
        assert_eq!(
            select_windows_proxy(raw, "https://chatgpt.com/test"),
            Some("127.0.0.1:7897".to_string())
        );
        assert_eq!(
            select_windows_proxy(raw, "http://example.com/test"),
            Some("127.0.0.1:8080".to_string())
        );
    }

    #[test]
    fn does_not_apply_another_protocols_proxy() {
        assert_eq!(
            select_windows_proxy("http=127.0.0.1:8080", "https://chatgpt.com/test"),
            None
        );
    }

    #[test]
    fn accepts_winhttp_proxy_prefix_and_rejects_direct() {
        assert_eq!(
            normalize_windows_proxy("PROXY 127.0.0.1:7897"),
            Some("127.0.0.1:7897".to_string())
        );
        assert_eq!(normalize_windows_proxy("DIRECT"), None);
    }

    #[test]
    fn bounded_json_accepts_a_body_at_the_exact_limit() {
        let body = r#"{"ok":true}"#;
        assert_eq!(
            response_json_limited_with::<TestPayload>(
                response(body),
                body.len() as u64,
                "test response",
            )
            .expect("body at the limit"),
            TestPayload { ok: true }
        );
    }

    #[test]
    fn bounded_json_rejects_a_body_one_byte_over_the_limit() {
        let body = r#"{"ok":true}"#;
        let error = response_json_limited_with::<TestPayload>(
            response(body),
            body.len() as u64 - 1,
            "test response",
        )
        .expect_err("oversized response");
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn bounded_json_rejects_an_oversized_declared_length_before_parsing() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n{\"ok\":true}";
        let response: ureq::Response = raw.parse().expect("synthetic response with header");
        let error = response_json_limited_with::<TestPayload>(response, 12, "test response")
            .expect_err("oversized declared length");
        assert!(error.contains("exceeds"));
    }
}
