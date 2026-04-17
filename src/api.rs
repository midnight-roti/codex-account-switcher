use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::model::{AccountRecord, QuotaState, QuotaWindow, UsageData};
use crate::storage;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

#[derive(Deserialize)]
struct UsageResponse {
    #[serde(default)]
    plan_type: String,
    rate_limit: RateLimitStatus,
}

#[derive(Deserialize)]
struct RateLimitStatus {
    allowed: bool,
    limit_reached: bool,
    primary_window: Option<WindowSnapshot>,
    #[serde(rename = "secondary_window")]
    secondary_window: Option<WindowSnapshot>,
}

#[derive(Deserialize)]
struct WindowSnapshot {
    limit_window_seconds: i64,
    used_percent: f64,
    reset_at: i64,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

#[derive(serde::Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'a str,
}

pub fn fetch_quota(mut account: AccountRecord) -> Result<AccountRecord> {
    if account_expired(&account) {
        refresh_token(&mut account)?;
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut request = client
        .get(usage_url())
        .header("Authorization", format!("Bearer {}", account.access_token))
        .header("Accept", "application/json")
        .header("User-Agent", "cas-tui");

    if !account.account_id.trim().is_empty() {
        request = request.header("ChatGPT-Account-Id", account.account_id.trim());
    }

    let response = request.send().context("quota request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!(
            "quota request failed with {}: {}",
            status,
            truncate(&body, 240)
        );
    }

    let payload: UsageResponse = response.json().context("failed to decode quota payload")?;
    let mut windows = Vec::new();
    if let Some(primary) = payload.rate_limit.primary_window {
        windows.push(map_window(primary));
    }
    if let Some(secondary) = payload.rate_limit.secondary_window {
        windows.push(map_window(secondary));
    }
    if windows.is_empty() {
        bail!("quota response did not include usage windows");
    }

    account.quota = QuotaState::Ready(UsageData {
        plan_type: payload.plan_type,
        allowed: payload.rate_limit.allowed,
        limit_reached: payload.rate_limit.limit_reached,
        windows,
    });

    Ok(account)
}

fn map_window(snapshot: WindowSnapshot) -> QuotaWindow {
    let used = snapshot.used_percent.clamp(0.0, 100.0);
    let left = (100.0 - used).clamp(0.0, 100.0);
    QuotaWindow {
        window_sec: snapshot.limit_window_seconds,
        used_percent: used,
        left_percent: left,
        reset_at: chrono::DateTime::from_timestamp(snapshot.reset_at, 0),
    }
}

fn refresh_token(account: &mut AccountRecord) -> Result<()> {
    if account.refresh_token.trim().is_empty() {
        bail!("refresh token is missing");
    }

    let client_id = if !account.client_id.trim().is_empty() {
        account.client_id.trim().to_string()
    } else {
        storage::parse_access_token(&account.access_token).client_id
    };
    if client_id.trim().is_empty() {
        bail!("cannot refresh token without client_id");
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build refresh client")?;

    let response = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&RefreshRequest {
            grant_type: "refresh_token",
            refresh_token: account.refresh_token.trim(),
            client_id: client_id.trim(),
        })
        .send()
        .context("refresh request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("refresh failed with {}: {}", status, truncate(&body, 240));
    }

    let payload: RefreshResponse = response
        .json()
        .context("failed to decode refresh payload")?;
    if payload.access_token.trim().is_empty() {
        bail!("refresh response missing access_token");
    }

    account.access_token = payload.access_token;
    if !payload.refresh_token.trim().is_empty() {
        account.refresh_token = payload.refresh_token;
    }

    let claims = storage::parse_access_token(&account.access_token);
    if !claims.client_id.trim().is_empty() {
        account.client_id = claims.client_id;
    } else {
        account.client_id = client_id;
    }
    if !claims.account_id.trim().is_empty() {
        account.account_id = storage::canonical_account_id(&[
            account.account_id.as_str(),
            claims.account_id.as_str(),
        ]);
    }
    account.expires_at = if payload.expires_in > 0 {
        Some(Utc::now() + ChronoDuration::seconds(payload.expires_in))
    } else {
        claims.expires_at
    };

    storage::upsert_managed_account(account).context("failed to persist refreshed account")?;
    Ok(())
}

fn account_expired(account: &AccountRecord) -> bool {
    let expiry = if let Some(expiry) = account.expires_at {
        Some(expiry)
    } else {
        storage::parse_access_token(&account.access_token).expires_at
    };

    match expiry {
        Some(expiry) => Utc::now() >= expiry - ChronoDuration::minutes(5),
        None => false,
    }
}

fn usage_url() -> String {
    std::env::var("CQ_USAGE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| USAGE_URL.to_string())
}

fn truncate(value: &str, max_len: usize) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    format!("{}...", &trimmed[..max_len])
}
