use std::time::SystemTime;

use crate::tray_icon::TrayIconKind;

pub const FIVE_HOURS_SECONDS: u64 = 5 * 60 * 60;
pub const ONE_DAY_SECONDS: u64 = 24 * 60 * 60;
pub const ONE_WEEK_SECONDS: u64 = 7 * ONE_DAY_SECONDS;

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Length of the provider's rolling quota window when the API exposes it.
    pub duration_seconds: Option<u64>,
    /// Compact provider-supplied label for windows whose duration is unknown.
    pub source_label: Option<String>,
}

impl UsageWindow {
    pub fn new(
        percentage: f64,
        resets_at: Option<SystemTime>,
        duration_seconds: Option<u64>,
    ) -> Self {
        Self {
            percentage,
            resets_at,
            duration_seconds,
            source_label: None,
        }
    }

    pub fn with_source_label(mut self, label: Option<String>) -> Self {
        self.source_label = label.filter(|label| !label.trim().is_empty());
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    /// Provider quota windows ordered from shortest to longest. Windows whose
    /// duration is unknown retain provider order after all known durations.
    pub windows: Vec<UsageWindow>,
}

impl UsageData {
    pub fn from_windows(mut windows: Vec<UsageWindow>) -> Self {
        for window in &mut windows {
            window.duration_seconds = window.duration_seconds.filter(|seconds| *seconds > 0);
        }
        windows.retain(|window| window.percentage.is_finite());
        windows.sort_by(
            |left, right| match (left.duration_seconds, right.duration_seconds) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            },
        );
        Self { windows }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

/// Why a single provider's poll failed while others may have succeeded.
/// Coarser than poller::PollError on purpose: this is display granularity
/// for the detail popup's per-provider status badges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderStatus {
    /// No credential for this provider exists on this machine: it was never
    /// signed in, or the login was removed. Kept separate from
    /// `AuthenticationFailed` because there is nothing to re-authenticate -
    /// telling the user to "sign in again" would be wrong for a provider they
    /// never signed in to.
    NotSignedIn,
    /// A credential exists but is unusable, expired, revoked, or was rejected
    /// by the provider.
    AuthenticationFailed,
    RateLimited,
    NetworkUnavailable,
    RequestFailed,
}

impl ProviderStatus {
    /// Both credential states park the provider until the user supplies a
    /// login, so they share the poll pause and the credential watch.
    pub fn needs_credentials(self) -> bool {
        matches!(self, Self::NotSignedIn | Self::AuthenticationFailed)
    }

    /// Only a broken existing login is worth interrupting the user about. A
    /// provider that was never signed in has nothing to recover, so it stays
    /// on the compact surfaces without raising a balloon.
    pub fn warrants_credential_alert(self) -> bool {
        self == Self::AuthenticationFailed
    }
}

/// One provider's slice of a poll pass: its usage, when that usage was
/// observed, and why it is missing when it is.
#[derive(Clone, Debug, Default)]
pub struct ProviderSlot {
    pub usage: Option<UsageData>,
    pub updated_unix: Option<u64>,
    pub error: Option<ProviderStatus>,
    pub retry_after_ms: Option<u32>,
}

/// The result of one poll pass, indexed by provider.
///
/// Indexed rather than a field per provider so that adding a provider does not
/// mean threading four more fields through every call site; the slots are
/// private precisely so nothing can index them by anything but a
/// [`TrayIconKind`].
#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    slots: [ProviderSlot; TrayIconKind::COUNT],
    /// At least one provider returned a remote 401/403 in this poll pass.
    pub remote_auth_rejection: bool,
}

impl AppUsageData {
    pub fn provider(&self, kind: TrayIconKind) -> &ProviderSlot {
        &self.slots[kind.index()]
    }

    pub fn provider_mut(&mut self, kind: TrayIconKind) -> &mut ProviderSlot {
        &mut self.slots[kind.index()]
    }

    pub fn usage(&self, kind: TrayIconKind) -> Option<&UsageData> {
        self.provider(kind).usage.as_ref()
    }

    pub fn error(&self, kind: TrayIconKind) -> Option<ProviderStatus> {
        self.provider(kind).error
    }

    /// Whether any provider produced usage at all. A pass with none of it is
    /// the whole-pass failure case.
    pub fn has_any_usage(&self) -> bool {
        self.slots.iter().any(|slot| slot.usage.is_some())
    }
}

/// Test-only builders. Production code writes through [`AppUsageData::provider_mut`];
/// tests read better as a chain than as four statements per provider.
#[cfg(test)]
impl AppUsageData {
    pub fn with_usage(mut self, kind: TrayIconKind, usage: UsageData) -> Self {
        self.provider_mut(kind).usage = Some(usage);
        self
    }

    pub fn with_updated_unix(mut self, kind: TrayIconKind, updated_unix: u64) -> Self {
        self.provider_mut(kind).updated_unix = Some(updated_unix);
        self
    }

    pub fn with_error(mut self, kind: TrayIconKind, error: ProviderStatus) -> Self {
        self.provider_mut(kind).error = Some(error);
        self
    }

    pub fn with_retry_after_ms(mut self, kind: TrayIconKind, retry_after_ms: u32) -> Self {
        self.provider_mut(kind).retry_after_ms = Some(retry_after_ms);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_windows_sort_known_durations_and_reject_invalid_values() {
        let data = UsageData::from_windows(vec![
            UsageWindow::new(3.0, None, Some(ONE_WEEK_SECONDS)),
            UsageWindow::new(f64::NAN, None, Some(FIVE_HOURS_SECONDS)),
            UsageWindow::new(1.0, None, None),
            UsageWindow::new(2.0, None, Some(FIVE_HOURS_SECONDS)),
        ]);

        assert_eq!(data.windows.len(), 3);
        assert_eq!(
            data.windows
                .iter()
                .map(|window| window.duration_seconds)
                .collect::<Vec<_>>(),
            vec![Some(FIVE_HOURS_SECONDS), Some(ONE_WEEK_SECONDS), None]
        );
    }

    #[test]
    fn zero_duration_is_treated_as_unknown() {
        let data = UsageData::from_windows(vec![UsageWindow::new(1.0, None, Some(0))]);
        assert_eq!(data.windows[0].duration_seconds, None);
    }
}
