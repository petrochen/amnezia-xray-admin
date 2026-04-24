//! Async backend operations for the TUI.
//!
//! Provides the bridge between the synchronous event loop and async SSH/Xray operations.
//! Operations are spawned on a tokio runtime and results are sent back via channels.

use std::sync::mpsc;

use crate::backend_trait::{SshBackend, XrayBackend};
use crate::config::Config;
use crate::error::AppError;
use crate::ssh::{expand_tilde, resolve_ssh_host, SshSession};
use crate::ui::telegram_setup::DeployStatus;
use crate::xray::client::{generate_amnezia_url, generate_vless_url, ServerInfo, XrayApiClient};
use crate::xray::config::{ensure_api_enabled, read_server_config};
use crate::xray::types::{VlessUrlParams, XrayUser};

/// Path to the Xray public key file on the server (used for vless:// URLs).
const PUBLIC_KEY_PATH: &str = "/opt/amnezia/xray/xray_public.key";
/// Path to bridge params JSON on the server (written by server setup scripts).
const BRIDGE_PARAMS_PATH: &str = "/etc/xray/bridge-params.json";

/// Bridge node parameters for double-hop XHTTP+Reality configuration.
pub struct BridgeParams {
    pub host: String,
    pub port: u16,
    pub public_key: String,
    pub short_id: String,
    pub sni: String,
    pub path: String,
}

/// Why a vless URL was requested
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VlessUrlIntent {
    /// User pressed [c] to copy URL to clipboard
    Clipboard,
    /// User pressed [q] to show QR code
    Qr,
}

/// Messages sent from backend async tasks to the UI thread.
pub enum BackendMsg {
    /// Dashboard data loaded (users + server info)
    DashboardData(Result<DashboardData, String>),
    /// SSH connection test result (Ok = xray version string)
    ConnectionTest(Result<String, String>),
    /// User added successfully
    UserAdded(Result<AddedUser, String>),
    /// User deleted successfully (Ok = deleted uuid)
    UserDeleted(Result<String, String>),
    /// Vless URL generated for an existing user (copy or QR)
    VlessUrl(Result<AddedUser, String>, VlessUrlIntent),
    /// Online IPs fetched for a user (Ok = (uuid, ips), Err = (uuid, error))
    OnlineIps(Result<(String, Vec<String>), (String, String)>),
    /// Telegram bot deployment progress update
    DeployProgress(DeployStatus),
    /// Telegram bot deployment result
    DeployBot(Result<String, String>),
}

/// Dashboard data bundle
pub struct DashboardData {
    pub users: Vec<XrayUser>,
    pub server_info: ServerInfo,
    pub container_uptime: String,
    pub latest_version: Option<String>,
    /// Whether `latest_version` reflects a fetch this cycle (false = skipped, keep prior cache).
    pub version_was_fetched: bool,
}

/// GitHub API endpoint for the latest Xray-core release.
const XRAY_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/XTLS/Xray-core/releases/latest";
/// Timeout for best-effort network calls (uptime probe, version check).
const NETWORK_PROBE_TIMEOUT_SECS: u32 = 3;

/// Fetch `docker ps` status for the backend's container. Empty string on failure or no match.
///
/// Uses an anchored regex filter so that `amnezia-xray-test` does not match `amnezia-xray`.
/// Container names are validated (`is_valid_container_name`) to safe characters so direct
/// interpolation is fine.
pub async fn fetch_container_uptime(backend: &dyn XrayBackend) -> String {
    backend
        .exec_on_host(&format!(
            "docker ps --filter 'name=^/{}$' --format '{{{{.Status}}}}'",
            backend.container_name()
        ))
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default()
}

/// Fetch the latest Xray-core release tag from GitHub. `None` on network failure or
/// unparseable response. Returned string is the version without a leading `v` (e.g. `25.8.3`).
///
/// Parsing is done in Rust via `serde_json` rather than a remote `grep|cut|tr` pipeline,
/// which was fragile to JSON field reordering or error bodies. The result is also validated
/// to match `[0-9.]+` before being returned, so a malformed payload cannot render garbage
/// in the UI.
pub async fn fetch_latest_xray_version(backend: &dyn XrayBackend) -> Option<String> {
    let out = backend
        .exec_on_host(&format!(
            "curl -sfL --max-time {} -H 'Accept: application/vnd.github+json' {}",
            NETWORK_PROBE_TIMEOUT_SECS, XRAY_LATEST_RELEASE_URL
        ))
        .await
        .ok()?;
    if !out.success() {
        return None;
    }
    parse_xray_version_from_json(&out.stdout)
}

/// Extract and validate the Xray version from a GitHub releases/latest JSON payload.
fn parse_xray_version_from_json(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = json.get("tag_name")?.as_str()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty()
        || !version.chars().all(|c| c.is_ascii_digit() || c == '.')
        || !version.chars().any(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(version.to_string())
}

/// Result of adding a user
pub struct AddedUser {
    pub name: String,
    pub uuid: String,
    pub vless_url: String,
}

/// Expand tilde in a PathBuf (e.g. `~/.ssh/id_ed25519` -> `/home/user/.ssh/id_ed25519`).
fn expand_key_path(path: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    path.map(|p| {
        let s = p.to_string_lossy();
        if s.starts_with("~/") {
            std::path::PathBuf::from(expand_tilde(&s))
        } else {
            p
        }
    })
}

/// Resolve SSH connection parameters from the app config.
pub fn resolve_connection_info(
    config: &Config,
) -> Result<(String, u16, String, Option<std::path::PathBuf>), AppError> {
    if let Some(ref alias) = config.ssh_host {
        match resolve_ssh_host(alias) {
            Some(sc) => {
                let host = sc.hostname.unwrap_or_else(|| alias.clone());
                let port = sc.port.unwrap_or(config.port);
                let user = sc.user.unwrap_or_else(|| config.user.clone());
                let key = expand_key_path(sc.identity_file.or_else(|| config.key_path.clone()));
                Ok((host, port, user, key))
            }
            None => {
                // Alias not found in ssh config, treat as hostname
                Ok((
                    alias.clone(),
                    config.port,
                    config.user.clone(),
                    expand_key_path(config.key_path.clone()),
                ))
            }
        }
    } else if let Some(ref host) = config.host {
        Ok((
            host.clone(),
            config.port,
            config.user.clone(),
            expand_key_path(config.key_path.clone()),
        ))
    } else {
        Err(AppError::Config("no host configured".to_string()))
    }
}

/// Connect and return an `SshBackend` (trait-based abstraction).
pub async fn connect_backend(config: &Config) -> Result<SshBackend, AppError> {
    let (hostname, port, user, key_path) = resolve_connection_info(config)?;
    let addr = if hostname.contains(':') {
        format!("[{}]:{}", hostname, port)
    } else {
        format!("{}:{}", hostname, port)
    };
    let session = SshSession::connect(&addr, &user, key_path.as_deref(), &config.container).await?;
    Ok(SshBackend::new(session, hostname))
}

/// Read the Xray public key from the server (needed for vless:// URL generation).
async fn read_public_key(backend: &dyn XrayBackend) -> Result<String, AppError> {
    let result = backend
        .exec_in_container(&format!("cat {}", PUBLIC_KEY_PATH))
        .await?;
    if result.success() {
        Ok(result.stdout.trim().to_string())
    } else {
        let msg = format!("failed to read public key: {}", result.stderr.trim());
        Err(AppError::Xray(crate::error::add_hint(&msg)))
    }
}

/// Read bridge params JSON from the server host (not inside container).
pub async fn read_bridge_params(backend: &dyn XrayBackend) -> Result<BridgeParams, AppError> {
    let result = backend
        .exec_on_host(&format!("cat {}", BRIDGE_PARAMS_PATH))
        .await?;
    if !result.success() {
        return Err(AppError::Xray(format!(
            "failed to read bridge params from {}: {}",
            BRIDGE_PARAMS_PATH,
            result.stderr.trim()
        )));
    }
    let v: serde_json::Value = serde_json::from_str(result.stdout.trim())
        .map_err(|e| AppError::Xray(format!("failed to parse bridge params: {}", e)))?;
    let host = v["host"]
        .as_str()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'host'".to_string()))?
        .to_string();
    let port = v["port"]
        .as_u64()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'port'".to_string()))? as u16;
    let public_key = v["publicKey"]
        .as_str()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'publicKey'".to_string()))?
        .to_string();
    let short_id = v["shortId"]
        .as_str()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'shortId'".to_string()))?
        .to_string();
    let sni = v["sni"]
        .as_str()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'sni'".to_string()))?
        .to_string();
    let path = v["path"]
        .as_str()
        .ok_or_else(|| AppError::Xray("bridge-params.json missing 'path'".to_string()))?
        .to_string();
    Ok(BridgeParams { host, port, public_key, short_id, sni, path })
}

/// Build VlessUrlParams from live server config. Reused by vless:// and vpn:// generators.
pub async fn build_vless_params(
    backend: &dyn XrayBackend,
    uuid: &str,
) -> Result<VlessUrlParams, AppError> {
    let server_config = read_server_config(backend).await?;
    let reality = server_config
        .reality_settings()
        .ok_or_else(|| AppError::Xray("no Reality settings in server config".to_string()))?;
    let port = server_config.vless_port().unwrap_or(443);
    let public_key = read_public_key(backend).await?;

    // Try to read xhttpSettings path from server config
    let path = server_config
        .find_vless_inbound()
        .and_then(|ib| ib.get("streamSettings"))
        .and_then(|ss| ss.get("xhttpSettings"))
        .and_then(|xh| xh.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or("/")
        .to_string();

    Ok(VlessUrlParams {
        uuid: uuid.to_string(),
        host: backend.hostname().to_string(),
        port,
        sni: reality.sni,
        public_key,
        short_id: reality.short_id,
        path,
    })
}

/// Build a bridge vless:// URL for a user (via bridge node, for censored regions).
pub async fn build_bridge_vless_url(
    backend: &dyn XrayBackend,
    uuid: &str,
) -> Result<String, AppError> {
    let bridge = read_bridge_params(backend).await?;
    let params = VlessUrlParams {
        uuid: uuid.to_string(),
        host: bridge.host,
        port: bridge.port,
        sni: bridge.sni,
        public_key: bridge.public_key,
        short_id: bridge.short_id,
        path: bridge.path,
    };
    Ok(generate_vless_url(&params))
}

/// Build a direct vless:// URL for a user (egress node, for abroad).
pub async fn build_direct_vless_url(
    backend: &dyn XrayBackend,
    uuid: &str,
) -> Result<String, AppError> {
    let params = build_vless_params(backend, uuid).await?;
    Ok(generate_vless_url(&params))
}

/// Build a vless:// URL for a user, using live server config for reality params.
pub async fn build_vless_url(
    backend: &dyn XrayBackend,
    uuid: &str,
) -> Result<String, AppError> {
    let params = build_vless_params(backend, uuid).await?;
    Ok(generate_vless_url(&params))
}

/// Build a vpn:// connection string for a user (compressed Xray config).
#[allow(dead_code)]
pub async fn build_amnezia_url(
    backend: &dyn XrayBackend,
    uuid: &str,
) -> Result<String, AppError> {
    let params = build_vless_params(backend, uuid).await?;
    Ok(generate_amnezia_url(&params))
}

/// Spawn: fetch dashboard data (user list + server info + per-user stats).
///
/// `fetch_version`: when true, hits the GitHub API to check for a newer Xray release.
/// This should only be `true` once per session — the dashboard auto-refreshes every 5 s,
/// and unauthenticated GitHub API is rate-limited to 60 req/h per IP.
pub fn spawn_fetch_dashboard(
    runtime: &tokio::runtime::Handle,
    config: Config,
    tx: mpsc::Sender<BackendMsg>,
    api_check_done: bool,
    fetch_version: bool,
) {
    runtime.spawn(async move {
        let result = fetch_dashboard_data(&config, api_check_done, fetch_version).await;
        let _ = tx.send(BackendMsg::DashboardData(result));
    });
}

async fn fetch_dashboard_data(
    config: &Config,
    api_check_done: bool,
    fetch_version: bool,
) -> Result<DashboardData, String> {
    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;

    // Ensure the Xray API is enabled (adds api/stats/policy sections if missing).
    // Only runs on the first successful refresh — skipped thereafter.
    if !api_check_done {
        ensure_api_enabled(&backend)
            .await
            .map_err(|e| e.to_string())?;
    }

    let client = XrayApiClient::new(&backend);

    let mut users = client.list_users().await.map_err(|e| e.to_string())?;
    let server_info = client.get_server_info().await.map_err(|e| e.to_string())?;

    // Enrich users with stats and online count
    for user in &mut users {
        if let Ok(stats) = client.get_user_stats(&user.email).await {
            user.stats = stats;
        }
        if let Ok(count) = client.get_online_count(&user.email).await {
            user.online_count = count;
        }
    }

    let container_uptime = fetch_container_uptime(&backend).await;

    let latest_version = if fetch_version {
        fetch_latest_xray_version(&backend).await
    } else {
        None
    };

    let _ = backend.close().await;

    Ok(DashboardData {
        users,
        server_info,
        container_uptime,
        latest_version,
        version_was_fetched: fetch_version,
    })
}

/// Spawn: test SSH connection and return xray version.
pub fn spawn_test_connection(
    runtime: &tokio::runtime::Handle,
    config: Config,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = test_connection(&config).await;
        let _ = tx.send(BackendMsg::ConnectionTest(result));
    });
}

async fn test_connection(config: &Config) -> Result<String, String> {
    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;
    let result = backend
        .exec_in_container("xray version")
        .await
        .map_err(|e| e.to_string())?;
    let _ = backend.close().await;

    if result.success() {
        Ok(result.stdout.trim().to_string())
    } else {
        Err(format!("xray version failed: {}", result.stderr.trim()))
    }
}

/// Spawn: add a new user.
pub fn spawn_add_user(
    runtime: &tokio::runtime::Handle,
    config: Config,
    name: String,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = add_user(&config, &name).await;
        let _ = tx.send(BackendMsg::UserAdded(result));
    });
}

async fn add_user(config: &Config, name: &str) -> Result<AddedUser, String> {
    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;
    let client = XrayApiClient::new(&backend);

    let uuid = client.add_user(name).await.map_err(|e| e.to_string())?;

    // URL generation is best-effort: if it fails, the user was still added successfully.
    // The URL can be regenerated later via [c] or [q] in the detail view.
    let vless_url = match build_vless_url(&backend, &uuid).await {
        Ok(url) => url,
        Err(e) => {
            eprintln!("warning: user added but URL generation failed: {}", e);
            String::new()
        }
    };

    let _ = backend.close().await;

    Ok(AddedUser {
        name: name.to_string(),
        uuid,
        vless_url,
    })
}

/// Spawn: delete a user by UUID.
pub fn spawn_delete_user(
    runtime: &tokio::runtime::Handle,
    config: Config,
    uuid: String,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = delete_user(&config, &uuid).await;
        let _ = tx.send(BackendMsg::UserDeleted(result));
    });
}

async fn delete_user(config: &Config, uuid: &str) -> Result<String, String> {
    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;
    let client = XrayApiClient::new(&backend);

    client.remove_user(uuid).await.map_err(|e| e.to_string())?;
    let _ = backend.close().await;

    Ok(uuid.to_string())
}

/// Generate a vless:// URL for an existing user.
pub fn spawn_vless_url(
    runtime: &tokio::runtime::Handle,
    config: Config,
    uuid: String,
    name: String,
    intent: VlessUrlIntent,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = generate_url(&config, &uuid, &name).await;
        let _ = tx.send(BackendMsg::VlessUrl(result, intent));
    });
}

/// Spawn: fetch online IPs for a user.
pub fn spawn_fetch_online_ips(
    runtime: &tokio::runtime::Handle,
    config: Config,
    uuid: String,
    email: String,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = fetch_online_ips(&config, &uuid, &email).await;
        let _ = tx.send(BackendMsg::OnlineIps(result));
    });
}

async fn fetch_online_ips(
    config: &Config,
    uuid: &str,
    email: &str,
) -> Result<(String, Vec<String>), (String, String)> {
    let backend = connect_backend(config)
        .await
        .map_err(|e| (uuid.to_string(), e.to_string()))?;
    let client = XrayApiClient::new(&backend);
    let ips = client
        .get_online_ips(email)
        .await
        .map_err(|e| (uuid.to_string(), e.to_string()))?;
    let _ = backend.close().await;
    Ok((uuid.to_string(), ips))
}

/// Spawn: deploy Telegram bot to VPS via SSH.
pub fn spawn_deploy_bot(
    runtime: &tokio::runtime::Handle,
    config: Config,
    token: String,
    tx: mpsc::Sender<BackendMsg>,
) {
    runtime.spawn(async move {
        let result = deploy_bot_with_progress(&config, &token, &tx).await;
        let _ = tx.send(BackendMsg::DeployBot(result));
    });
}

/// Get VPS public IP. Tries curl on the host first, falls back to SSH config host.
async fn get_vps_public_ip(backend: &SshBackend, config: &Config) -> String {
    // Try external service via SSH on the host
    for url in &[
        "https://ifconfig.me",
        "https://icanhazip.com",
        "https://api.ipify.org",
    ] {
        if let Ok(result) = backend
            .exec_on_host(&format!("curl -sf --max-time 5 {}", url))
            .await
        {
            if result.success() {
                let ip = result.stdout.trim().to_string();
                if !ip.is_empty() && ip.parse::<std::net::IpAddr>().is_ok() {
                    return ip;
                }
            }
        }
    }
    // Fallback: resolve from SSH config (host or ssh_host alias)
    if let Ok((hostname, ..)) = resolve_connection_info(config) {
        return hostname;
    }
    config
        .host
        .clone()
        .or_else(|| config.ssh_host.clone())
        .unwrap_or_else(|| "UNKNOWN_IP".to_string())
}

/// Deploy bot with progress updates sent to the TUI channel.
async fn deploy_bot_with_progress(
    config: &Config,
    token: &str,
    tx: &mpsc::Sender<BackendMsg>,
) -> Result<String, String> {
    let _ = tx.send(BackendMsg::DeployProgress(DeployStatus::Connecting));
    deploy_bot_inner(config, token, Some(tx)).await
}

pub async fn deploy_bot(config: &Config, token: &str) -> Result<String, String> {
    deploy_bot_inner(config, token, None).await
}

async fn deploy_bot_inner(
    config: &Config,
    token: &str,
    tx: Option<&mpsc::Sender<BackendMsg>>,
) -> Result<String, String> {
    let admin_id = config
        .telegram_admin_chat_id
        .ok_or_else(|| "admin_id is required for deploy".to_string())?;

    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;

    // Pull pre-built Docker image from GitHub Container Registry
    if let Some(tx) = tx {
        let _ = tx.send(BackendMsg::DeployProgress(DeployStatus::BuildingImage));
    }

    let image = &config.bot_image;
    let result = backend
        .exec_on_host(&format!("docker pull {} 2>&1", image))
        .await
        .map_err(|e| format!("Docker pull failed: {}", e))?;
    if !result.success() {
        return Err(format!("Docker pull failed: {}", result.combined_output()));
    }

    // Stop existing container if running
    let _ = backend
        .exec_on_host("docker stop axadmin 2>/dev/null; docker rm axadmin 2>/dev/null")
        .await;

    // Start container
    if let Some(tx) = tx {
        let _ = tx.send(BackendMsg::DeployProgress(DeployStatus::StartingBot));
    }
    // Get VPS public IP so the bot can generate correct vless:// URLs
    let vps_ip = get_vps_public_ip(&backend, config).await;

    let run_cmd = format!(
        "docker run -d --name axadmin --restart unless-stopped \
         -v /var/run/docker.sock:/var/run/docker.sock \
         -e TELEGRAM_TOKEN='{}' \
         -e ADMIN_ID='{}' \
         -e XRAY_CONTAINER='{}' \
         {} \
         --telegram-bot --local --container '{}' --admin-id {} --host '{}' 2>&1",
        token, admin_id, config.container, image, config.container, admin_id, vps_ip
    );
    let result = backend
        .exec_on_host(&run_cmd)
        .await
        .map_err(|e| format!("Docker start failed: {}", e))?;
    if !result.success() {
        return Err(format!("Docker start failed: {}", result.combined_output()));
    }

    // Verify container is running
    if let Some(tx) = tx {
        let _ = tx.send(BackendMsg::DeployProgress(DeployStatus::Verifying));
    }
    let result = backend
        .exec_on_host("docker inspect axadmin --format '{{.State.Status}}' 2>&1")
        .await
        .map_err(|e| format!("Verification failed: {}", e))?;

    let _ = backend.close().await;

    if !result.success() {
        return Err(format!("Verification failed: {}", result.combined_output()));
    }

    let status = result.stdout.trim().to_string();
    if status.contains("Up") || status.contains("running") {
        Ok("Bot deployed and running! Send /start to your bot.".to_string())
    } else {
        Err(format!("Bot container not running. Status: {}", status))
    }
}

async fn generate_url(config: &Config, uuid: &str, name: &str) -> Result<AddedUser, String> {
    let backend = connect_backend(config).await.map_err(|e| e.to_string())?;
    let vless_url = build_vless_url(&backend, uuid)
        .await
        .map_err(|e| e.to_string())?;
    let _ = backend.close().await;

    Ok(AddedUser {
        name: name.to_string(),
        uuid: uuid.to_string(),
        vless_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_connection_with_host() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 2222,
            user: "admin".to_string(),
            key_path: None,
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let (host, port, user, key) = resolve_connection_info(&config).unwrap();
        assert_eq!(host, "1.2.3.4");
        assert_eq!(port, 2222);
        assert_eq!(user, "admin");
        assert!(key.is_none());
    }

    #[test]
    fn test_resolve_connection_no_host() {
        let config = Config::default();
        let result = resolve_connection_info(&config);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_connection_ssh_host_not_in_config() {
        let config = Config {
            ssh_host: Some("nonexistent-alias".to_string()),
            host: None,
            port: 22,
            user: "root".to_string(),
            key_path: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        // Falls back to treating alias as hostname
        let (host, port, user, _key) = resolve_connection_info(&config).unwrap();
        assert_eq!(host, "nonexistent-alias");
        assert_eq!(port, 22);
        assert_eq!(user, "root");
    }

    #[test]
    fn test_resolve_connection_expands_tilde_in_key_path() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: Some(std::path::PathBuf::from("~/.ssh/id_ed25519")),
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let (_host, _port, _user, key) = resolve_connection_info(&config).unwrap();
        let key_path = key.expect("key_path should be Some");
        // Tilde should be expanded to the home directory
        assert!(
            !key_path.to_string_lossy().starts_with("~/"),
            "tilde should be expanded, got: {}",
            key_path.display()
        );
        assert!(
            key_path.to_string_lossy().ends_with(".ssh/id_ed25519"),
            "path suffix should be preserved, got: {}",
            key_path.display()
        );
    }

    #[test]
    fn test_resolve_connection_absolute_key_path_unchanged() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: Some(std::path::PathBuf::from("/home/user/.ssh/id_rsa")),
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let (_host, _port, _user, key) = resolve_connection_info(&config).unwrap();
        let key_path = key.expect("key_path should be Some");
        assert_eq!(key_path.to_string_lossy(), "/home/user/.ssh/id_rsa");
    }

    #[test]
    fn test_expand_key_path_none() {
        assert!(expand_key_path(None).is_none());
    }

    #[test]
    fn test_public_key_path_is_inside_container() {
        // The public key lives inside the Amnezia container (no bind mounts).
        // read_public_key() must use exec_in_container(), not exec_command().
        assert_eq!(PUBLIC_KEY_PATH, "/opt/amnezia/xray/xray_public.key");
        assert!(PUBLIC_KEY_PATH.starts_with("/opt/amnezia/"));
    }

    #[test]
    fn test_parse_xray_version_valid() {
        let body = r#"{"tag_name":"v25.8.3","name":"v25.8.3"}"#;
        assert_eq!(parse_xray_version_from_json(body), Some("25.8.3".into()));
    }

    #[test]
    fn test_parse_xray_version_no_v_prefix() {
        let body = r#"{"tag_name":"25.8.3"}"#;
        assert_eq!(parse_xray_version_from_json(body), Some("25.8.3".into()));
    }

    #[test]
    fn test_parse_xray_version_missing_field() {
        let body = r#"{"name":"v25.8.3"}"#;
        assert_eq!(parse_xray_version_from_json(body), None);
    }

    #[test]
    fn test_parse_xray_version_malformed_json() {
        assert_eq!(parse_xray_version_from_json("not json"), None);
        assert_eq!(parse_xray_version_from_json(""), None);
    }

    #[test]
    fn test_parse_xray_version_rejects_non_semver() {
        // Rate-limit or error bodies, prerelease tags, etc. must not render as versions.
        let body = r#"{"tag_name":"rate limit exceeded"}"#;
        assert_eq!(parse_xray_version_from_json(body), None);
        let body = r#"{"tag_name":"v1.0-beta"}"#;
        assert_eq!(parse_xray_version_from_json(body), None);
        let body = r#"{"tag_name":"v"}"#;
        assert_eq!(parse_xray_version_from_json(body), None);
        let body = r#"{"tag_name":""}"#;
        assert_eq!(parse_xray_version_from_json(body), None);
    }

    #[tokio::test]
    async fn test_deploy_bot_requires_admin_id() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(), // no admin_id
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let result = deploy_bot(&config, "123:abc").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("admin_id is required"));
    }

    #[tokio::test]
    async fn test_deploy_bot_with_admin_id_attempts_connection() {
        let config = Config {
            host: Some("192.0.2.1".to_string()), // non-routable IP
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: Some(123456789),
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        // With admin_id set, it should pass the admin_id check and fail at SSH connection
        let result = deploy_bot(&config, "123:abc").await;
        assert!(result.is_err());
        // Should NOT be the "admin_id is required" error
        let err = result.unwrap_err();
        assert!(!err.contains("admin_id is required"), "got: {}", err);
    }
}
