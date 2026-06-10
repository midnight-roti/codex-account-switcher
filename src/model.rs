use chrono::{DateTime, Utc};

pub const FIVE_HOUR_WINDOW_SECONDS: i64 = 18_000;
pub const WEEK_WINDOW_SECONDS: i64 = 604_800;

const MIN_MONTH_WINDOW_SECONDS: i64 = 28 * 24 * 60 * 60;
const MAX_MONTH_WINDOW_SECONDS: i64 = 31 * 24 * 60 * 60;

#[derive(Clone, Debug, Default)]
pub struct AccountRecord {
    pub label: String,
    pub email: String,
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub client_id: String,
    pub managed: bool,
    pub codex_active: bool,
    pub quota: QuotaState,
}

impl AccountRecord {
    pub fn key(&self) -> String {
        if !self.account_id.trim().is_empty() {
            return self.account_id.trim().to_string();
        }
        self.email.trim().to_lowercase()
    }

    pub fn display_name(&self) -> String {
        let label = self.label.trim();
        if !label.is_empty() {
            return label.to_string();
        }
        let email = self.email.trim();
        if !email.is_empty() {
            return email.to_string();
        }
        if !self.account_id.trim().is_empty() {
            return self.account_id.trim().to_string();
        }
        "unknown-account".to_string()
    }

    pub fn plan_type(&self) -> &str {
        match &self.quota {
            QuotaState::Ready(data) if !data.plan_type.trim().is_empty() => data.plan_type.trim(),
            _ => "Unknown",
        }
    }

    pub fn is_exhausted(&self) -> bool {
        match &self.quota {
            QuotaState::Ready(data) => {
                let five_hour = data.window_by_seconds(FIVE_HOUR_WINDOW_SECONDS);
                let long_period = data.long_window();
                five_hour.map(|w| w.left_percent <= 0.0).unwrap_or(false)
                    || long_period.map(|w| w.left_percent <= 0.0).unwrap_or(false)
            }
            _ => false,
        }
    }

    pub fn sort_tuple(&self) -> (i32, i32, i64, String) {
        match &self.quota {
            QuotaState::Ready(data) => {
                let five_hour = data
                    .window_by_seconds(FIVE_HOUR_WINDOW_SECONDS)
                    .map(|w| (w.left_percent * 100.0) as i64)
                    .unwrap_or(-1);
                let long_period = data
                    .long_window()
                    .map(|w| (w.left_percent * 100.0) as i64)
                    .unwrap_or(-1);
                let exhausted_rank = if self.is_exhausted() { 1 } else { 0 };
                (
                    exhausted_rank,
                    0,
                    -(five_hour + long_period),
                    self.display_name().to_lowercase(),
                )
            }
            QuotaState::Loading => (
                if self.is_exhausted() { 1 } else { 0 },
                1,
                i64::MAX / 4,
                self.display_name().to_lowercase(),
            ),
            _ => (
                if self.is_exhausted() { 1 } else { 0 },
                2,
                i64::MAX / 2,
                self.display_name().to_lowercase(),
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub enum QuotaState {
    Idle,
    Loading,
    Ready(UsageData),
    Error(String),
}

impl Default for QuotaState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub plan_type: String,
    pub allowed: bool,
    pub limit_reached: bool,
    pub windows: Vec<QuotaWindow>,
}

impl UsageData {
    pub fn window_by_seconds(&self, window_sec: i64) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|window| window.window_sec == window_sec)
    }

    pub fn long_window(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .filter(|window| window.window_sec != FIVE_HOUR_WINDOW_SECONDS)
            .max_by_key(|window| window.window_sec)
    }

    pub fn long_window_label(&self) -> &'static str {
        self.long_window()
            .map(QuotaWindow::label)
            .unwrap_or("Period")
    }
}

#[derive(Clone, Debug)]
pub struct QuotaWindow {
    pub window_sec: i64,
    pub used_percent: f64,
    pub left_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
}

impl QuotaWindow {
    pub fn label(&self) -> &'static str {
        match self.window_sec {
            FIVE_HOUR_WINDOW_SECONDS => "5h",
            WEEK_WINDOW_SECONDS => "Week",
            MIN_MONTH_WINDOW_SECONDS..=MAX_MONTH_WINDOW_SECONDS => "Month",
            _ => "Period",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONTH_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;

    fn usage(windows: &[(i64, f64)]) -> UsageData {
        UsageData {
            windows: windows
                .iter()
                .map(|(window_sec, left_percent)| QuotaWindow {
                    window_sec: *window_sec,
                    used_percent: 100.0 - left_percent,
                    left_percent: *left_percent,
                    reset_at: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn long_window_uses_weekly_window_for_paid_plans() {
        let data = usage(&[
            (FIVE_HOUR_WINDOW_SECONDS, 40.0),
            (WEEK_WINDOW_SECONDS, 70.0),
        ]);

        let window = data.long_window().expect("weekly window");
        assert_eq!(window.window_sec, WEEK_WINDOW_SECONDS);
        assert_eq!(data.long_window_label(), "Week");
    }

    #[test]
    fn long_window_uses_monthly_window_when_weekly_is_absent() {
        let data = usage(&[
            (FIVE_HOUR_WINDOW_SECONDS, 40.0),
            (MONTH_WINDOW_SECONDS, 70.0),
        ]);

        let window = data.long_window().expect("monthly window");
        assert_eq!(window.window_sec, MONTH_WINDOW_SECONDS);
        assert_eq!(data.long_window_label(), "Month");
    }

    #[test]
    fn exhaustion_uses_monthly_window_for_free_plans() {
        let account = AccountRecord {
            quota: QuotaState::Ready(usage(&[
                (FIVE_HOUR_WINDOW_SECONDS, 40.0),
                (MONTH_WINDOW_SECONDS, 0.0),
            ])),
            ..Default::default()
        };

        assert!(account.is_exhausted());
    }
}
