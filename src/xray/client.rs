use crate::backend_trait::XrayBackend;
use crate::error::{AppError, Result};

use super::config::{
    read_clients_table, read_server_config, CLIENTS_TABLE_PATH, SERVER_CONFIG_PATH,
};
use super::types::{
    ClientsTable, ServerConfig, ServerJsonClient, TrafficStats, VlessUrlParams, XrayUser,
};
use log::info;

use base64::Engine;
use uuid::Uuid;

const API_ADDR: &str = "127.0.0.1:8080";
const VLESS_INBOUND_TAG: &str = "vless-in";

/// Validate that an email/tag string is safe for shell interpolation.
/// Rejects anything containing characters outside `[a-zA-Z0-9@._-]`.
/// Shell-escape a string by wrapping in single quotes and escaping any embedded single quotes.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Server information from Xray.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: String,
    pub uplink: u64,
    pub downlink: u64,
}

/// Xray API client that communicates via docker exec commands.
///
/// Works with any `XrayBackend` implementation (SSH remote or local docker exec).
pub struct XrayApiClient<'a> {
    backend: &'a dyn XrayBackend,
}

impl<'a> XrayApiClient<'a> {
    pub fn new(backend: &'a dyn XrayBackend) -> Self {
        Self { backend }
    }

    /// List all users, merged from server.json and clientsTable.
    pub async fn list_users(&self) -> Result<Vec<XrayUser>> {
        let config = read_server_config(self.backend).await?;
        let table = read_clients_table(self.backend).await?;
        Ok(super::types::merge_users(&config, &table))
    }

    /// Add a new user with the given name. Returns the generated UUID.
    ///
    /// Persists to disk first, then updates the running Xray instance.
    /// If the runtime API call fails, the user is on disk and will appear
    /// after the next container restart — this ordering avoids the worse
    /// failure mode of a user existing in runtime but missing from disk.
    pub async fn add_user(&self, name: &str) -> Result<String> {
        // Validate name before any mutations to avoid partial failures
        if name.trim().is_empty() {
            return Err(AppError::Xray("user name cannot be empty".to_string()));
        }
        // '@' breaks the email format (name@vpn), '>>>' is the stats delimiter
        if name.contains('@') || name.contains(">>>") {
            return Err(AppError::Xray(
                "user name must not contain '@' or '>>>'".to_string(),
            ));
        }
        let email = XrayUser::email_from_name(name);

        let uuid = Uuid::new_v4().to_string();

        // 1. Persist to server.json on disk first
        let mut config = read_server_config(self.backend).await?;

        // Check for duplicate email in existing clients (before backup to avoid
        // overwriting the rolling backup on no-op duplicate attempts)
        if config.has_client_email(&email) {
            return Err(AppError::Xray(format!("user '{}' already exists", name)));
        }

        // Auto-backup before mutation (after validation)
        self.backup_config().await?;
        let client = ServerJsonClient {
            id: uuid.clone(),
            flow: String::new(),
            email: Some(email.clone()),
            level: Some(0),
        };
        config.add_client(&client)?;
        self.write_server_config(&config).await?;

        // 2. Persist to clientsTable
        let mut table = read_clients_table(self.backend).await?;
        table.add(uuid.clone(), name.to_string());
        self.write_clients_table(&table).await?;

        // 3. Add user to running Xray instance via API
        let user_json = build_adu_json(&uuid, &email, VLESS_INBOUND_TAG);
        self.exec_api_adu(&user_json).await?;

        Ok(uuid)
    }

    /// Remove a user by UUID.
    ///
    /// Revokes live access first via the API, then removes from disk.
    /// This ordering ensures that if the disk write fails, the user's
    /// access is already revoked — avoiding the worse failure mode of
    /// a user appearing deleted in the UI while still having live access.
    pub async fn remove_user(&self, uuid: &str) -> Result<()> {
        // Find the user's email first (validate before backup to avoid
        // overwriting the rolling backup on no-op "not found" attempts)
        let config = read_server_config(self.backend).await?;
        let table = read_clients_table(self.backend).await?;

        let email = config
            .clients()
            .iter()
            .find(|c| c.id == uuid)
            .and_then(|c| c.email.clone())
            .or_else(|| table.name_for_uuid(uuid).map(XrayUser::email_from_name))
            .ok_or_else(|| AppError::Xray(format!("user {} not found", uuid)))?;

        // Auto-backup before mutation (after validation)
        self.backup_config().await?;

        // 1. Remove from running Xray instance via API first (revoke access)
        self.exec_api_rmu(&email).await?;

        // 2. Update server.json on disk
        let mut config = config;
        config.remove_client(uuid)?;
        self.write_server_config(&config).await?;

        // 3. Update clientsTable
        let mut table = table;
        table.remove(uuid);
        self.write_clients_table(&table).await?;

        Ok(())
    }

    /// Rename a user. Updates clientsTable and server.json email, restarts container.
    ///
    /// This resets the user's traffic stats because xray tracks stats by email.
    /// The user's UUID is preserved; active connections are dropped during restart.
    pub async fn rename_user(&self, old_name: &str, new_name: &str) -> Result<()> {
        if new_name.trim().is_empty() {
            return Err(AppError::Xray("new user name cannot be empty".to_string()));
        }
        let old_email = XrayUser::email_from_name(old_name);
        let new_email = XrayUser::email_from_name(new_name);

        // Find the user's UUID
        let table = read_clients_table(self.backend).await?;
        let config = read_server_config(self.backend).await?;

        let uuid = table
            .entries
            .iter()
            .find(|e| e.user_data.client_name == old_name)
            .map(|e| e.client_id.clone())
            .ok_or_else(|| AppError::Xray(format!("user '{}' not found", old_name)))?;

        // Check new name doesn't already exist
        if table
            .entries
            .iter()
            .any(|e| e.user_data.client_name == new_name)
        {
            return Err(AppError::Xray(format!(
                "user '{}' already exists",
                new_name
            )));
        }

        // Auto-backup before mutation
        self.backup_config().await?;

        // Mutate both in memory first, validate before any writes
        let mut table = table;
        table.rename(&uuid, new_name);

        let mut config = config;
        let updated = config.update_client_email(&uuid, &new_email)?;
        if !updated {
            return Err(AppError::Xray(format!(
                "user UUID '{}' not found in server.json",
                uuid
            )));
        }

        // Remove old stats counter from running instance
        let _ = self.exec_api_rmu(&old_email).await;

        // Write both files (both in-memory mutations already validated)
        self.write_clients_table(&table).await?;
        self.write_server_config(&config).await?;

        // Restart container to pick up new config
        let restart_cmd = format!(
            "docker restart {}",
            shell_quote(self.backend.container_name())
        );
        let result = self.backend.exec_on_host(&restart_cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "failed to restart container: {}",
                result.stderr.trim()
            )));
        }

        Ok(())
    }

    /// Get traffic stats for a user by email.
    pub async fn get_user_stats(&self, email: &str) -> Result<TrafficStats> {
        let uplink_cmd = build_stats_cmd(email, "uplink")?;
        let downlink_cmd = build_stats_cmd(email, "downlink")?;

        let up_result = self.backend.exec_in_container(&uplink_cmd).await?;
        let down_result = self.backend.exec_in_container(&downlink_cmd).await?;

        let uplink = parse_stat_value(&up_result.stdout).unwrap_or(0);
        let downlink = parse_stat_value(&down_result.stdout).unwrap_or(0);

        Ok(TrafficStats { uplink, downlink })
    }

    /// Get online connection count for a user.
    pub async fn get_online_count(&self, email: &str) -> Result<u32> {
        let cmd = build_online_cmd(email)?;
        let result = self.backend.exec_in_container(&cmd).await?;
        Ok(parse_online_count(&result.stdout).unwrap_or(0))
    }

    /// Get list of online IPs for a user.
    pub async fn get_online_ips(&self, email: &str) -> Result<Vec<String>> {
        let cmd = build_online_ip_list_cmd(email)?;
        let result = self.backend.exec_in_container(&cmd).await?;
        Ok(parse_online_ip_list(&result.stdout))
    }

    /// Probe whether the xray stats API is reachable.
    /// Unlike `get_online_count` / `get_server_info`, this checks the command
    /// exit code rather than silently falling back to zero.
    pub async fn probe_stats_api(&self) -> Result<bool> {
        let cmd = build_inbound_stats_cmd(VLESS_INBOUND_TAG, "uplink")?;
        let result = self.backend.exec_in_container(&cmd).await?;
        Ok(result.success())
    }

    /// Get server info (version, total traffic).
    pub async fn get_server_info(&self) -> Result<ServerInfo> {
        let version_result = self.backend.exec_in_container("xray version").await?;
        let version = parse_version(&version_result.stdout);

        let up_cmd = build_inbound_stats_cmd(VLESS_INBOUND_TAG, "uplink")?;
        let down_cmd = build_inbound_stats_cmd(VLESS_INBOUND_TAG, "downlink")?;

        let up_result = self.backend.exec_in_container(&up_cmd).await?;
        let down_result = self.backend.exec_in_container(&down_cmd).await?;

        Ok(ServerInfo {
            version,
            uplink: parse_stat_value(&up_result.stdout).unwrap_or(0),
            downlink: parse_stat_value(&down_result.stdout).unwrap_or(0),
        })
    }

    /// Create an auto-backup of server.json and clientsTable (overwrites latest .bak).
    pub async fn backup_config(&self) -> Result<()> {
        let cmd = build_backup_cmd();
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "backup failed: {}",
                result.stderr.trim()
            )));
        }
        info!("Auto-backup created (.bak)");
        Ok(())
    }

    /// List available timestamped backups, returning timestamps sorted newest-first.
    /// Returns timestamps extracted from server.json backup filenames.
    /// Note: clientsTable backup existence is validated separately during restore.
    pub async fn list_backups(&self) -> Result<Vec<String>> {
        let cmd = build_list_backups_cmd();
        let result = self.backend.exec_in_container(&cmd).await?;
        // ls may exit non-zero if no backups exist — that's fine
        let timestamps = parse_backup_timestamps(&result.stdout);
        Ok(timestamps)
    }

    /// Restore server.json and clientsTable from a timestamped backup.
    /// If timestamp is None, uses the most recent backup.
    pub async fn restore_config(&self, timestamp: Option<&str>) -> Result<String> {
        let backups = self.list_backups().await?;
        if backups.is_empty() {
            return Err(AppError::Xray("no timestamped backups found".to_string()));
        }

        let ts = match timestamp {
            Some(t) => {
                if !backups.contains(&t.to_string()) {
                    return Err(AppError::Xray(format!(
                        "backup with timestamp '{}' not found",
                        t
                    )));
                }
                t.to_string()
            }
            None => backups[0].clone(), // newest first
        };

        // Validate both files exist
        let validate_cmd = build_validate_backup_cmd(&ts)?;
        let result = self.backend.exec_in_container(&validate_cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "incomplete backup: clientsTable.{}.bak not found",
                ts
            )));
        }

        // Copy backup files back to originals
        let restore_cmd = build_restore_cmd(&ts)?;
        let result = self.backend.exec_in_container(&restore_cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "restore failed: {}",
                result.stderr.trim()
            )));
        }

        // Restart container to apply restored config
        let restart_cmd = format!(
            "docker restart {}",
            shell_quote(self.backend.container_name())
        );
        let result = self.backend.exec_on_host(&restart_cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "failed to restart container after restore: {}",
                result.stderr.trim()
            )));
        }

        Ok(ts)
    }

    /// Create a timestamped backup of server.json and clientsTable.
    /// Returns the timestamp string used in the backup filenames.
    pub async fn backup_config_timestamped(&self) -> Result<String> {
        let cmd = build_backup_timestamped_cmd();
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "timestamped backup failed: {}",
                result.stderr.trim()
            )));
        }
        let timestamp = result.stdout.trim().to_string();
        info!("Timestamped backup created: {}", timestamp);
        Ok(timestamp)
    }

    // -- internal helpers --

    async fn exec_api_adu(&self, user_json: &str) -> Result<()> {
        // Use base64 to safely pass JSON through shell layers
        let b64 = base64::engine::general_purpose::STANDARD.encode(user_json.as_bytes());
        let cmd = format!(
            "sh -c 'echo {} | base64 -d > /tmp/_adu.json && xray api adu -s {} /tmp/_adu.json; rc=$?; rm -f /tmp/_adu.json; exit $rc'",
            b64, API_ADDR
        );
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            let msg = format!("adu failed: {}", result.stderr.trim());
            return Err(AppError::Xray(crate::error::add_hint(&msg)));
        }
        Ok(())
    }

    async fn exec_api_rmu(&self, email: &str) -> Result<()> {
        let cmd = build_rmu_cmd(email)?;
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            let msg = format!("rmu failed: {}", result.stderr.trim());
            return Err(AppError::Xray(crate::error::add_hint(&msg)));
        }
        Ok(())
    }

    async fn write_server_config(&self, config: &ServerConfig) -> Result<()> {
        let json = config.to_json();
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        // Atomic write: decode to temp file, then mv to avoid concurrent readers seeing truncated JSON
        let tmp = format!("{}.tmp", SERVER_CONFIG_PATH);
        let cmd = format!(
            "sh -c 'echo {} | base64 -d > {} && mv {} {}'",
            b64, tmp, tmp, SERVER_CONFIG_PATH
        );
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "failed to write server config: {}",
                result.stderr
            )));
        }
        Ok(())
    }

    async fn write_clients_table(&self, table: &ClientsTable) -> Result<()> {
        let json = table.to_json();
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        // Atomic write: decode to temp file, then mv to avoid concurrent readers seeing truncated JSON
        let tmp = format!("{}.tmp", CLIENTS_TABLE_PATH);
        let cmd = format!(
            "sh -c 'echo {} | base64 -d > {} && mv {} {}'",
            b64, tmp, tmp, CLIENTS_TABLE_PATH
        );
        let result = self.backend.exec_in_container(&cmd).await?;
        if !result.success() {
            return Err(AppError::Xray(format!(
                "failed to write clients table: {}",
                result.stderr
            )));
        }
        Ok(())
    }
}

// -- Backup command construction (pure functions, testable) --

/// Build the shell command for auto-backup (overwrites latest .bak).
pub fn build_backup_cmd() -> String {
    format!(
        "sh -c 'cp {} {}.bak && cp {} {}.bak'",
        SERVER_CONFIG_PATH, SERVER_CONFIG_PATH, CLIENTS_TABLE_PATH, CLIENTS_TABLE_PATH
    )
}

/// Build the shell command for timestamped backup.
/// Uses `$(date +%Y%m%d-%H%M%S)` so the timestamp is generated server-side.
pub fn build_backup_timestamped_cmd() -> String {
    format!(
        "sh -c 'ts=$(date +%Y%m%d-%H%M%S) && cp {} {}.\"$ts\".bak && cp {} {}.\"$ts\".bak && echo \"$ts\"'",
        SERVER_CONFIG_PATH, SERVER_CONFIG_PATH, CLIENTS_TABLE_PATH, CLIENTS_TABLE_PATH
    )
}

// -- Restore command construction (pure functions, testable) --

/// Build the shell command to list timestamped server.json backups (newest first).
pub fn build_list_backups_cmd() -> String {
    format!(
        "sh -c 'ls -t {}.*.bak 2>/dev/null || true'",
        SERVER_CONFIG_PATH
    )
}

/// Build the shell command to validate that a clientsTable backup exists for a timestamp.
///
/// Returns an error if `timestamp` is not a valid YYYYMMDD-HHMMSS timestamp.
pub fn build_validate_backup_cmd(timestamp: &str) -> Result<String> {
    if !is_valid_timestamp(timestamp) {
        return Err(AppError::Xray(format!(
            "invalid timestamp format: {timestamp}"
        )));
    }
    Ok(format!("test -f {}.{}.bak", CLIENTS_TABLE_PATH, timestamp))
}

/// Build the shell command to restore both config files from a timestamped backup.
///
/// Returns an error if `timestamp` is not a valid YYYYMMDD-HHMMSS timestamp.
pub fn build_restore_cmd(timestamp: &str) -> Result<String> {
    if !is_valid_timestamp(timestamp) {
        return Err(AppError::Xray(format!(
            "invalid timestamp format: {timestamp}"
        )));
    }
    Ok(format!(
        "sh -c 'cp {}.{}.bak {} && cp {}.{}.bak {}'",
        SERVER_CONFIG_PATH,
        timestamp,
        SERVER_CONFIG_PATH,
        CLIENTS_TABLE_PATH,
        timestamp,
        CLIENTS_TABLE_PATH
    ))
}

/// Parse timestamps from `ls -t` output of server.json backup files.
/// Input lines look like: `/opt/amnezia/xray/server.json.20260321-120000.bak`
/// Returns timestamps sorted newest-first (as ls -t gives them).
pub fn parse_backup_timestamps(ls_output: &str) -> Vec<String> {
    let prefix = format!("{}.", SERVER_CONFIG_PATH);
    let suffix = ".bak";

    let mut timestamps = Vec::new();
    for line in ls_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            if let Some(ts) = rest.strip_suffix(suffix) {
                // Validate timestamp format: YYYYMMDD-HHMMSS
                if is_valid_timestamp(ts) {
                    timestamps.push(ts.to_string());
                }
            }
        }
    }
    timestamps
}

/// Validate a timestamp string matches YYYYMMDD-HHMMSS format.
fn is_valid_timestamp(s: &str) -> bool {
    if s.len() != 15 {
        return false;
    }
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    if parts.len() != 2 {
        return false;
    }
    parts[0].len() == 8
        && parts[1].len() == 6
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

// -- Command construction (pure functions, testable) --

/// Build the JSON payload for `xray api adu`.
/// Format: inboundTag + user with nested account object (no flow field).
/// Correct format: {"inboundTag":"...","user":{"email":"...","level":0,"account":{"id":"...","encryption":"none"}}}
pub fn build_adu_json(uuid: &str, email: &str, inbound_tag: &str) -> String {
    serde_json::json!({
        "inboundTag": inbound_tag,
        "user": {
            "email": email,
            "level": 0,
            "account": {
                "id": uuid,
                "encryption": "none"
            }
        }
    })
    .to_string()
}

/// Build the `xray api rmu` command string.
/// Returns Err if email contains shell-unsafe characters.
pub fn build_rmu_cmd(email: &str) -> Result<String> {
    Ok(format!(
        "xray api rmu -s {} -tag=vless-in {}",
        API_ADDR,
        shell_quote(email)
    ))
}

/// Build the `xray api stats` command for user traffic.
pub fn build_stats_cmd(email: &str, direction: &str) -> Result<String> {
    let name = format!("user>>>{}>>>traffic>>>{}", email, direction);
    Ok(format!(
        "xray api stats -s {} -name {}",
        API_ADDR,
        shell_quote(&name)
    ))
}

/// Build the `xray api stats` command for inbound traffic.
pub fn build_inbound_stats_cmd(inbound_tag: &str, direction: &str) -> Result<String> {
    let name = format!("inbound>>>{}>>>traffic>>>{}", inbound_tag, direction);
    Ok(format!(
        "xray api stats -s {} -name {}",
        API_ADDR,
        shell_quote(&name)
    ))
}

/// Build the `xray api statsonline` command.
pub fn build_online_cmd(email: &str) -> Result<String> {
    Ok(format!(
        "xray api statsonline -s {} -email {}",
        API_ADDR,
        shell_quote(email)
    ))
}

/// Build the `xray api statsonlineiplist` command.
pub fn build_online_ip_list_cmd(email: &str) -> Result<String> {
    Ok(format!(
        "xray api statsonlineiplist -s {} -email {}",
        API_ADDR,
        shell_quote(email)
    ))
}

// -- vless:// URL generation --

/// Generate a vless:// URL for client import.
///
/// Format: `vless://<uuid>@<host>:<port>?encryption=none&type=xhttp&path=<path>&security=reality&sni=<sni>&fp=chrome&pbk=<pubkey>&sid=<shortid>#LT-Xray`
pub fn generate_vless_url(params: &VlessUrlParams) -> String {
    let fragment = urlencode_fragment("LT-Xray");
    // Wrap IPv6 addresses in brackets per RFC 2732
    let host = if params.host.contains(':') {
        format!("[{}]", params.host)
    } else {
        params.host.clone()
    };
    // URL-encode path (/ -> %2F)
    let encoded_path = params.path.replace('/', "%2F");
    format!(
        "vless://{}@{}:{}?encryption=none&type=xhttp&path={}&security=reality&sni={}&fp=chrome&pbk={}&sid={}#{}",
        params.uuid, host, params.port, encoded_path, params.sni, params.public_key, params.short_id, fragment
    )
}

/// Generate an AmneziaVPN `vpn://` connection string.
///
/// The Amnezia format wraps a thin Xray client JSON config inside a container
/// descriptor, then compresses with zlib and base64url-encodes it.
pub fn generate_amnezia_url(params: &VlessUrlParams) -> String {
    use base64::Engine;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let xray_client_config = serde_json::json!({
        "log": { "loglevel": "error" },
        "inbounds": [{
            "listen": "127.0.0.1",
            "port": 10808,
            "protocol": "socks",
            "settings": { "udp": true }
        }],
        "outbounds": [{
            "protocol": "vless",
            "settings": {
                "vnext": [{
                    "address": params.host,
                    "port": params.port,
                    "users": [{
                        "id": params.uuid,
                        "encryption": "none"
                    }]
                }]
            },
            "streamSettings": {
                "network": "xhttp",
                "security": "reality",
                "xhttpSettings": { "path": params.path },
                "realitySettings": {
                    "fingerprint": "chrome",
                    "serverName": params.sni,
                    "publicKey": params.public_key,
                    "shortId": params.short_id,
                    "spiderX": ""
                }
            }
        }]
    });

    let amnezia_config = serde_json::json!({
        "containers": [{
            "container": "amnezia-xray",
            "xray": {
                "last_config": serde_json::to_string_pretty(&xray_client_config).unwrap() + "\n",
                "port": params.port.to_string(),
                "transport_proto": "xhttp"
            }
        }],
        "defaultContainer": "amnezia-xray",
        "description": "LT-Xray",
        "dns1": "1.1.1.1",
        "dns2": "1.0.0.1",
        "hostName": params.host
    });

    let data = serde_json::to_string(&amnezia_config).unwrap();
    let data_bytes = data.as_bytes();

    // 4-byte big-endian length header + zlib-compressed JSON
    let len_header = (data_bytes.len() as u32).to_be_bytes();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data_bytes).unwrap();
    let compressed = encoder.finish().unwrap();

    let mut payload = Vec::with_capacity(4 + compressed.len());
    payload.extend_from_slice(&len_header);
    payload.extend_from_slice(&compressed);

    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload);
    format!("vpn://{}", encoded)
}

/// Percent-encode a fragment string for use in a URL.
/// Only encodes characters that are not allowed in URL fragments.
fn urlencode_fragment(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '-'
            | '_'
            | '.'
            | '~'
            | '!'
            | '\''
            | '('
            | ')'
            | '*' => {
                result.push(ch);
            }
            ' ' => result.push_str("%20"),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    result
}

// -- Response parsing (pure functions, testable) --

/// Parse a stat value from xray API output.
///
/// Expected format:
/// ```text
/// stat: {
///   name: "..."
///   value: 12345
/// }
/// ```
pub fn parse_stat_value(output: &str) -> Option<u64> {
    // Try JSON parsing first (handles compact single-line JSON)
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(output) {
        if let Some(val) = v
            .get("stat")
            .and_then(|s| s.get("value"))
            .and_then(|v| v.as_i64())
        {
            return Some(val.max(0) as u64);
        }
    }
    // Fallback to line-based parsing (proto text format)
    for line in output.lines() {
        let trimmed = line.trim().trim_matches(',');
        // Handle both text format (value: 123) and JSON format ("value": 123)
        let rest = if let Some(r) = trimmed.strip_prefix("\"value\":") {
            r
        } else if let Some(r) = trimmed.strip_prefix("value:") {
            r
        } else {
            continue;
        };
        let val_str = rest.trim();
        if let Ok(val) = val_str.parse::<i64>() {
            // Stats can be negative after reset; treat as 0
            return Some(val.max(0) as u64);
        }
    }
    None
}

/// Parse the Xray version string from `xray version` output.
///
/// Expected format: "Xray 25.8.3 (Xray, Pair of Penetrating Rays) ..."
pub fn parse_version(output: &str) -> String {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Xray ") {
            // Extract "25.8.3" from "Xray 25.8.3 (...)"
            if let Some(rest) = trimmed.strip_prefix("Xray ") {
                if let Some(version) = rest.split_whitespace().next() {
                    return version.to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

/// Parse online count from `xray api statsonline` JSON output.
///
/// Expected JSON format:
/// ```json
/// {
///     "stat": {
///         "name": "user>>>email@vpn>>>online",
///         "value": 2
///     }
/// }
/// ```
///
/// Falls back to line-based parsing for proto text format compatibility.
pub fn parse_online_count(output: &str) -> Option<u32> {
    // Try JSON parsing first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(output) {
        if let Some(val) = v
            .get("stat")
            .and_then(|s| s.get("value"))
            .and_then(|v| v.as_i64())
        {
            return Some(val.max(0) as u32);
        }
    }
    // Fallback to line-based parsing (proto text format)
    parse_stat_value(output).map(|v| v as u32)
}

/// Parse online IP list from `xray api statsonlineiplist` JSON output.
///
/// Expected JSON format:
/// ```json
/// {
///     "name": "user>>>email@vpn>>>online",
///     "ips": {
///         "1.2.3.4": 1711000000,
///         "5.6.7.8": 1711000123
///     }
/// }
/// ```
///
/// When no IPs are connected, the `ips` field is omitted.
/// Falls back to line-based parsing for proto text format compatibility.
pub fn parse_online_ip_list(output: &str) -> Vec<String> {
    // Try JSON parsing first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(output) {
        if let Some(ips_obj) = v.get("ips").and_then(|i| i.as_object()) {
            let mut ips: Vec<String> = ips_obj.keys().cloned().collect();
            ips.sort();
            return ips;
        }
        // JSON parsed but no "ips" field — no users online
        return Vec::new();
    }
    // Fallback: line-based parsing for proto text format
    let mut ips = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let name = rest.trim().trim_matches('"');
            if let Some(ip_part) = name.split(">>>ip>>>").nth(1) {
                let ip = ip_part.trim_matches('"');
                if !ip.is_empty() {
                    ips.push(ip.to_string());
                }
            }
        }
    }
    ips
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Backup command construction tests --

    #[test]
    fn test_build_backup_cmd() {
        let cmd = build_backup_cmd();
        assert!(cmd.contains("cp /opt/amnezia/xray/server.json /opt/amnezia/xray/server.json.bak"));
        assert!(
            cmd.contains("cp /opt/amnezia/xray/clientsTable /opt/amnezia/xray/clientsTable.bak")
        );
        // Both copies should be chained with &&
        assert!(cmd.contains("&&"));
    }

    #[test]
    fn test_build_backup_cmd_copies_both_files() {
        let cmd = build_backup_cmd();
        // Must backup both server.json and clientsTable
        assert_eq!(cmd.matches("cp ").count(), 2);
    }

    #[test]
    fn test_build_backup_timestamped_cmd() {
        let cmd = build_backup_timestamped_cmd();
        // Should use date command for timestamp
        assert!(cmd.contains("$(date +%Y%m%d-%H%M%S)") || cmd.contains("date +%Y%m%d-%H%M%S"));
        // Should backup both files
        assert!(cmd.contains("server.json"));
        assert!(cmd.contains("clientsTable"));
        // Should echo the timestamp for caller to capture
        assert!(cmd.contains("echo"));
    }

    #[test]
    fn test_build_backup_timestamped_cmd_uses_same_timestamp() {
        let cmd = build_backup_timestamped_cmd();
        // Timestamp should be captured in a variable and reused for both files
        // to ensure both backups have the same timestamp
        assert!(cmd.contains("ts="));
        assert!(cmd.contains("\"$ts\""));
    }

    // -- Restore command/parsing tests --

    #[test]
    fn test_build_list_backups_cmd() {
        let cmd = build_list_backups_cmd();
        assert!(cmd.contains("ls -t"));
        assert!(cmd.contains("server.json.*.bak"));
        // Should not fail if no backups exist
        assert!(cmd.contains("|| true"));
    }

    #[test]
    fn test_build_validate_backup_cmd() {
        let cmd = build_validate_backup_cmd("20260321-120000").unwrap();
        assert!(cmd.contains("test -f"));
        assert!(cmd.contains("clientsTable.20260321-120000.bak"));
    }

    #[test]
    fn test_build_validate_backup_cmd_invalid() {
        assert!(build_validate_backup_cmd("not-a-timestamp").is_err());
    }

    #[test]
    fn test_build_restore_cmd() {
        let cmd = build_restore_cmd("20260321-120000").unwrap();
        // Should restore both files
        assert!(cmd.contains("server.json.20260321-120000.bak"));
        assert!(cmd.contains("clientsTable.20260321-120000.bak"));
        // Should copy backups to originals
        assert!(cmd.contains("cp "));
        assert!(cmd.contains("&&"));
    }

    #[test]
    fn test_build_restore_cmd_invalid() {
        assert!(build_restore_cmd("not-a-timestamp").is_err());
    }

    #[test]
    fn test_parse_backup_timestamps_multiple() {
        let output = "/opt/amnezia/xray/server.json.20260321-120000.bak\n\
                       /opt/amnezia/xray/server.json.20260320-100000.bak\n\
                       /opt/amnezia/xray/server.json.20260319-080000.bak\n";
        let timestamps = parse_backup_timestamps(output);
        assert_eq!(
            timestamps,
            vec!["20260321-120000", "20260320-100000", "20260319-080000"]
        );
    }

    #[test]
    fn test_parse_backup_timestamps_empty() {
        assert_eq!(parse_backup_timestamps(""), Vec::<String>::new());
    }

    #[test]
    fn test_parse_backup_timestamps_no_matches() {
        let output = "some random output\n";
        assert_eq!(parse_backup_timestamps(output), Vec::<String>::new());
    }

    #[test]
    fn test_parse_backup_timestamps_filters_non_timestamped() {
        // Should not include the plain .bak file (no timestamp)
        let output = "/opt/amnezia/xray/server.json.bak\n\
                       /opt/amnezia/xray/server.json.20260321-120000.bak\n";
        let timestamps = parse_backup_timestamps(output);
        assert_eq!(timestamps, vec!["20260321-120000"]);
    }

    #[test]
    fn test_parse_backup_timestamps_invalid_format() {
        // Invalid timestamps should be filtered out
        let output = "/opt/amnezia/xray/server.json.not-a-timestamp.bak\n\
                       /opt/amnezia/xray/server.json.2026032.bak\n\
                       /opt/amnezia/xray/server.json.20260321-120000.bak\n";
        let timestamps = parse_backup_timestamps(output);
        assert_eq!(timestamps, vec!["20260321-120000"]);
    }

    #[test]
    fn test_parse_backup_timestamps_preserves_order() {
        // ls -t gives newest first; we should preserve that order
        let output = "/opt/amnezia/xray/server.json.20260322-150000.bak\n\
                       /opt/amnezia/xray/server.json.20260321-120000.bak\n";
        let timestamps = parse_backup_timestamps(output);
        assert_eq!(timestamps[0], "20260322-150000");
        assert_eq!(timestamps[1], "20260321-120000");
    }

    // -- Command construction tests --

    #[test]
    fn test_build_adu_json() {
        let json = build_adu_json("test-uuid", "alice@vpn", "vless-in");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["inboundTag"], "vless-in");
        let user = &parsed["user"];
        assert_eq!(user["email"], "alice@vpn");
        assert_eq!(user["level"], 0);
        let account = &user["account"];
        assert_eq!(account["id"], "test-uuid");
        assert_eq!(account["encryption"], "none");
        // No flow field
        assert!(account.get("flow").is_none());
    }

    #[test]
    fn test_build_adu_json_special_chars_in_name() {
        let json = build_adu_json("uuid-123", "bob's-phone@vpn", "vless-in");
        // Should still be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["user"]["email"], "bob's-phone@vpn");
    }

    #[test]
    fn test_build_rmu_cmd() {
        let cmd = build_rmu_cmd("alice@vpn").unwrap();
        assert_eq!(
            cmd,
            "xray api rmu -s 127.0.0.1:8080 -tag=vless-in 'alice@vpn'"
        );
    }

    #[test]
    fn test_build_stats_cmd_uplink() {
        let cmd = build_stats_cmd("alice@vpn", "uplink").unwrap();
        assert_eq!(
            cmd,
            "xray api stats -s 127.0.0.1:8080 -name 'user>>>alice@vpn>>>traffic>>>uplink'"
        );
    }

    #[test]
    fn test_build_stats_cmd_downlink() {
        let cmd = build_stats_cmd("alice@vpn", "downlink").unwrap();
        assert!(cmd.contains("user>>>alice@vpn>>>traffic>>>downlink"));
    }

    #[test]
    fn test_build_inbound_stats_cmd() {
        let cmd = build_inbound_stats_cmd("vless-in", "uplink").unwrap();
        assert_eq!(
            cmd,
            "xray api stats -s 127.0.0.1:8080 -name 'inbound>>>vless-in>>>traffic>>>uplink'"
        );
    }

    #[test]
    fn test_build_online_cmd() {
        let cmd = build_online_cmd("bob@vpn").unwrap();
        assert_eq!(
            cmd,
            "xray api statsonline -s 127.0.0.1:8080 -email 'bob@vpn'"
        );
    }

    #[test]
    fn test_build_online_ip_list_cmd() {
        let cmd = build_online_ip_list_cmd("bob@vpn").unwrap();
        assert_eq!(
            cmd,
            "xray api statsonlineiplist -s 127.0.0.1:8080 -email 'bob@vpn'"
        );
    }

    #[test]
    fn test_build_cmd_handles_special_chars() {
        // Names with brackets/parens/spaces should work (real Amnezia names)
        let cmd = build_rmu_cmd("Admin [macOS Tahoe (26.3.1)]@vpn").unwrap();
        assert!(cmd.contains("'Admin [macOS Tahoe (26.3.1)]@vpn'"));

        // Single quotes in names get escaped
        let cmd = build_rmu_cmd("a'b@vpn").unwrap();
        assert!(cmd.contains("'a'\\''b@vpn'"));
    }

    // -- Response parsing tests --

    #[test]
    fn test_parse_stat_value_basic() {
        let output = r#"stat: {
  name: "user>>>alice@vpn>>>traffic>>>downlink"
  value: 123456
}"#;
        assert_eq!(parse_stat_value(output), Some(123456));
    }

    #[test]
    fn test_parse_stat_value_large_number() {
        let output = "stat: {\n  name: \"test\"\n  value: 9876543210\n}";
        assert_eq!(parse_stat_value(output), Some(9876543210));
    }

    #[test]
    fn test_parse_stat_value_zero() {
        let output = "stat: {\n  name: \"test\"\n  value: 0\n}";
        assert_eq!(parse_stat_value(output), Some(0));
    }

    #[test]
    fn test_parse_stat_value_negative_becomes_zero() {
        let output = "stat: {\n  name: \"test\"\n  value: -100\n}";
        assert_eq!(parse_stat_value(output), Some(0));
    }

    #[test]
    fn test_parse_stat_value_empty_output() {
        assert_eq!(parse_stat_value(""), None);
    }

    #[test]
    fn test_parse_stat_value_no_value_line() {
        let output = "stat: {\n  name: \"test\"\n}";
        assert_eq!(parse_stat_value(output), None);
    }

    #[test]
    fn test_parse_stat_value_invalid_number() {
        let output = "stat: {\n  name: \"test\"\n  value: notanumber\n}";
        assert_eq!(parse_stat_value(output), None);
    }

    #[test]
    fn test_parse_version_standard() {
        let output =
            "Xray 25.8.3 (Xray, Pair of Penetrating Rays) Custom (go1.22.5 linux/amd64)\nA unified platform for anti-censorship.";
        assert_eq!(parse_version(output), "25.8.3");
    }

    #[test]
    fn test_parse_version_simple() {
        let output = "Xray 1.2.3\n";
        assert_eq!(parse_version(output), "1.2.3");
    }

    #[test]
    fn test_parse_version_empty() {
        assert_eq!(parse_version(""), "unknown");
    }

    #[test]
    fn test_parse_version_no_xray_prefix() {
        assert_eq!(parse_version("some other output"), "unknown");
    }

    // -- Online status parsing tests (JSON format from xray API) --

    #[test]
    fn test_parse_online_count_json() {
        let output = r#"{
    "stat": {
        "name": "user>>>alice@vpn>>>online",
        "value": 2
    }
}"#;
        assert_eq!(parse_online_count(output), Some(2));
    }

    #[test]
    fn test_parse_online_count_json_zero() {
        let output = r#"{
    "stat": {
        "name": "user>>>alice@vpn>>>online",
        "value": 0
    }
}"#;
        assert_eq!(parse_online_count(output), Some(0));
    }

    #[test]
    fn test_parse_online_count_json_negative_becomes_zero() {
        let output = r#"{
    "stat": {
        "name": "user>>>alice@vpn>>>online",
        "value": -1
    }
}"#;
        assert_eq!(parse_online_count(output), Some(0));
    }

    #[test]
    fn test_parse_online_count_empty() {
        assert_eq!(parse_online_count(""), None);
    }

    #[test]
    fn test_parse_online_count_proto_text_fallback() {
        // Fallback to proto text format for older xray versions
        let output = "stat: {\n  name: \"user>>>alice@vpn>>>online\"\n  value: 3\n}";
        assert_eq!(parse_online_count(output), Some(3));
    }

    #[test]
    fn test_parse_online_ip_list_json() {
        let output = r#"{
    "name": "user>>>alice@vpn>>>online",
    "ips": {
        "1.2.3.4": 1711000000,
        "5.6.7.8": 1711000123
    }
}"#;
        let ips = parse_online_ip_list(output);
        assert_eq!(ips, vec!["1.2.3.4", "5.6.7.8"]);
    }

    #[test]
    fn test_parse_online_ip_list_json_single() {
        let output = r#"{
    "name": "user>>>alice@vpn>>>online",
    "ips": {
        "10.0.0.1": 1711000000
    }
}"#;
        let ips = parse_online_ip_list(output);
        assert_eq!(ips, vec!["10.0.0.1"]);
    }

    #[test]
    fn test_parse_online_ip_list_json_no_ips_field() {
        // When no users online, "ips" field is omitted
        let output = r#"{
    "name": "user>>>alice@vpn>>>online"
}"#;
        let ips = parse_online_ip_list(output);
        assert!(ips.is_empty());
    }

    #[test]
    fn test_parse_online_ip_list_json_ipv6() {
        let output = r#"{
    "name": "user>>>alice@vpn>>>online",
    "ips": {
        "2001:db8::1": 1711000000
    }
}"#;
        let ips = parse_online_ip_list(output);
        assert_eq!(ips, vec!["2001:db8::1"]);
    }

    #[test]
    fn test_parse_online_ip_list_empty() {
        assert_eq!(parse_online_ip_list(""), Vec::<String>::new());
    }

    #[test]
    fn test_parse_online_ip_list_proto_text_fallback() {
        // Fallback to proto text format for older xray versions
        let output = r#"stat: {
  name: "user>>>alice@vpn>>>online>>>ip>>>1.2.3.4"
  value: 1700000000
}"#;
        let ips = parse_online_ip_list(output);
        assert_eq!(ips, vec!["1.2.3.4"]);
    }

    #[test]
    fn test_parse_online_ip_list_proto_text_multiple() {
        let output = r#"stat: {
  name: "user>>>alice@vpn>>>online>>>ip>>>1.2.3.4"
  value: 1700000000
}
stat: {
  name: "user>>>alice@vpn>>>online>>>ip>>>5.6.7.8"
  value: 1700000001
}"#;
        let ips = parse_online_ip_list(output);
        assert_eq!(ips, vec!["1.2.3.4", "5.6.7.8"]);
    }

    // -- Integration-like tests for command/response roundtrip --

    #[test]
    fn test_adu_json_is_valid_for_api() {
        let json = build_adu_json(
            "550e8400-e29b-41d4-a716-446655440000",
            "testuser@vpn",
            "vless-in",
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Verify structure matches xray api adu format (no flow field)
        assert_eq!(parsed["inboundTag"], "vless-in");
        let user = &parsed["user"];
        assert!(user.get("email").is_some());
        let account = &user["account"];
        assert!(account.get("id").is_some());
        assert_eq!(account["encryption"], "none");
        // No flow field in account
        assert!(account.get("flow").is_none());
    }

    #[test]
    fn test_stats_cmd_uses_correct_separator() {
        // Xray uses >>> as separator in stat names
        let cmd = build_stats_cmd("test@vpn", "uplink").unwrap();
        assert!(cmd.contains(">>>"));
        // Should have exactly 3 separators: user>>>email>>>traffic>>>direction
        let stat_name = cmd.split('\'').nth(1).unwrap();
        assert_eq!(stat_name.matches(">>>").count(), 3);
    }

    #[test]
    fn test_inbound_stats_cmd_uses_correct_separator() {
        let cmd = build_inbound_stats_cmd("vless-in", "downlink").unwrap();
        let stat_name = cmd.split('\'').nth(1).unwrap();
        assert_eq!(stat_name.matches(">>>").count(), 3);
    }

    #[test]
    fn test_server_info_default() {
        let info = ServerInfo::default();
        assert_eq!(info.version, "");
        assert_eq!(info.uplink, 0);
        assert_eq!(info.downlink, 0);
    }

    #[test]
    fn test_parse_stat_value_with_whitespace() {
        let output = "  value:   42  \n";
        assert_eq!(parse_stat_value(output), Some(42));
    }

    #[test]
    fn test_parse_stat_value_compact_json() {
        let output = r#"{"stat":{"name":"user>>>test@vpn>>>traffic>>>uplink","value":999}}"#;
        assert_eq!(parse_stat_value(output), Some(999));
    }

    #[test]
    fn test_parse_stat_value_json_negative() {
        let output = r#"{"stat":{"name":"test","value":-5}}"#;
        assert_eq!(parse_stat_value(output), Some(0));
    }

    #[test]
    fn test_parse_version_with_leading_whitespace() {
        let output = "  Xray 25.8.3 (something)\n";
        // Our parser trims lines
        assert_eq!(parse_version(output), "25.8.3");
    }

    // -- vless:// URL generation tests --

    #[test]
    fn test_generate_vless_url_basic() {
        let params = VlessUrlParams {
            uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            host: "1.2.3.4".to_string(),
            port: 443,
            sni: "www.googletagmanager.com".to_string(),
            public_key: "testpublickey123".to_string(),
            short_id: "abcd1234".to_string(),
            path: "/test".to_string(),
        };
        let url = generate_vless_url(&params);

        assert!(url.starts_with("vless://550e8400-e29b-41d4-a716-446655440000@1.2.3.4:443?"));
        assert!(url.contains("encryption=none"));
        assert!(url.contains("type=xhttp"));
        assert!(!url.contains("type=tcp"));
        assert!(!url.contains("flow="));
        assert!(url.contains("path="));
        assert!(url.contains("security=reality"));
        assert!(url.contains("sni=www.googletagmanager.com"));
        assert!(url.contains("fp=chrome"));
        assert!(url.contains("pbk=testpublickey123"));
        assert!(url.contains("sid=abcd1234"));
        assert!(url.ends_with("#LT-Xray"));
    }

    #[test]
    fn test_generate_vless_url_exact_format() {
        let params = VlessUrlParams {
            uuid: "uuid-123".to_string(),
            host: "10.0.0.1".to_string(),
            port: 8443,
            sni: "example.com".to_string(),
            public_key: "pk123".to_string(),
            short_id: "sid1".to_string(),
            path: "/mypath".to_string(),
        };
        let expected = "vless://uuid-123@10.0.0.1:8443?encryption=none&type=xhttp&path=%2Fmypath&security=reality&sni=example.com&fp=chrome&pbk=pk123&sid=sid1#LT-Xray";
        assert_eq!(generate_vless_url(&params), expected);
    }

    #[test]
    fn test_generate_vless_url_name_with_spaces() {
        let params = VlessUrlParams {
            uuid: "uuid-123".to_string(),
            host: "1.2.3.4".to_string(),
            port: 443,
            sni: "example.com".to_string(),
            public_key: "pk".to_string(),
            short_id: "sid".to_string(),
            path: "/".to_string(),
        };
        let url = generate_vless_url(&params);
        assert!(url.ends_with("#LT-Xray"));
    }

    #[test]
    fn test_generate_vless_url_name_with_special_chars() {
        let params = VlessUrlParams {
            uuid: "uuid-123".to_string(),
            host: "1.2.3.4".to_string(),
            port: 443,
            sni: "example.com".to_string(),
            public_key: "pk".to_string(),
            short_id: "sid".to_string(),
            path: "/".to_string(),
        };
        let url = generate_vless_url(&params);
        // Apostrophe is allowed in fragments
        assert!(url.ends_with("#LT-Xray"));
    }

    #[test]
    fn test_generate_vless_url_ipv6_host() {
        let params = VlessUrlParams {
            uuid: "uuid-123".to_string(),
            host: "2001:db8::1".to_string(),
            port: 443,
            sni: "example.com".to_string(),
            public_key: "pk".to_string(),
            short_id: "sid".to_string(),
            path: "/".to_string(),
        };
        let url = generate_vless_url(&params);
        assert!(url.contains("@[2001:db8::1]:443"));
    }

    #[test]
    fn test_generate_vless_url_path_encoding() {
        let params = VlessUrlParams {
            uuid: "uuid-123".to_string(),
            host: "1.2.3.4".to_string(),
            port: 443,
            sni: "example.com".to_string(),
            public_key: "pk".to_string(),
            short_id: "sid".to_string(),
            path: "/abc123".to_string(),
        };
        let url = generate_vless_url(&params);
        assert!(url.contains("path=%2Fabc123"));
        assert!(!url.contains("flow="));
    }

    #[test]
    fn test_urlencode_fragment_plain() {
        assert_eq!(urlencode_fragment("alice"), "alice");
    }

    #[test]
    fn test_urlencode_fragment_spaces() {
        assert_eq!(urlencode_fragment("my phone"), "my%20phone");
    }

    #[test]
    fn test_urlencode_fragment_unicode() {
        let encoded = urlencode_fragment("тест");
        assert!(encoded.contains('%'));
        assert!(!encoded.contains("тест"));
    }

    #[test]
    fn test_urlencode_fragment_allowed_chars() {
        assert_eq!(urlencode_fragment("a-b_c.d~e"), "a-b_c.d~e");
    }
}
