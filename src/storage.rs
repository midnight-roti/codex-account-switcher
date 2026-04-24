use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::model::AccountRecord;

const APP_CONFIG_DIR_NAME: &str = "codex-account-switcher";
const LEGACY_CONFIG_DIR_NAME: &str = "codex-quota";

#[derive(Clone, Debug, Default)]
pub struct AccessTokenClaims {
    pub client_id: String,
    pub account_id: String,
    pub email: String,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Default, Deserialize, Serialize)]
struct ManagedStore {
    #[serde(default)]
    accounts: Vec<ManagedAccount>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ManagedAccount {
    #[serde(default)]
    label: String,
    #[serde(default)]
    email: String,
    account_id: String,
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at_ms: i64,
    #[serde(default)]
    client_id: String,
}

pub fn load_accounts() -> Result<Vec<AccountRecord>> {
    let mut managed = load_managed_accounts()?;
    let codex = load_codex_account()?;

    let mut changed = false;
    for external in [codex.clone()].into_iter().flatten() {
        if !external.access_token.trim().is_empty() && !external.account_id.trim().is_empty() {
            upsert_managed_account(&external)?;
            changed = true;
        }
    }
    if changed {
        managed = load_managed_accounts()?;
    }

    for account in &mut managed {
        account.codex_active = codex
            .as_ref()
            .map(|current| same_identity(account, current))
            .unwrap_or(false);
    }

    Ok(managed)
}

pub fn upsert_managed_account(account: &AccountRecord) -> Result<()> {
    if account.access_token.trim().is_empty() {
        bail!("access token is empty");
    }

    let mut store = load_managed_store()?;
    let claims = parse_access_token(&account.access_token);
    let item = ManagedAccount {
        label: account.label.trim().to_string(),
        email: if !account.email.trim().is_empty() {
            account.email.trim().to_string()
        } else {
            claims.email
        },
        account_id: canonical_account_id(&[
            account.account_id.as_str(),
            claims.account_id.as_str(),
        ]),
        access_token: account.access_token.trim().to_string(),
        refresh_token: account.refresh_token.trim().to_string(),
        expires_at_ms: account
            .expires_at
            .or(claims.expires_at)
            .map(|expiry| expiry.timestamp_millis())
            .unwrap_or_default(),
        client_id: if !account.client_id.trim().is_empty() {
            account.client_id.trim().to_string()
        } else {
            claims.client_id
        },
    };

    if item.account_id.trim().is_empty() {
        bail!("account_id is missing");
    }

    match store
        .accounts
        .iter_mut()
        .find(|existing| canonical_account_id(&[existing.account_id.as_str()]) == item.account_id)
    {
        Some(existing) => merge_managed_account(existing, &item),
        None => store.accounts.push(item),
    }

    save_managed_store(&store)
}

pub fn delete_account(account: &AccountRecord) -> Result<()> {
    let mut store = load_managed_store()?;
    let before = store.accounts.len();
    store.accounts.retain(|item| {
        let record = managed_to_record(item.clone());
        !same_identity(account, &record)
    });
    if store.accounts.len() != before {
        save_managed_store(&store)?;
    }

    if account.codex_active {
        delete_codex_auth_account()?;
    }
    Ok(())
}

pub fn apply_account_to_codex(account: &AccountRecord) -> Result<PathBuf> {
    let path = codex_auth_path();
    let mut root = read_json_map_or_default(&path)?;
    let tokens = object_mut(&mut root, "tokens");
    tokens.insert(
        "access_token".to_string(),
        Value::String(account.access_token.clone()),
    );
    tokens.insert(
        "id_token".to_string(),
        Value::String(account.access_token.clone()),
    );
    if !account.refresh_token.trim().is_empty() {
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(account.refresh_token.clone()),
        );
    }
    if !account.account_id.trim().is_empty() {
        tokens.insert(
            "account_id".to_string(),
            Value::String(account.account_id.clone()),
        );
    }
    root.insert(
        "last_refresh".to_string(),
        Value::String(Utc::now().to_rfc3339()),
    );
    write_json_map(&path, &root)?;
    Ok(path)
}

pub fn parse_access_token(token: &str) -> AccessTokenClaims {
    let token = token.trim();
    if token.is_empty() {
        return AccessTokenClaims::default();
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return AccessTokenClaims::default();
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]);
    let Ok(payload) = decoded else {
        return AccessTokenClaims::default();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
        return AccessTokenClaims::default();
    };

    let client_id = first_non_empty(&[
        value
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        value.get("cid").and_then(Value::as_str).unwrap_or_default(),
        value
            .get("clientId")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ]);

    let auth_account_id = match value.get("https://api.openai.com/auth") {
        Some(Value::String(raw)) => raw.trim().to_string(),
        Some(Value::Object(map)) => map
            .get("chatgpt_account_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => String::new(),
    };
    let account_id = canonical_account_id(&[
        auth_account_id.as_str(),
        value
            .get("account_id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        value.get("sub").and_then(Value::as_str).unwrap_or_default(),
    ]);

    let expires_at = value
        .get("exp")
        .and_then(Value::as_i64)
        .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single());

    AccessTokenClaims {
        client_id,
        account_id,
        email: value
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        expires_at,
    }
}

pub fn canonical_account_id(ids: &[&str]) -> String {
    let mut trimmed = ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .collect::<Vec<_>>();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(uuid_like) = trimmed.iter().find(|id| is_uuid_like(id)) {
        return (*uuid_like).to_string();
    }
    trimmed.remove(0).to_string()
}

fn load_managed_accounts() -> Result<Vec<AccountRecord>> {
    let store = load_managed_store()?;
    let mut accounts = Vec::new();
    for item in store.accounts {
        if item.access_token.trim().is_empty() {
            continue;
        }
        accounts.push(managed_to_record(item));
    }
    accounts.sort_by_key(|account| account.display_name().to_lowercase());
    Ok(accounts)
}

fn managed_to_record(item: ManagedAccount) -> AccountRecord {
    let claims = parse_access_token(&item.access_token);
    AccountRecord {
        label: first_non_empty(&[item.label.as_str(), item.email.as_str()]),
        email: first_non_empty(&[item.email.as_str(), claims.email.as_str()]),
        account_id: canonical_account_id(&[item.account_id.as_str(), claims.account_id.as_str()]),
        access_token: item.access_token,
        refresh_token: item.refresh_token,
        expires_at: if item.expires_at_ms > 0 {
            chrono::DateTime::from_timestamp_millis(item.expires_at_ms)
        } else {
            claims.expires_at
        },
        client_id: first_non_empty(&[item.client_id.as_str(), claims.client_id.as_str()]),
        managed: true,
        codex_active: false,
        quota: crate::model::QuotaState::Idle,
    }
}

fn merge_managed_account(existing: &mut ManagedAccount, incoming: &ManagedAccount) {
    if existing.label.trim().is_empty() {
        existing.label = incoming.label.clone();
    }
    if existing.email.trim().is_empty() {
        existing.email = incoming.email.clone();
    }
    if existing.client_id.trim().is_empty() {
        existing.client_id = incoming.client_id.clone();
    }
    if existing.refresh_token.trim().is_empty() {
        existing.refresh_token = incoming.refresh_token.clone();
    }

    if incoming.expires_at_ms > existing.expires_at_ms {
        existing.access_token = incoming.access_token.clone();
        existing.expires_at_ms = incoming.expires_at_ms;
        if !incoming.refresh_token.trim().is_empty() {
            existing.refresh_token = incoming.refresh_token.clone();
        }
        if !incoming.client_id.trim().is_empty() {
            existing.client_id = incoming.client_id.clone();
        }
    }

    if existing.access_token.trim().is_empty() {
        existing.access_token = incoming.access_token.clone();
        existing.expires_at_ms = incoming.expires_at_ms;
    }
}

fn load_managed_store() -> Result<ManagedStore> {
    let path = managed_accounts_path()?;
    if !path.exists() {
        return Ok(ManagedStore::default());
    }
    let data =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str::<ManagedStore>(&data)
        .with_context(|| format!("failed to decode {}", path.display()))
}

fn save_managed_store(store: &ManagedStore) -> Result<()> {
    let path = managed_accounts_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(store).context("failed to serialize accounts")?;
    fs::write(&path, data).with_context(|| format!("failed to write {}", path.display()))
}

fn load_codex_account() -> Result<Option<AccountRecord>> {
    let path = codex_auth_path();
    if !path.exists() {
        return Ok(None);
    }
    let root = read_json_map(&path)?;
    let tokens = root
        .get("tokens")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if access_token.is_empty() {
        return Ok(None);
    }
    let claims = parse_access_token(&access_token);
    Ok(Some(AccountRecord {
        label: claims.email.clone(),
        email: claims.email.clone(),
        account_id: canonical_account_id(&[
            tokens
                .get("account_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            claims.account_id.as_str(),
        ]),
        access_token,
        refresh_token: tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        expires_at: claims.expires_at,
        client_id: claims.client_id,
        managed: false,
        codex_active: true,
        quota: crate::model::QuotaState::Idle,
    }))
}

fn delete_codex_auth_account() -> Result<()> {
    let path = codex_auth_path();
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json_map(&path)?;
    let tokens = object_mut(&mut root, "tokens");
    tokens.remove("access_token");
    tokens.remove("refresh_token");
    tokens.remove("account_id");
    write_json_map(&path, &root)
}

fn same_identity(left: &AccountRecord, right: &AccountRecord) -> bool {
    let left_id = canonical_account_id(&[left.account_id.as_str()]);
    let right_id = canonical_account_id(&[right.account_id.as_str()]);
    if !left_id.is_empty() && left_id == right_id {
        return true;
    }
    !left.email.trim().is_empty() && left.email.trim().eq_ignore_ascii_case(right.email.trim())
}

fn app_config_dir() -> Result<PathBuf> {
    let base = std::env::var("CAS_CONFIG_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| BaseDirs::new().map(|dirs| dirs.config_dir().to_path_buf()))
        .ok_or_else(|| anyhow!("failed to locate user config directory"))?;

    let target = base.join(APP_CONFIG_DIR_NAME);
    migrate_legacy_config_dir(&base, &target)?;
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;
    Ok(target)
}

fn migrate_legacy_config_dir(base: &Path, target: &Path) -> Result<()> {
    if target.exists() && fs::read_dir(target)?.next().is_some() {
        return Ok(());
    }

    let legacy_base = std::env::var("CQ_CONFIG_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| base.to_path_buf());
    let legacy = legacy_base.join(LEGACY_CONFIG_DIR_NAME);
    if !legacy.exists() {
        return Ok(());
    }

    fs::create_dir_all(target)?;
    for name in [
        "accounts.json",
        "settings.json",
        "ui_state.json",
        "update_state.json",
    ] {
        let src = legacy.join(name);
        let dst = target.join(name);
        if !src.exists() || dst.exists() {
            continue;
        }
        fs::copy(&src, &dst).with_context(|| format!("failed to migrate {}", src.display()))?;
    }
    Ok(())
}

fn managed_accounts_path() -> Result<PathBuf> {
    Ok(app_config_dir()?.join("accounts.json"))
}

fn codex_auth_path() -> PathBuf {
    if let Ok(path) = std::env::var("CODEX_AUTH_PATH") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir).join("auth.json");
        }
    }
    home_dir().join(".codex").join("auth.json")
}

fn read_json_map(path: &Path) -> Result<Map<String, Value>> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&data)
        .with_context(|| format!("failed to decode {}", path.display()))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))
}

fn read_json_map_or_default(path: &Path) -> Result<Map<String, Value>> {
    if path.exists() {
        read_json_map(path)
    } else {
        Ok(Map::new())
    }
}

fn write_json_map(path: &Path, root: &Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(&json!(root)).context("failed to serialize JSON")?;
    fs::write(path, data).with_context(|| format!("failed to write {}", path.display()))
}

fn object_mut<'a>(root: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !matches!(root.get(key), Some(Value::Object(_))) {
        root.insert(key.to_string(), Value::Object(Map::new()));
    }
    root.get_mut(key).and_then(Value::as_object_mut).unwrap()
}

fn home_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn first_non_empty(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

fn is_uuid_like(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    chars.len() == 36
        && chars.iter().enumerate().all(|(idx, ch)| match idx {
            8 | 13 | 18 | 23 => *ch == '-',
            _ => ch.is_ascii_hexdigit(),
        })
}
