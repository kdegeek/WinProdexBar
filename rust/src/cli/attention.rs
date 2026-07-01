//! Manage the square-display attention takeover file.

use anyhow::Context;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Args, Debug, Clone)]
pub struct AttentionArgs {
    #[command(subcommand)]
    pub command: AttentionCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AttentionCommand {
    /// Activate the display takeover for Codex or Claude
    Set(SetAttentionArgs),

    /// Clear the display takeover
    Clear(ClearAttentionArgs),

    /// Print the current attention file state
    Status(StatusAttentionArgs),
}

#[derive(Args, Debug, Clone)]
pub struct SetAttentionArgs {
    /// Provider that needs attention: codex or claude
    #[arg(long)]
    pub provider: String,

    /// Machine-readable reason, for example approval or finished
    #[arg(long, default_value = "needs_attention")]
    pub reason: String,

    /// Short screen action label
    #[arg(long, default_value = "OPEN")]
    pub action: String,

    /// Attention JSON file path
    #[arg(long = "file")]
    pub file: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ClearAttentionArgs {
    /// Attention JSON file path
    #[arg(long = "file")]
    pub file: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct StatusAttentionArgs {
    /// Attention JSON file path
    #[arg(long = "file")]
    pub file: Option<PathBuf>,

    /// Output compact JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AttentionFile {
    provider: String,
    reason: String,
    action: String,
    active: bool,
}

pub async fn run(args: AttentionArgs) -> anyhow::Result<()> {
    match args.command {
        AttentionCommand::Set(args) => {
            let provider = normalize_provider(&args.provider)?;
            let attention = AttentionFile {
                provider,
                reason: args.reason,
                action: args.action,
                active: true,
            };
            let path = attention_path(args.file)?;
            write_attention_file(&path, &attention)?;
            println!(
                "attention active: {} ({})",
                attention.provider,
                path.display()
            );
        }
        AttentionCommand::Clear(args) => {
            let path = attention_path(args.file)?;
            clear_attention_file(&path)?;
            println!("attention cleared: {}", path.display());
        }
        AttentionCommand::Status(args) => {
            let path = attention_path(args.file)?;
            let attention = read_attention_file(&path)?;
            print_status(attention.as_ref(), &path, args.json)?;
        }
    }
    Ok(())
}

fn normalize_provider(raw: &str) -> anyhow::Result<String> {
    let provider = raw.trim().to_ascii_lowercase();
    match provider.as_str() {
        "codex" | "claude" => Ok(provider),
        _ => anyhow::bail!("attention provider must be codex or claude"),
    }
}

fn attention_path(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let config_dir = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .context("could not determine config directory; pass --file")?;
    Ok(config_dir.join("CodexBar").join("attention.json"))
}

fn write_attention_file(path: &Path, attention: &AttentionFile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(attention)?;
    std::fs::write(path, format!("{json}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn clear_attention_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn read_attention_file(path: &Path) -> anyhow::Result<Option<AttentionFile>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let attention = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            Ok(Some(attention))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn print_status(attention: Option<&AttentionFile>, path: &Path, json: bool) -> anyhow::Result<()> {
    if json {
        let payload = serde_json::json!({
            "path": path,
            "attention": attention,
        });
        println!("{}", serde_json::to_string(&payload)?);
    } else if let Some(attention) = attention.filter(|attention| attention.active) {
        println!(
            "attention active: {} reason={} action={} ({})",
            attention.provider,
            attention.reason,
            attention.action,
            path.display()
        );
    } else {
        println!("attention inactive: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_codex_attention_file_for_display_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.json");
        let attention = AttentionFile {
            provider: normalize_provider("Codex").unwrap(),
            reason: "approval".to_string(),
            action: "OPEN".to_string(),
            active: true,
        };

        write_attention_file(&path, &attention).unwrap();

        let written = read_attention_file(&path).unwrap().unwrap();
        assert_eq!(
            written,
            AttentionFile {
                provider: "codex".to_string(),
                reason: "approval".to_string(),
                action: "OPEN".to_string(),
                active: true,
            }
        );
    }

    #[test]
    fn rejects_unsupported_attention_providers() {
        let error = normalize_provider("ollama").unwrap_err().to_string();
        assert!(error.contains("codex or claude"));
    }

    #[test]
    fn clear_attention_file_allows_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.json");

        clear_attention_file(&path).unwrap();
    }
}
