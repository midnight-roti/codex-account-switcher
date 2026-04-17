use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::{distributions::Alphanumeric, Rng};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::model::AccountRecord;
use crate::storage;

const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_SCOPE: &str = "openid profile email offline_access";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_ADDRESS: &str = "127.0.0.1:1455";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Deserialize, Default)]
struct MeResponse {
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
}

pub fn login_account() -> Result<AccountRecord> {
    let verifier = random_string(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(32);

    let auth_url = build_authorize_url(&state, &challenge)?;
    webbrowser::open(auth_url.as_str()).context("failed to open browser")?;

    let code = wait_for_callback(&state)?;
    let token = exchange_code(&code, &verifier)?;

    let claims = storage::parse_access_token(&token.access_token);
    let me = fetch_me(&token.access_token).unwrap_or_default();

    let account_id = storage::canonical_account_id(&[claims.account_id.as_str()]);
    if account_id.trim().is_empty() {
        bail!("failed to extract account_id from token");
    }

    Ok(AccountRecord {
        label: if !me.email.trim().is_empty() {
            me.email.trim().to_string()
        } else if !me.name.trim().is_empty() {
            me.name.trim().to_string()
        } else if !claims.email.trim().is_empty() {
            claims.email.clone()
        } else {
            account_id.clone()
        },
        email: if !me.email.trim().is_empty() {
            me.email.trim().to_string()
        } else {
            claims.email
        },
        account_id,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: if token.expires_in > 0 {
            Some(chrono::Utc::now() + chrono::Duration::seconds(token.expires_in))
        } else {
            claims.expires_at
        },
        client_id: if !claims.client_id.trim().is_empty() {
            claims.client_id
        } else {
            OAUTH_CLIENT_ID.to_string()
        },
        managed: true,
        codex_active: false,
        opencode_active: false,
        quota: crate::model::QuotaState::Idle,
    })
}

fn build_authorize_url(state: &str, challenge: &str) -> Result<Url> {
    let mut url = Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", OAUTH_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "codex-account-switcher");
    Ok(url)
}

fn wait_for_callback(expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(CALLBACK_ADDRESS)
        .with_context(|| format!("failed to bind callback server on {}", CALLBACK_ADDRESS))?;
    listener
        .set_nonblocking(true)
        .context("failed to set callback socket nonblocking")?;

    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0_u8; 4096];
                let bytes = stream
                    .read(&mut buffer)
                    .context("failed to read callback")?;
                let request = String::from_utf8_lossy(&buffer[..bytes]);
                let first_line = request
                    .lines()
                    .next()
                    .ok_or_else(|| anyhow!("callback request was empty"))?;
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .ok_or_else(|| anyhow!("callback request line was malformed"))?;
                let url = Url::parse(&format!("http://localhost{}", path))
                    .context("failed to parse callback URL")?;
                let state = url
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_default();
                let code = url
                    .query_pairs()
                    .find(|(key, _)| key == "code")
                    .map(|(_, value)| value.to_string())
                    .unwrap_or_default();

                if state != expected_state {
                    respond(
                        &mut stream,
                        400,
                        "State mismatch. You can close this window.",
                    )?;
                    bail!("oauth callback state mismatch");
                }
                if code.trim().is_empty() {
                    respond(
                        &mut stream,
                        400,
                        "Missing authorization code. You can close this window.",
                    )?;
                    bail!("oauth callback missing code");
                }

                respond(
                    &mut stream,
                    200,
                    "Authentication successful. You can close this window.",
                )?;
                return Ok(code);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("oauth login timed out");
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("failed to accept oauth callback"),
        }
    }
}

fn respond(stream: &mut std::net::TcpStream, status: u16, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn exchange_code(code: &str, verifier: &str) -> Result<TokenResponse> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build oauth client")?;

    let response = client
        .post(OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OAUTH_CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ])
        .send()
        .context("token exchange failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!(
            "oauth token exchange failed with {}: {}",
            status,
            truncate(&body, 240)
        );
    }

    let token: TokenResponse = response.json().context("failed to decode token payload")?;
    if token.access_token.trim().is_empty() || token.refresh_token.trim().is_empty() {
        bail!("oauth token response was incomplete");
    }
    Ok(token)
}

fn fetch_me(access_token: &str) -> Result<MeResponse> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build profile client")?;
    let response = client
        .get("https://api.openai.com/v1/me")
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Accept", "application/json")
        .send()
        .context("profile request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!(
            "profile request failed with {}: {}",
            status,
            truncate(&body, 200)
        );
    }

    response.json().context("failed to decode profile payload")
}

fn random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn truncate(value: &str, max_len: usize) -> String {
    let trimmed = value.trim();
    if trimmed.len() <= max_len {
        return trimmed.to_string();
    }
    format!("{}...", &trimmed[..max_len])
}
