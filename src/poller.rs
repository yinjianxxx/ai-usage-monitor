use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::models::{
    AppUsageData, ProviderStatus, UsageData, UsageWindow, FIVE_HOURS_SECONDS, ONE_DAY_SECONDS,
    ONE_WEEK_SECONDS,
};
use crate::tray_icon::TrayIconKind;
use crate::{claude_cli, claude_desktop};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CLAUDE_USER_AGENT: &str = "claude-code/2.1.85";
const CLAUDE_USAGE_NORMAL_POLL_MS: u64 = 180_000;
const CLAUDE_USAGE_FAST_POLL_MS: u64 = 120_000;
const CLAUDE_USAGE_FAST_EXTRA: u32 = 2;
const CLAUDE_RATE_LIMIT_MIN_RETRY_MS: u32 = 300_000;
const CLAUDE_RATE_LIMIT_MAX_RETRY_MS: u32 = 3_600_000;
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
const AUTH_CONFIRM_RETRY_DELAY_MS: u64 = 1_000;
const CODEX_REQUEST_TIMEOUT_SECS: u64 = 10;
const CODEX_RETRY_DELAY_MS: u64 = 1_000;
const ANTIGRAVITY_REQUEST_TIMEOUT_SECS: u64 = 10;
const ANTIGRAVITY_FIVE_HOUR_RESET_GRACE_SECS: u64 = 15 * 60;
pub(crate) const AUTH_REJECTION_RECHECK_MS: u32 = 15 * 60 * 1_000;
const CODEX_KEYRING_SERVICE: &str = "Codex Auth";
const ANTIGRAVITY_CREDENTIAL_TARGET: &str = "gemini:antigravity";
const ANTIGRAVITY_USER_QUOTA_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const ANTIGRAVITY_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
/// grok CLI's own billing endpoint. Not part of the documented xAI API - the
/// CLI uses it to render its `/usage` view - so treat a shape change as a
/// normal provider outage rather than a bug in the response parser.
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// grok CLI marks its own token with this header; the endpoint rejects a
/// bearer token that arrives without it.
const GROK_TOKEN_AUTH_HEADER: &str = "x-xai-token-auth";
const GROK_TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const GROK_REQUEST_TIMEOUT_SECS: u64 = 15;
/// `auth.json` is a registry keyed by `{issuer}::{client_id}`, and an
/// enterprise OIDC entry there is signed by someone other than xAI. Only
/// issuers under xAI's own domain may have their token sent to xAI.
const GROK_ISSUER_HOST_SUFFIX: &str = ".x.ai";
const GROK_PREFERRED_ISSUER_PREFIX: &str = "https://auth.x.ai::";
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    AuthRequired,
    AuthForbidden,
    NoCredentials,
    /// A local credential source exists but could not be turned into a usable
    /// token: unreadable, malformed, or truncated.
    ///
    /// Displays as `AuthenticationFailed` like the rejection errors - the
    /// recovery is the same, sign in again - but is deliberately not one of
    /// them: nothing was rejected remotely, so this must not arm the bounded
    /// service recheck. The credential watch is the right recovery, and it
    /// fires exactly when the CLI rewrites the file.
    CredentialUnusable,
    RateLimited(Option<u32>),
    NetworkUnavailable,
    RequestFailed,
}

#[derive(Debug)]
pub struct PollFailure {
    pub error: PollError,
    pub data: Box<AppUsageData>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ClaudeSources,
    Codex,
    Antigravity,
    Grok,
    /// Several providers at once, naming exactly which.
    ///
    /// The set, not "all four": the watch re-reads a credential every poll
    /// pass and every 15 seconds while polling is parked, so a mode that meant
    /// "everything" would keep reading a provider the user revoked far more
    /// often than the detection sweep ever did.
    Providers([bool; TrayIconKind::COUNT]),
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}
#[derive(Clone)]
struct CachedClaudeUsage {
    token_hash: u64,
    fetched_at: SystemTime,
    data: UsageData,
    fast_polls_remaining: u32,
}

#[derive(Clone, Copy)]
struct ClaudeRateLimit {
    token_hash: u64,
    until: SystemTime,
}

#[derive(Default)]
struct ClaudePollState {
    cached: Option<CachedClaudeUsage>,
    rate_limit: Option<ClaudeRateLimit>,
    auth_rejected_token_hash: Option<u64>,
}

static CLAUDE_POLL_STATE: OnceLock<Mutex<ClaudePollState>> = OnceLock::new();

static CLAUDE_CLI_UPDATE_NOTIFICATION: OnceLock<Mutex<Option<claude_cli::UpdateResult>>> =
    OnceLock::new();

pub(crate) fn take_claude_cli_update_notification() -> Option<claude_cli::UpdateResult> {
    CLAUDE_CLI_UPDATE_NOTIFICATION
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()?
        .take()
}

fn queue_claude_cli_update_notification(result: claude_cli::UpdateResult) {
    if result.version_changed {
        if let Ok(mut pending) = CLAUDE_CLI_UPDATE_NOTIFICATION
            .get_or_init(|| Mutex::new(None))
            .lock()
        {
            *pending = Some(result);
        }
    }
}

#[derive(Clone, Copy)]
struct AuthRejectionBackoff {
    token_hash: u64,
    retry_at: Instant,
}

static CODEX_AUTH_REJECTION: OnceLock<Mutex<Option<AuthRejectionBackoff>>> = OnceLock::new();
static ANTIGRAVITY_AUTH_REJECTION: OnceLock<Mutex<Option<AuthRejectionBackoff>>> = OnceLock::new();
static GROK_AUTH_REJECTION: OnceLock<Mutex<Option<AuthRejectionBackoff>>> = OnceLock::new();

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: Option<f64>,
    /// Provider-defined rolling window; do not infer it from the ChatGPT plan.
    limit_window_seconds: Option<u64>,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct AntigravityAuthFile {
    token: AntigravityTokenData,
}

#[derive(Deserialize)]
struct AntigravityTokenData {
    access_token: String,
}

struct GrokTokenData {
    access_token: String,
}

#[derive(Deserialize)]
struct GrokBillingResponse {
    config: GrokBillingConfig,
}

/// Only the fields this app displays. The endpoint also reports prepaid
/// balances and top-up settings, which are not quota windows.
#[derive(Deserialize)]
struct GrokBillingConfig {
    /// Server-computed used percentage. Omitted at zero usage, which is how
    /// a free-tier account reports "nothing used" - absent is 0%, not
    /// "unknown".
    #[serde(rename = "creditUsagePercent")]
    credit_usage_percent: Option<f64>,
    #[serde(rename = "currentPeriod")]
    current_period: Option<GrokBillingPeriod>,
    #[serde(rename = "onDemandCap")]
    on_demand_cap: Option<GrokAmount>,
    #[serde(rename = "onDemandUsed")]
    on_demand_used: Option<GrokAmount>,
}

#[derive(Deserialize)]
struct GrokBillingPeriod {
    #[serde(rename = "type")]
    period_type: Option<String>,
    start: Option<String>,
    end: String,
}

#[derive(Deserialize)]
struct GrokAmount {
    #[serde(default)]
    val: f64,
}

#[derive(Deserialize)]
struct AntigravityLoadResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityModelsResponse {
    models: HashMap<String, AntigravityModelInfo>,
}

#[derive(Deserialize)]
struct AntigravityModelInfo {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Deserialize)]
struct AntigravityQuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityUserQuotaResponse {
    #[serde(default)]
    buckets: Vec<AntigravityUserQuotaBucket>,
}

#[derive(Deserialize)]
struct AntigravityUserQuotaBucket {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
    disabled: Option<bool>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryResponse {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
    #[serde(rename = "quotaSummary")]
    quota_summary: Option<AntigravityQuotaSummaryEnvelope>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryEnvelope {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    buckets: Option<Vec<AntigravityQuotaSummaryBucket>>,
}

#[derive(Clone, Deserialize)]
struct AntigravityQuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

/// `CredReadW` reports a target that does not exist with this code; anything
/// else it fails with means an entry is there and could not be read.
const ERROR_NOT_FOUND: u32 = 1168;

#[link(name = "Kernel32")]
extern "system" {
    fn GetLastError() -> u32;
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

/// One provider's work for a single poll pass.
///
/// Boxed rather than generic so the pass is a table: adding a provider adds a
/// row here instead of two more parameters and another copy of the
/// error-handling block below.
struct PollTarget<'a> {
    kind: TrayIconKind,
    enabled: bool,
    poll: Box<dyn FnOnce() -> Result<UsageData, PollError> + Send + 'a>,
}

pub fn poll(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_grok: bool,
    force_claude_refresh: bool,
) -> Result<AppUsageData, PollFailure> {
    poll_with(vec![
        PollTarget {
            kind: TrayIconKind::Claude,
            enabled: show_claude_code,
            poll: Box::new(move || poll_claude_code(force_claude_refresh)),
        },
        PollTarget {
            kind: TrayIconKind::Codex,
            enabled: show_codex,
            poll: Box::new(poll_codex),
        },
        PollTarget {
            kind: TrayIconKind::Antigravity,
            enabled: show_antigravity,
            poll: Box::new(poll_antigravity),
        },
        PollTarget {
            kind: TrayIconKind::Grok,
            enabled: show_grok,
            poll: Box::new(poll_grok),
        },
    ])
}

fn poll_with(targets: Vec<PollTarget<'_>>) -> Result<AppUsageData, PollFailure> {
    // Fetch the enabled providers concurrently: results reach the UI only
    // once the whole pass finishes, so a slow endpoint would otherwise hold
    // back every other provider's fresh numbers for its full duration.
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .into_iter()
            .map(
                |PollTarget {
                     kind,
                     enabled,
                     poll,
                 }| (kind, enabled.then(|| scope.spawn(poll))),
            )
            .collect();
        handles
            .into_iter()
            .map(|(kind, handle)| {
                (
                    kind,
                    handle.map(|handle| handle.join().unwrap_or(Err(PollError::RequestFailed))),
                )
            })
            .collect::<Vec<_>>()
    });

    let mut data = AppUsageData::default();
    let mut errors = Vec::new();
    let active_provider_count = results
        .iter()
        .filter(|(_, result)| result.is_some())
        .count();

    for (kind, result) in results {
        let Some(result) = result else { continue };
        match result {
            Ok(usage) => data.provider_mut(kind).usage = Some(usage),
            Err(error) => {
                if active_provider_count > 1 {
                    diagnose::log(format!(
                        "{} usage poll failed: {error:?}",
                        kind.diagnostic_label()
                    ));
                }
                let slot = data.provider_mut(kind);
                slot.error = Some(provider_status(error));
                if let PollError::RateLimited(retry_after_ms) = error {
                    slot.retry_after_ms = retry_after_ms;
                }
                record_poll_error(&mut data, error);
                errors.push(error);
            }
        }
    }

    if data.has_any_usage() {
        Ok(data)
    } else {
        Err(PollFailure {
            error: aggregate_poll_errors(&errors),
            data: Box::new(data),
        })
    }
}

fn aggregate_poll_errors(errors: &[PollError]) -> PollError {
    let Some(&first) = errors.first() else {
        return PollError::RequestFailed;
    };
    if errors.len() == 1 {
        return first;
    }

    let all_require_user_action = errors.iter().all(|error| {
        matches!(
            error,
            PollError::AuthRequired
                | PollError::AuthForbidden
                | PollError::NoCredentials
                | PollError::CredentialUnusable
        )
    });
    if all_require_user_action {
        return first;
    }

    let retry_after_ms = errors
        .iter()
        .filter_map(|error| match error {
            PollError::RateLimited(value) => *value,
            _ => None,
        })
        .max();
    if errors
        .iter()
        .any(|error| matches!(error, PollError::RateLimited(_)))
    {
        PollError::RateLimited(retry_after_ms)
    } else if errors.contains(&PollError::NetworkUnavailable) {
        PollError::NetworkUnavailable
    } else {
        PollError::RequestFailed
    }
}

/// Collapse a poll error to display granularity (see models::ProviderStatus).
///
/// `NoCredentials` stays distinct from the rejection errors: a provider with
/// no local credential was never signed in, so the recovery it needs is a
/// first sign-in, not a re-authentication.
pub fn provider_status(error: PollError) -> ProviderStatus {
    match error {
        PollError::NoCredentials => ProviderStatus::NotSignedIn,
        PollError::AuthRequired | PollError::AuthForbidden | PollError::CredentialUnusable => {
            ProviderStatus::AuthenticationFailed
        }
        PollError::RateLimited(_) => ProviderStatus::RateLimited,
        PollError::NetworkUnavailable => ProviderStatus::NetworkUnavailable,
        PollError::RequestFailed => ProviderStatus::RequestFailed,
    }
}

fn record_poll_error(data: &mut AppUsageData, error: PollError) {
    if matches!(error, PollError::AuthRequired | PollError::AuthForbidden) {
        data.remote_auth_rejection = true;
    }
}

fn claude_poll_state() -> &'static Mutex<ClaudePollState> {
    CLAUDE_POLL_STATE.get_or_init(|| Mutex::new(ClaudePollState::default()))
}

fn token_hash(token: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

fn note_claude_auth_candidate(token_hash: u64) {
    if let Ok(mut state) = claude_poll_state().lock() {
        if state.auth_rejected_token_hash != Some(token_hash) {
            state.auth_rejected_token_hash = None;
        }
    }
}

fn record_claude_auth_rejection(token_hash: u64) {
    if let Ok(mut state) = claude_poll_state().lock() {
        state.auth_rejected_token_hash = Some(token_hash);
    }
}

fn clear_claude_auth_rejection() {
    if let Ok(mut state) = claude_poll_state().lock() {
        state.auth_rejected_token_hash = None;
    }
}

fn claude_auth_rejection_matches(token_hash: u64) -> bool {
    claude_poll_state()
        .lock()
        .is_ok_and(|state| state.auth_rejected_token_hash == Some(token_hash))
}

fn claude_poll_interval_ms(cached: &CachedClaudeUsage) -> u64 {
    if cached.fast_polls_remaining > 0 {
        CLAUDE_USAGE_FAST_POLL_MS
    } else {
        CLAUDE_USAGE_NORMAL_POLL_MS
    }
}

fn claude_cache_is_fresh(
    cached: &CachedClaudeUsage,
    token_hash: u64,
    force_refresh: bool,
    now: SystemTime,
) -> bool {
    if force_refresh || cached.token_hash != token_hash {
        return false;
    }
    let Ok(age) = now.duration_since(cached.fetched_at) else {
        return false;
    };
    // A snapshot taken before a window's reset goes stale the moment that
    // reset passes: the server reports the refilled window while the cache
    // would keep showing it exhausted for up to a full cadence. This cannot
    // loop against a lagging server: a confirming fetch whose reply still
    // carries the old reset time re-caches with fetched_at past that reset.
    let reset_elapsed = cached.data.windows.iter().any(
        |window| matches!(window.resets_at, Some(reset) if reset > cached.fetched_at && now >= reset),
    );
    if reset_elapsed {
        return false;
    }
    age < Duration::from_millis(claude_poll_interval_ms(cached))
}

/// Padding past the exact cooldown deadline so the aligned tick's fetch
/// cannot land a few milliseconds early and be served from the cache.
const CLAUDE_ALIGN_MARGIN_MS: u64 = 250;
/// Floor for an aligned tick so an overdue deadline never arms a zero-delay
/// timer loop.
const CLAUDE_ALIGN_MIN_DELAY_MS: u64 = 1_000;

/// Delay until the next poll tick should fire so the Claude fetch lands right
/// at its cache-cooldown deadline (180s/120s after the previous fetch).
/// Without this the deadline falls between fixed ticks and the observed
/// cadence stretches by up to one user poll interval. Returns None when the
/// fixed schedule should own the timer: no cached data yet, a rate-limit
/// backoff pending, or a user cadence at least as coarse as the cooldown
/// (every tick fetches then, so there is nothing to align).
pub fn claude_aligned_poll_delay_ms(poll_interval_ms: u32) -> Option<u32> {
    let state = claude_poll_state().lock().ok()?;
    if state.rate_limit.is_some() {
        return None;
    }
    let cached = state.cached.as_ref()?;
    let age = SystemTime::now().duration_since(cached.fetched_at).ok()?;
    aligned_poll_delay_ms(poll_interval_ms, claude_poll_interval_ms(cached), age)
}

fn aligned_poll_delay_ms(poll_interval_ms: u32, cadence_ms: u64, age: Duration) -> Option<u32> {
    if u64::from(poll_interval_ms) >= cadence_ms {
        return None;
    }
    let age_ms = age.as_millis().min(u128::from(u64::MAX)) as u64;
    let due_ms = cadence_ms
        .saturating_sub(age_ms)
        .saturating_add(CLAUDE_ALIGN_MARGIN_MS);
    Some(due_ms.clamp(CLAUDE_ALIGN_MIN_DELAY_MS, u64::from(poll_interval_ms)) as u32)
}

fn cached_claude_usage(token_hash: u64, force_refresh: bool) -> Option<UsageData> {
    let state = claude_poll_state().lock().ok()?;
    let cached = state.cached.as_ref()?;
    let now = SystemTime::now();
    if !claude_cache_is_fresh(cached, token_hash, force_refresh, now) {
        if force_refresh && cached.token_hash == token_hash {
            diagnose::log("Claude usage manual refresh bypassed the normal cache cooldown");
        }
        return None;
    }
    let age = now.duration_since(cached.fetched_at).ok()?;
    let interval_ms = claude_poll_interval_ms(cached);
    diagnose::log(format!(
        "Claude usage poll skipped; using cached usage data age={}s cadence={}s",
        age.as_secs(),
        interval_ms / 1000
    ));
    Some(cached.data.clone())
}

fn claude_usage_increased(previous: &UsageData, current: &UsageData) -> bool {
    current.windows.iter().any(|current_window| {
        previous.windows.iter().any(|previous_window| {
            previous_window.duration_seconds == current_window.duration_seconds
                && previous_window.source_label == current_window.source_label
                && current_window.percentage > previous_window.percentage
        })
    })
}

fn next_claude_fast_polls(
    cached: Option<&CachedClaudeUsage>,
    token_hash: u64,
    data: &UsageData,
) -> u32 {
    cached
        .filter(|cached| cached.token_hash == token_hash)
        .map_or(0, |cached| {
            if claude_usage_increased(&cached.data, data) {
                CLAUDE_USAGE_FAST_EXTRA + 1
            } else {
                cached.fast_polls_remaining.saturating_sub(1)
            }
        })
}

fn store_cached_claude_usage(token_hash: u64, data: &UsageData) {
    if let Ok(mut state) = claude_poll_state().lock() {
        let fast_polls_remaining = next_claude_fast_polls(state.cached.as_ref(), token_hash, data);
        let interval_ms = if fast_polls_remaining > 0 {
            CLAUDE_USAGE_FAST_POLL_MS
        } else {
            CLAUDE_USAGE_NORMAL_POLL_MS
        };
        diagnose::log(format!(
            "Claude usage poll succeeded; next cadence={}s fast_polls_remaining={fast_polls_remaining}",
            interval_ms / 1000
        ));
        state.cached = Some(CachedClaudeUsage {
            token_hash,
            fetched_at: SystemTime::now(),
            data: data.clone(),
            fast_polls_remaining,
        });
        state.rate_limit = None;
    }
}

fn claude_rate_limit_delay_ms(retry_after_ms: Option<u32>) -> u32 {
    retry_after_ms
        .unwrap_or(CLAUDE_RATE_LIMIT_MIN_RETRY_MS)
        .clamp(
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS,
            CLAUDE_RATE_LIMIT_MAX_RETRY_MS,
        )
}

fn store_claude_rate_limit(token_hash: u64, retry_after_ms: Option<u32>) -> u32 {
    let delay_ms = claude_rate_limit_delay_ms(retry_after_ms);
    if let Ok(mut state) = claude_poll_state().lock() {
        state.rate_limit = Some(ClaudeRateLimit {
            token_hash,
            until: SystemTime::now()
                .checked_add(Duration::from_millis(delay_ms as u64))
                .unwrap_or_else(SystemTime::now),
        });
    }
    delay_ms
}

fn claude_rate_limit_remaining_ms(token_hash: u64) -> Option<u32> {
    let mut state = claude_poll_state().lock().ok()?;
    let rate_limit = state.rate_limit?;
    if rate_limit.token_hash != token_hash {
        state.rate_limit = None;
        return None;
    }
    match rate_limit.until.duration_since(SystemTime::now()) {
        Ok(remaining) if !remaining.is_zero() => {
            Some(remaining.as_millis().clamp(1, u32::MAX as u128) as u32)
        }
        _ => {
            state.rate_limit = None;
            None
        }
    }
}

fn poll_claude_code(force_refresh: bool) -> Result<UsageData, PollError> {
    let creds = match select_claude_credentials() {
        ClaudeCredentialSelection::Usable(creds) => creds,
        ClaudeCredentialSelection::Refreshable(creds) => {
            diagnose::log(format!(
                "Claude access credential is locally expired; confirming with the usage endpoint source={}",
                creds.source.diagnostic_label()
            ));
            creds
        }
        ClaudeCredentialSelection::LoginRequired { source, problem } => {
            let source = source
                .as_ref()
                .map(CredentialSource::diagnostic_label)
                .unwrap_or_else(|| "none".to_string());
            diagnose::log(format!(
                "Claude credential is not usable source={source} reason={}",
                problem.code()
            ));
            if let Some(result) = try_claude_desktop(force_refresh) {
                return result;
            }
            let poll_error = claude_credential_problem_poll_error(problem);
            if matches!(poll_error, PollError::NoCredentials) {
                clear_claude_auth_rejection();
            }
            return Err(poll_error);
        }
    };

    let token_hash = token_hash(&creds.access_token);
    note_claude_auth_candidate(token_hash);
    if let Some(remaining_ms) = claude_rate_limit_remaining_ms(token_hash) {
        diagnose::log(format!(
            "Claude usage poll skipped; rate-limit backoff remaining={}s",
            remaining_ms.div_ceil(1000)
        ));
        return Err(PollError::RateLimited(Some(remaining_ms)));
    }
    if let Some(cached) = cached_claude_usage(token_hash, force_refresh) {
        return Ok(cached);
    }

    match fetch_usage_with_fallback(&creds.access_token) {
        Ok(data) => {
            clear_claude_auth_rejection();
            store_cached_claude_usage(token_hash, &data);
            Ok(data)
        }
        Err(PollError::AuthRequired | PollError::AuthForbidden) => {
            if should_auto_recover_claude(&creds.source) {
                diagnose::log("Claude credential rejected; starting hidden `claude update`");
                match recover_claude_credentials(&creds) {
                    Ok((new_hash, data, update_result)) => {
                        clear_claude_auth_rejection();
                        store_cached_claude_usage(new_hash, &data);
                        if let Some(update_result) = update_result {
                            queue_claude_cli_update_notification(update_result);
                        }
                        diagnose::log(
                            "Claude credential recovery succeeded and usage retry passed",
                        );
                        Ok(data)
                    }
                    Err(failure) => {
                        let authentication_failure = matches!(
                            failure.poll_error,
                            PollError::AuthRequired
                                | PollError::AuthForbidden
                                | PollError::NoCredentials
                        );
                        if authentication_failure {
                            if let Some(result) = try_claude_desktop(force_refresh) {
                                return result;
                            }
                            record_claude_auth_rejection(token_hash);
                        } else {
                            clear_claude_auth_rejection();
                        }
                        diagnose::log(format!(
                            "Claude credential recovery failed reason={}",
                            failure.reason
                        ));
                        Err(failure.poll_error)
                    }
                }
            } else {
                record_claude_auth_rejection(token_hash);
                diagnose::log(
                    "Claude credential rejected; credential source is WSL and CLI recovery is Windows-only",
                );
                if let Some(result) = try_claude_desktop(force_refresh) {
                    return result;
                }
                Err(PollError::AuthRequired)
            }
        }
        Err(PollError::RateLimited(retry_after_ms)) => {
            let delay_ms = store_claude_rate_limit(token_hash, retry_after_ms);
            Err(PollError::RateLimited(Some(delay_ms)))
        }
        Err(error) => Err(error),
    }
}

/// Tries Claude Desktop when it has a readable, eligible token cache. `None`
/// means the normal CLI/WSL result remains authoritative; `Some` means at
/// least one Desktop candidate was available and the enclosed result is
/// authoritative.
fn try_claude_desktop(force_refresh: bool) -> Option<Result<UsageData, PollError>> {
    if !claude_desktop::enabled() {
        return None;
    }

    let candidates = match claude_desktop::read_candidates(now_unix_millis()) {
        Ok(candidates) => candidates,
        Err(error) => {
            diagnose::log(format!(
                "Claude Desktop credentials unavailable reason={}",
                error.code()
            ));
            return None;
        }
    };
    diagnose::log(format!(
        "Claude Desktop credentials found {} eligible access-token candidate(s)",
        candidates.len()
    ));
    Some(poll_claude_desktop_candidates(candidates, force_refresh))
}

fn poll_claude_desktop_candidates(
    candidates: Vec<claude_desktop::DesktopTokenCandidate>,
    force_refresh: bool,
) -> Result<UsageData, PollError> {
    let mut last_auth_rejection = None;

    for candidate in candidates {
        let token_hash = token_hash(candidate.access_token());
        note_claude_auth_candidate(token_hash);
        if let Some(remaining_ms) = claude_rate_limit_remaining_ms(token_hash) {
            return Err(PollError::RateLimited(Some(remaining_ms)));
        }
        if let Some(cached) = cached_claude_usage(token_hash, force_refresh) {
            return Ok(cached);
        }

        diagnose::log(format!(
            "Claude Desktop credential trying source={}",
            candidate.source_label()
        ));
        match fetch_usage_with_fallback(candidate.access_token()) {
            Ok(data) => {
                clear_claude_auth_rejection();
                store_cached_claude_usage(token_hash, &data);
                diagnose::log("Claude Desktop credential usage retry succeeded");
                return Ok(data);
            }
            Err(PollError::AuthRequired | PollError::AuthForbidden) => {
                last_auth_rejection = Some(token_hash);
            }
            Err(PollError::RateLimited(retry_after_ms)) => {
                let delay_ms = store_claude_rate_limit(token_hash, retry_after_ms);
                return Err(PollError::RateLimited(Some(delay_ms)));
            }
            Err(error) => return Err(error),
        }
    }

    if let Some(token_hash) = last_auth_rejection {
        record_claude_auth_rejection(token_hash);
        diagnose::log("Claude Desktop credentials exhausted rejected candidates");
        Err(PollError::AuthRequired)
    } else {
        Err(PollError::NoCredentials)
    }
}

fn should_auto_recover_claude(source: &CredentialSource) -> bool {
    matches!(source, CredentialSource::Windows(_))
}

fn recover_claude_credentials(
    previous: &Credentials,
) -> Result<(u64, UsageData, Option<claude_cli::UpdateResult>), ClaudeRecoveryFailure> {
    let previous_hash = token_hash(&previous.access_token);
    if let Ok(current) = read_credentials_from_source(&previous.source) {
        if token_hash(&current.access_token) != previous_hash {
            diagnose::log(
                "Claude credential changed during auth confirmation; retrying without CLI update",
            );
            return verify_recovered_claude_credentials(current, None);
        }
    }

    let update_result = claude_cli::run_update().map_err(|reason| ClaudeRecoveryFailure {
        reason,
        poll_error: PollError::AuthRequired,
    })?;
    let refreshed = read_credentials_from_source(&previous.source).map_err(|problem| {
        ClaudeRecoveryFailure {
            reason: format!("credential_{}", problem.code()),
            poll_error: PollError::AuthRequired,
        }
    })?;
    if token_hash(&refreshed.access_token) == previous_hash {
        return Err(ClaudeRecoveryFailure {
            reason: "credential_unchanged".to_string(),
            poll_error: PollError::AuthRequired,
        });
    }

    verify_recovered_claude_credentials(refreshed, Some(update_result))
}

fn verify_recovered_claude_credentials(
    refreshed: Credentials,
    update_result: Option<claude_cli::UpdateResult>,
) -> Result<(u64, UsageData, Option<claude_cli::UpdateResult>), ClaudeRecoveryFailure> {
    let refreshed_hash = token_hash(&refreshed.access_token);
    let usage = match fetch_usage_with_fallback(&refreshed.access_token) {
        Ok(data) => data,
        Err(PollError::RateLimited(retry_after_ms)) => {
            let delay_ms = store_claude_rate_limit(refreshed_hash, retry_after_ms);
            return Err(ClaudeRecoveryFailure {
                reason: format!("usage_retry_rate_limited_{delay_ms}"),
                poll_error: PollError::RateLimited(Some(delay_ms)),
            });
        }
        Err(error) => {
            return Err(ClaudeRecoveryFailure {
                reason: format!("usage_retry_{error:?}"),
                poll_error: error,
            })
        }
    };
    Ok((refreshed_hash, usage, update_result))
}

struct ClaudeRecoveryFailure {
    reason: String,
    poll_error: PollError,
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        LocalCredential::Usable(creds) => creds,
        LocalCredential::Missing => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
        LocalCredential::Unusable => {
            diagnose::log(
                "Codex usage poll failed: the Codex credentials on this machine are unusable",
            );
            return Err(PollError::CredentialUnusable);
        }
    };

    let token_hash = token_hash(&creds.access_token);
    if auth_rejection_is_backed_off(&CODEX_AUTH_REJECTION, token_hash) {
        diagnose::log("Codex usage poll skipped; rejected credentials have not changed");
        return Err(PollError::AuthRequired);
    }

    match fetch_codex_usage(&creds.access_token, creds.account_id.as_deref()) {
        Ok(data) => {
            clear_auth_rejection(&CODEX_AUTH_REJECTION);
            Ok(data)
        }
        Err(PollError::AuthRequired | PollError::AuthForbidden) => {
            record_auth_rejection(&CODEX_AUTH_REJECTION, token_hash);
            diagnose::log(
                "Codex usage endpoint returned auth required; automatic CLI refresh is disabled because it would require running a model-capable Codex command.",
            );
            Err(PollError::AuthRequired)
        }
        Err(error) => Err(error),
    }
}
fn poll_antigravity() -> Result<UsageData, PollError> {
    let creds = match read_antigravity_credentials() {
        LocalCredential::Usable(creds) => creds,
        LocalCredential::Missing => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
        LocalCredential::Unusable => {
            diagnose::log("Antigravity usage poll failed: the Antigravity credentials on this machine are unusable");
            return Err(PollError::CredentialUnusable);
        }
    };

    let token_hash = token_hash(&creds.access_token);
    if auth_rejection_is_backed_off(&ANTIGRAVITY_AUTH_REJECTION, token_hash) {
        diagnose::log("Antigravity usage poll skipped; rejected credentials have not changed");
        return Err(PollError::AuthRequired);
    }

    match fetch_antigravity_usage(&creds.access_token) {
        Ok(data) => {
            clear_auth_rejection(&ANTIGRAVITY_AUTH_REJECTION);
            Ok(data)
        }
        Err(PollError::AuthRequired | PollError::AuthForbidden) => {
            record_auth_rejection(&ANTIGRAVITY_AUTH_REJECTION, token_hash);
            Err(PollError::AuthRequired)
        }
        Err(error) => Err(error),
    }
}

fn poll_grok() -> Result<UsageData, PollError> {
    let creds = match read_grok_credentials() {
        LocalCredential::Usable(creds) => creds,
        LocalCredential::Missing => {
            diagnose::log("Grok usage poll failed: no Grok credentials found");
            return Err(PollError::NoCredentials);
        }
        LocalCredential::Unusable => {
            diagnose::log(
                "Grok usage poll failed: the Grok credentials on this machine are unusable",
            );
            return Err(PollError::CredentialUnusable);
        }
    };

    // No local expiry check before the request. grok CLI mints six-hour
    // access tokens and refreshes them only when it is itself run, so a
    // locally expired token is the normal state between sessions - and the
    // server, not this app's clock, decides whether it still works. A
    // rejection parks the provider until the credential file changes, which
    // is exactly when grok CLI has refreshed it.
    let token_hash = token_hash(&creds.access_token);
    if auth_rejection_is_backed_off(&GROK_AUTH_REJECTION, token_hash) {
        diagnose::log("Grok usage poll skipped; rejected credentials have not changed");
        return Err(PollError::AuthRequired);
    }

    match fetch_grok_usage(&creds.access_token) {
        Ok(data) => {
            clear_auth_rejection(&GROK_AUTH_REJECTION);
            Ok(data)
        }
        Err(PollError::AuthRequired | PollError::AuthForbidden) => {
            record_auth_rejection(&GROK_AUTH_REJECTION, token_hash);
            Err(PollError::AuthRequired)
        }
        Err(error) => Err(error),
    }
}

fn fetch_grok_usage(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent_for_url(
        GROK_BILLING_URL,
        Duration::from_secs(GROK_REQUEST_TIMEOUT_SECS),
    )?;
    let resp = match agent
        .get(GROK_BILLING_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set(GROK_TOKEN_AUTH_HEADER, GROK_TOKEN_AUTH_VALUE)
        .set("Accept", "application/json")
        .set("User-Agent", "gengchou")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Grok billing endpoint",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Grok billing endpoint request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: GrokBillingResponse =
        match crate::http_client::response_json_limited(resp, "Grok billing response") {
            Ok(response) => response,
            Err(error) => {
                diagnose::log_error("unable to parse Grok billing response", error);
                return Err(PollError::RequestFailed);
            }
        };

    grok_usage_from_billing(response).ok_or_else(|| {
        diagnose::log("Grok billing response did not contain a usable quota window");
        PollError::RequestFailed
    })
}

/// Turn one billing period into the single window Grok reports.
///
/// The period is mandatory: without it there is no reset time, and a
/// percentage with no reset would overwrite a complete cached window with
/// half a one. The window length is measured from the period itself rather
/// than assumed from its type, so a calendar month stays 28-31 days instead
/// of a rounded 30.
fn grok_usage_from_billing(response: GrokBillingResponse) -> Option<UsageData> {
    let config = response.config;
    let period = config.current_period?;
    let resets_at = parse_iso8601(Some(&period.end))?;
    let duration_seconds = period
        .start
        .as_deref()
        .and_then(|start| parse_iso8601(Some(start)))
        .and_then(|start| resets_at.duration_since(start).ok())
        .map(|duration| duration.as_secs())
        .filter(|seconds| *seconds > 0);
    let percent = grok_used_percent(
        config.credit_usage_percent,
        config.on_demand_used.map(|amount| amount.val),
        config.on_demand_cap.map(|amount| amount.val),
    )?;
    // Only when the length is unmeasurable does the period type get used, and
    // then only as the label the surfaces already show for unknown windows.
    let source_label = duration_seconds
        .is_none()
        .then(|| grok_period_label(period.period_type.as_deref()));
    Some(UsageData::from_windows(vec![UsageWindow::new(
        percent,
        Some(resets_at),
        duration_seconds,
    )
    .with_source_label(source_label)]))
}

fn grok_period_label(period_type: Option<&str>) -> String {
    period_type
        .unwrap_or_default()
        .trim()
        .strip_prefix("USAGE_PERIOD_TYPE_")
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Used percentage, preferring the value the server already computed.
///
/// A missing percentage with no on-demand allowance is zero usage, not a
/// failure: that is what a free-tier account returns.
fn grok_used_percent(
    credit_usage_percent: Option<f64>,
    on_demand_used: Option<f64>,
    on_demand_cap: Option<f64>,
) -> Option<f64> {
    if let Some(percent) = credit_usage_percent {
        return (percent.is_finite() && (0.0..=100.0).contains(&percent)).then_some(percent);
    }
    match (on_demand_used, on_demand_cap) {
        (Some(used), Some(cap)) if cap > 0.0 => {
            (used.is_finite() && used >= 0.0).then(|| (used / cap * 100.0).clamp(0.0, 100.0))
        }
        _ => Some(0.0),
    }
}

fn auth_rejection_is_backed_off(
    state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>,
    token_hash: u64,
) -> bool {
    let Some(state) = state.get() else {
        return false;
    };
    let Ok(mut rejection) = state.lock() else {
        return false;
    };
    match *rejection {
        Some(value) if value.token_hash == token_hash && Instant::now() < value.retry_at => true,
        Some(_) => {
            *rejection = None;
            false
        }
        None => false,
    }
}

fn record_auth_rejection(state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>, token_hash: u64) {
    let state = state.get_or_init(|| Mutex::new(None));
    if let Ok(mut rejection) = state.lock() {
        *rejection = Some(AuthRejectionBackoff {
            token_hash,
            retry_at: Instant::now() + Duration::from_millis(u64::from(AUTH_REJECTION_RECHECK_MS)),
        });
    }
}

fn clear_auth_rejection(state: &OnceLock<Mutex<Option<AuthRejectionBackoff>>>) {
    if let Some(state) = state.get() {
        if let Ok(mut rejection) = state.lock() {
            *rejection = None;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandRunError {
    SpawnFailed,
    TimedOut,
    WaitFailed,
}

/// Spawn a command and wait up to `timeout` for it to finish while preserving
/// the distinction between an unavailable probe and a completed command.
fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, CommandRunError> {
    let mut child = cmd.spawn().map_err(|_| CommandRunError::SpawnFailed)?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|_| CommandRunError::WaitFailed)
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandRunError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return Err(CommandRunError::WaitFailed),
        }
    }
}

fn build_agent_for_url(url: &str, timeout: Duration) -> Result<ureq::Agent, PollError> {
    crate::http_client::build_agent(url, timeout).map_err(|error| {
        diagnose::log_error("unable to initialize provider HTTP client", error);
        PollError::NetworkUnavailable
    })
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    let mut snapshot = match mode {
        CredentialWatchMode::ClaudeSources => claude_credential_watch_snapshot(),
        CredentialWatchMode::Codex => vec![codex_credential_watch_signature()],
        CredentialWatchMode::Antigravity => vec![antigravity_credential_watch_signature()],
        CredentialWatchMode::Grok => vec![grok_credential_watch_signature()],
        CredentialWatchMode::Providers(watched) => {
            let mut snapshot = Vec::new();
            if watched[TrayIconKind::Claude.index()] {
                snapshot.extend(claude_credential_watch_snapshot());
            }
            if watched[TrayIconKind::Codex.index()] {
                snapshot.push(codex_credential_watch_signature());
            }
            if watched[TrayIconKind::Antigravity.index()] {
                snapshot.push(antigravity_credential_watch_signature());
            }
            if watched[TrayIconKind::Grok.index()] {
                snapshot.push(grok_credential_watch_signature());
            }
            snapshot
        }
    };
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

/// Which providers have a credential on this machine right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetectedProviders {
    pub claude: bool,
    pub codex: bool,
    pub antigravity: bool,
    pub grok: bool,
}

impl DetectedProviders {
    pub fn any(self) -> bool {
        self.claude || self.codex || self.antigravity || self.grok
    }
}

/// What a local credential source turned out to be.
///
/// Only the Windows-local reads are classified this way. A WSL probe stays an
/// `Option`: `wsl.exe` failing to start, a broken distro and a distro that
/// simply has no credential are indistinguishable from the outside, so
/// calling any of them "unusable" would raise a sign-in warning for a probe
/// that was merely slow. Those keep moving on to the next source.
enum LocalCredential<T> {
    /// Nothing to read: no file, no keyring entry, no home directory.
    Missing,
    /// A source is there but did not yield a token.
    Unusable,
    Usable(T),
}

impl<T> LocalCredential<T> {
    fn is_usable(&self) -> bool {
        matches!(self, Self::Usable(_))
    }

    fn usable(self) -> Option<T> {
        match self {
            Self::Usable(value) => Some(value),
            _ => None,
        }
    }

    /// Fall back to another source, keeping the more specific verdict.
    ///
    /// A usable credential anywhere wins. Otherwise `Unusable` outranks
    /// `Missing`: one source being broken is what the user needs to hear,
    /// even if the others simply are not there.
    fn or_else(self, next: impl FnOnce() -> LocalCredential<T>) -> LocalCredential<T> {
        match self {
            Self::Usable(value) => Self::Usable(value),
            first => match next() {
                Self::Usable(value) => Self::Usable(value),
                Self::Unusable => Self::Unusable,
                Self::Missing => first,
            },
        }
    }

    fn or_else_wsl(self, next: impl FnOnce() -> Option<T>) -> LocalCredential<T> {
        self.or_else(|| match next() {
            Some(value) => Self::Usable(value),
            None => Self::Missing,
        })
    }
}

/// Which providers a detection pass may read credentials for.
///
/// Access is granted once for every provider but revoked one at a time, so a
/// pass that ignored this would read a source the user turned off - at every
/// start and then every half hour, for as long as the app runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetectionScope {
    pub claude: bool,
    pub codex: bool,
    pub antigravity: bool,
    pub grok: bool,
}

impl DetectionScope {
    pub fn any(self) -> bool {
        self.claude || self.codex || self.antigravity || self.grok
    }
}

/// The raw source probes the detector reduces to an answer.
///
/// Split from the probing itself so the decision is testable without a
/// filesystem, a Windows keyring, or WSL - the same shape as
/// `claude_config_dir_from` and `codex_direct_keyring_target_from_path`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetectionInputs {
    pub claude_windows: bool,
    pub claude_desktop: bool,
    pub claude_wsl: bool,
    pub codex_windows: bool,
    pub codex_wsl: bool,
    pub antigravity_windows: bool,
    pub antigravity_wsl: bool,
    pub grok_windows: bool,
    pub grok_wsl: bool,
}

pub fn detect_from(inputs: DetectionInputs) -> DetectedProviders {
    DetectedProviders {
        claude: inputs.claude_windows || inputs.claude_desktop || inputs.claude_wsl,
        codex: inputs.codex_windows || inputs.codex_wsl,
        antigravity: inputs.antigravity_windows || inputs.antigravity_wsl,
        grok: inputs.grok_windows || inputs.grok_wsl,
    }
}

/// Probe the credential sources that are cheap to reach, for every provider
/// the caller's scope still allows.
///
/// "Cheap" excludes stopped WSL distros: reading inside one starts its virtual
/// machine, and this runs on a timer. Providers that only live in a stopped
/// distro stay undetected and can be enabled by hand, which then goes through
/// the full credential resolution including every distro.
///
/// Runs on a worker thread - never call it from the UI thread.
///
/// `scope` is honoured before any source is touched: every probe below is
/// guarded by it, and `&&` short-circuits, so a provider outside the scope has
/// none of its files, keyring entries or WSL distros read at all.
pub fn detect_signed_in_providers(scope: DetectionScope) -> DetectedProviders {
    if !scope.any() {
        diagnose::log("provider detection skipped: no provider is in scope");
        return DetectedProviders::default();
    }
    let running = list_running_wsl_distros();
    let claude_wsl = scope.claude
        && running.iter().any(|distro| {
            read_credentials_from_source(&CredentialSource::Wsl {
                distro: distro.clone(),
            })
            .is_ok()
        });
    let inputs = DetectionInputs {
        claude_windows: scope.claude
            && windows_credential_source()
                .is_some_and(|source| read_credentials_from_source(&source).is_ok()),
        claude_desktop: scope.claude
            && claude_desktop::enabled()
            && claude_desktop::read_candidates(now_unix_millis()).is_ok(),
        claude_wsl,
        codex_windows: scope.codex && read_windows_codex_credentials().is_usable(),
        codex_wsl: scope.codex
            && !running.is_empty()
            && read_codex_credentials_from_wsl(&running).is_some(),
        antigravity_windows: scope.antigravity
            && read_windows_antigravity_credentials().is_usable(),
        antigravity_wsl: scope.antigravity
            && !running.is_empty()
            && read_antigravity_credentials_from_wsl(&running).is_some(),
        grok_windows: scope.grok && read_windows_grok_credentials().is_usable(),
        grok_wsl: scope.grok
            && !running.is_empty()
            && read_grok_credentials_from_wsl(&running).is_some(),
    };
    let detected = detect_from(inputs);
    diagnose::log(format!(
        "provider detection: claude={} codex={} antigravity={} grok={} (scope: claude={} codex={} antigravity={} grok={}, running WSL distros: {})",
        detected.claude,
        detected.codex,
        detected.antigravity,
        detected.grok,
        scope.claude,
        scope.codex,
        scope.antigravity,
        scope.grok,
        running.len()
    ));
    detected
}

fn claude_credential_watch_snapshot() -> CredentialWatchSnapshot {
    let mut snapshot = all_known_credential_sources()
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect::<Vec<_>>();
    if claude_desktop::enabled() {
        snapshot.extend(
            claude_desktop::credential_watch_paths()
                .into_iter()
                .map(|path| windows_credential_watch_signature(&path)),
        );
    }
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn claude_config_dir_from(configured: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    configured
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".claude")))
}

fn windows_claude_config_dir() -> Option<PathBuf> {
    claude_config_dir_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn windows_credential_source() -> Option<CredentialSource> {
    Some(CredentialSource::Windows(
        windows_claude_config_dir()?.join(".credentials.json"),
    ))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::read(path) {
        Ok(content) => content_watch_signature(&key, &content),
        Err(_) => format!("{key}|missing"),
    }
}

/// Deliberately Windows-only, unlike the credential read above.
///
/// The watch re-samples every 15 seconds while polling is parked. Shelling
/// out to `wsl.exe` at that rate would be far more expensive than the problem
/// it solves, so a sign-in that happens inside WSL is picked up by the
/// slower authentication recheck or the detection sweep instead.
fn codex_credential_watch_signature() -> String {
    let Some(codex_home) = codex_home() else {
        return "win:codex-auth|missing".to_string();
    };
    let auth_path = codex_home.join("auth.json");
    let file_signature = windows_credential_watch_signature(&auth_path);
    let keyring_signature = codex_direct_keyring_target(&codex_home)
        .and_then(|target| {
            read_windows_generic_credential_quiet(&target).map(|content| {
                content_watch_signature(&format!("wincred:{target}"), content.as_bytes())
            })
        })
        .unwrap_or_else(|| "wincred:codex-auth|missing".to_string());
    format!("{file_signature};{keyring_signature}")
}

fn content_watch_signature(key: &str, content: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{key}|present|{}|{}", content.len(), hasher.finish())
}

const WSL_CREDENTIAL_MISSING_EXIT: i32 = 44;
const WSL_CREDENTIAL_UNREADABLE_EXIT: i32 = 45;
const WSL_CREDENTIAL_READ_SCRIPT: &str = r#"
config_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
credential_path="$config_dir/.credentials.json"
if [ ! -e "$credential_path" ]; then exit 44; fi
if [ ! -r "$credential_path" ]; then exit 45; fi
cat -- "$credential_path"
"#;
const WSL_CREDENTIAL_PATH_SCRIPT: &str = r#"
config_dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
printf '%s\n' "$config_dir/.credentials.json"
"#;

fn read_wsl_credential_bytes(distro: &str) -> Result<Vec<u8>, ClaudeCredentialProblem> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            // `-e` and not `--`: with `--`, wsl.exe hands the remaining
            // arguments to the distribution's login shell as one command line,
            // which re-parses them and drops the quoting around the script. The
            // script then runs statement by statement in that outer shell, so
            // any variable it assigns is gone by the next statement and every
            // path check silently fails. `-e` executes the binary directly, so
            // the script reaches `sh -lc` as a single argument.
            .arg("-e")
            .arg("sh")
            .arg("-lc")
            .arg(WSL_CREDENTIAL_READ_SCRIPT)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )
    .map_err(|_| ClaudeCredentialProblem::ProbeUnavailable)?;

    match output.status.code() {
        Some(WSL_CREDENTIAL_MISSING_EXIT) => Err(ClaudeCredentialProblem::Missing),
        Some(WSL_CREDENTIAL_UNREADABLE_EXIT) => Err(ClaudeCredentialProblem::Unreadable),
        _ if output.status.success() => Ok(output.stdout),
        _ => Err(ClaudeCredentialProblem::ProbeUnavailable),
    }
}

/// Codex inside WSL keeps `auth.json` under `$CODEX_HOME` (default
/// `~/.codex`), the same layout as the Windows install. The Windows keyring
/// fallback has no WSL equivalent, so the file is the only source there.
const WSL_CODEX_CREDENTIAL_SCRIPT: &str = r#"
config_dir="${CODEX_HOME:-$HOME/.codex}"
credential_path="$config_dir/auth.json"
if [ ! -e "$credential_path" ]; then exit 44; fi
if [ ! -r "$credential_path" ]; then exit 45; fi
cat -- "$credential_path"
"#;

/// grok CLI keeps `auth.json` under `$GROK_HOME` (default `~/.grok`), the
/// same layout as the Windows install and with no keyring fallback.
const WSL_GROK_CREDENTIAL_SCRIPT: &str = r#"
config_dir="${GROK_HOME:-$HOME/.grok}"
credential_path="$config_dir/auth.json"
if [ ! -e "$credential_path" ]; then exit 44; fi
if [ ! -r "$credential_path" ]; then exit 45; fi
cat -- "$credential_path"
"#;

/// Antigravity CLI prefers the OS keyring, but WSL normally has no Secret
/// Service, so it falls back to this file. That fallback is the only
/// Antigravity credential in a WSL install that Windows can reach - the
/// `gemini:antigravity` Credential Manager entry is written by the Windows
/// IDE and CLI only.
const WSL_ANTIGRAVITY_CREDENTIAL_SCRIPT: &str = r#"
credential_path="$HOME/.gemini/antigravity-cli/antigravity-oauth-token"
if [ ! -e "$credential_path" ]; then exit 44; fi
if [ ! -r "$credential_path" ]; then exit 45; fi
cat -- "$credential_path"
"#;

/// Run one of the credential scripts above in a distro and return stdout.
///
/// Callers treat every failure the same way (missing, unreadable, broken
/// distro, timeout): move on to the next source.
fn read_wsl_script_output(distro: &str, script: &'static str) -> Option<Vec<u8>> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            // `-e` and not `--`: with `--`, wsl.exe hands the remaining
            // arguments to the distribution's login shell as one command line,
            // which re-parses them and drops the quoting around the script. The
            // script then runs statement by statement in that outer shell, so
            // any variable it assigns is gone by the next statement and every
            // path check silently fails. `-e` executes the binary directly, so
            // the script reaches `sh -lc` as a single argument.
            .arg("-e")
            .arg("sh")
            .arg("-lc")
            .arg(script)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )
    .ok()?;
    output.status.success().then_some(output.stdout)
}

fn codex_tokens_from_auth_json(content: &str) -> Option<CodexTokenData> {
    serde_json::from_str::<CodexAuthFile>(content)
        .ok()?
        .tokens
        .filter(|tokens| !tokens.access_token.is_empty())
}

fn antigravity_token_from_auth_json(content: &str) -> Option<AntigravityTokenData> {
    serde_json::from_str::<AntigravityAuthFile>(content)
        .ok()
        .map(|auth| auth.token)
        .filter(|token| !token.access_token.is_empty())
}

fn read_codex_credentials_from_wsl(distros: &[String]) -> Option<CodexTokenData> {
    for distro in distros {
        let Some(bytes) = read_wsl_script_output(distro, WSL_CODEX_CREDENTIAL_SCRIPT) else {
            continue;
        };
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        if let Some(tokens) = codex_tokens_from_auth_json(&content) {
            diagnose::log(format!("loaded Codex credentials from WSL distro {distro}"));
            return Some(tokens);
        }
    }
    None
}

fn read_antigravity_credentials_from_wsl(distros: &[String]) -> Option<AntigravityTokenData> {
    for distro in distros {
        let Some(bytes) = read_wsl_script_output(distro, WSL_ANTIGRAVITY_CREDENTIAL_SCRIPT) else {
            continue;
        };
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        if let Some(token) = antigravity_token_from_auth_json(&content) {
            diagnose::log(format!(
                "loaded Antigravity credentials from WSL distro {distro}"
            ));
            return Some(token);
        }
    }
    None
}

fn resolved_wsl_credential_path(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            // `-e` and not `--`: with `--`, wsl.exe hands the remaining
            // arguments to the distribution's login shell as one command line,
            // which re-parses them and drops the quoting around the script. The
            // script then runs statement by statement in that outer shell, so
            // any variable it assigns is gone by the next statement and every
            // path check silently fails. `-e` executes the binary directly, so
            // the script reaches `sh -lc` as a single argument.
            .arg("-e")
            .arg("sh")
            .arg("-lc")
            .arg(WSL_CREDENTIAL_PATH_SCRIPT)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_string())
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let key = format!("wsl:{distro}");
    Some(match read_wsl_credential_bytes(distro) {
        Ok(content) => content_watch_signature(&key, &content),
        Err(problem) => format!("{key}|{}", problem.code()),
    })
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    let result = fetch_claude_usage_with_auth_retry(
        || try_usage_endpoint(token),
        || std::thread::sleep(Duration::from_millis(AUTH_CONFIRM_RETRY_DELAY_MS)),
    )?;
    match result {
        Some(data) => {
            if data.windows.iter().any(|window| window.resets_at.is_none()) {
                diagnose::log(
                    "usage endpoint omitted one or more reset timers; keeping usage data and refusing Messages API fallback because it sends a model request.",
                );
            }
            Ok(data)
        }
        None => {
            diagnose::log(
                "usage endpoint unavailable; refusing Messages API fallback because it sends a model request.",
            );
            Err(PollError::RequestFailed)
        }
    }
}

fn fetch_claude_usage_with_auth_retry(
    mut fetch_once: impl FnMut() -> Result<Option<UsageData>, PollError>,
    mut retry_wait: impl FnMut(),
) -> Result<Option<UsageData>, PollError> {
    let started = Instant::now();
    match fetch_once() {
        Err(PollError::AuthRequired) => {
            diagnose::log(format!(
                "Claude usage endpoint returned an unconfirmed auth error after {}ms; retrying once in {}ms",
                started.elapsed().as_millis(),
                AUTH_CONFIRM_RETRY_DELAY_MS
            ));
            retry_wait();
            let result = fetch_once();
            match &result {
                Ok(_) => diagnose::log(format!(
                    "Claude usage endpoint auth error did not repeat; total_elapsed_ms={}",
                    started.elapsed().as_millis()
                )),
                Err(PollError::AuthRequired) => diagnose::log(format!(
                    "Claude usage endpoint auth failure confirmed; total_elapsed_ms={}",
                    started.elapsed().as_millis()
                )),
                Err(error) => diagnose::log(format!(
                    "Claude usage endpoint auth confirmation returned a different result; total_elapsed_ms={} error={error:?}",
                    started.elapsed().as_millis()
                )),
            }
            result
        }
        result => result,
    }
}

fn try_usage_endpoint(token: &str) -> Result<Option<UsageData>, PollError> {
    let agent = build_agent_for_url(USAGE_URL, Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS))?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", CLAUDE_USER_AGENT)
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            match http_status_poll_error("Claude usage endpoint", code, &response) {
                auth @ (PollError::AuthRequired | PollError::AuthForbidden) => return Err(auth),
                rate_limited @ PollError::RateLimited(_) => return Err(rate_limited),
                _ => {
                    diagnose::log(
                        "refusing Messages API fallback because it sends a model request",
                    );
                    return Ok(None);
                }
            }
        }
        Err(error) => {
            diagnose::log_error("Claude usage endpoint request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: UsageResponse =
        match crate::http_client::response_json_limited(resp, "Claude usage endpoint response") {
            Ok(response) => response,
            Err(error) => {
                diagnose::log_error("unable to parse Claude usage endpoint response", error);
                return Ok(None);
            }
        };
    let mut windows = Vec::new();

    if let Some(bucket) = &response.five_hour {
        windows.push(UsageWindow::new(
            bucket.utilization,
            parse_iso8601(bucket.resets_at.as_deref()),
            Some(FIVE_HOURS_SECONDS),
        ));
    }

    if let Some(bucket) = &response.seven_day {
        windows.push(UsageWindow::new(
            bucket.utilization,
            parse_iso8601(bucket.resets_at.as_deref()),
            Some(ONE_WEEK_SECONDS),
        ));
    }

    Ok(Some(UsageData::from_windows(windows)))
}

fn classify_http_status(code: u16, retry_after_ms: Option<u32>) -> PollError {
    match code {
        401 => PollError::AuthRequired,
        403 => PollError::AuthForbidden,
        429 => PollError::RateLimited(retry_after_ms),
        _ => PollError::RequestFailed,
    }
}

fn http_status_poll_error(endpoint: &str, code: u16, response: &ureq::Response) -> PollError {
    let retry_after_ms = (code == 429).then(|| retry_after_ms(response)).flatten();
    let error = classify_http_status(code, retry_after_ms);
    match error {
        PollError::AuthRequired | PollError::AuthForbidden => {
            diagnose::log(format!("{endpoint} returned auth error status {code}"))
        }
        PollError::RateLimited(retry_after_ms) => diagnose::log(format!(
            "{endpoint} returned rate limit status 429; retry_after_ms={}",
            retry_after_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )),
        _ => diagnose::log(format!("{endpoint} returned HTTP status {code}")),
    }
    error
}

fn retry_after_ms(response: &ureq::Response) -> Option<u32> {
    retry_after_value_ms(response.header("Retry-After")?, SystemTime::now())
}

fn retry_after_value_ms(value: &str, now: SystemTime) -> Option<u32> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000).min(u32::MAX as u64) as u32);
    }

    let retry_unix = parse_retry_after_http_date(value)?;
    let now_unix = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(
        retry_unix
            .saturating_sub(now_unix)
            .saturating_mul(1000)
            .min(u32::MAX as u64) as u32,
    )
}

fn parse_retry_after_http_date(value: &str) -> Option<u64> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[5] != "GMT" || !parts[0].ends_with(',') {
        return None;
    }
    let day = parts[1].parse::<u64>().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts[3].parse::<u64>().ok()?;
    parse_datetime_to_unix(&format!("{year:04}-{month:02}-{day:02}T{}", parts[4])).ok()
}

enum CodexAttemptError {
    Retryable(PollError),
    Final(PollError),
}

impl CodexAttemptError {
    fn poll_error(self) -> PollError {
        match self {
            Self::Retryable(error) | Self::Final(error) => error,
        }
    }
}

fn codex_http_status_is_retryable(code: u16) -> bool {
    matches!(code, 401 | 403 | 502..=504)
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent_for_url(
        CODEX_USAGE_URL,
        Duration::from_secs(CODEX_REQUEST_TIMEOUT_SECS),
    )?;
    fetch_codex_usage_with_retry(
        || fetch_codex_usage_once(&agent, token, account_id),
        || std::thread::sleep(Duration::from_millis(CODEX_RETRY_DELAY_MS)),
    )
}

fn fetch_codex_usage_with_retry(
    mut fetch_once: impl FnMut() -> Result<UsageData, CodexAttemptError>,
    mut retry_wait: impl FnMut(),
) -> Result<UsageData, PollError> {
    let started = Instant::now();
    match fetch_once() {
        Ok(data) => Ok(data),
        Err(CodexAttemptError::Final(error)) => Err(error),
        Err(CodexAttemptError::Retryable(_)) => {
            diagnose::log(format!(
                "Codex usage endpoint retryable failure after {}ms; retrying once in {}ms",
                started.elapsed().as_millis(),
                CODEX_RETRY_DELAY_MS
            ));
            retry_wait();
            match fetch_once() {
                Ok(data) => {
                    diagnose::log(format!(
                        "Codex usage endpoint recovered on retry; total_elapsed_ms={}",
                        started.elapsed().as_millis()
                    ));
                    Ok(data)
                }
                Err(error) => {
                    let error = error.poll_error();
                    diagnose::log(format!(
                        "Codex usage endpoint retry failed; total_elapsed_ms={} error={error:?}",
                        started.elapsed().as_millis()
                    ));
                    Err(error)
                }
            }
        }
    }
}

fn fetch_codex_usage_once(
    agent: &ureq::Agent,
    token: &str,
    account_id: Option<&str>,
) -> Result<UsageData, CodexAttemptError> {
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            let error = http_status_poll_error("Codex usage endpoint", code, &response);
            return Err(if codex_http_status_is_retryable(code) {
                CodexAttemptError::Retryable(error)
            } else {
                CodexAttemptError::Final(error)
            });
        }
        Err(error) => {
            diagnose::log_error("Codex usage endpoint request failed", error);
            return Err(CodexAttemptError::Retryable(PollError::NetworkUnavailable));
        }
    };

    let response: CodexUsageResponse =
        match crate::http_client::response_json_limited(resp, "Codex usage response") {
            Ok(response) => response,
            Err(error) => {
                diagnose::log_error("unable to parse Codex usage response", error);
                return Err(CodexAttemptError::Final(PollError::RequestFailed));
            }
        };

    codex_usage_from_response(response).ok_or_else(|| {
        diagnose::log("Codex usage response did not contain a usable quota window");
        CodexAttemptError::Final(PollError::RequestFailed)
    })
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let details = *response.rate_limit.flatten()?;
    let mut windows = Vec::new();

    if let Some(window) = details.primary_window.flatten() {
        windows.extend(codex_usage_window(&window, "Primary"));
    }

    if let Some(window) = details.secondary_window.flatten() {
        windows.extend(codex_usage_window(&window, "Secondary"));
    }

    Some(UsageData::from_windows(windows))
}

fn codex_usage_window(window: &CodexRateLimitWindow, fallback_label: &str) -> Option<UsageWindow> {
    let duration_seconds = window.limit_window_seconds.filter(|seconds| *seconds > 0);
    Some(
        UsageWindow::new(
            window.used_percent?,
            unix_to_system_time(window.reset_at),
            duration_seconds,
        )
        .with_source_label(
            duration_seconds
                .is_none()
                .then(|| fallback_label.to_string()),
        ),
    )
}

fn antigravity_credential_watch_signature() -> String {
    let Some(content) = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET) else {
        return format!("{ANTIGRAVITY_CREDENTIAL_TARGET}|missing");
    };
    content_watch_signature(ANTIGRAVITY_CREDENTIAL_TARGET, content.as_bytes())
}

fn fetch_antigravity_usage(token: &str) -> Result<UsageData, PollError> {
    let mut errors = Vec::new();

    for base_url in ANTIGRAVITY_ENDPOINTS {
        match fetch_antigravity_usage_from_endpoint(base_url, token) {
            Ok(data) => return Ok(data),
            Err(error) => errors.push(error),
        }
    }

    Err(aggregate_poll_errors(&errors))
}

fn fetch_antigravity_usage_from_endpoint(
    base_url: &str,
    token: &str,
) -> Result<UsageData, PollError> {
    let agent = build_agent_for_url(
        base_url,
        Duration::from_secs(ANTIGRAVITY_REQUEST_TIMEOUT_SECS),
    )?;
    let project = fetch_antigravity_project(&agent, base_url, token)?;
    if let Some(project) = project.as_deref() {
        let per_model = match fetch_antigravity_user_quota(&agent, token, project) {
            Ok(data) if !data.is_empty() => Some(data),
            Ok(_) => None,
            Err(error) => {
                diagnose::log(format!(
                    "Antigravity retrieveUserQuota unavailable; continuing with weekly summary: {error:?}"
                ));
                None
            }
        };
        let summary = match fetch_antigravity_quota_summary(&agent, base_url, token, project) {
            Ok(data) if !data.is_empty() => Some(data),
            Ok(_) => None,
            Err(error) => {
                diagnose::log(format!(
                    "Antigravity retrieveUserQuotaSummary unavailable; continuing with per-model quota: {error:?}"
                ));
                None
            }
        };

        if let Some(data) = merge_antigravity_usage_sources(per_model, summary) {
            return Ok(data);
        }
    }

    let window = fetch_antigravity_model_quota(&agent, base_url, token, project.as_deref())?;
    Ok(UsageData::from_windows(vec![window]))
}

fn fetch_antigravity_user_quota(
    agent: &ureq::Agent,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(ANTIGRAVITY_USER_QUOTA_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity retrieveUserQuota",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuota request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: AntigravityUserQuotaResponse = match crate::http_client::response_json_limited(
        resp,
        "Antigravity retrieveUserQuota response",
    ) {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuota response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    Ok(antigravity_usage_from_user_quota(response).unwrap_or_default())
}

fn fetch_antigravity_project(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
) -> Result<Option<String>, PollError> {
    let body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:loadCodeAssist"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity loadCodeAssist",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity loadCodeAssist request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: AntigravityLoadResponse = match crate::http_client::response_json_limited(
        resp,
        "Antigravity loadCodeAssist response",
    ) {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Antigravity loadCodeAssist response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(response.project.filter(|project| !project.is_empty()))
}

fn fetch_antigravity_model_quota(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
    project: Option<&str>,
) -> Result<UsageWindow, PollError> {
    let body = match project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };

    let resp = match agent
        .post(&format!("{base_url}/v1internal:fetchAvailableModels"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity fetchAvailableModels",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity fetchAvailableModels request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: AntigravityModelsResponse = match crate::http_client::response_json_limited(
        resp,
        "Antigravity fetchAvailableModels response",
    ) {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity fetchAvailableModels response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    best_antigravity_section(response.models.into_iter().filter_map(|(model, info)| {
        let quota = info.quota_info?;
        if !is_antigravity_display_model(&model) {
            return None;
        }
        antigravity_section_from_quota(quota)
    }))
    .ok_or(PollError::RequestFailed)
}

fn fetch_antigravity_quota_summary(
    agent: &ureq::Agent,
    base_url: &str,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:retrieveUserQuotaSummary"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, response)) => {
            return Err(http_status_poll_error(
                "Antigravity retrieveUserQuotaSummary",
                code,
                &response,
            ));
        }
        Err(error) => {
            diagnose::log_error("Antigravity retrieveUserQuotaSummary request failed", error);
            return Err(PollError::NetworkUnavailable);
        }
    };

    let response: AntigravityQuotaSummaryResponse = match crate::http_client::response_json_limited(
        resp,
        "Antigravity retrieveUserQuotaSummary response",
    ) {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuotaSummary response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    antigravity_usage_from_summary(response).ok_or(PollError::RequestFailed)
}

fn antigravity_section_from_quota(quota: AntigravityQuotaInfo) -> Option<UsageWindow> {
    let remaining = quota.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageWindow::new(
        (1.0 - remaining) * 100.0,
        parse_iso8601(quota.reset_time.as_deref()),
        None,
    ))
}

fn antigravity_usage_from_user_quota(response: AntigravityUserQuotaResponse) -> Option<UsageData> {
    antigravity_usage_from_user_quota_at(response, SystemTime::now())
}

fn antigravity_usage_from_user_quota_at(
    response: AntigravityUserQuotaResponse,
    now: SystemTime,
) -> Option<UsageData> {
    let window = best_antigravity_section(response.buckets.into_iter().filter_map(|bucket| {
        if bucket.disabled.unwrap_or(false) {
            return None;
        }
        let model = bucket.model_id?.trim().to_ascii_lowercase();
        let model = model.strip_prefix("models/").unwrap_or(&model);
        if !is_antigravity_display_model(model) {
            return None;
        }
        let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
        let resets_at = parse_iso8601(bucket.reset_time.as_deref())?;
        if !is_plausible_antigravity_five_hour_reset(resets_at, now) {
            return None;
        }
        Some(UsageWindow::new(
            (1.0 - remaining) * 100.0,
            Some(resets_at),
            Some(FIVE_HOURS_SECONDS),
        ))
    }))?;

    Some(UsageData::from_windows(vec![window]))
}

fn is_plausible_antigravity_five_hour_reset(resets_at: SystemTime, now: SystemTime) -> bool {
    let Ok(remaining) = resets_at.duration_since(now) else {
        return false;
    };
    !remaining.is_zero()
        && remaining
            <= Duration::from_secs(FIVE_HOURS_SECONDS + ANTIGRAVITY_FIVE_HOUR_RESET_GRACE_SECS)
}

fn merge_antigravity_usage_sources(
    per_model: Option<UsageData>,
    summary: Option<UsageData>,
) -> Option<UsageData> {
    let mut windows = summary.map(|usage| usage.windows).unwrap_or_default();
    let summary_has_five_hour = windows
        .iter()
        .any(|window| window.duration_seconds == Some(FIVE_HOURS_SECONDS));

    if !summary_has_five_hour {
        for candidate in per_model
            .into_iter()
            .flat_map(|usage| usage.windows.into_iter())
        {
            let duplicates_summary = windows.iter().any(|window| {
                (window.percentage - candidate.percentage).abs() < 0.000_001
                    && window.resets_at == candidate.resets_at
            });
            if !duplicates_summary {
                upsert_usage_window(&mut windows, candidate);
            }
        }
    }

    (!windows.is_empty()).then(|| UsageData::from_windows(windows))
}

fn antigravity_section_from_summary_bucket(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<UsageWindow> {
    let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
    let duration_seconds = antigravity_summary_bucket_duration_seconds(bucket);
    let source_label = duration_seconds.is_none().then(|| {
        bucket
            .window
            .clone()
            .or_else(|| bucket.display_name.clone())
            .unwrap_or_default()
    });
    Some(
        UsageWindow::new(
            (1.0 - remaining) * 100.0,
            parse_iso8601(bucket.reset_time.as_deref()),
            duration_seconds,
        )
        .with_source_label(source_label),
    )
}

fn antigravity_summary_bucket_duration_seconds(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<u64> {
    match bucket
        .bucket_id
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("gemini-5h" | "3p-5h") => return Some(FIVE_HOURS_SECONDS),
        Some("gemini-weekly" | "3p-weekly") => return Some(ONE_WEEK_SECONDS),
        _ => {}
    }

    if let Some(seconds) = usage_window_duration_seconds(bucket.window.as_deref()) {
        return Some(seconds);
    }

    let text = format!(
        "{} {}",
        bucket.bucket_id.as_deref().unwrap_or_default(),
        bucket.display_name.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let words = text
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();

    if words
        .iter()
        .any(|word| matches!(*word, "weekly" | "week" | "7d" | "1w"))
    {
        return Some(ONE_WEEK_SECONDS);
    }
    if words.iter().any(|word| *word == "5h")
        || words
            .windows(2)
            .any(|pair| pair == ["five", "hour"] || pair == ["5", "hour"])
    {
        return Some(FIVE_HOURS_SECONDS);
    }

    None
}

fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let mut fallback = None;

    let groups = response
        .groups
        .or_else(|| response.quota_summary.and_then(|summary| summary.groups))
        .unwrap_or_default();
    for group in groups {
        let is_gemini = is_antigravity_gemini_summary_group(&group);
        let usage = antigravity_usage_from_summary_group(group);

        if is_gemini && usage.is_some() {
            return usage;
        }

        if fallback.is_none() {
            fallback = usage;
        }
    }

    fallback
}

fn antigravity_usage_from_summary_group(group: AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut windows = Vec::new();

    for bucket in group.buckets.unwrap_or_default() {
        let Some(window) = antigravity_section_from_summary_bucket(&bucket) else {
            continue;
        };
        upsert_usage_window(&mut windows, window);
    }

    (!windows.is_empty()).then(|| UsageData::from_windows(windows))
}

fn upsert_usage_window(windows: &mut Vec<UsageWindow>, candidate: UsageWindow) {
    let same_window = |window: &&mut UsageWindow| {
        window.duration_seconds == candidate.duration_seconds
            && window.source_label.as_deref() == candidate.source_label.as_deref()
    };
    if let Some(existing) = windows.iter_mut().find(same_window) {
        if candidate.percentage > existing.percentage {
            *existing = candidate;
        }
    } else {
        windows.push(candidate);
    }
}

fn usage_window_duration_seconds(label: Option<&str>) -> Option<u64> {
    let label = label?.trim().to_ascii_lowercase();
    match label.as_str() {
        "5h" => Some(FIVE_HOURS_SECONDS),
        "daily" | "1d" | "24h" => Some(ONE_DAY_SECONDS),
        "weekly" | "7d" | "1w" => Some(ONE_WEEK_SECONDS),
        "monthly" | "30d" => Some(30 * ONE_DAY_SECONDS),
        "annual" | "yearly" | "365d" => Some(365 * ONE_DAY_SECONDS),
        _ => {
            let (number, multiplier) = if let Some(value) = label.strip_suffix('h') {
                (value, 60 * 60)
            } else if let Some(value) = label.strip_suffix('d') {
                (value, ONE_DAY_SECONDS)
            } else if let Some(value) = label.strip_suffix('w') {
                (value, ONE_WEEK_SECONDS)
            } else {
                return None;
            };
            number.parse::<u64>().ok()?.checked_mul(multiplier)
        }
    }
}

fn is_antigravity_gemini_summary_group(group: &AntigravityQuotaSummaryGroup) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
        || group
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains("gemini"))
        || group.buckets.as_ref().is_some_and(|buckets| {
            buckets.iter().any(|bucket| {
                bucket
                    .bucket_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini-"))
                    || bucket
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
            })
        })
}

fn best_antigravity_section<I>(sections: I) -> Option<UsageWindow>
where
    I: IntoIterator<Item = UsageWindow>,
{
    sections.into_iter().max_by(|a, b| {
        a.percentage
            .partial_cmp(&b.percentage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.resets_at.cmp(&b.resets_at))
    })
}

fn is_antigravity_display_model(model: &str) -> bool {
    model.starts_with("gemini")
        || model.starts_with("claude")
        || model.starts_with("gpt")
        || model.starts_with("image")
        || model.starts_with("imagen")
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

#[derive(Clone)]
struct Credentials {
    access_token: String,
    access_expires_at: Option<i64>,
    refresh_token_present: bool,
    refresh_token_expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
}

impl CredentialSource {
    fn diagnostic_label(&self) -> String {
        match self {
            Self::Windows(path) => format!("windows:{}", path.display()),
            Self::Wsl { distro } => format!("wsl:{distro}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaudeCredentialProblem {
    Missing,
    ProbeUnavailable,
    Unreadable,
    InvalidJson,
    UnsupportedShape,
    RefreshMissing,
    RefreshExpired,
}

impl ClaudeCredentialProblem {
    fn code(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::ProbeUnavailable => "probe_unavailable",
            Self::Unreadable => "unreadable",
            Self::InvalidJson => "invalid_json",
            Self::UnsupportedShape => "unsupported_shape",
            Self::RefreshMissing => "refresh_missing",
            Self::RefreshExpired => "refresh_expired",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Missing => 0,
            Self::ProbeUnavailable => 1,
            Self::Unreadable | Self::InvalidJson | Self::UnsupportedShape => 2,
            Self::RefreshMissing | Self::RefreshExpired => 3,
        }
    }
}

fn claude_credential_problem_poll_error(problem: ClaudeCredentialProblem) -> PollError {
    match problem {
        ClaudeCredentialProblem::Missing => PollError::NoCredentials,
        ClaudeCredentialProblem::ProbeUnavailable => PollError::RequestFailed,
        ClaudeCredentialProblem::Unreadable
        | ClaudeCredentialProblem::InvalidJson
        | ClaudeCredentialProblem::UnsupportedShape
        | ClaudeCredentialProblem::RefreshMissing
        | ClaudeCredentialProblem::RefreshExpired => PollError::AuthRequired,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaudeCredentialLifecycle {
    Usable,
    Refreshable,
    LoginRequired(ClaudeCredentialProblem),
}

enum ClaudeCredentialSelection {
    Usable(Credentials),
    Refreshable(Credentials),
    LoginRequired {
        source: Option<CredentialSource>,
        problem: ClaudeCredentialProblem,
    },
}

fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn classify_claude_credentials(
    credentials: &Credentials,
    now_ms: i64,
) -> ClaudeCredentialLifecycle {
    if !timestamp_is_expired(credentials.access_expires_at, now_ms) {
        return ClaudeCredentialLifecycle::Usable;
    }
    if !credentials.refresh_token_present {
        return ClaudeCredentialLifecycle::LoginRequired(ClaudeCredentialProblem::RefreshMissing);
    }
    if timestamp_is_expired(credentials.refresh_token_expires_at, now_ms) {
        return ClaudeCredentialLifecycle::LoginRequired(ClaudeCredentialProblem::RefreshExpired);
    }
    ClaudeCredentialLifecycle::Refreshable
}

fn select_claude_credentials() -> ClaudeCredentialSelection {
    select_claude_credentials_at(now_unix_millis())
}

fn select_claude_credentials_at(now_ms: i64) -> ClaudeCredentialSelection {
    let probes = all_known_credential_sources()
        .into_iter()
        .map(|source| {
            let result = read_credentials_from_source(&source);
            (source, result)
        })
        .collect::<Vec<_>>();
    choose_claude_credentials(probes, now_ms)
}

fn choose_claude_credentials(
    probes: impl IntoIterator<
        Item = (
            CredentialSource,
            Result<Credentials, ClaudeCredentialProblem>,
        ),
    >,
    now_ms: i64,
) -> ClaudeCredentialSelection {
    let mut refreshable = None;
    let mut best_problem: Option<(Option<CredentialSource>, ClaudeCredentialProblem)> = None;

    for (source, result) in probes {
        match result {
            Ok(credentials) => match classify_claude_credentials(&credentials, now_ms) {
                ClaudeCredentialLifecycle::Usable => {
                    return ClaudeCredentialSelection::Usable(credentials)
                }
                ClaudeCredentialLifecycle::Refreshable => {
                    refreshable.get_or_insert(credentials);
                }
                ClaudeCredentialLifecycle::LoginRequired(problem) => {
                    diagnose::log(format!(
                        "Claude credential source rejected source={} reason={}",
                        source.diagnostic_label(),
                        problem.code()
                    ));
                    if best_problem
                        .as_ref()
                        .is_none_or(|(_, current)| problem.priority() > current.priority())
                    {
                        best_problem = Some((Some(source), problem));
                    }
                }
            },
            Err(problem) => {
                diagnose::log(format!(
                    "Claude credential source unavailable source={} reason={}",
                    source.diagnostic_label(),
                    problem.code()
                ));
                if best_problem
                    .as_ref()
                    .is_none_or(|(_, current)| problem.priority() > current.priority())
                {
                    best_problem = Some((Some(source), problem));
                }
            }
        }
    }

    if let Some(credentials) = refreshable {
        return ClaudeCredentialSelection::Refreshable(credentials);
    }
    let (source, problem) = best_problem.unwrap_or((None, ClaudeCredentialProblem::Missing));
    ClaudeCredentialSelection::LoginRequired { source, problem }
}

fn read_credentials_from_source(
    source: &CredentialSource,
) -> Result<Credentials, ClaudeCredentialProblem> {
    match source {
        CredentialSource::Windows(path) => read_windows_credentials(path),
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn read_windows_credentials(cred_path: &Path) -> Result<Credentials, ClaudeCredentialProblem> {
    let content = std::fs::read_to_string(cred_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ClaudeCredentialProblem::Missing
        } else {
            ClaudeCredentialProblem::Unreadable
        }
    })?;
    parse_credentials(&content, CredentialSource::Windows(cred_path.to_path_buf()))
}

fn diagnostic_expiry(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn diagnostic_first_line(bytes: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .chars()
        .filter(|ch| !ch.is_control())
        .take(240)
        .collect::<String>();
    (!line.is_empty()).then_some(line)
}

fn run_diagnostic_command(program: &str, args: &[&str]) -> Option<std::process::Output> {
    run_with_timeout(
        Command::new(program)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(10),
    )
    .ok()
}

/// User-triggered, read-only Claude login diagnostics. The report deliberately
/// includes no credential values, account identifiers, or raw CLI output.
/// `claude auth status` is non-model and is never called from background polls.
pub fn claude_auth_diagnostics_report() -> String {
    let now_ms = now_unix_millis();
    let mut lines = vec![
        "Claude authentication diagnostics".to_string(),
        "ReadOnly: true".to_string(),
    ];

    let sources = all_known_credential_sources();
    lines.push(format!("CredentialSourceCount: {}", sources.len()));
    for (index, source) in sources.into_iter().enumerate() {
        let path = match &source {
            CredentialSource::Windows(path) => path.display().to_string(),
            CredentialSource::Wsl { distro } => {
                resolved_wsl_credential_path(distro).unwrap_or_else(|| "unavailable".to_string())
            }
        };
        lines.push(format!("Source[{index}]: {}", source.diagnostic_label()));
        lines.push(format!("Source[{index}].Path: {path}"));
        match read_credentials_from_source(&source) {
            Ok(credentials) => {
                let remotely_rejected =
                    claude_auth_rejection_matches(token_hash(&credentials.access_token));
                let (state, reason) = if remotely_rejected {
                    ("login_required", "auth_rejected")
                } else {
                    match classify_claude_credentials(&credentials, now_ms) {
                        ClaudeCredentialLifecycle::Usable => ("usable", "none"),
                        ClaudeCredentialLifecycle::Refreshable => ("refreshable", "none"),
                        ClaudeCredentialLifecycle::LoginRequired(problem) => {
                            ("login_required", problem.code())
                        }
                    }
                };
                lines.push(format!("Source[{index}].File: present"));
                lines.push(format!("Source[{index}].State: {state}"));
                lines.push(format!("Source[{index}].Reason: {reason}"));
                lines.push(format!(
                    "Source[{index}].AccessExpiresAtMs: {}",
                    diagnostic_expiry(credentials.access_expires_at)
                ));
                lines.push(format!(
                    "Source[{index}].RefreshTokenPresent: {}",
                    credentials.refresh_token_present
                ));
                lines.push(format!(
                    "Source[{index}].RefreshExpiresAtMs: {}",
                    diagnostic_expiry(credentials.refresh_token_expires_at)
                ));
            }
            Err(problem) => {
                let (file, state) = match problem {
                    ClaudeCredentialProblem::Missing => ("missing", "login_required"),
                    ClaudeCredentialProblem::ProbeUnavailable => ("unknown", "probe_unavailable"),
                    _ => ("unusable", "login_required"),
                };
                lines.push(format!("Source[{index}].File: {file}"));
                lines.push(format!("Source[{index}].State: {state}"));
                lines.push(format!("Source[{index}].Reason: {}", problem.code()));
            }
        }
    }

    let cli_path = run_diagnostic_command("where.exe", &["claude"])
        .filter(|output| output.status.success())
        .and_then(|output| diagnostic_first_line(&output.stdout));
    lines.push(format!(
        "ClaudeCliPath: {}",
        cli_path.as_deref().unwrap_or("not_found")
    ));

    let cli_version = run_diagnostic_command("claude", &["--version"])
        .filter(|output| output.status.success())
        .and_then(|output| diagnostic_first_line(&output.stdout));
    lines.push(format!(
        "ClaudeCliVersion: {}",
        cli_version.as_deref().unwrap_or("unavailable")
    ));

    match run_diagnostic_command("claude", &["auth", "status"]) {
        Some(output) if output.status.success() => {
            let status = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
            let logged_in = status
                .as_ref()
                .and_then(|value| value.get("loggedIn"))
                .and_then(serde_json::Value::as_bool)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let auth_method = status
                .as_ref()
                .and_then(|value| value.get("authMethod"))
                .and_then(serde_json::Value::as_str)
                .map(|value| {
                    value
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(80)
                        .collect::<String>()
                })
                .unwrap_or_else(|| "unknown".to_string());
            lines.push(format!("ClaudeAuthLoggedIn: {logged_in}"));
            lines.push(format!("ClaudeAuthMethod: {auth_method}"));
        }
        Some(output) => lines.push(format!(
            "ClaudeAuthStatus: failed_exit_{}",
            output.status.code().unwrap_or(-1)
        )),
        None => lines.push("ClaudeAuthStatus: unavailable".to_string()),
    }
    lines.push("RecoveryCommand: claude auth login".to_string());
    lines.join("\n")
}

fn codex_home() -> Option<PathBuf> {
    codex_home_from(
        std::env::var_os("CODEX_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn codex_home_from(configured: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    configured
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".codex")))
}

fn codex_direct_keyring_target_from_path(path: &str) -> Option<String> {
    let digest = crate::updater::sha256_hex(path.as_bytes()).ok()?;
    let short = digest.get(..16).unwrap_or(&digest);
    // keyring-rs' Windows backend uses "{user}.{service}" as the generic
    // credential target by default. Codex passes the computed cli key as the
    // user and "Codex Auth" as the service.
    Some(format!("cli|{short}.{CODEX_KEYRING_SERVICE}"))
}

fn codex_direct_keyring_target(codex_home: &Path) -> Option<String> {
    let canonical = codex_home
        .canonicalize()
        .unwrap_or_else(|_| codex_home.to_path_buf());
    codex_direct_keyring_target_from_path(&canonical.to_string_lossy())
}

/// Classify a file that is supposed to hold a credential.
///
/// `NotFound` is the only error that counts as missing: any other read failure
/// is a file that exists and cannot be used, which the user has to act on.
fn local_credential_from_file<T>(
    path: &Path,
    parse: impl FnOnce(&str) -> Option<T>,
) -> LocalCredential<T> {
    match std::fs::read_to_string(path) {
        Ok(content) => match parse(&content) {
            Some(value) => LocalCredential::Usable(value),
            None => {
                diagnose::log(format!(
                    "credential file {} could not be parsed",
                    path.display()
                ));
                LocalCredential::Unusable
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalCredential::Missing,
        Err(error) => {
            diagnose::log(format!(
                "credential file {} exists but could not be read: {error}",
                path.display()
            ));
            LocalCredential::Unusable
        }
    }
}

fn read_windows_codex_credentials() -> LocalCredential<CodexTokenData> {
    let Some(codex_home) = codex_home() else {
        return LocalCredential::Missing;
    };
    let auth_path = codex_home.join("auth.json");
    let tokens =
        local_credential_from_file(&auth_path, codex_tokens_from_auth_json).or_else(|| {
            let Some(target) = codex_direct_keyring_target(&codex_home) else {
                return LocalCredential::Missing;
            };
            match read_windows_generic_credential_classified(&target) {
                LocalCredential::Usable(content) => match codex_tokens_from_auth_json(&content) {
                    Some(tokens) => {
                        diagnose::log("loaded Codex credentials from Windows Credential Manager");
                        LocalCredential::Usable(tokens)
                    }
                    None => {
                        diagnose::log(
                            "Codex credentials in the Windows keyring could not be parsed",
                        );
                        LocalCredential::Unusable
                    }
                },
                LocalCredential::Unusable => LocalCredential::Unusable,
                LocalCredential::Missing => LocalCredential::Missing,
            }
        });

    if matches!(tokens, LocalCredential::Missing) {
        diagnose::log(format!(
            "no readable Codex Desktop/CLI credentials found at {} or in the direct Windows keyring",
            auth_path.display()
        ));
    }
    tokens
}

/// Windows first, then any WSL distro that is already running.
///
/// Restricted to running distros even on the polling path, unlike Claude's
/// older all-distro resolution. A provider with no credential anywhere parks
/// on "not signed in" and gets re-checked on a timer, so reading every distro
/// here would start their virtual machines on a schedule for users who simply
/// do not have Codex. A distro the user actually works in is already running.
fn read_codex_credentials() -> LocalCredential<CodexTokenData> {
    read_windows_codex_credentials()
        .or_else_wsl(|| read_codex_credentials_from_wsl(&list_running_wsl_distros()))
}

fn read_windows_antigravity_credentials() -> LocalCredential<AntigravityTokenData> {
    match read_windows_generic_credential_classified(ANTIGRAVITY_CREDENTIAL_TARGET) {
        LocalCredential::Usable(content) => match antigravity_token_from_auth_json(&content) {
            Some(tokens) => LocalCredential::Usable(tokens),
            None => {
                diagnose::log("Antigravity credentials in the Windows keyring could not be parsed");
                LocalCredential::Unusable
            }
        },
        LocalCredential::Unusable => LocalCredential::Unusable,
        LocalCredential::Missing => {
            diagnose::log(format!(
                "no Windows credential entry {ANTIGRAVITY_CREDENTIAL_TARGET}"
            ));
            LocalCredential::Missing
        }
    }
}

/// See `read_codex_credentials` for why this stops at running distros.
fn read_antigravity_credentials() -> LocalCredential<AntigravityTokenData> {
    read_windows_antigravity_credentials()
        .or_else_wsl(|| read_antigravity_credentials_from_wsl(&list_running_wsl_distros()))
}

fn grok_home() -> Option<PathBuf> {
    grok_home_from(
        std::env::var_os("GROK_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn grok_home_from(configured: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    configured
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".grok")))
}

fn grok_auth_path() -> Option<PathBuf> {
    grok_home().map(|home| home.join("auth.json"))
}

fn read_windows_grok_credentials() -> LocalCredential<GrokTokenData> {
    let Some(auth_path) = grok_auth_path() else {
        return LocalCredential::Missing;
    };
    let tokens = local_credential_from_file(&auth_path, grok_token_from_auth_json);
    if matches!(tokens, LocalCredential::Missing) {
        diagnose::log(format!(
            "no readable Grok CLI credentials found at {}",
            auth_path.display()
        ));
    }
    tokens
}

fn read_grok_credentials_from_wsl(distros: &[String]) -> Option<GrokTokenData> {
    for distro in distros {
        let Some(bytes) = read_wsl_script_output(distro, WSL_GROK_CREDENTIAL_SCRIPT) else {
            continue;
        };
        let Ok(content) = String::from_utf8(bytes) else {
            continue;
        };
        if let Some(tokens) = grok_token_from_auth_json(&content) {
            diagnose::log(format!("loaded Grok credentials from WSL distro {distro}"));
            return Some(tokens);
        }
    }
    None
}

fn read_grok_credentials() -> LocalCredential<GrokTokenData> {
    read_windows_grok_credentials()
        .or_else_wsl(|| read_grok_credentials_from_wsl(&list_running_wsl_distros()))
}

fn grok_credential_watch_signature() -> String {
    let Some(auth_path) = grok_auth_path() else {
        return "win:grok-auth|missing".to_string();
    };
    windows_credential_watch_signature(&auth_path)
}

/// Pick the entry grok CLI itself would use.
///
/// `auth.json` is a registry keyed by `{issuer}::{client_id}`, so one file can
/// hold several sign-ins. xAI's own OAuth scope wins; anything else under an
/// `x.ai` issuer is the fallback. An entry from some other issuer - an
/// enterprise IdP - is skipped rather than used, because its token was not
/// issued by xAI and must not be sent to xAI. `serde_json` keeps object keys
/// sorted, so the fallback is stable between polls.
fn grok_token_from_auth_json(content: &str) -> Option<GrokTokenData> {
    let registry: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(content).ok()?;
    let usable_token = |entry: &serde_json::Value| -> Option<String> {
        let token = entry.get("key")?.as_str()?.trim();
        (!token.is_empty()).then(|| token.to_string())
    };
    let issued_by_xai = |key: &str, entry: &serde_json::Value| {
        let issuer = entry
            .get("oidc_issuer")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| key.split("::").next().unwrap_or_default());
        grok_issuer_is_xai(issuer)
    };
    let preferred = registry.iter().find(|(key, entry)| {
        key.starts_with(GROK_PREFERRED_ISSUER_PREFIX)
            && issued_by_xai(key, entry)
            && usable_token(entry).is_some()
    });
    preferred
        .or_else(|| {
            registry
                .iter()
                .find(|(key, entry)| issued_by_xai(key, entry) && usable_token(entry).is_some())
        })
        .and_then(|(_, entry)| usable_token(entry))
        .map(|access_token| GrokTokenData { access_token })
}

fn grok_issuer_is_xai(issuer: &str) -> bool {
    let Some(rest) = issuer.trim().strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host == "x.ai" || host.ends_with(GROK_ISSUER_HOST_SUFFIX)
}

fn read_windows_generic_credential(target: &str) -> Option<String> {
    let result = read_windows_generic_credential_quiet(target);
    if result.is_none() {
        diagnose::log(format!(
            "unable to read Windows generic credential target {target}"
        ));
    }
    result
}

/// Any failure at all, for callers that only need the bytes.
fn read_windows_generic_credential_quiet(target: &str) -> Option<String> {
    read_windows_generic_credential_classified(target).usable()
}

/// Read a Credential Manager entry, separating "no such entry" from an entry
/// that is there and cannot be used.
///
/// Only `ERROR_NOT_FOUND` is missing. A read that fails for any other reason,
/// an empty blob, and a blob that is not UTF-8 all mean something is stored
/// that cannot be turned into a credential - which the user has to act on, and
/// which used to be reported as if they had never signed in.
fn read_windows_generic_credential_classified(target: &str) -> LocalCredential<String> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();

    let ok = unsafe {
        CredReadW(
            target_wide.as_mut_ptr(),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
    };

    if ok == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_NOT_FOUND {
            return LocalCredential::Missing;
        }
        diagnose::log(format!(
            "Windows credential target {target} exists but could not be read (error {error})"
        ));
        return LocalCredential::Unusable;
    }
    if credential.is_null() {
        return LocalCredential::Missing;
    }

    unsafe {
        let cred = &*credential;
        if cred.credential_blob_size == 0 || cred.credential_blob.is_null() {
            CredFree(credential as *mut c_void);
            diagnose::log(format!(
                "Windows credential target {target} holds an empty blob"
            ));
            return LocalCredential::Unusable;
        }
        let bytes =
            std::slice::from_raw_parts(cred.credential_blob, cred.credential_blob_size as usize);
        let text = String::from_utf8(bytes.to_vec());
        CredFree(credential as *mut c_void);
        match text {
            Ok(text) => LocalCredential::Usable(text),
            Err(_) => {
                diagnose::log(format!(
                    "Windows credential target {target} is not valid UTF-8"
                ));
                LocalCredential::Unusable
            }
        }
    }
}

fn read_wsl_credentials(distro: &str) -> Result<Credentials, ClaudeCredentialProblem> {
    let bytes = read_wsl_credential_bytes(distro)?;
    let content = String::from_utf8(bytes).map_err(|_| ClaudeCredentialProblem::InvalidJson)?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(
    content: &str,
    source: CredentialSource,
) -> Result<Credentials, ClaudeCredentialProblem> {
    let json: serde_json::Value =
        serde_json::from_str(content).map_err(|_| ClaudeCredentialProblem::InvalidJson)?;
    let oauth = json
        .get("claudeAiOauth")
        .and_then(serde_json::Value::as_object)
        .ok_or(ClaudeCredentialProblem::UnsupportedShape)?;
    let access_token = oauth
        .get("accessToken")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(ClaudeCredentialProblem::UnsupportedShape)?
        .to_string();
    let access_expires_at = oauth.get("expiresAt").and_then(serde_json::Value::as_i64);
    // Never retain, log, or expose the refresh-token value. Presence and its
    // expiry are sufficient to classify the recovery path.
    let refresh_token_present = oauth
        .get("refreshToken")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|token| !token.is_empty());
    let refresh_token_expires_at = oauth
        .get("refreshTokenExpiresAt")
        .and_then(serde_json::Value::as_i64);

    Ok(Credentials {
        access_token,
        access_expires_at,
        refresh_token_present,
        refresh_token_expires_at,
        source,
    })
}

/// Installed distros change about as often as Windows itself, but every
/// credential read and watch snapshot re-ran `wsl.exe -l -q`. That spawn
/// costs the full 5s timeout whenever WSL is absent or broken (a common
/// setup: it fails with REGDB_E_CLASSNOTREG), and the enumeration happens on
/// the UI thread during the auth-error watch. Cache it, including the
/// failure, so a stalled WSL cannot be paid for on every tick.
const WSL_DISTRO_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

struct WslDistroCache {
    fetched_at: Instant,
    distros: Vec<String>,
}

fn wsl_cache_is_fresh(entry: &WslDistroCache, now: Instant) -> bool {
    now.duration_since(entry.fetched_at) < WSL_DISTRO_CACHE_TTL
}

static WSL_DISTRO_CACHE: OnceLock<Mutex<Option<WslDistroCache>>> = OnceLock::new();

fn list_wsl_distros() -> Vec<String> {
    let cache = WSL_DISTRO_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(cached) = cache.lock() {
        if let Some(entry) = cached.as_ref() {
            if wsl_cache_is_fresh(entry, Instant::now()) {
                return entry.distros.clone();
            }
        }
    }

    let distros = enumerate_wsl_distros();
    if let Ok(mut cached) = cache.lock() {
        *cached = Some(WslDistroCache {
            fetched_at: Instant::now(),
            distros: distros.clone(),
        });
    }
    distros
}

fn enumerate_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Ok(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Distros that are already up.
///
/// Reading a credential inside a stopped distro starts its virtual machine,
/// which is far too heavy for something on a timer. A distro the user is
/// actively working in is already running, so probing it costs nothing.
/// Deliberately uncached: the running set changes whenever the user starts or
/// stops a distro, and a stale answer here means probing a stopped distro.
fn list_running_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q", "--running"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    decode_wsl_text(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn timestamp_is_expired(expires_at: Option<i64>, now_ms: i64) -> bool {
    expires_at.is_some_and(|expires_at| now_ms >= expires_at)
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
/// The APIs return formats like "2026-03-05T08:00:00.321598+00:00" and
/// "2026-06-13T22:08:54Z"; non-zero UTC offsets are converted, not dropped.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?.trim();
    let t_pos = s.find('T')?;
    let time_tail = &s[t_pos + 1..];

    // Split off the timezone suffix: 'Z', or a '+HH:MM' / '-HH:MM' offset.
    let (datetime_part, offset_secs) = if let Some(z_rel) = time_tail.find(['Z', 'z']) {
        (&s[..t_pos + 1 + z_rel], 0)
    } else if let Some(sign_rel) = time_tail.find(['+', '-']) {
        let sign_pos = t_pos + 1 + sign_rel;
        (&s[..sign_pos], parse_utc_offset_secs(&s[sign_pos..])?)
    } else {
        (s, 0)
    };

    let local_secs = parse_datetime_to_unix(datetime_part).ok()?;
    // "08:00 at +02:00" is 06:00 UTC: subtract the offset.
    let utc_secs = (local_secs as i64).checked_sub(offset_secs)?;
    if utc_secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(utc_secs as u64))
}

/// Parse "+HH:MM", "-HHMM", or "+HH" into signed seconds east of UTC.
fn parse_utc_offset_secs(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    let (hours, minutes) = match rest.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None if rest.len() == 2 => (rest, "0"),
        None => return None,
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

/// Minimal datetime parser - avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    // Bounds before arithmetic: month indexes month_days, day-1 must not
    // underflow, and a huge year would spin the per-year loop below. The
    // input is a provider API response, so a malformed value must fail the
    // parse rather than panic or wrap into a bogus timestamp.
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(());
    }

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;
    if hour > 23 || min > 59 || sec > 59 {
        return Err(());
    }

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section as "X%·Yh" style text for compact surfaces.
/// These units deliberately stay English (d/h/m/s/now) in every UI
/// language: they are terse, universally recognizable, and keep the taskbar
/// and floating surfaces compact.
#[cfg(test)]
fn format_line(section: &UsageWindow) -> String {
    let pct = crate::compact_view::display_percent_text(section.percentage);
    let cd = format_countdown(section.resets_at);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct}\u{00b7}{cd}")
    }
}

pub(crate) fn format_countdown(resets_at: Option<SystemTime>) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return "now".to_string(),
    };

    format_countdown_from_secs(remaining.as_secs())
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64) -> String {
    if total_secs == 0 {
        return "now".to_string();
    }
    if total_secs < 60 {
        return format!("{total_secs}s");
    }

    // All relative-time surfaces use the same display bucket: once seconds
    // are hidden, a partial minute counts as the next minute. Derive the
    // compact unit from that rounded value as well, otherwise 45m 01s becomes
    // "45m" here while tooltips and the detail popup say "46m".
    let total_minutes = display_minutes_from_secs(total_secs);
    if total_minutes >= 24 * 60 {
        format!("{}d", total_minutes / (24 * 60))
    } else if total_minutes >= 60 {
        format!("{}h", total_minutes / 60)
    } else {
        format!("{total_minutes}m")
    }
}

/// Shared minute rounding policy for compact, tooltip, and detail surfaces.
pub(crate) fn display_minutes_from_secs(total_secs: u64) -> u64 {
    total_secs.div_ceil(60).max(1)
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    if total_secs <= 60 {
        return Duration::from_secs(1);
    }

    let total_minutes = display_minutes_from_secs(total_secs);
    let next_bucket_minutes = if total_minutes >= 24 * 60 {
        (total_minutes / (24 * 60)) * (24 * 60) - 1
    } else if total_minutes >= 60 {
        (total_minutes / 60) * 60 - 1
    } else {
        total_minutes - 1
    };
    let next_bucket_secs = next_bucket_minutes.saturating_mul(60);

    Duration::from_secs(total_secs.saturating_sub(next_bucket_secs).max(1))
}

/// Returns true if any reported window has reached "now".
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    data.windows
        .iter()
        .any(|window| matches!(window.resets_at, Some(t) if now.duration_since(t).is_ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_credentials(
        source: CredentialSource,
        access_expires_at: Option<i64>,
        refresh_token_present: bool,
        refresh_token_expires_at: Option<i64>,
    ) -> Credentials {
        Credentials {
            access_token: format!("test-token-{}", source.diagnostic_label()),
            access_expires_at,
            refresh_token_present,
            refresh_token_expires_at,
            source,
        }
    }

    #[test]
    fn claude_config_dir_prefers_the_supported_override_and_ignores_an_empty_one() {
        let home = PathBuf::from(r"C:\Users\Example");
        let configured = PathBuf::from(r"D:\ClaudeProfile");
        assert_eq!(
            claude_config_dir_from(Some(configured.clone()), Some(home.clone())),
            Some(configured)
        );
        assert_eq!(
            claude_config_dir_from(Some(PathBuf::new()), Some(home.clone())),
            Some(home.join(".claude"))
        );
        assert_eq!(claude_config_dir_from(None, None), None);
    }

    #[test]
    fn codex_home_ignores_an_empty_override() {
        let home = PathBuf::from(r"C:\Users\Example");
        let configured = PathBuf::from(r"D:\CodexProfile");
        assert_eq!(
            codex_home_from(Some(configured.clone()), Some(home.clone())),
            Some(configured)
        );
        assert_eq!(
            codex_home_from(Some(PathBuf::new()), Some(home.clone())),
            Some(home.join(".codex"))
        );
        assert_eq!(codex_home_from(None, None), None);
    }

    #[test]
    fn wsl_read_watch_and_diagnostics_share_the_supported_config_resolution() {
        for script in [WSL_CREDENTIAL_READ_SCRIPT, WSL_CREDENTIAL_PATH_SCRIPT] {
            assert!(script.contains("${CLAUDE_CONFIG_DIR:-$HOME/.claude}"));
            assert!(script.contains(".credentials.json"));
        }
    }

    #[test]
    fn claude_credential_parser_retains_only_refresh_presence_and_expiry() {
        let source = CredentialSource::Windows(PathBuf::from("credentials.json"));
        let credentials = parse_credentials(
            r#"{"claudeAiOauth":{"accessToken":"access-value","expiresAt":2000,"refreshToken":"refresh-value","refreshTokenExpiresAt":3000}}"#,
            source,
        )
        .expect("supported Claude credential shape");
        assert_eq!(credentials.access_token, "access-value");
        assert_eq!(credentials.access_expires_at, Some(2000));
        assert!(credentials.refresh_token_present);
        assert_eq!(credentials.refresh_token_expires_at, Some(3000));
    }

    #[test]
    fn claude_credential_parser_distinguishes_invalid_json_from_an_unsupported_shape() {
        let source = CredentialSource::Windows(PathBuf::from("credentials.json"));
        assert!(matches!(
            parse_credentials("not json", source.clone()),
            Err(ClaudeCredentialProblem::InvalidJson)
        ));
        assert!(matches!(
            parse_credentials(r#"{"claudeAiOauth":{}}"#, source),
            Err(ClaudeCredentialProblem::UnsupportedShape)
        ));
    }

    #[test]
    fn claude_credential_problems_distinguish_missing_from_unusable_credentials() {
        assert_eq!(
            claude_credential_problem_poll_error(ClaudeCredentialProblem::Missing),
            PollError::NoCredentials
        );
        for problem in [
            ClaudeCredentialProblem::Unreadable,
            ClaudeCredentialProblem::InvalidJson,
            ClaudeCredentialProblem::UnsupportedShape,
            ClaudeCredentialProblem::RefreshMissing,
            ClaudeCredentialProblem::RefreshExpired,
        ] {
            assert_eq!(
                claude_credential_problem_poll_error(problem),
                PollError::AuthRequired
            );
        }
    }

    #[test]
    fn unavailable_claude_probe_is_transient_and_never_an_authentication_warning() {
        let error = claude_credential_problem_poll_error(ClaudeCredentialProblem::ProbeUnavailable);

        assert_eq!(error, PollError::RequestFailed);
        assert_eq!(provider_status(error), ProviderStatus::RequestFailed);
        assert!(!provider_status(error).warrants_credential_alert());
    }

    #[test]
    fn command_runner_distinguishes_spawn_failure_from_timeout() {
        let missing_program = std::env::temp_dir().join(format!(
            "gengchou-missing-command-{}-{}",
            std::process::id(),
            now_unix_millis()
        ));
        let mut missing_command = Command::new(missing_program);
        assert!(matches!(
            run_with_timeout(&mut missing_command, Duration::from_millis(10)),
            Err(CommandRunError::SpawnFailed)
        ));

        assert!(matches!(
            run_with_timeout(
                Command::new("powershell.exe")
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Start-Sleep -Seconds 30",
                    ])
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()),
                Duration::from_millis(10),
            ),
            Err(CommandRunError::TimedOut)
        ));
    }

    #[test]
    fn claude_credential_lifecycle_uses_the_recorded_expiries_without_fixed_lifetimes() {
        let source = CredentialSource::Windows(PathBuf::from("credentials.json"));
        let usable = test_credentials(source.clone(), Some(2_000), false, None);
        assert!(matches!(
            classify_claude_credentials(&usable, 1_000),
            ClaudeCredentialLifecycle::Usable
        ));

        let refreshable_unknown = test_credentials(source.clone(), Some(900), true, None);
        assert!(matches!(
            classify_claude_credentials(&refreshable_unknown, 1_000),
            ClaudeCredentialLifecycle::Refreshable
        ));

        let no_refresh = test_credentials(source.clone(), Some(900), false, None);
        assert!(matches!(
            classify_claude_credentials(&no_refresh, 1_000),
            ClaudeCredentialLifecycle::LoginRequired(ClaudeCredentialProblem::RefreshMissing)
        ));

        let refresh_expired = test_credentials(source, Some(900), true, Some(1_000));
        assert!(matches!(
            classify_claude_credentials(&refresh_expired, 1_000),
            ClaudeCredentialLifecycle::LoginRequired(ClaudeCredentialProblem::RefreshExpired)
        ));
    }

    #[test]
    fn later_usable_claude_source_beats_earlier_invalid_or_refreshable_sources() {
        let windows = CredentialSource::Windows(PathBuf::from("credentials.json"));
        let wsl_refreshable = CredentialSource::Wsl {
            distro: "refreshable".to_string(),
        };
        let wsl_usable = CredentialSource::Wsl {
            distro: "usable".to_string(),
        };
        let probes = vec![
            (windows, Err(ClaudeCredentialProblem::InvalidJson)),
            (
                wsl_refreshable.clone(),
                Ok(test_credentials(
                    wsl_refreshable,
                    Some(900),
                    true,
                    Some(2_000),
                )),
            ),
            (
                wsl_usable.clone(),
                Ok(test_credentials(wsl_usable, Some(2_000), false, None)),
            ),
        ];
        match choose_claude_credentials(probes, 1_000) {
            ClaudeCredentialSelection::Usable(credentials) => assert!(matches!(
                credentials.source,
                CredentialSource::Wsl { ref distro } if distro == "usable"
            )),
            _ => panic!("a later usable source must win"),
        }
    }

    #[test]
    fn compact_countdown_always_uses_english_units() {
        assert_eq!(format_countdown_from_secs(2 * 86_400), "2d");
        assert_eq!(format_countdown_from_secs(3 * 3_600), "3h");
        assert_eq!(format_countdown_from_secs(42 * 60), "42m");
        assert_eq!(format_countdown_from_secs(17), "17s");
        assert_eq!(format_countdown_from_secs(0), "now");
    }

    #[test]
    fn compact_countdown_uses_the_shared_ceil_minute_policy() {
        assert_eq!(format_countdown_from_secs(45 * 60), "45m");
        assert_eq!(format_countdown_from_secs(45 * 60 + 1), "46m");
        assert_eq!(format_countdown_from_secs(59 * 60 + 1), "1h");
        assert_eq!(format_countdown_from_secs(23 * 60 * 60 + 59 * 60 + 1), "1d");
    }

    #[test]
    fn compact_countdown_timer_follows_the_rounded_bucket_boundary() {
        assert_eq!(
            time_until_display_change_from_secs(45 * 60 + 30),
            Duration::from_secs(30)
        );
        assert_eq!(
            time_until_display_change_from_secs(45 * 60),
            Duration::from_secs(60)
        );
        assert_eq!(
            time_until_display_change_from_secs(60),
            Duration::from_secs(1)
        );
        assert_eq!(
            time_until_display_change_from_secs(60 * 60 + 1),
            Duration::from_secs(61)
        );
    }

    #[test]
    fn compact_line_uses_now_for_elapsed_reset() {
        let usage = UsageWindow::new(
            85.0,
            SystemTime::now().checked_sub(Duration::from_secs(1)),
            Some(FIVE_HOURS_SECONDS),
        );

        assert_eq!(format_line(&usage), "85%·now");
    }

    #[test]
    fn every_provider_status_uses_the_same_rate_limit_classification() {
        assert_eq!(
            classify_http_status(429, Some(120_000)),
            PollError::RateLimited(Some(120_000))
        );
        assert_eq!(classify_http_status(401, None), PollError::AuthRequired);
        assert_eq!(classify_http_status(403, None), PollError::AuthForbidden);
        assert_eq!(classify_http_status(500, None), PollError::RequestFailed);
    }

    #[test]
    fn claude_cli_recovery_is_limited_to_windows_credentials() {
        let windows =
            CredentialSource::Windows(PathBuf::from(r"C:\Users\test\.claude\.credentials.json"));
        let wsl = CredentialSource::Wsl {
            distro: "Ubuntu".to_string(),
        };

        assert!(should_auto_recover_claude(&windows));
        assert!(!should_auto_recover_claude(&wsl));
    }

    #[test]
    fn codex_retries_auth_and_transient_gateway_statuses() {
        assert!(codex_http_status_is_retryable(401));
        assert!(codex_http_status_is_retryable(403));
        assert!(codex_http_status_is_retryable(502));
        assert!(codex_http_status_is_retryable(503));
        assert!(codex_http_status_is_retryable(504));
        assert!(!codex_http_status_is_retryable(429));
        assert!(!codex_http_status_is_retryable(500));
    }

    #[test]
    fn claude_auth_failure_retries_once_and_recovers() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let data = fetch_claude_usage_with_auth_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(PollError::AuthRequired)
                } else {
                    Ok(Some(usage_with_percent(42.0)))
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect("second Claude attempt should recover")
        .expect("second Claude attempt should return usage");

        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);
        assert_eq!(data.windows[0].percentage, 42.0);
    }

    #[test]
    fn claude_auth_confirmation_returns_the_second_attempt_error() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let error = fetch_claude_usage_with_auth_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(PollError::AuthRequired)
                } else {
                    Err(PollError::RequestFailed)
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect_err("the second Claude attempt should determine the result");

        assert_eq!(error, PollError::RequestFailed);
        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);
    }

    #[test]
    fn claude_non_401_failures_are_not_retried() {
        use std::cell::Cell;

        for expected in [
            PollError::AuthForbidden,
            PollError::NoCredentials,
            PollError::RateLimited(None),
            PollError::NetworkUnavailable,
            PollError::RequestFailed,
        ] {
            let attempts = Cell::new(0_u8);
            let waits = Cell::new(0_u8);
            let error = fetch_claude_usage_with_auth_retry(
                || {
                    attempts.set(attempts.get() + 1);
                    Err(expected)
                },
                || waits.set(waits.get() + 1),
            )
            .expect_err("non-auth Claude failures should be returned immediately");

            assert_eq!(error, expected);
            assert_eq!(attempts.get(), 1);
            assert_eq!(waits.get(), 0);
        }
    }

    #[test]
    fn codex_transient_failure_retries_once_and_recovers() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let data = fetch_codex_usage_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(CodexAttemptError::Retryable(PollError::RequestFailed))
                } else {
                    Ok(usage_with_percent(42.0))
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect("second Codex attempt should recover");

        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);
        assert_eq!(data.windows[0].percentage, 42.0);
    }

    #[test]
    fn codex_auth_confirmation_returns_the_second_attempt_error() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let error = fetch_codex_usage_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(CodexAttemptError::Retryable(PollError::AuthRequired))
                } else {
                    Err(CodexAttemptError::Final(PollError::RequestFailed))
                }
            },
            || waits.set(waits.get() + 1),
        )
        .expect_err("the second Codex attempt should determine the result");

        assert_eq!(error, PollError::RequestFailed);
        assert_eq!(attempts.get(), 2);
        assert_eq!(waits.get(), 1);
    }

    #[test]
    fn codex_final_failure_is_not_retried() {
        use std::cell::Cell;

        let attempts = Cell::new(0_u8);
        let waits = Cell::new(0_u8);
        let error = fetch_codex_usage_with_retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(CodexAttemptError::Final(PollError::RateLimited(None)))
            },
            || waits.set(waits.get() + 1),
        )
        .expect_err("final failures must not be retried");

        assert_eq!(error, PollError::RateLimited(None));
        assert_eq!(attempts.get(), 1);
        assert_eq!(waits.get(), 0);
    }

    #[test]
    fn auth_rejection_recheck_is_exposed_in_milliseconds() {
        assert_eq!(AUTH_REJECTION_RECHECK_MS, 15 * 60 * 1_000);
    }

    #[test]
    fn retry_after_accepts_seconds_and_caps_large_values() {
        assert_eq!(retry_after_value_ms("120", UNIX_EPOCH), Some(120_000));
        assert_eq!(
            retry_after_value_ms("999999999999", UNIX_EPOCH),
            Some(u32::MAX)
        );
    }

    #[test]
    fn retry_after_accepts_http_date_and_rejects_invalid_values() {
        let retry_unix = parse_retry_after_http_date("Mon, 13 Jul 2026 12:00:00 GMT")
            .expect("valid IMF-fixdate should parse");
        let now = UNIX_EPOCH + Duration::from_secs(retry_unix - 120);

        assert_eq!(
            retry_after_value_ms("Mon, 13 Jul 2026 12:00:00 GMT", now),
            Some(120_000)
        );
        assert_eq!(retry_after_value_ms("not-a-date", now), None);
    }

    #[test]
    fn credential_content_fingerprint_detects_same_length_replacement() {
        let before = content_watch_signature("credential", b"token-a");
        let after = content_watch_signature("credential", b"token-b");

        assert_ne!(before, after);
        assert!(before.contains("|present|7|"));
    }

    #[test]
    fn codex_direct_keyring_target_matches_official_windows_mapping() {
        assert_eq!(
            codex_direct_keyring_target_from_path("abc").as_deref(),
            Some("cli|ba7816bf8f01cfea.Codex Auth")
        );
    }

    #[test]
    fn a_pass_with_nothing_in_scope_reads_no_credential_source() {
        assert_eq!(
            detect_signed_in_providers(DetectionScope::default()),
            DetectedProviders::default()
        );
    }

    #[test]
    fn a_broken_source_outranks_a_missing_one_when_falling_back() {
        let broken = || LocalCredential::<u8>::Unusable;
        let absent = || LocalCredential::<u8>::Missing;

        assert!(matches!(
            LocalCredential::Missing.or_else(broken),
            LocalCredential::Unusable
        ));
        assert!(matches!(
            LocalCredential::Unusable.or_else(absent),
            LocalCredential::Unusable
        ));
        assert!(matches!(
            LocalCredential::Unusable.or_else(|| LocalCredential::Usable(1u8)),
            LocalCredential::Usable(1)
        ));
        assert!(matches!(
            LocalCredential::Missing.or_else(absent),
            LocalCredential::Missing
        ));
    }

    #[test]
    fn a_working_wsl_distro_still_rescues_a_broken_windows_credential() {
        assert!(matches!(
            LocalCredential::<u8>::Unusable.or_else_wsl(|| Some(7)),
            LocalCredential::Usable(7)
        ));
        assert!(matches!(
            LocalCredential::<u8>::Unusable.or_else_wsl(|| None),
            LocalCredential::Unusable
        ));
    }

    #[test]
    fn a_credential_file_is_missing_only_when_it_is_not_there() {
        let root = std::env::temp_dir().join(format!(
            "gengchou-local-credential-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap_or_default();
        let parse = |content: &str| (content == "good").then_some(1u8);

        assert!(matches!(
            local_credential_from_file(&root.join("absent.json"), parse),
            LocalCredential::Missing
        ));

        let malformed = root.join("malformed.json");
        std::fs::write(&malformed, "bad").unwrap_or_default();
        assert!(matches!(
            local_credential_from_file(&malformed, parse),
            LocalCredential::Unusable
        ));

        let usable = root.join("usable.json");
        std::fs::write(&usable, "good").unwrap_or_default();
        assert!(matches!(
            local_credential_from_file(&usable, parse),
            LocalCredential::Usable(1)
        ));

        std::fs::remove_dir_all(&root).unwrap_or_default();
    }

    #[test]
    fn an_unusable_local_credential_reads_as_authentication_failed_without_a_remote_rejection() {
        assert_eq!(
            provider_status(PollError::CredentialUnusable),
            ProviderStatus::AuthenticationFailed
        );

        let mut data = AppUsageData::default();
        record_poll_error(&mut data, PollError::CredentialUnusable);
        assert!(
            !data.remote_auth_rejection,
            "a local credential nobody rejected must not arm the bounded service recheck"
        );
    }

    #[test]
    fn missing_windows_credential_has_stable_signature() {
        let path = std::env::temp_dir().join(format!(
            "gengchou-missing-credential-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));

        assert!(windows_credential_watch_signature(&path).ends_with("|missing"));
    }

    /// Build a poll pass table from the fixed three-provider order used by
    /// these cases, so a test states only which providers are on and what
    /// each one returns.
    fn poll_targets<'a>(
        enabled: [bool; TrayIconKind::COUNT],
        polls: [Box<dyn FnOnce() -> Result<UsageData, PollError> + Send + 'a>; TrayIconKind::COUNT],
    ) -> Vec<PollTarget<'a>> {
        TrayIconKind::ALL
            .into_iter()
            .zip(enabled)
            .zip(polls)
            .map(|((kind, enabled), poll)| PollTarget {
                kind,
                enabled,
                poll,
            })
            .collect()
    }

    /// Shape of a real `/v1/billing?format=credits` response on a free-tier
    /// account: no `creditUsagePercent`, no on-demand allowance, one weekly
    /// period. A billing response carries no token or account identifier.
    const GROK_FREE_TIER_BILLING: &str = r#"{"config":{"currentPeriod":{
        "type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-08-15T00:00:00+00:00",
        "end":"2026-08-22T00:00:00+00:00"},"onDemandCap":{"val":0},
        "onDemandUsed":{"val":0},"isUnifiedBillingUser":true,
        "prepaidBalance":{"val":0}}}"#;

    fn grok_usage(text: &str) -> Option<UsageData> {
        let response: GrokBillingResponse = serde_json::from_str(text).ok()?;
        grok_usage_from_billing(response)
    }

    #[test]
    fn grok_missing_percentage_is_zero_not_missing_data() {
        let data = grok_usage(GROK_FREE_TIER_BILLING).expect("free tier billing should parse");
        assert_eq!(data.windows.len(), 1);
        assert_eq!(data.windows[0].percentage, 0.0);
        assert_eq!(data.windows[0].duration_seconds, Some(7 * 24 * 60 * 60));
        assert_eq!(data.windows[0].source_label, None);
        assert_eq!(
            data.windows[0]
                .resets_at
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            Some(1_787_356_800)
        );
    }

    /// Verified against the live endpoint on 2026-08-31: a paid account
    /// reports the percentage directly, and both period bounds carry
    /// fractional seconds and a numeric UTC offset rather than `Z`.
    #[test]
    fn grok_parses_the_live_response_shape() {
        let live = r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY",
            "start":"2026-08-30T18:29:39.234502+00:00",
            "end":"2026-09-06T18:29:39.234502+00:00"},
            "creditUsagePercent":5.0,"onDemandCap":{"val":0},"onDemandUsed":{"val":0},
            "isUnifiedBillingUser":true,"prepaidBalance":{"val":0},
            "billingPeriodStart":"2026-08-30T18:29:39.234502+00:00",
            "billingPeriodEnd":"2026-09-06T18:29:39.234502+00:00",
            "topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD","productUsage":{}}}"#;
        let data = grok_usage(live).expect("live response shape should parse");
        assert_eq!(data.windows[0].percentage, 5.0);
        assert_eq!(data.windows[0].duration_seconds, Some(7 * 24 * 60 * 60));
        assert_eq!(data.windows[0].source_label, None);
    }

    #[test]
    fn grok_window_length_is_measured_not_assumed() {
        // A February billing period is 28 days, so a fixed 30-day constant
        // would mislabel it. The period reports its own start and end.
        let february = GROK_FREE_TIER_BILLING
            .replace("USAGE_PERIOD_TYPE_WEEKLY", "USAGE_PERIOD_TYPE_MONTHLY")
            .replace("2026-08-15T00:00:00+00:00", "2027-02-01T00:00:00+00:00")
            .replace("2026-08-22T00:00:00+00:00", "2027-03-01T00:00:00+00:00");
        let data = grok_usage(&february).expect("monthly billing should parse");
        assert_eq!(data.windows[0].duration_seconds, Some(28 * 24 * 60 * 60));
    }

    #[test]
    fn grok_unmeasurable_window_falls_back_to_the_reported_period_type() {
        let no_start =
            GROK_FREE_TIER_BILLING.replace(r#""start":"2026-08-15T00:00:00+00:00","#, "");
        let data = grok_usage(&no_start).expect("billing without a start should still parse");
        assert_eq!(data.windows[0].duration_seconds, None);
        assert_eq!(data.windows[0].source_label.as_deref(), Some("weekly"));
    }

    #[test]
    fn grok_rejects_billing_without_a_usable_reset() {
        for text in [
            r#"{"config":{"onDemandCap":{"val":0}}}"#,
            r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"nope"}}}"#,
        ] {
            assert!(grok_usage(text).is_none(), "{text}");
        }
    }

    #[test]
    fn grok_percentage_prefers_the_server_value_and_rejects_impossible_ones() {
        assert_eq!(
            grok_used_percent(Some(42.5), Some(9.0), Some(10.0)),
            Some(42.5)
        );
        assert_eq!(grok_used_percent(Some(101.0), None, None), None);
        assert_eq!(grok_used_percent(Some(-1.0), None, None), None);
        assert_eq!(grok_used_percent(Some(f64::NAN), None, None), None);
        // Falls back to the on-demand ratio, and never divides by a zero cap:
        // a free-tier account reports every allowance as zero.
        assert_eq!(grok_used_percent(None, Some(10.0), Some(40.0)), Some(25.0));
        assert_eq!(grok_used_percent(None, Some(0.0), Some(0.0)), Some(0.0));
        assert_eq!(grok_used_percent(None, None, None), Some(0.0));
    }

    #[test]
    fn grok_auth_json_prefers_the_xai_oauth_scope() {
        let content = r#"{
            "https://accounts.x.ai/sign-in::legacy": {"key": "legacy-token"},
            "https://auth.x.ai::b1a00492": {"key": "oauth-token",
                "oidc_issuer": "https://auth.x.ai"}
        }"#;
        assert_eq!(
            grok_token_from_auth_json(content).map(|token| token.access_token),
            Some("oauth-token".to_string())
        );
    }

    #[test]
    fn grok_auth_json_never_hands_a_third_party_token_to_xai() {
        // An enterprise OIDC entry is signed by someone other than xAI, so it
        // is skipped rather than used - even when it is the only entry.
        let enterprise_only = r#"{
            "https://login.example.com::client": {"key": "enterprise-token",
                "oidc_issuer": "https://login.example.com"}
        }"#;
        assert!(grok_token_from_auth_json(enterprise_only).is_none());

        // A hostname that merely ends in the domain text is not the domain.
        let lookalike = r#"{
            "https://auth.notx.ai::client": {"key": "lookalike-token",
                "oidc_issuer": "https://auth.notx.ai"}
        }"#;
        assert!(grok_token_from_auth_json(lookalike).is_none());

        let mixed = r#"{
            "https://login.example.com::client": {"key": "enterprise-token",
                "oidc_issuer": "https://login.example.com"},
            "https://accounts.x.ai/sign-in::legacy": {"key": "legacy-token"}
        }"#;
        assert_eq!(
            grok_token_from_auth_json(mixed).map(|token| token.access_token),
            Some("legacy-token".to_string())
        );
    }

    #[test]
    fn grok_auth_json_ignores_entries_without_a_usable_token() {
        assert!(grok_token_from_auth_json("{}").is_none());
        assert!(grok_token_from_auth_json("not json").is_none());
        assert!(grok_token_from_auth_json(r#"{"https://auth.x.ai::c": {}}"#).is_none());
        assert!(grok_token_from_auth_json(r#"{"https://auth.x.ai::c": {"key": "  "}}"#).is_none());
    }

    #[test]
    fn grok_home_ignores_an_empty_override() {
        let home = PathBuf::from(r"C:\Users\Example");
        let configured = PathBuf::from(r"D:\GrokProfile");
        assert_eq!(
            grok_home_from(Some(configured.clone()), Some(home.clone())),
            Some(configured)
        );
        assert_eq!(
            grok_home_from(Some(PathBuf::new()), Some(home.clone())),
            Some(home.join(".grok"))
        );
        assert_eq!(grok_home_from(None, None), None);
    }

    fn usage_with_percent(percentage: f64) -> UsageData {
        UsageData::from_windows(vec![UsageWindow::new(
            percentage,
            None,
            Some(FIVE_HOURS_SECONDS),
        )])
    }

    fn cached_claude_usage_for_test(
        percentage: f64,
        fetched_at: SystemTime,
        fast_polls_remaining: u32,
    ) -> CachedClaudeUsage {
        CachedClaudeUsage {
            token_hash: 7,
            fetched_at,
            data: usage_with_percent(percentage),
            fast_polls_remaining,
        }
    }

    #[test]
    fn claude_cache_uses_completion_based_normal_and_fast_deadlines() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let normal_fresh = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(179)).unwrap(),
            0,
        );
        let normal_due = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(180)).unwrap(),
            0,
        );
        let fast_fresh = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(119)).unwrap(),
            1,
        );
        let fast_due = cached_claude_usage_for_test(
            10.0,
            now.checked_sub(Duration::from_secs(120)).unwrap(),
            1,
        );

        assert!(claude_cache_is_fresh(&normal_fresh, 7, false, now));
        assert!(!claude_cache_is_fresh(&normal_due, 7, false, now));
        assert!(claude_cache_is_fresh(&fast_fresh, 7, false, now));
        assert!(!claude_cache_is_fresh(&fast_due, 7, false, now));
        assert!(!claude_cache_is_fresh(&normal_fresh, 7, true, now));
    }

    fn cached_usage_with_reset(fetched_at: SystemTime, resets_at: SystemTime) -> CachedClaudeUsage {
        CachedClaudeUsage {
            token_hash: 7,
            fetched_at,
            data: UsageData::from_windows(vec![UsageWindow::new(
                90.0,
                Some(resets_at),
                Some(FIVE_HOURS_SECONDS),
            )]),
            fast_polls_remaining: 0,
        }
    }

    #[test]
    fn claude_cache_goes_stale_once_a_cached_window_reset_passes() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let fetched_at = now.checked_sub(Duration::from_secs(60)).unwrap();

        // Snapshot taken before the reset, reset has passed: stale even
        // though the cadence deadline is still 120s away.
        let past_reset = cached_usage_with_reset(
            fetched_at,
            now.checked_sub(Duration::from_secs(10)).unwrap(),
        );
        assert!(!claude_cache_is_fresh(&past_reset, 7, false, now));

        // Reset still ahead: the normal cadence owns freshness.
        let upcoming_reset = cached_usage_with_reset(
            fetched_at,
            now.checked_add(Duration::from_secs(600)).unwrap(),
        );
        assert!(claude_cache_is_fresh(&upcoming_reset, 7, false, now));

        // Server still reporting a reset older than the snapshot (lagging
        // propagation): no re-fetch loop, the cadence owns freshness again.
        let lagging_reset = cached_usage_with_reset(
            fetched_at,
            fetched_at.checked_sub(Duration::from_secs(10)).unwrap(),
        );
        assert!(claude_cache_is_fresh(&lagging_reset, 7, false, now));
    }

    #[test]
    fn aligned_poll_delay_pulls_the_tick_to_the_cooldown_deadline() {
        // Deadline between fixed ticks: align to it (plus the fetch margin).
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(130)),
            Some(50_250)
        );
        // Deadline beyond the next fixed tick: keep the fixed cadence.
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(30)),
            Some(60_000)
        );
        // Deadline already passed: fire after the minimum delay, not 0ms.
        assert_eq!(
            aligned_poll_delay_ms(60_000, 180_000, Duration::from_secs(200)),
            Some(1_000)
        );
        // User cadence at least as coarse as the cooldown: nothing to align,
        // every fixed tick fetches anyway.
        assert_eq!(
            aligned_poll_delay_ms(300_000, 180_000, Duration::from_secs(30)),
            None
        );
        assert_eq!(
            aligned_poll_delay_ms(120_000, 120_000, Duration::from_secs(30)),
            None
        );
    }

    #[test]
    fn claude_usage_growth_arms_three_fast_follow_up_polls() {
        let previous = cached_claude_usage_for_test(10.0, SystemTime::now(), 0);
        let increased = usage_with_percent(11.0);
        assert_eq!(
            next_claude_fast_polls(Some(&previous), 7, &increased),
            CLAUDE_USAGE_FAST_EXTRA + 1
        );

        let fast = cached_claude_usage_for_test(11.0, SystemTime::now(), 3);
        assert_eq!(
            next_claude_fast_polls(Some(&fast), 7, &usage_with_percent(11.0)),
            2
        );
        assert_eq!(
            next_claude_fast_polls(Some(&fast), 8, &usage_with_percent(12.0)),
            0
        );
    }

    #[test]
    fn claude_rate_limit_delay_keeps_manual_refresh_inside_backoff() {
        assert_eq!(
            claude_rate_limit_delay_ms(None),
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS
        );
        assert_eq!(
            claude_rate_limit_delay_ms(Some(1_000)),
            CLAUDE_RATE_LIMIT_MIN_RETRY_MS
        );
        assert_eq!(
            claude_rate_limit_delay_ms(Some(u32::MAX)),
            CLAUDE_RATE_LIMIT_MAX_RETRY_MS
        );
    }

    #[test]
    fn enabled_providers_are_polled_concurrently() {
        use std::sync::{Arc, Condvar, Mutex};

        let rendezvous = Arc::new((Mutex::new(0_u8), Condvar::new()));
        let make_poll = |percentage| {
            let rendezvous = Arc::clone(&rendezvous);
            move || {
                let (lock, ready) = &*rendezvous;
                let mut started = lock.lock().expect("provider rendezvous lock should work");
                *started += 1;
                ready.notify_all();
                let (started, wait) = ready
                    .wait_timeout_while(started, Duration::from_secs(5), |started| *started < 2)
                    .expect("provider rendezvous wait should work");
                if wait.timed_out() {
                    return Err(PollError::RequestFailed);
                }
                drop(started);
                Ok(usage_with_percent(percentage))
            }
        };

        let data = poll_with(poll_targets(
            [true, true, false, false],
            [
                Box::new(make_poll(11.0)),
                Box::new(make_poll(22.0)),
                Box::new(|| unreachable!("antigravity is disabled")),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect("both concurrent providers should succeed");

        assert_eq!(
            data.usage(TrayIconKind::Claude).unwrap().windows[0].percentage,
            11.0
        );
        assert_eq!(
            data.usage(TrayIconKind::Codex).unwrap().windows[0].percentage,
            22.0
        );
    }

    #[test]
    fn claude_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(poll_targets(
            [true, true, false, false],
            [
                Box::new(|| Err(PollError::AuthRequired)),
                Box::new(|| Ok(usage_with_percent(42.0))),
                Box::new(|| unreachable!("antigravity is disabled")),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect("codex data should keep the poll successful");

        assert!(data.usage(TrayIconKind::Claude).is_none());
        assert_eq!(
            data.error(TrayIconKind::Claude),
            Some(ProviderStatus::AuthenticationFailed)
        );
        assert!(data.error(TrayIconKind::Codex).is_none());
        assert_eq!(
            data.usage(TrayIconKind::Codex).unwrap().windows[0].percentage,
            42.0
        );
    }

    #[test]
    fn codex_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(poll_targets(
            [true, true, false, false],
            [
                Box::new(|| Ok(usage_with_percent(64.0))),
                Box::new(|| Err(PollError::RequestFailed)),
                Box::new(|| unreachable!("antigravity is disabled")),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect("claude data should keep the poll successful");

        assert_eq!(
            data.usage(TrayIconKind::Claude).unwrap().windows[0].percentage,
            64.0
        );
        assert!(data.usage(TrayIconKind::Codex).is_none());
    }

    #[test]
    fn rate_limit_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(poll_targets(
            [true, true, false, false],
            [
                Box::new(|| Err(PollError::RateLimited(Some(120_000)))),
                Box::new(|| Ok(usage_with_percent(42.0))),
                Box::new(|| unreachable!("antigravity is disabled")),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect("codex data should keep the poll successful");

        assert!(data.usage(TrayIconKind::Claude).is_none());
        assert_eq!(
            data.error(TrayIconKind::Claude),
            Some(ProviderStatus::RateLimited)
        );
        assert_eq!(
            data.usage(TrayIconKind::Codex).unwrap().windows[0].percentage,
            42.0
        );
        assert_eq!(
            data.provider(TrayIconKind::Claude).retry_after_ms,
            Some(120_000)
        );
        assert_eq!(data.provider(TrayIconKind::Codex).retry_after_ms, None);
    }
    #[test]
    fn mixed_all_provider_failure_does_not_claim_every_provider_needs_login() {
        let failure = poll_with(poll_targets(
            [true, true, true, false],
            [
                Box::new(|| Err(PollError::AuthRequired)),
                Box::new(|| Err(PollError::RequestFailed)),
                Box::new(|| Err(PollError::NoCredentials)),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect_err("all-provider failure should return an error");

        assert_eq!(failure.error, PollError::RequestFailed);
        assert_eq!(
            failure.data.error(TrayIconKind::Claude),
            Some(ProviderStatus::AuthenticationFailed)
        );
        assert_eq!(
            failure.data.error(TrayIconKind::Codex),
            Some(ProviderStatus::RequestFailed)
        );
        // Never signed in, not rejected: the user has nothing to re-authenticate.
        assert_eq!(
            failure.data.error(TrayIconKind::Antigravity),
            Some(ProviderStatus::NotSignedIn)
        );
    }

    #[test]
    fn all_provider_auth_failures_still_require_login() {
        let failure = poll_with(poll_targets(
            [true, true, true, false],
            [
                Box::new(|| Err(PollError::AuthRequired)),
                Box::new(|| Err(PollError::NoCredentials)),
                Box::new(|| Err(PollError::AuthRequired)),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect_err("all-provider authentication failure should return an error");

        assert!(matches!(
            failure.error,
            PollError::AuthRequired | PollError::NoCredentials
        ));
        assert_eq!(
            failure.data.error(TrayIconKind::Claude),
            Some(ProviderStatus::AuthenticationFailed)
        );
        assert_eq!(
            failure.data.error(TrayIconKind::Codex),
            Some(ProviderStatus::NotSignedIn)
        );
        assert_eq!(
            failure.data.error(TrayIconKind::Antigravity),
            Some(ProviderStatus::AuthenticationFailed)
        );
        // Both still park the provider and keep the credential watch armed;
        // only the wording and the balloon differ.
        for status in [
            failure.data.error(TrayIconKind::Claude),
            failure.data.error(TrayIconKind::Codex),
            failure.data.error(TrayIconKind::Antigravity),
        ] {
            assert!(status.is_some_and(ProviderStatus::needs_credentials));
        }
    }

    /// Detection is an OR across that provider's sources, so a credential
    /// found anywhere counts and a provider with none stays undetected.
    #[test]
    fn detection_accepts_any_source_for_a_provider() {
        assert_eq!(
            detect_from(DetectionInputs {
                claude_wsl: true,
                codex_windows: true,
                ..Default::default()
            }),
            DetectedProviders {
                claude: true,
                codex: true,
                antigravity: false,
                grok: false,
            }
        );
        assert_eq!(
            detect_from(DetectionInputs {
                claude_desktop: true,
                antigravity_wsl: true,
                ..Default::default()
            }),
            DetectedProviders {
                claude: true,
                codex: false,
                antigravity: true,
                grok: false,
            }
        );
        assert!(!detect_from(DetectionInputs::default()).any());
        assert!(detect_from(DetectionInputs {
            codex_wsl: true,
            ..Default::default()
        })
        .any());
    }

    /// The whole point of the split: a missing credential and a rejected one
    /// need different words and only one of them deserves a notification.
    #[test]
    fn missing_credentials_are_not_reported_as_an_authentication_failure() {
        assert_eq!(
            provider_status(PollError::NoCredentials),
            ProviderStatus::NotSignedIn
        );
        assert_eq!(
            provider_status(PollError::AuthRequired),
            ProviderStatus::AuthenticationFailed
        );
        assert_eq!(
            provider_status(PollError::AuthForbidden),
            ProviderStatus::AuthenticationFailed
        );
        assert!(!ProviderStatus::NotSignedIn.warrants_credential_alert());
        assert!(ProviderStatus::AuthenticationFailed.warrants_credential_alert());
    }

    #[test]
    fn alternative_endpoint_auth_error_needs_consensus_before_login_is_required() {
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::RequestFailed,
                PollError::RequestFailed,
            ]),
            PollError::RequestFailed
        );
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::AuthRequired,
                PollError::AuthRequired,
            ]),
            PollError::AuthRequired
        );
        assert_eq!(
            aggregate_poll_errors(&[
                PollError::AuthRequired,
                PollError::RateLimited(Some(120_000)),
                PollError::RequestFailed,
            ]),
            PollError::RateLimited(Some(120_000))
        );
    }

    #[test]
    fn rejected_credential_backoff_clears_for_a_new_token_or_success() {
        let state = OnceLock::new();

        assert!(!auth_rejection_is_backed_off(&state, 11));
        record_auth_rejection(&state, 11);
        assert!(auth_rejection_is_backed_off(&state, 11));
        assert!(!auth_rejection_is_backed_off(&state, 22));

        record_auth_rejection(&state, 22);
        clear_auth_rejection(&state);
        assert!(!auth_rejection_is_backed_off(&state, 22));
    }

    #[test]
    fn antigravity_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(poll_targets(
            [false, true, true, false],
            [
                Box::new(|| unreachable!("claude code is disabled")),
                Box::new(|| Ok(usage_with_percent(42.0))),
                Box::new(|| Err(PollError::NoCredentials)),
                Box::new(|| unreachable!("grok is disabled")),
            ],
        ))
        .expect("codex data should keep the poll successful");

        assert!(data.usage(TrayIconKind::Antigravity).is_none());
        assert_eq!(
            data.usage(TrayIconKind::Codex).unwrap().windows[0].percentage,
            42.0
        );
    }

    #[test]
    fn codex_weekly_only_window_is_not_treated_as_five_hour_usage() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 1,
                        "limit_window_seconds": 604800,
                        "reset_at": 1783872000
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percentage, 1.0);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn codex_provider_defined_thirty_day_window_is_preserved_without_plan_assumptions() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 17,
                        "limit_window_seconds": 2592000,
                        "reset_at": 1786464000
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");

        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].percentage, 17.0);
        assert_eq!(
            usage.windows[0].duration_seconds,
            Some(30 * ONE_DAY_SECONDS)
        );
    }

    #[test]
    fn codex_windows_are_ordered_by_duration_instead_of_api_position() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 12,
                        "limit_window_seconds": 604800,
                        "reset_at": 1783872000
                    },
                    "secondary_window": {
                        "used_percent": 34,
                        "limit_window_seconds": 18000,
                        "reset_at": 1783353600
                    }
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| window.duration_seconds)
                .collect::<Vec<_>>(),
            vec![Some(FIVE_HOURS_SECONDS), Some(ONE_WEEK_SECONDS)]
        );
        assert_eq!(usage.windows[0].percentage, 34.0);
        assert_eq!(usage.windows[1].percentage, 12.0);
    }

    #[test]
    fn codex_unknown_durations_keep_distinct_fallback_labels() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 3,
                        "limit_window_seconds": 0,
                        "reset_at": 1783872000
                    },
                    "secondary_window": {
                        "used_percent": 4,
                        "reset_at": 1783958400
                    }
                }
            }"#,
        )
        .expect("Codex response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be present");
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| window.source_label.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Primary"), Some("Secondary")]
        );
    }

    #[test]
    fn antigravity_summary_prefers_gemini_group() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "groups": [
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "window": "weekly",
                                "resetTime": "2026-06-20T18:32:02Z",
                                "remainingFraction": 1
                            },
                            {
                                "bucketId": "3p-5h",
                                "window": "5h",
                                "resetTime": "2026-06-13T23:32:02Z",
                                "remainingFraction": 1
                            }
                        ]
                    },
                    {
                        "displayName": "Gemini Models",
                        "description": "Models within this group: Gemini Flash, Gemini Pro",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit",
                                "window": "weekly",
                                "resetTime": "2026-06-20T17:08:54Z",
                                "remainingFraction": 0.99304295
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit",
                                "window": "5h",
                                "resetTime": "2026-06-13T22:08:54Z",
                                "remainingFraction": 0.9582575
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("summary response should deserialize");

        let usage =
            antigravity_usage_from_summary(response).expect("Gemini quota should be selected");

        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert_eq!(usage.windows[1].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert!((usage.windows[0].percentage - 4.17425).abs() < 0.000001);
        assert!((usage.windows[1].percentage - 0.695705).abs() < 0.000001);
        assert!(usage
            .windows
            .iter()
            .all(|window| window.resets_at.is_some()));
    }

    #[test]
    fn antigravity_user_quota_accepts_only_plausible_five_hour_resets() {
        let response: AntigravityUserQuotaResponse = serde_json::from_str(
            r#"{
                "buckets": [
                    {
                        "modelId": "models/gemini-3.5-flash-high",
                        "remainingFraction": 0.8,
                        "resetTime": "2026-06-13T22:08:54Z"
                    },
                    {
                        "modelId": "claude-opus",
                        "remainingFraction": 0.25,
                        "resetTime": "2026-06-13T22:18:54Z"
                    },
                    {
                        "modelId": "tab-completion",
                        "remainingFraction": 0.0,
                        "resetTime": "2026-06-13T22:18:54Z"
                    },
                    {
                        "modelId": "gpt-disabled",
                        "remainingFraction": 0.0,
                        "resetTime": "2026-06-13T22:18:54Z",
                        "disabled": true
                    }
                ]
            }"#,
        )
        .expect("user quota response should deserialize");

        let now = parse_iso8601(Some("2026-06-13T20:00:00Z")).unwrap();
        let usage = antigravity_usage_from_user_quota_at(response, now)
            .expect("a supported per-model quota should be selected");

        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert!((usage.windows[0].percentage - 75.0).abs() < f64::EPSILON);
        assert!(usage.windows[0].resets_at.is_some());
    }

    #[test]
    fn antigravity_user_quota_rejects_weekly_reset_mislabeled_as_five_hour() {
        let response: AntigravityUserQuotaResponse = serde_json::from_str(
            r#"{
                "buckets": [
                    {
                        "modelId": "models/gemini-3.5-flash-high",
                        "remainingFraction": 0.77,
                        "resetTime": "2026-06-20T17:08:54Z"
                    }
                ]
            }"#,
        )
        .expect("user quota response should deserialize");
        let now = parse_iso8601(Some("2026-06-13T20:00:00Z")).unwrap();

        assert!(antigravity_usage_from_user_quota_at(response, now).is_none());
    }

    #[test]
    fn antigravity_sources_merge_five_hour_and_weekly_windows() {
        let per_model =
            UsageData::from_windows(vec![UsageWindow::new(35.0, None, Some(FIVE_HOURS_SECONDS))]);
        let summary =
            UsageData::from_windows(vec![UsageWindow::new(12.0, None, Some(ONE_WEEK_SECONDS))]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("both quota sources should merge");

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].duration_seconds, Some(FIVE_HOURS_SECONDS));
        assert_eq!(merged.windows[1].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert_eq!(merged.windows[0].percentage, 35.0);
        assert_eq!(merged.windows[1].percentage, 12.0);
    }

    #[test]
    fn antigravity_sources_keep_weekly_only_free_plan_without_inventing_five_hour() {
        let summary =
            UsageData::from_windows(vec![UsageWindow::new(7.0, None, Some(ONE_WEEK_SECONDS))]);

        let merged = merge_antigravity_usage_sources(None, Some(summary))
            .expect("weekly-only usage should remain usable");

        assert_eq!(merged.windows.len(), 1);
        assert_eq!(merged.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn antigravity_summary_five_hour_bucket_is_authoritative() {
        let per_model =
            UsageData::from_windows(vec![UsageWindow::new(40.0, None, Some(FIVE_HOURS_SECONDS))]);
        let summary = UsageData::from_windows(vec![
            UsageWindow::new(20.0, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(8.0, None, Some(ONE_WEEK_SECONDS)),
        ]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("summary windows should remain usable");

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].percentage, 20.0);
        assert_eq!(merged.windows[1].percentage, 8.0);
    }

    #[test]
    fn antigravity_sources_suppress_per_model_duplicate_of_weekly_summary() {
        let reset = Some(UNIX_EPOCH + Duration::from_secs(1_767_225_600));
        let per_model = UsageData::from_windows(vec![UsageWindow::new(
            23.0,
            reset,
            Some(FIVE_HOURS_SECONDS),
        )]);
        let summary =
            UsageData::from_windows(vec![UsageWindow::new(23.0, reset, Some(ONE_WEEK_SECONDS))]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("weekly summary should remain usable");

        assert_eq!(merged.windows.len(), 1);
        assert_eq!(merged.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
    }

    #[test]
    fn antigravity_sources_keep_equal_usage_with_distinct_reset_times() {
        let per_model = UsageData::from_windows(vec![UsageWindow::new(
            23.0,
            Some(UNIX_EPOCH + Duration::from_secs(1_767_225_600)),
            Some(FIVE_HOURS_SECONDS),
        )]);
        let summary = UsageData::from_windows(vec![UsageWindow::new(
            23.0,
            Some(UNIX_EPOCH + Duration::from_secs(1_767_232_800)),
            Some(ONE_WEEK_SECONDS),
        )]);

        let merged = merge_antigravity_usage_sources(Some(per_model), Some(summary))
            .expect("distinct quota windows should remain usable");

        assert_eq!(merged.windows.len(), 2);
    }

    #[test]
    fn antigravity_summary_accepts_nested_quota_summary_envelope() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "quotaSummary": {
                    "groups": [
                        {
                            "displayName": "Gemini Models",
                            "buckets": [
                                {
                                    "bucketId": "gemini-weekly",
                                    "displayName": "Weekly Quota",
                                    "remainingFraction": 0.6,
                                    "resetTime": "2026-06-20T17:08:54Z"
                                }
                            ]
                        }
                    ]
                }
            }"#,
        )
        .expect("nested summary response should deserialize");

        let usage = antigravity_usage_from_summary(response)
            .expect("nested weekly quota should be selected");

        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].duration_seconds, Some(ONE_WEEK_SECONDS));
        assert!((usage.windows[0].percentage - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn antigravity_summary_infers_five_hour_window_without_explicit_window_field() {
        let bucket = AntigravityQuotaSummaryBucket {
            bucket_id: Some("gemini-5h".to_string()),
            display_name: Some("Five Hour Quota".to_string()),
            window: None,
            remaining_fraction: Some(0.5),
            reset_time: None,
        };

        assert_eq!(
            antigravity_summary_bucket_duration_seconds(&bucket),
            Some(FIVE_HOURS_SECONDS)
        );
    }

    #[test]
    fn provider_window_labels_accept_known_and_numeric_durations() {
        assert_eq!(
            usage_window_duration_seconds(Some("daily")),
            Some(ONE_DAY_SECONDS)
        );
        assert_eq!(
            usage_window_duration_seconds(Some("12h")),
            Some(12 * 60 * 60)
        );
        assert_eq!(
            usage_window_duration_seconds(Some("2w")),
            Some(2 * ONE_WEEK_SECONDS)
        );
        assert_eq!(usage_window_duration_seconds(Some("rolling")), None);
    }

    #[test]
    fn iso8601_accepts_the_provider_formats() {
        // 2026-01-01T00:00:00Z is 1767225600 seconds after the epoch.
        let expected = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(parse_iso8601(Some("2026-01-01T00:00:00Z")), Some(expected));
        assert_eq!(
            parse_iso8601(Some("2026-01-01T00:00:00.321598+00:00")),
            Some(expected)
        );
        assert_eq!(
            parse_iso8601(Some("1970-01-01T00:00:00Z")),
            Some(UNIX_EPOCH)
        );
    }

    #[test]
    fn iso8601_converts_utc_offsets_instead_of_dropping_them() {
        let utc = parse_iso8601(Some("2026-03-05T06:00:00Z"));
        assert!(utc.is_some());
        assert_eq!(parse_iso8601(Some("2026-03-05T08:00:00+02:00")), utc);
        assert_eq!(parse_iso8601(Some("2026-03-05T01:00:00-05:00")), utc);
        assert_eq!(parse_iso8601(Some("2026-03-05T08:00:00+0200")), utc);
    }

    #[test]
    fn wsl_distro_cache_serves_within_ttl_and_expires_after() {
        // Enumerating WSL costs a process spawn - and a full 5s timeout when
        // WSL is absent or broken - so a fresh entry must be reused rather
        // than re-probed on every credential read.
        let now = Instant::now();
        let entry = WslDistroCache {
            fetched_at: now,
            distros: vec!["Ubuntu".to_string()],
        };
        assert!(wsl_cache_is_fresh(&entry, now));
        assert!(wsl_cache_is_fresh(
            &entry,
            now + WSL_DISTRO_CACHE_TTL - Duration::from_secs(1)
        ));
        assert!(!wsl_cache_is_fresh(&entry, now + WSL_DISTRO_CACHE_TTL));
        assert!(!wsl_cache_is_fresh(
            &entry,
            now + WSL_DISTRO_CACHE_TTL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn iso8601_rejects_malformed_input_without_panicking() {
        for input in [
            "2026-99-05T00:00:00Z",      // month out of range
            "2026-00-05T00:00:00Z",      // month zero
            "2026-03-00T00:00:00Z",      // day zero
            "2026-03-05T99:00:00Z",      // hour out of range
            "1969-12-31T23:59:59Z",      // before the epoch
            "2026-03-05T08:00:00+99:00", // bogus offset
            "not a timestamp",
            "",
        ] {
            assert_eq!(parse_iso8601(Some(input)), None, "input: {input}");
        }
        assert_eq!(parse_iso8601(None), None);
    }
}
