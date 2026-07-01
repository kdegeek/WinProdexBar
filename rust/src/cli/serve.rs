//! Local HTTP server for scriptable usage/cost JSON.

use clap::Args;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use super::provider_context::PersistedProviderContexts;
use super::usage::ProviderSelection;
use crate::core::{
    DisplayAttention, DisplayPayloadBuilder, FetchContext, ProviderFetchResult, ProviderId,
    SourceMode, instantiate_provider,
};
use crate::cost_scanner::CostScanner;

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    /// Local HTTP host. Use 0.0.0.0 with --device-secret for LAN devices.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Local HTTP port
    #[arg(long, default_value = "8080")]
    pub port: u16,

    /// Upstream provider refresh interval in seconds. Device polls still use cached data.
    #[arg(long = "refresh-interval", default_value = "600")]
    pub refresh_interval: u64,

    /// Required secret for /display when serving LAN devices
    #[arg(long = "device-secret")]
    pub device_secret: Option<String>,

    /// Optional Codex/Claude attention JSON file consumed by /display
    #[arg(long = "attention-file")]
    pub attention_file: Option<PathBuf>,
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let allow_lan_hosts = !is_loopback_bind_host(&args.host);
    if allow_lan_hosts && args.device_secret.as_deref().unwrap_or_default().is_empty() {
        anyhow::bail!("--device-secret is required when --host is not loopback");
    }

    let listener = TcpListener::bind((args.host.as_str(), args.port)).await?;
    eprintln!(
        "CodexBar server listening on http://{}:{}",
        args.host, args.port
    );

    let display_cache = DisplayCache::default();
    warm_display_cache(display_cache.clone(), ProviderId::all().to_vec());

    loop {
        let (stream, _) = listener.accept().await?;
        let security = ServeSecurity {
            allow_lan_hosts,
            device_secret: args.device_secret.clone(),
            attention_file: args.attention_file.clone(),
            display_cache: display_cache.clone(),
            display_ttl: Duration::from_secs(args.refresh_interval.max(1)),
        };
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, security).await {
                tracing::debug!("serve client error: {error}");
            }
        });
    }
}

#[derive(Debug, Clone)]
struct ServeSecurity {
    allow_lan_hosts: bool,
    device_secret: Option<String>,
    attention_file: Option<PathBuf>,
    display_cache: DisplayCache,
    display_ttl: Duration,
}

#[derive(Debug, Clone, Default)]
struct DisplayCache {
    state: Arc<Mutex<DisplayCacheState>>,
}

#[derive(Debug, Default)]
struct DisplayCacheState {
    entries: HashMap<String, CachedDisplay>,
    refreshing: HashSet<String>,
}

#[derive(Debug, Clone)]
struct CachedDisplay {
    fetched_at: Instant,
    results: Vec<(ProviderId, ProviderFetchResult)>,
}

async fn handle_client(mut stream: TcpStream, security: ServeSecurity) -> anyhow::Result<()> {
    let mut buffer = vec![0_u8; 8192];
    let n = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    let response = match parse_request(&request) {
        Ok(request) => route_request(&request, &security).await,
        Err(status) => json_response(status, serde_json::json!({ "error": "bad request" })),
    };
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn route_request(request: &ServeRequest, security: &ServeSecurity) -> String {
    if request.method != "GET" {
        return json_response(405, serde_json::json!({ "error": "method not allowed" }));
    }
    if !allowed_host(&request.host, security.allow_lan_hosts) {
        return json_response(403, serde_json::json!({ "error": "forbidden host" }));
    }
    if request.path == "/display" && !display_secret_allowed(request, security) {
        return json_response(401, serde_json::json!({ "error": "unauthorized" }));
    }

    match request.path.as_str() {
        "/health" => json_response(
            200,
            serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }),
        ),
        "/usage" => usage_response(request.query.get("provider").map(String::as_str)).await,
        "/display" => {
            display_response(
                request.query.get("provider").map(String::as_str),
                security.attention_file.as_ref(),
                security.display_cache.clone(),
                security.display_ttl,
            )
            .await
        }
        "/cost" => cost_response(request.query.get("provider").map(String::as_str)).await,
        _ => json_response(404, serde_json::json!({ "error": "not found" })),
    }
}

fn display_secret_allowed(request: &ServeRequest, security: &ServeSecurity) -> bool {
    let Some(secret) = security.device_secret.as_deref() else {
        return true;
    };
    !secret.is_empty()
        && request
            .query
            .get("secret")
            .is_some_and(|value| value == secret)
}

async fn usage_response(provider: Option<&str>) -> String {
    let selection = match ProviderSelection::from_arg(provider) {
        Ok(selection) => selection,
        Err(error) => {
            return json_response(400, serde_json::json!({ "error": error.to_string() }));
        }
    };
    let base_ctx = server_base_context();
    let provider_contexts = PersistedProviderContexts::load();
    let mut results = Vec::new();
    for provider_id in selection.as_list() {
        let provider = instantiate_provider(provider_id);
        let ctx = provider_contexts.fetch_context(provider_id, &base_ctx, None);
        match provider.fetch_usage(&ctx).await {
            Ok(result) => results.push(serde_json::json!({
                "provider": provider_id.cli_name(),
                "source": result.source_label,
                "usage": result.usage,
                "cost": result.cost,
            })),
            Err(error) => results.push(serde_json::json!({
                "provider": provider_id.cli_name(),
                "error": error.to_string(),
            })),
        }
    }
    json_response(200, serde_json::Value::Array(results))
}

async fn display_response(
    provider: Option<&str>,
    attention_file: Option<&PathBuf>,
    cache: DisplayCache,
    ttl: Duration,
) -> String {
    let selection = match ProviderSelection::from_arg(provider) {
        Ok(selection) => selection,
        Err(error) => {
            return json_response(400, serde_json::json!({ "error": error.to_string() }));
        }
    };
    let providers = selection.as_list();
    let cache_key = display_cache_key(&providers);
    let results = cached_display_results(cache, cache_key, providers, ttl).await;
    let payload = display_payload_from_results(&results, attention_file);
    json_response(
        200,
        serde_json::to_value(payload).unwrap_or_else(|_| serde_json::json!({})),
    )
}

fn display_payload_from_results(
    results: &[(ProviderId, ProviderFetchResult)],
    attention_file: Option<&PathBuf>,
) -> crate::core::DisplayPayload {
    DisplayPayloadBuilder::payload_at(
        results
            .iter()
            .map(|(provider_id, result)| (*provider_id, result)),
        chrono::Utc::now(),
        attention_file.and_then(read_attention_file),
    )
}

fn display_cache_key(providers: &[ProviderId]) -> String {
    providers
        .iter()
        .map(|provider| provider.cli_name())
        .collect::<Vec<_>>()
        .join(",")
}

async fn cached_display_results(
    cache: DisplayCache,
    cache_key: String,
    providers: Vec<ProviderId>,
    ttl: Duration,
) -> Vec<(ProviderId, ProviderFetchResult)> {
    let now = Instant::now();
    {
        let mut state = cache.state.lock().await;
        if let Some(entry) = state.entries.get(&cache_key).cloned() {
            if now.duration_since(entry.fetched_at) <= ttl {
                return entry.results;
            }
            if state.refreshing.insert(cache_key.clone()) {
                let cache = cache.clone();
                let cache_key = cache_key.clone();
                tokio::spawn(async move {
                    refresh_display_cache(cache, cache_key, providers).await;
                });
            }
            return entry.results;
        }
        if !state.refreshing.insert(cache_key.clone()) {
            return Vec::new();
        }
    }
    tokio::spawn(async move {
        refresh_display_cache(cache, cache_key, providers).await;
    });
    Vec::new()
}

fn warm_display_cache(cache: DisplayCache, providers: Vec<ProviderId>) {
    let cache_key = display_cache_key(&providers);
    tokio::spawn(async move {
        refresh_display_cache(cache, cache_key, providers).await;
    });
}

async fn refresh_display_cache(
    cache: DisplayCache,
    cache_key: String,
    providers: Vec<ProviderId>,
) {
    let fresh_results = collect_usage_results(providers).await;
    let mut state = cache.state.lock().await;
    let previous_results = state
        .entries
        .get(&cache_key)
        .map(|entry| entry.results.as_slice());
    let results = merge_display_results(previous_results, fresh_results);
    state.entries.insert(
        cache_key.clone(),
        CachedDisplay {
            fetched_at: Instant::now(),
            results,
        },
    );
    state.refreshing.remove(&cache_key);
}

fn merge_display_results(
    previous: Option<&[(ProviderId, ProviderFetchResult)]>,
    mut fresh: Vec<(ProviderId, ProviderFetchResult)>,
) -> Vec<(ProviderId, ProviderFetchResult)> {
    let Some(previous) = previous else {
        return fresh;
    };
    for (previous_id, previous_result) in previous {
        match fresh
            .iter_mut()
            .find(|(fresh_id, _)| fresh_id == previous_id)
        {
            Some((_, fresh_result))
                if fresh_result.source_label == "cli" && previous_result.source_label != "cli" =>
            {
                *fresh_result = previous_result.clone();
            }
            Some(_) => {}
            None => fresh.push((*previous_id, previous_result.clone())),
        }
    }
    fresh
}

#[derive(Debug, Deserialize)]
struct AttentionFile {
    provider: String,
    reason: Option<String>,
    action: Option<String>,
    active: Option<bool>,
}

fn read_attention_file(path: &PathBuf) -> Option<DisplayAttention> {
    let raw = std::fs::read_to_string(path).ok()?;
    let attention: AttentionFile = serde_json::from_str(&raw).ok()?;
    if attention.active == Some(false) {
        return None;
    }
    let provider = attention.provider.to_ascii_lowercase();
    if provider != "codex" && provider != "claude" {
        return None;
    }
    Some(DisplayAttention {
        provider,
        reason: attention
            .reason
            .unwrap_or_else(|| "needs_attention".to_string()),
        action: attention.action.unwrap_or_else(|| "OPEN".to_string()),
    })
}

async fn collect_usage_results(
    providers: Vec<ProviderId>,
) -> Vec<(ProviderId, ProviderFetchResult)> {
    let base_ctx = server_base_context();
    let provider_contexts = PersistedProviderContexts::load();
    let mut results = Vec::new();
    for provider_id in providers {
        let provider = instantiate_provider(provider_id);
        let source_override = match provider_id {
            ProviderId::Claude | ProviderId::Codex => Some(SourceMode::OAuth),
            _ => None,
        };
        let ctx = provider_contexts.fetch_context(provider_id, &base_ctx, source_override);
        match provider.fetch_usage(&ctx).await {
            Ok(result) => results.push((provider_id, result)),
            Err(error) => tracing::debug!(
                "display feed skipped {} after usage fetch error: {error}",
                provider_id.cli_name()
            ),
        }
    }
    results
}

fn server_base_context() -> FetchContext {
    FetchContext {
        source_mode: SourceMode::Auto,
        include_credits: true,
        web_timeout: 60,
        verbose: false,
        manual_cookie_header: None,
        api_key: None,
        workspace_id: None,
        api_region: None,
    }
}

async fn cost_response(provider: Option<&str>) -> String {
    let selection = match ProviderSelection::from_arg(provider) {
        Ok(selection) => selection,
        Err(error) => {
            return json_response(400, serde_json::json!({ "error": error.to_string() }));
        }
    };
    let scanner = CostScanner::new(30);
    let mut results = Vec::new();
    for provider_id in selection.as_list() {
        let (supported, summary) = match provider_id {
            ProviderId::Codex => (true, scanner.scan_codex()),
            ProviderId::Claude => (true, scanner.scan_claude()),
            _ => (false, Default::default()),
        };
        if supported {
            results.push(serde_json::json!({
                "provider": provider_id.cli_name(),
                "supported": true,
                "days_scanned": 30,
                "cost": {
                    "total_usd": summary.total_cost_usd,
                    "currency": "USD"
                },
                "tokens": {
                    "input": summary.input_tokens,
                    "output": summary.output_tokens,
                    "cached": summary.cached_tokens
                },
                "sessions_count": summary.sessions_count,
                "by_model": summary.by_model,
            }));
        } else {
            results.push(serde_json::json!({
                "provider": provider_id.cli_name(),
                "supported": false,
                "error": "Local cost scanning not available for this provider"
            }));
        }
    }
    json_response(200, serde_json::Value::Array(results))
}

#[derive(Debug)]
struct ServeRequest {
    method: String,
    path: String,
    host: String,
    query: std::collections::HashMap<String, String>,
}

fn parse_request(raw: &str) -> Result<ServeRequest, u16> {
    let mut lines = raw.split("\r\n");
    let first = lines.next().ok_or(400_u16)?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or(400_u16)?.to_uppercase();
    let target = parts.next().ok_or(400_u16)?;
    if parts.next().is_none() || !target.starts_with('/') {
        return Err(400);
    }

    let mut hosts = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(400);
        };
        if name.trim().eq_ignore_ascii_case("host") {
            hosts.push(value.trim().to_string());
        }
    }
    if hosts.len() != 1 {
        return Err(400);
    }

    let (path, query) = parse_target(target);
    Ok(ServeRequest {
        method,
        path,
        host: hosts.remove(0),
        query,
    })
}

fn parse_target(target: &str) -> (String, std::collections::HashMap<String, String>) {
    let Some((path, query_string)) = target.split_once('?') else {
        return (target.to_string(), Default::default());
    };
    let query = query_string
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect();
    (path.to_string(), query)
}

fn allowed_host(host: &str, allow_lan_hosts: bool) -> bool {
    let trimmed = host.trim();
    if trimmed.is_empty() || trimmed.contains(',') {
        return false;
    }
    let without_port = if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((addr, port)) = rest.split_once(']') else {
            return false;
        };
        if !port.is_empty() && !valid_port_suffix(port) {
            return false;
        }
        format!("[{addr}]")
    } else {
        let segments: Vec<_> = trimmed.split(':').collect();
        match segments.as_slice() {
            [host] => host.to_string(),
            [host, port] if valid_port(port) => host.to_string(),
            _ => return false,
        }
    };
    matches!(
        without_port.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "localhost." | "[::1]"
    ) || (allow_lan_hosts && is_allowed_lan_host(&without_port))
}

fn is_loopback_bind_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
        || host.parse::<IpAddr>().is_ok_and(|addr| addr.is_loopback())
}

fn is_allowed_lan_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    host.parse::<IpAddr>().is_ok_and(|addr| {
        addr.is_loopback()
            || match addr {
                IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
                IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local(),
            }
    })
}

fn valid_port_suffix(raw: &str) -> bool {
    raw.is_empty() || raw.strip_prefix(':').is_some_and(valid_port)
}

fn valid_port(raw: &str) -> bool {
    raw.parse::<u16>().is_ok_and(|port| port > 0)
}

fn url_decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut bytes = raw.as_bytes().iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'+' {
            out.push(' ');
        } else if byte == b'%' {
            let hi = bytes.next();
            let lo = bytes.next();
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let Ok(value) =
                    u8::from_str_radix(std::str::from_utf8(&[hi, lo]).unwrap_or_default(), 16)
            {
                out.push(value as char);
            }
        } else {
            out.push(byte as char);
        }
    }
    out
}

fn json_response(status: u16, payload: serde_json::Value) -> String {
    let body = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{RateWindow, UsageSnapshot};

    #[test]
    fn rejects_non_loopback_hosts() {
        assert!(allowed_host("127.0.0.1:8080", false));
        assert!(allowed_host("localhost", false));
        assert!(allowed_host("[::1]:8080", false));
        assert!(!allowed_host("192.168.1.20:8080", false));
        assert!(!allowed_host("example.com", false));
        assert!(!allowed_host("127.0.0.1, example.com", false));
    }

    #[test]
    fn allows_private_hosts_only_for_lan_device_mode() {
        assert!(allowed_host("192.168.1.20:8080", true));
        assert!(allowed_host("10.0.0.7", true));
        assert!(allowed_host("172.16.0.3", true));
        assert!(!allowed_host("8.8.8.8", true));
        assert!(!allowed_host("example.com", true));
    }

    #[test]
    fn display_secret_is_required_when_configured() {
        let security = ServeSecurity {
            allow_lan_hosts: true,
            device_secret: Some("topsecret".to_string()),
            attention_file: None,
            display_cache: DisplayCache::default(),
            display_ttl: Duration::from_secs(60),
        };
        let allowed = parse_request(
            "GET /display?provider=all&secret=topsecret HTTP/1.1\r\nHost: 192.168.1.20:8080\r\n\r\n",
        )
        .unwrap();
        let rejected =
            parse_request("GET /display?provider=all HTTP/1.1\r\nHost: 192.168.1.20:8080\r\n\r\n")
                .unwrap();
        assert!(display_secret_allowed(&allowed, &security));
        assert!(!display_secret_allowed(&rejected, &security));
    }

    #[test]
    fn reads_codex_claude_attention_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.json");
        std::fs::write(
            &path,
            r#"{"provider":"codex","reason":"approval","action":"OPEN"}"#,
        )
        .unwrap();

        let attention = read_attention_file(&path).unwrap();
        assert_eq!(attention.provider, "codex");
        assert_eq!(attention.reason, "approval");
        assert_eq!(attention.action, "OPEN");
    }

    #[test]
    fn ignores_inactive_or_unsupported_attention_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attention.json");
        std::fs::write(&path, r#"{"provider":"ollama","active":true}"#).unwrap();
        assert!(read_attention_file(&path).is_none());

        std::fs::write(&path, r#"{"provider":"claude","active":false}"#).unwrap();
        assert!(read_attention_file(&path).is_none());
    }

    #[test]
    fn parses_usage_route_provider_query() {
        let request =
            parse_request("GET /usage?provider=deepseek HTTP/1.1\r\nHost: localhost:8080\r\n\r\n")
                .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/usage");
        assert_eq!(request.query.get("provider"), Some(&"deepseek".to_string()));
    }

    #[test]
    fn parses_display_route_provider_query() {
        let request =
            parse_request("GET /display?provider=all HTTP/1.1\r\nHost: localhost:8080\r\n\r\n")
                .unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/display");
        assert_eq!(request.query.get("provider"), Some(&"all".to_string()));
    }

    #[test]
    fn display_cache_key_normalizes_provider_selection() {
        let providers = ProviderSelection::from_arg(Some("both")).unwrap().as_list();
        assert_eq!(display_cache_key(&providers), "codex,claude");
    }

    #[test]
    fn merge_display_results_keeps_non_cli_result_over_cli_fallback() {
        let oauth = ProviderFetchResult::new(UsageSnapshot::new(RateWindow::new(50.0)), "oauth");
        let cli = ProviderFetchResult::new(UsageSnapshot::new(RateWindow::new(100.0)), "cli");

        let merged = merge_display_results(
            Some(&[(ProviderId::Claude, oauth.clone())]),
            vec![(ProviderId::Claude, cli)],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].1.source_label, "oauth");
        assert_eq!(merged[0].1.usage.primary.used_percent, 50.0);
    }
}
