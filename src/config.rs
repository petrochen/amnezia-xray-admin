use crate::error::{AppError, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Default SSH port
const DEFAULT_SSH_PORT: u16 = 22;
/// Default SSH user
const DEFAULT_SSH_USER: &str = "root";
/// Default container name
const DEFAULT_CONTAINER: &str = "amnezia-xray";
const DEFAULT_BOT_IMAGE: &str = "ghcr.io/gaiverrr/amnezia-xray-admin:latest";

/// Validate that a container name contains only safe characters.
/// Docker container names allow `[a-zA-Z0-9][a-zA-Z0-9_.-]`.
fn is_valid_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// CLI arguments for amnezia-xray-admin
#[derive(Parser, Debug)]
#[command(name = "amnezia-xray-admin")]
#[command(about = "Hacker-aesthetic TUI dashboard for managing Amnezia VPN's Xray server")]
#[command(
    long_about = "Hacker-aesthetic TUI dashboard for managing Amnezia VPN's Xray (VLESS + XTLS-Reality) server.\n\nConnects to your VPS via SSH, talks to the Xray gRPC API for live user management\nand traffic stats. No container restarts needed.\n\nOn first run, a setup wizard guides you through the SSH connection.\nConfig is saved to ~/.config/amnezia-xray-admin/config.toml."
)]
#[command(after_help = "\
EXAMPLES:
  Launch TUI (interactive dashboard):
    amnezia-xray-admin
    amnezia-xray-admin --ssh-host vps-vpn
    amnezia-xray-admin --host 1.2.3.4 --key ~/.ssh/id_ed25519

  User management:
    amnezia-xray-admin --list-users
    amnezia-xray-admin --add-user Friend
    amnezia-xray-admin --delete-user Friend --yes
    amnezia-xray-admin --rename-user \"Old Name\" \"New Name\"
    amnezia-xray-admin --user-url Friend
    amnezia-xray-admin --user-qr Friend

  Server info:
    amnezia-xray-admin --check-server
    amnezia-xray-admin --server-info
    amnezia-xray-admin --online-status

  Backups:
    amnezia-xray-admin --backup
    amnezia-xray-admin --restore
    amnezia-xray-admin --restore 20260321-143000

  Telegram bot:
    amnezia-xray-admin --deploy-bot --telegram-token <TOKEN> --admin-id <ID>
    amnezia-xray-admin --telegram-bot --local --admin-id <ID> --container amnezia-xray")]
#[command(version)]
pub struct Cli {
    /// SSH host (IP or hostname) to connect to
    #[arg(long = "host")]
    pub host: Option<String>,

    /// SSH port
    #[arg(long = "port")]
    pub port: Option<u16>,

    /// SSH user
    #[arg(long = "user")]
    pub user: Option<String>,

    /// Path to SSH private key
    #[arg(long = "key")]
    pub key_path: Option<PathBuf>,

    /// SSH config Host alias (e.g. vps-vpn). If set, host/port/user/key are resolved from ~/.ssh/config
    #[arg(long = "ssh-host")]
    pub ssh_host: Option<String>,

    /// Docker container name running Xray
    #[arg(long = "container")]
    pub container: Option<String>,

    /// List users and exit (non-interactive mode)
    #[arg(long = "list-users")]
    pub list_users: bool,

    /// Check server: verify API setup, print version, user count, and exit
    #[arg(long = "check-server")]
    pub check_server: bool,

    /// Get vless:// URL for a user by name and exit
    #[arg(long = "user-url")]
    pub user_url: Option<String>,

    /// Show QR code for a user's vless:// URL in terminal and exit
    #[arg(long = "user-qr")]
    pub user_qr: Option<String>,

    /// Get AmneziaVPN vpn:// connection string for a user by name and exit
    #[arg(long = "user-vpn")]
    pub user_vpn: Option<String>,

    /// Show online status for all users and exit
    #[arg(long = "online-status")]
    pub online_status: bool,

    /// Show server info (version, traffic, user count, API status) and exit
    #[arg(long = "server-info")]
    pub server_info: bool,

    /// Use local backend (direct docker exec) instead of SSH — for running on VPS
    #[arg(long = "local")]
    pub local: bool,

    /// Run as Telegram bot daemon (requires --telegram-token or TELEGRAM_TOKEN env var)
    #[arg(long = "telegram-bot")]
    pub telegram_bot: bool,

    /// Telegram bot token (can also be set via TELEGRAM_TOKEN env var)
    #[arg(long = "telegram-token", env = "TELEGRAM_TOKEN")]
    pub telegram_token: Option<String>,

    /// Deploy Telegram bot to VPS via SSH and exit
    #[arg(long = "deploy-bot")]
    pub deploy_bot: bool,

    /// Telegram admin chat ID (your Telegram user ID; send /start to @userinfobot to find it)
    #[arg(long = "admin-id", env = "ADMIN_ID")]
    pub admin_id: Option<i64>,

    /// Create a timestamped backup of server.json and clientsTable
    #[arg(long = "backup")]
    pub backup: bool,

    /// Restore server.json and clientsTable from a backup. Optionally specify a timestamp (YYYYMMDD-HHMMSS); defaults to latest.
    #[arg(long = "restore", num_args = 0..=1, default_missing_value = "")]
    pub restore: Option<String>,

    /// Add a new user and print their vless:// URL
    #[arg(long = "add-user")]
    pub add_user: Option<String>,

    /// Delete a user by name
    #[arg(long = "delete-user")]
    pub delete_user: Option<String>,

    /// Rename a user: --rename-user <OLD_NAME> <NEW_NAME>
    #[arg(long = "rename-user", num_args = 2, value_names = ["OLD_NAME", "NEW_NAME"])]
    pub rename_user: Option<Vec<String>>,

    /// Skip interactive confirmation (for --delete-user)
    #[arg(long = "yes")]
    pub yes: bool,

    /// Full snapshot: backup all config, keys, and xray binary to host
    #[arg(long = "snapshot")]
    pub snapshot: bool,

    /// Restore from a snapshot. Optionally specify a tag; defaults to latest.
    #[arg(long = "snapshot-restore", num_args = 0..=1, default_missing_value = "")]
    pub snapshot_restore: Option<String>,

    /// List available snapshots
    #[arg(long = "snapshot-list")]
    pub snapshot_list: bool,

    /// Upgrade Xray binary to latest release (creates snapshot first)
    #[arg(long = "upgrade-xray")]
    pub upgrade_xray: bool,

    /// Host directory for storing snapshots (default: /data/projects/xray-backup)
    #[arg(long = "snapshot-dir")]
    pub snapshot_dir: Option<String>,

    /// Run as HTTP agent (bridge stats/management API)
    #[arg(long = "http-agent")]
    pub http_agent: bool,

    /// Secret key for HTTP agent authentication (required with --http-agent)
    #[arg(long = "secret")]
    pub secret: Option<String>,

    /// HTTP agent listen port (default: 9090)
    #[arg(long = "agent-port", default_value = "9090")]
    pub agent_port: u16,

    /// Bridge agent URL (e.g. http://51.250.73.78:9090/secret-key)
    #[arg(long = "bridge-agent-url", env = "BRIDGE_AGENT_URL")]
    pub bridge_agent_url: Option<String>,
}

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// SSH host (IP or hostname)
    pub host: Option<String>,
    /// SSH port (default: 22)
    #[serde(default = "default_port")]
    pub port: u16,
    /// SSH user (default: root)
    #[serde(default = "default_user")]
    pub user: String,
    /// Path to SSH private key
    pub key_path: Option<PathBuf>,
    /// SSH config Host alias (e.g. vps-vpn)
    pub ssh_host: Option<String>,
    /// Docker container name (default: amnezia-xray)
    #[serde(default = "default_container")]
    pub container: String,
    /// Telegram bot token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_token: Option<String>,
    /// Telegram bot admin chat ID (set via --admin-id or ADMIN_ID env var)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_admin_chat_id: Option<i64>,
    /// Docker image for --deploy-bot (default: ghcr.io/gaiverrr/amnezia-xray-admin:latest)
    #[serde(default = "default_bot_image")]
    pub bot_image: String,
    /// Host directory for snapshots (default: /data/projects/xray-backup)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_dir: Option<String>,
    /// Bridge agent base URL (e.g. http://51.250.73.78:9090/secret-key)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge_agent_url: Option<String>,
}

fn default_bot_image() -> String {
    DEFAULT_BOT_IMAGE.to_string()
}

fn default_port() -> u16 {
    DEFAULT_SSH_PORT
}

fn default_user() -> String {
    DEFAULT_SSH_USER.to_string()
}

fn default_container() -> String {
    DEFAULT_CONTAINER.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: None,
            port: DEFAULT_SSH_PORT,
            user: DEFAULT_SSH_USER.to_string(),
            key_path: None,
            ssh_host: None,
            container: DEFAULT_CONTAINER.to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: DEFAULT_BOT_IMAGE.to_string(),
            snapshot_dir: None,
            bridge_agent_url: None,
        }
    }
}

impl Config {
    /// Returns the snapshot directory, using the configured value or the default.
    pub fn snapshot_dir(&self) -> &str {
        self.snapshot_dir
            .as_deref()
            .unwrap_or(crate::xray::snapshot::DEFAULT_SNAPSHOT_HOST_DIR)
    }

    /// Returns the config file path: ~/.config/amnezia-xray-admin/config.toml
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| AppError::Config("cannot determine config directory".to_string()))?;
        Ok(config_dir.join("amnezia-xray-admin").join("config.toml"))
    }

    /// Load config from the default config file path.
    /// Returns default config if the file does not exist.
    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        Self::load_from(&path)
    }

    /// Load config from a specific path.
    /// Returns default config if the file does not exist.
    pub fn load_from(path: &PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        if !is_valid_container_name(&config.container) {
            return Err(AppError::Config(format!(
                "invalid container name '{}': must start with alphanumeric, then only alphanumeric, hyphen, underscore, and dot",
                config.container
            )));
        }
        Ok(config)
    }

    /// Save config to the default config file path with 0600 permissions.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        self.save_to(&path)
    }

    /// Save config to a specific path with 0600 permissions.
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| AppError::Config(format!("TOML serialize error: {}", e)))?;
        fs::write(path, &content)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(path, perms)?;
        }

        Ok(())
    }

    /// Merge CLI arguments into this config. CLI args take precedence.
    pub fn merge_cli(&mut self, cli: &Cli) {
        if let Some(ref host) = cli.host {
            self.host = Some(host.clone());
        }
        if let Some(port) = cli.port {
            self.port = port;
        }
        if let Some(ref user) = cli.user {
            self.user = user.clone();
        }
        if let Some(ref key_path) = cli.key_path {
            self.key_path = Some(key_path.clone());
        }
        if let Some(ref ssh_host) = cli.ssh_host {
            self.ssh_host = Some(ssh_host.clone());
        }
        if let Some(admin_id) = cli.admin_id {
            self.telegram_admin_chat_id = Some(admin_id);
        }
        if let Some(ref dir) = cli.snapshot_dir {
            self.snapshot_dir = Some(dir.clone());
        }
        if let Some(ref container) = cli.container {
            if is_valid_container_name(container) {
                self.container = container.clone();
            } else {
                eprintln!(
                    "Warning: invalid container name '{}', using '{}'",
                    container, self.container
                );
            }
        }
        if let Some(ref url) = cli.bridge_agent_url {
            self.bridge_agent_url = Some(url.clone());
        }
    }

    /// Returns true if this config has enough info to attempt an SSH connection.
    /// Either ssh_host (config alias) or host must be set.
    pub fn has_connection_info(&self) -> bool {
        self.ssh_host.is_some() || self.host.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.host, None);
        assert_eq!(config.port, 22);
        assert_eq!(config.user, "root");
        assert_eq!(config.key_path, None);
        assert_eq!(config.ssh_host, None);
        assert_eq!(config.container, "amnezia-xray");
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let path = PathBuf::from("/tmp/nonexistent-amnezia-test-config.toml");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn test_load_full_config() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
host = "192.168.1.100"
port = 2222
user = "admin"
key_path = "/home/admin/.ssh/id_ed25519"
ssh_host = "vps-vpn"
container = "my-xray"
"#
        )
        .unwrap();

        let config = Config::load_from(&f.path().to_path_buf()).unwrap();
        assert_eq!(config.host.as_deref(), Some("192.168.1.100"));
        assert_eq!(config.port, 2222);
        assert_eq!(config.user, "admin");
        assert_eq!(
            config.key_path,
            Some(PathBuf::from("/home/admin/.ssh/id_ed25519"))
        );
        assert_eq!(config.ssh_host.as_deref(), Some("vps-vpn"));
        assert_eq!(config.container, "my-xray");
    }

    #[test]
    fn test_load_partial_config_uses_defaults() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
host = "10.0.0.1"
"#
        )
        .unwrap();

        let config = Config::load_from(&f.path().to_path_buf()).unwrap();
        assert_eq!(config.host.as_deref(), Some("10.0.0.1"));
        assert_eq!(config.port, 22);
        assert_eq!(config.user, "root");
        assert_eq!(config.container, "amnezia-xray");
    }

    #[test]
    fn test_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 2222,
            user: "deployer".to_string(),
            key_path: Some(PathBuf::from("/keys/id_rsa")),
            ssh_host: Some("my-server".to_string()),
            container: "xray-test".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        config.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(config, loaded);
    }

    #[cfg(unix)]
    #[test]
    fn test_save_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = Config::default();
        config.save_to(&path).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_save_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");

        let config = Config::default();
        config.save_to(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn test_merge_cli_all_fields() {
        let mut config = Config::default();
        let cli = Cli {
            host: Some("5.6.7.8".to_string()),
            port: Some(3333),
            user: Some("testuser".to_string()),
            key_path: Some(PathBuf::from("/tmp/key")),
            ssh_host: Some("alias".to_string()),
            container: Some("ctr".to_string()),
            list_users: false,
            check_server: false,
            user_url: None,
            user_qr: None,
            user_vpn: None,
            online_status: false,
            server_info: false,
            local: false,
            telegram_bot: false,
            telegram_token: None,
            deploy_bot: false,
            admin_id: None,
            backup: false,
            restore: None,
            add_user: None,
            rename_user: None,
            delete_user: None,
            yes: false,
            snapshot: false,
            snapshot_restore: None,
            snapshot_list: false,
            upgrade_xray: false,
            snapshot_dir: None,
            http_agent: false,
            secret: None,
            agent_port: 9090,
            bridge_agent_url: None,
        };
        config.merge_cli(&cli);

        assert_eq!(config.host.as_deref(), Some("5.6.7.8"));
        assert_eq!(config.port, 3333);
        assert_eq!(config.user, "testuser");
        assert_eq!(config.key_path, Some(PathBuf::from("/tmp/key")));
        assert_eq!(config.ssh_host.as_deref(), Some("alias"));
        assert_eq!(config.container, "ctr");
    }

    #[test]
    fn test_merge_cli_partial_preserves_config() {
        let mut config = Config {
            host: Some("original.host".to_string()),
            port: 2222,
            user: "original".to_string(),
            key_path: Some(PathBuf::from("/original/key")),
            ssh_host: Some("original-alias".to_string()),
            container: "original-ctr".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let cli = Cli {
            host: None,
            port: Some(4444),
            user: None,
            key_path: None,
            ssh_host: None,
            container: None,
            list_users: false,
            check_server: false,
            user_url: None,
            user_qr: None,
            user_vpn: None,
            online_status: false,
            server_info: false,
            local: false,
            telegram_bot: false,
            telegram_token: None,
            deploy_bot: false,
            admin_id: None,
            backup: false,
            restore: None,
            add_user: None,
            rename_user: None,
            delete_user: None,
            yes: false,
            snapshot: false,
            snapshot_restore: None,
            snapshot_list: false,
            upgrade_xray: false,
            snapshot_dir: None,
            http_agent: false,
            secret: None,
            agent_port: 9090,
            bridge_agent_url: None,
        };
        config.merge_cli(&cli);

        assert_eq!(config.host.as_deref(), Some("original.host"));
        assert_eq!(config.port, 4444);
        assert_eq!(config.user, "original");
        assert_eq!(config.key_path, Some(PathBuf::from("/original/key")));
        assert_eq!(config.ssh_host.as_deref(), Some("original-alias"));
        assert_eq!(config.container, "original-ctr");
    }

    #[test]
    fn test_merge_cli_none_preserves_defaults() {
        let mut config = Config::default();
        let cli = Cli {
            host: None,
            port: None,
            user: None,
            key_path: None,
            ssh_host: None,
            container: None,
            list_users: false,
            check_server: false,
            user_url: None,
            user_qr: None,
            user_vpn: None,
            online_status: false,
            server_info: false,
            local: false,
            telegram_bot: false,
            telegram_token: None,
            deploy_bot: false,
            admin_id: None,
            backup: false,
            restore: None,
            add_user: None,
            rename_user: None,
            delete_user: None,
            yes: false,
            snapshot: false,
            snapshot_restore: None,
            snapshot_list: false,
            upgrade_xray: false,
            snapshot_dir: None,
            http_agent: false,
            secret: None,
            agent_port: 9090,
            bridge_agent_url: None,
        };
        config.merge_cli(&cli);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn test_has_connection_info() {
        let mut config = Config::default();
        assert!(!config.has_connection_info());

        config.host = Some("1.2.3.4".to_string());
        assert!(config.has_connection_info());

        config.host = None;
        config.ssh_host = Some("alias".to_string());
        assert!(config.has_connection_info());
    }

    #[test]
    fn test_load_invalid_toml() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "this is {{{{ not valid toml").unwrap();

        let result = Config::load_from(&f.path().to_path_buf());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("TOML parse error"));
    }

    #[test]
    fn test_config_roundtrip_toml() {
        let config = Config {
            host: Some("example.com".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: Some("vps-vpn".to_string()),
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn test_config_path_returns_valid_path() {
        let path = Config::config_path()
            .expect("config_path should succeed on systems with a home directory");
        assert!(path.ends_with("amnezia-xray-admin/config.toml"));
    }

    #[test]
    fn test_config_telegram_token_serialization() {
        let mut config = Config::default();
        config.telegram_token = Some("123456:ABCdef".to_string());
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("telegram_token = \"123456:ABCdef\""));

        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.telegram_token, Some("123456:ABCdef".to_string()));
    }

    #[test]
    fn test_config_telegram_token_omitted_when_none() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(!toml_str.contains("telegram_token"));
    }

    #[test]
    fn test_config_telegram_fields_roundtrip() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: Some("123:abc".to_string()),
            telegram_admin_chat_id: Some(987654321),
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
        assert_eq!(parsed.telegram_token, Some("123:abc".to_string()));
        assert_eq!(parsed.telegram_admin_chat_id, Some(987654321));
    }

    #[test]
    fn test_config_load_with_telegram_fields() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
host = "10.0.0.1"
port = 22
user = "root"
container = "amnezia-xray"
telegram_token = "999:xyz"
telegram_admin_chat_id = 12345
"#
        )
        .unwrap();

        let config = Config::load_from(&f.path().to_path_buf()).unwrap();
        assert_eq!(config.telegram_token, Some("999:xyz".to_string()));
        assert_eq!(config.telegram_admin_chat_id, Some(12345));
    }

    #[test]
    fn test_config_load_without_telegram_fields_defaults_to_none() {
        let mut f = NamedTempFile::new().unwrap();
        write!(
            f,
            r#"
host = "10.0.0.1"
"#
        )
        .unwrap();

        let config = Config::load_from(&f.path().to_path_buf()).unwrap();
        assert_eq!(config.telegram_token, None);
        assert_eq!(config.telegram_admin_chat_id, None);
    }

    #[test]
    fn test_cli_parse_rename_user() {
        let cli = Cli::parse_from(["app", "--rename-user", "OldName", "NewName"]);
        assert_eq!(
            cli.rename_user,
            Some(vec!["OldName".to_string(), "NewName".to_string()])
        );
    }

    #[test]
    fn test_cli_parse_rename_user_with_brackets() {
        let cli = Cli::parse_from(["app", "--rename-user", "Admin [macOS]", "Admin [iPhone]"]);
        assert_eq!(
            cli.rename_user,
            Some(vec![
                "Admin [macOS]".to_string(),
                "Admin [iPhone]".to_string()
            ])
        );
    }

    #[test]
    fn test_cli_parse_delete_user() {
        let cli = Cli::parse_from(["app", "--delete-user", "Alice"]);
        assert_eq!(cli.delete_user, Some("Alice".to_string()));
        assert!(!cli.yes);
    }

    #[test]
    fn test_cli_parse_delete_user_with_yes() {
        let cli = Cli::parse_from(["app", "--delete-user", "Bob", "--yes"]);
        assert_eq!(cli.delete_user, Some("Bob".to_string()));
        assert!(cli.yes);
    }

    #[test]
    fn test_cli_parse_yes_without_delete() {
        let cli = Cli::parse_from(["app", "--yes"]);
        assert!(cli.yes);
        assert_eq!(cli.delete_user, None);
    }

    #[test]
    fn test_cli_delete_user_with_brackets() {
        let cli = Cli::parse_from(["app", "--delete-user", "Admin [macOS Tahoe]", "--yes"]);
        assert_eq!(cli.delete_user, Some("Admin [macOS Tahoe]".to_string()));
    }
}
