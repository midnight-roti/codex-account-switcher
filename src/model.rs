use chrono::{DateTime, Utc};

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
    pub opencode_active: bool,
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
                let hourly = data.window_by_seconds(18_000);
                let weekly = data.window_by_seconds(604_800);
                hourly.map(|w| w.left_percent <= 0.0).unwrap_or(false)
                    || weekly.map(|w| w.left_percent <= 0.0).unwrap_or(false)
            }
            _ => false,
        }
    }

    pub fn sort_tuple(&self) -> (i32, i32, i64, String) {
        match &self.quota {
            QuotaState::Ready(data) => {
                let hourly = data
                    .window_by_seconds(18_000)
                    .map(|w| (w.left_percent * 100.0) as i64)
                    .unwrap_or(-1);
                let weekly = data
                    .window_by_seconds(604_800)
                    .map(|w| (w.left_percent * 100.0) as i64)
                    .unwrap_or(-1);
                let exhausted_rank = if self.is_exhausted() { 1 } else { 0 };
                (
                    exhausted_rank,
                    0,
                    -(hourly + weekly),
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
}

#[derive(Clone, Debug)]
pub struct QuotaWindow {
    pub window_sec: i64,
    pub used_percent: f64,
    pub left_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
}
