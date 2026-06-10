use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;

#[derive(Clone, Debug, Default)]
struct TokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Debug)]
struct SessionUsage {
    modified_at: SystemTime,
    started_at: Option<DateTime<Utc>>,
    id: String,
    cwd: String,
    model: String,
    token_events: usize,
    total: Option<TokenUsage>,
}

pub fn run(args: &[String]) -> Result<()> {
    let options = UsageOptions::parse(args)?;
    let root = options
        .sessions_dir
        .clone()
        .unwrap_or_else(default_sessions_dir);

    let mut sessions = load_session_usage(&root)
        .with_context(|| format!("failed to read Codex sessions from {}", root.display()))?;
    sessions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));

    if !options.all {
        sessions.retain(|session| session.total.is_some());
    }
    sessions.truncate(options.limit);

    print_usage_table(&sessions);
    Ok(())
}

#[derive(Debug)]
struct UsageOptions {
    limit: usize,
    all: bool,
    sessions_dir: Option<PathBuf>,
}

impl UsageOptions {
    fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self {
            limit: 20,
            all: false,
            sessions_dir: None,
        };

        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--all" => options.all = true,
                "--limit" | "-n" => {
                    index += 1;
                    let Some(value) = args.get(index) else {
                        return Err(anyhow!("{} requires a number", args[index - 1]));
                    };
                    options.limit = value
                        .parse::<usize>()
                        .with_context(|| format!("invalid limit: {}", value))?;
                }
                "--sessions-dir" => {
                    index += 1;
                    let Some(value) = args.get(index) else {
                        return Err(anyhow!("--sessions-dir requires a path"));
                    };
                    options.sessions_dir = Some(PathBuf::from(value));
                }
                "--help" | "-h" => {
                    print_usage_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown usage option: {}", other)),
            }
            index += 1;
        }

        Ok(options)
    }
}

fn load_session_usage(root: &Path) -> Result<Vec<SessionUsage>> {
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files)?;

    files
        .into_iter()
        .map(|path| parse_session_file(&path))
        .collect()
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl_files(&path, files)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_session_file(path: &Path) -> Result<SessionUsage> {
    let metadata = fs::metadata(path)?;
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut session = SessionUsage {
        modified_at: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        started_at: None,
        id: session_id_from_path(path),
        cwd: String::new(),
        model: String::new(),
        token_events: 0,
        total: None,
    };

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        update_session_metadata(&mut session, &value);
        if let Some(total) = token_count_total(&value) {
            session.token_events += 1;
            session.total = Some(total);
        }
    }

    Ok(session)
}

fn update_session_metadata(session: &mut SessionUsage, value: &Value) {
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        let payload = &value["payload"];
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            session.id = id.to_string();
        }
        if let Some(timestamp) = payload.get("timestamp").and_then(Value::as_str) {
            session.started_at = parse_timestamp(timestamp);
        }
        if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
            session.cwd = cwd.to_string();
        }
    }

    if value.get("type").and_then(Value::as_str) == Some("turn_context") {
        let payload = &value["payload"];
        if session.cwd.is_empty() {
            if let Some(cwd) = payload.get("cwd").and_then(Value::as_str) {
                session.cwd = cwd.to_string();
            }
        }
        if let Some(model) = payload.get("model").and_then(Value::as_str) {
            session.model = model.to_string();
        }
    }
}

fn token_count_total(value: &Value) -> Option<TokenUsage> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    if value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let usage = value.pointer("/payload/info/total_token_usage")?;
    Some(TokenUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_output_tokens: usage
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn print_usage_table(sessions: &[SessionUsage]) {
    println!(
        "{:<16} {:<10} {:>12} {:>12} {:>12} {:>12} {:>12}  Session",
        "Started", "Model", "Input", "Cached", "Output", "Reasoning", "Total"
    );
    println!("{}", "-".repeat(118));

    for session in sessions {
        let started = session
            .started_at
            .map(format_datetime)
            .unwrap_or_else(|| "-".to_string());
        let model = empty_dash(&session.model);
        let name = if session.cwd.trim().is_empty() {
            session.id.as_str()
        } else {
            session.cwd.as_str()
        };

        if let Some(total) = &session.total {
            println!(
                "{:<16} {:<10} {:>12} {:>12} {:>12} {:>12} {:>12}  {}",
                started,
                truncate(&model, 10),
                format_count(total.input_tokens),
                format_count(total.cached_input_tokens),
                format_count(total.output_tokens),
                format_count(total.reasoning_output_tokens),
                format_count(total.total_tokens),
                truncate(name, 42)
            );
        } else {
            println!(
                "{:<16} {:<10} {:>12} {:>12} {:>12} {:>12} {:>12}  {}",
                started,
                truncate(&model, 10),
                "-",
                "-",
                "-",
                "-",
                "-",
                truncate(name, 42)
            );
        }
    }
}

fn print_usage_help() {
    println!("Usage: cas usage [--limit N] [--all] [--sessions-dir PATH]");
    println!();
    println!("Reads local Codex JSONL session logs and prints the latest cumulative token count per chat.");
}

fn default_sessions_dir() -> PathBuf {
    home_dir().join(".codex").join("sessions")
}

fn home_dir() -> PathBuf {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unknown-session")
        .trim_start_matches("rollout-")
        .to_string()
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn format_datetime(value: DateTime<Utc>) -> String {
    value
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn format_count(value: u64) -> String {
    let raw = value.to_string();
    let mut formatted = String::new();
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted.chars().rev().collect()
}

fn empty_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "-".to_string()
    } else {
        value.trim().to_string()
    }
}

fn truncate(value: &str, max_len: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_len {
        return trimmed.to_string();
    }
    let keep = max_len.saturating_sub(3);
    format!("{}...", trimmed.chars().take(keep).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_total_token_usage_from_token_count_event() {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 4,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 2,
                        "total_tokens": 13
                    }
                }
            }
        });

        let usage = token_count_total(&value).expect("token usage");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cached_input_tokens, 4);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.reasoning_output_tokens, 2);
        assert_eq!(usage.total_tokens, 13);
    }

    #[test]
    fn ignores_non_token_count_events() {
        let value = serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "something_else" }
        });

        assert!(token_count_total(&value).is_none());
    }

    #[test]
    fn formats_counts_with_grouping() {
        assert_eq!(format_count(6_600_483), "6,600,483");
    }
}
