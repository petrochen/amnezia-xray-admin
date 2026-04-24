mod app;
mod backend;
mod backend_trait;
mod config;
mod error;
mod http_agent;
mod ssh;
mod telegram;
mod ui;
mod xray;

use backend_trait::{LocalBackend, XrayBackend};
use clap::Parser;
use config::{Cli, Config};
use std::io::IsTerminal;

fn main() {
    let cli = Cli::parse();

    let mut config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Warning: failed to load config: {}", e);
            Config::default()
        }
    };

    config.merge_cli(&cli);

    // Create tokio runtime for async SSH/Xray operations
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to create async runtime: {}", e);
            std::process::exit(1);
        }
    };

    let local = cli.local;

    // Non-interactive CLI commands
    if cli.list_users {
        if let Err(e) = runtime.block_on(cli_list_users(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.check_server {
        if let Err(e) = runtime.block_on(cli_check_server(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref name) = cli.user_url {
        if let Err(e) = runtime.block_on(cli_user_url(&config, name, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref name) = cli.user_qr {
        if let Err(e) = runtime.block_on(cli_user_qr(&config, name, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref name) = cli.user_vpn {
        if let Err(e) = runtime.block_on(cli_user_vpn(&config, name, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.online_status {
        if let Err(e) = runtime.block_on(cli_online_status(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.server_info {
        if let Err(e) = runtime.block_on(cli_server_info(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.telegram_bot {
        if !local {
            eprintln!(
                "Error: --telegram-bot requires --local flag (must run on the VPS, not over SSH)"
            );
            eprintln!(
                "Deploy the bot to VPS with: cargo run -- --deploy-bot --telegram-token <TOKEN>"
            );
            std::process::exit(1);
        }
        let token = match cli
            .telegram_token
            .clone()
            .or_else(|| config.telegram_token.clone())
        {
            Some(t) => t,
            None => {
                eprintln!("Error: --telegram-token or TELEGRAM_TOKEN env var is required");
                std::process::exit(1);
            }
        };
        if config.telegram_admin_chat_id.is_none() {
            eprintln!("Error: Admin ID required. Use --admin-id <your_telegram_id> or set ADMIN_ID env var.");
            eprintln!("To find your Telegram ID, send /start to @userinfobot.");
            std::process::exit(1);
        }
        if let Err(e) = runtime.block_on(cli_telegram_bot(&config, &token, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.http_agent {
        let secret = cli.secret.unwrap_or_else(|| {
            eprintln!("Error: --secret is required with --http-agent");
            std::process::exit(1);
        });
        let container = config.container.clone();
        if let Err(e) = runtime.block_on(http_agent::run_agent(cli.agent_port, secret, container)) {
            eprintln!("Agent error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.deploy_bot {
        let token = match cli
            .telegram_token
            .clone()
            .or_else(|| config.telegram_token.clone())
        {
            Some(t) => t,
            None => {
                eprintln!("Error: --telegram-token or TELEGRAM_TOKEN env var is required for --deploy-bot");
                std::process::exit(1);
            }
        };
        if config.telegram_admin_chat_id.is_none() {
            eprintln!("Error: --admin-id is required for --deploy-bot.");
            eprintln!("To find your Telegram ID, send /start to @userinfobot.");
            std::process::exit(1);
        }
        if let Err(e) = runtime.block_on(cli_deploy_bot(&config, &token)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref name) = cli.add_user {
        if let Err(e) = runtime.block_on(cli_add_user(&config, name, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref name) = cli.delete_user {
        if let Err(e) = runtime.block_on(cli_delete_user(&config, name, local, cli.yes)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref names) = cli.rename_user {
        if let Err(e) = runtime.block_on(cli_rename_user(&config, &names[0], &names[1], local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.backup {
        if let Err(e) = runtime.block_on(cli_backup(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref restore_ts) = cli.restore {
        let ts = if restore_ts.is_empty() {
            None
        } else {
            Some(restore_ts.as_str())
        };
        if let Err(e) = runtime.block_on(cli_restore(&config, ts, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.snapshot {
        if let Err(e) = runtime.block_on(cli_snapshot(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.snapshot_list {
        if let Err(e) = runtime.block_on(cli_snapshot_list(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if let Some(ref tag) = cli.snapshot_restore {
        let tag = if tag.is_empty() {
            None
        } else {
            Some(tag.as_str())
        };
        if let Err(e) = runtime.block_on(cli_snapshot_restore(&config, tag, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.upgrade_xray {
        if let Err(e) = runtime.block_on(cli_upgrade_xray(&config, local)) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    // Initialize terminal
    let mut terminal = match app::init_terminal() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to initialize terminal: {}", e);
            std::process::exit(1);
        }
    };

    // Create app and run event loop
    let mut application = app::App::with_config(config, runtime.handle().clone());
    let result = app::run(&mut application, &mut terminal);

    // Always restore terminal, even on error
    if let Err(e) = app::restore_terminal(&mut terminal) {
        eprintln!("Failed to restore terminal: {}", e);
    }

    if let Err(e) = result {
        eprintln!("Application error: {}", e);
        std::process::exit(1);
    }
}

/// Create a backend for CLI commands: either LocalBackend (--local) or SshBackend (default).
async fn connect_cli_backend(config: &Config, local: bool) -> error::Result<Box<dyn XrayBackend>> {
    if local {
        // Use --host if provided (e.g. from deploy), otherwise auto-detect
        let hostname = if let Some(ref host) = config.host {
            host.clone()
        } else {
            get_local_hostname().await
        };
        Ok(Box::new(LocalBackend::new(
            config.container.clone(),
            hostname,
        )))
    } else {
        let backend = backend::connect_backend(config).await?;
        Ok(Box::new(backend))
    }
}

/// Get the server's public IP for vless URL generation.
///
/// In --local mode (especially inside Docker), `hostname -I` returns the
/// container's private IP, which is unusable for VPN clients. Instead, query
/// an external service for the real public IP first.
async fn get_local_hostname() -> String {
    // Try to get the public IP via external service (works from inside Docker)
    for url in &[
        "https://ifconfig.me",
        "https://icanhazip.com",
        "https://api.ipify.org",
    ] {
        if let Ok(output) = tokio::process::Command::new("curl")
            .args(["-sf", "--max-time", "5", url])
            .output()
            .await
        {
            if output.status.success() {
                let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !ip.is_empty() && looks_like_ip(&ip) {
                    return ip;
                }
            }
        }
    }
    // Fallback to hostname -I (useful when running directly on VPS without internet issues)
    if let Ok(output) = tokio::process::Command::new("hostname")
        .arg("-I")
        .output()
        .await
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(ip) = stdout.split_whitespace().next() {
            return ip.to_string();
        }
    }
    // All methods failed — return a placeholder that signals the issue
    eprintln!("Warning: could not determine server IP address. VPN URLs may be unusable.");
    "UNKNOWN_IP".to_string()
}

/// Basic check that a string looks like an IPv4 or IPv6 address.
fn looks_like_ip(s: &str) -> bool {
    // Must not contain HTML or spaces (rate-limit pages, error messages)
    if s.contains('<') || s.contains(' ') || s.len() > 45 {
        return false;
    }
    s.parse::<std::net::IpAddr>().is_ok()
}

async fn cli_check_server(config: &Config, local: bool) -> error::Result<()> {
    if !local {
        let (_, port, user, _) = backend::resolve_connection_info(config)?;
        eprintln!(
            "Connecting to {}@{}:{}...",
            user,
            config
                .ssh_host
                .as_deref()
                .or(config.host.as_deref())
                .unwrap_or("?"),
            port
        );
    }

    let backend = connect_cli_backend(config, local).await?;

    // Ensure API is enabled (idempotent — no restart if already configured)
    eprintln!("Checking API configuration...");
    let modified = xray::config::ensure_api_enabled(backend.as_ref()).await?;
    if modified {
        eprintln!("API was not configured — enabled and container restarted.");
    } else {
        eprintln!("API already configured.");
    }

    let client = xray::client::XrayApiClient::new(backend.as_ref());

    let users = client.list_users().await?;
    let server_info = client.get_server_info().await?;
    let api_ok = client.probe_stats_api().await.unwrap_or(false);

    if api_ok {
        println!(
            "API enabled, {} users, xray v{}",
            users.len(),
            server_info.version
        );
    } else {
        println!(
            "API degraded (stats API unreachable), {} users, xray v{}",
            users.len(),
            server_info.version
        );
    }

    Ok(())
}

async fn cli_user_url(config: &Config, name: &str, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    let user = users.iter().find(|u| u.name == name);
    let user = match user {
        Some(u) => u,
        None => {
            return Err(error::AppError::Xray(format!("user '{}' not found", name)));
        }
    };

    let bridge_url = backend::build_bridge_vless_url(backend.as_ref(), &user.uuid).await?;
    let direct_url = backend::build_direct_vless_url(backend.as_ref(), &user.uuid).await?;

    println!("Bridge: {}", bridge_url);
    println!("Direct: {}", direct_url);
    Ok(())
}

async fn cli_user_qr(config: &Config, name: &str, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    let user = users.iter().find(|u| u.name == name);
    let user = match user {
        Some(u) => u,
        None => {
            return Err(error::AppError::Xray(format!("user '{}' not found", name)));
        }
    };

    let vless_url = backend::build_vless_url(backend.as_ref(), &user.uuid).await?;

    match ui::qr::render_qr_to_lines(&vless_url) {
        Ok(lines) => {
            for line in &lines {
                println!("{}", line);
            }
            println!();
            println!("{}", name);
            println!("{}", vless_url);
        }
        Err(e) => {
            return Err(error::AppError::Xray(format!(
                "QR generation failed: {}",
                e
            )));
        }
    }

    Ok(())
}

async fn cli_user_vpn(config: &Config, name: &str, local: bool) -> error::Result<()> {
    use xray::client::generate_amnezia_url;
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    let user = users.iter().find(|u| u.name == name);
    let user = match user {
        Some(u) => u,
        None => {
            return Err(error::AppError::Xray(format!("user '{}' not found", name)));
        }
    };

    // Bridge vpn:// config
    let bridge = backend::read_bridge_params(backend.as_ref()).await?;
    let bridge_params = xray::types::VlessUrlParams {
        uuid: user.uuid.clone(),
        host: bridge.host,
        port: bridge.port,
        sni: bridge.sni,
        public_key: bridge.public_key,
        short_id: bridge.short_id,
        path: bridge.path,
    };
    let bridge_vpn = generate_amnezia_url(&bridge_params);

    // Direct vpn:// config
    let direct_params = backend::build_vless_params(backend.as_ref(), &user.uuid).await?;
    let direct_vpn = generate_amnezia_url(&direct_params);

    println!("Bridge: {}", bridge_vpn);
    println!("Direct: {}", direct_vpn);
    Ok(())
}

async fn cli_online_status(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    if users.is_empty() {
        println!("No users found.");
        return Ok(());
    }

    // Collect online status for each user
    let mut rows: Vec<(String, u32, Vec<String>)> = Vec::new();
    for user in &users {
        let count = client.get_online_count(&user.email).await.unwrap_or(0);
        let ips = if count > 0 {
            client.get_online_ips(&user.email).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        let name = if user.name.is_empty() {
            user.uuid[..std::cmp::min(8, user.uuid.len())].to_string()
        } else {
            user.name.clone()
        };
        rows.push((name, count, ips));
    }

    // Print table
    println!("{:<30} {:<8} IPs", "NAME", "ONLINE");
    println!("{}", "-".repeat(60));

    for (name, count, ips) in &rows {
        let online = if *count > 0 {
            format!("● {}", count)
        } else {
            "○".to_string()
        };
        let ip_str = if ips.is_empty() {
            "-".to_string()
        } else {
            ips.join(", ")
        };
        println!("{:<30} {:<8} {}", name, online, ip_str);
    }

    Ok(())
}

async fn cli_server_info(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());

    let server_info = client.get_server_info().await?;
    let users = client.list_users().await?;
    // Check API status by probing the stats endpoint — `xray version` works
    // without the API, so a successful version alone does not prove the
    // runtime API is healthy.  `probe_stats_api` checks the command exit code
    // instead of silently falling back to zero.
    let api_ok = client.probe_stats_api().await.unwrap_or(false);
    let api_status = if api_ok {
        "enabled"
    } else if server_info.version != "unknown" {
        "degraded (version ok, stats API unreachable)"
    } else {
        "unknown"
    };

    // Get container uptime and check for newer Xray via shared helpers
    // (keeps the 3 s GitHub timeout and anchored docker filter consistent with the TUI/Telegram).
    let uptime_raw = backend::fetch_container_uptime(backend.as_ref()).await;
    let uptime = if uptime_raw.is_empty() {
        "unknown".to_string()
    } else {
        uptime_raw
    };
    let latest_version = backend::fetch_latest_xray_version(backend.as_ref()).await;

    let version_display = match &latest_version {
        Some(latest) if latest != &server_info.version => {
            format!("v{} (update available: v{})", server_info.version, latest)
        }
        Some(_) => format!("v{} (latest)", server_info.version),
        None => format!("v{}", server_info.version),
    };

    println!("Xray:           {}", version_display);
    println!("API status:     {}", api_status);
    println!("Uptime:         {}", uptime);
    println!("Users:          {}", users.len());
    println!(
        "Total upload:   {}",
        ui::dashboard::format_bytes(server_info.uplink)
    );
    println!(
        "Total download: {}",
        ui::dashboard::format_bytes(server_info.downlink)
    );

    Ok(())
}

async fn cli_list_users(config: &Config, local: bool) -> error::Result<()> {
    if !local {
        let (_, port, user, _) = backend::resolve_connection_info(config)?;
        eprintln!(
            "Connecting to {}@{}:{}...",
            user,
            config
                .ssh_host
                .as_deref()
                .or(config.host.as_deref())
                .unwrap_or("?"),
            port
        );
    }

    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    // Fetch stats for each user
    let mut users_with_stats = Vec::new();
    for mut user in users {
        if let Ok(stats) = client.get_user_stats(&user.email).await {
            user.stats = stats;
        }
        if let Ok(count) = client.get_online_count(&user.email).await {
            user.online_count = count;
        }
        users_with_stats.push(user);
    }

    if users_with_stats.is_empty() {
        println!("No users found.");
        return Ok(());
    }

    // Print header
    println!(
        "{:<30} {:<10} {:<12} {:<12} {:<8}",
        "NAME", "UUID", "UPLOAD", "DOWNLOAD", "ONLINE"
    );
    println!("{}", "-".repeat(72));

    for user in &users_with_stats {
        let name = if user.name.is_empty() {
            &user.uuid[..std::cmp::min(8, user.uuid.len())]
        } else {
            &user.name
        };
        let uuid_short = &user.uuid[..std::cmp::min(8, user.uuid.len())];
        let online = if user.online_count > 0 {
            format!("● {}", user.online_count)
        } else {
            "○".to_string()
        };

        println!(
            "{:<30} {:<10} {:<12} {:<12} {:<8}",
            name,
            uuid_short,
            ui::dashboard::format_bytes(user.stats.uplink),
            ui::dashboard::format_bytes(user.stats.downlink),
            online,
        );
    }

    Ok(())
}

async fn cli_telegram_bot(config: &Config, token: &str, local: bool) -> error::Result<()> {
    env_logger::init();
    let backend = connect_cli_backend(config, local).await?;

    // Ensure the Xray API is configured before starting the bot.
    // On a fresh server, the API may not be enabled yet — without this,
    // commands like /add would fail on the runtime API call.
    eprintln!("Checking Xray API configuration...");
    match xray::config::ensure_api_enabled(backend.as_ref()).await {
        Ok(true) => eprintln!("API was not configured — enabled and container restarted."),
        Ok(false) => eprintln!("API already configured."),
        Err(e) => {
            return Err(error::AppError::Xray(format!(
                "failed to ensure Xray API is enabled (bot cannot operate without it): {}",
                e
            )));
        }
    }

    telegram::run_bot(token, backend, config.clone()).await
}

async fn cli_backup(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());

    eprintln!("Creating timestamped backup...");
    let timestamp = client.backup_config_timestamped().await?;

    println!("Backup created:");
    println!("  server.json.{}.bak", timestamp);
    println!("  clientsTable.{}.bak", timestamp);

    Ok(())
}

async fn cli_restore(config: &Config, timestamp: Option<&str>, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());

    // List available backups first
    let backups = client.list_backups().await?;
    if backups.is_empty() {
        return Err(error::AppError::Xray(
            "no timestamped backups found".to_string(),
        ));
    }

    eprintln!("Available backups:");
    for (i, ts) in backups.iter().enumerate() {
        let marker = if i == 0 { " (latest)" } else { "" };
        eprintln!("  {}{}", ts, marker);
    }

    let ts = client.restore_config(timestamp).await?;

    println!("Restored from backup {}:", ts);
    println!("  server.json.{}.bak -> server.json", ts);
    println!("  clientsTable.{}.bak -> clientsTable", ts);
    println!("Container restarted.");

    Ok(())
}

async fn cli_add_user(config: &Config, name: &str, local: bool) -> error::Result<()> {
    if name.trim().is_empty() {
        return Err(error::AppError::Xray(
            "user name cannot be empty".to_string(),
        ));
    }

    let backend = connect_cli_backend(config, local).await?;

    // Pre-check: reject duplicate names before potentially bootstrapping the API,
    // so we don't trigger a backup + restart for a no-op duplicate attempt.
    // Check both server.json emails (normalized configs) and clientsTable names
    // (pre-bootstrap configs where emails haven't been backfilled yet).
    let email = xray::types::XrayUser::email_from_name(name);
    let server_config = xray::config::read_server_config(backend.as_ref()).await?;
    let clients_table = xray::config::read_clients_table(backend.as_ref()).await?;
    if server_config.has_client_email(&email) || clients_table.has_name(name) {
        return Err(error::AppError::Xray(format!(
            "user '{}' already exists",
            name
        )));
    }
    drop(server_config);
    drop(clients_table);

    // Ensure API is enabled (idempotent — no restart if already configured)
    let modified = xray::config::ensure_api_enabled(backend.as_ref()).await?;
    if modified {
        eprintln!("API was not configured — enabled and container restarted.");
    }

    let client = xray::client::XrayApiClient::new(backend.as_ref());

    let uuid = client.add_user(name).await?;

    println!("User added successfully.");
    println!("Name:  {}", name);
    println!("UUID:  {}", uuid);

    // Bridge URL generation is best-effort: if it fails, the user was still added successfully.
    match backend::build_bridge_vless_url(backend.as_ref(), &uuid).await {
        Ok(url) => println!("Bridge: {}", url),
        Err(e) => eprintln!(
            "Warning: bridge URL generation failed: {}. Use --user-url to retry.",
            e
        ),
    }

    Ok(())
}

async fn cli_delete_user(config: &Config, name: &str, local: bool, yes: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let users = client.list_users().await?;

    let user = users.iter().find(|u| u.name == name);
    let user = match user {
        Some(u) => u,
        None => {
            return Err(error::AppError::Xray(format!("user '{}' not found", name)));
        }
    };

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(error::AppError::Config(
                "Interactive confirmation required. Use --yes to skip.".to_string(),
            ));
        }
        eprintln!(
            "Are you sure you want to delete user '{}'? Type the user name to confirm:",
            name
        );
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(error::AppError::Io)?;
        if input.trim() != name {
            eprintln!("Name does not match. Deletion cancelled.");
            return Ok(());
        }
    }

    client.remove_user(&user.uuid).await?;
    println!("User '{}' deleted.", name);

    Ok(())
}

async fn cli_rename_user(
    config: &Config,
    old_name: &str,
    new_name: &str,
    local: bool,
) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let client = xray::client::XrayApiClient::new(backend.as_ref());

    client.rename_user(old_name, new_name).await?;

    println!("User renamed: '{}' -> '{}'", old_name, new_name);
    println!("Note: rename resets traffic stats history for this user.");

    Ok(())
}

async fn cli_deploy_bot(config: &Config, token: &str) -> error::Result<()> {
    if !ui::telegram_setup::is_valid_token(token) {
        return Err(error::AppError::Config(
            "Invalid token format (expected <digits>:<secret>)".to_string(),
        ));
    }

    eprintln!("Connecting to VPS...");
    eprintln!("Deploying Telegram bot...");

    match backend::deploy_bot(config, token).await {
        Ok(msg) => {
            println!("{}", msg);
            Ok(())
        }
        Err(e) => Err(error::AppError::Xray(e)),
    }
}

// ── Snapshot & Upgrade commands (delegating to xray::snapshot module) ──

async fn cli_snapshot(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    eprintln!("Creating snapshot...");
    let info = xray::snapshot::create_snapshot(backend.as_ref(), config.snapshot_dir()).await?;
    println!(
        "Snapshot created: {} (v{}, {} users)",
        info.tag, info.version, info.users_count
    );
    Ok(())
}

async fn cli_snapshot_list(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;
    let snapshots = xray::snapshot::list_snapshots(backend.as_ref(), config.snapshot_dir()).await?;

    if snapshots.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    println!("{:<20} {:<20} {:<10}", "TAG", "XRAY VERSION", "USERS");
    println!("{}", "-".repeat(50));
    for s in &snapshots {
        println!("{:<20} {:<20} {:<10}", s.tag, s.version, s.users_count);
    }

    Ok(())
}

async fn cli_snapshot_restore(
    config: &Config,
    tag: Option<&str>,
    local: bool,
) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;

    let snapshots = xray::snapshot::list_snapshots(backend.as_ref(), config.snapshot_dir()).await?;
    if snapshots.is_empty() {
        return Err(error::AppError::Xray("no snapshots found".to_string()));
    }

    eprintln!("Available snapshots:");
    for s in &snapshots {
        eprintln!("  {} (v{}, {} users)", s.tag, s.version, s.users_count);
    }

    let restore_tag = match tag {
        Some(t) => {
            if !snapshots.iter().any(|s| s.tag == t) {
                return Err(error::AppError::Xray(format!(
                    "snapshot '{}' not found. Available: {}",
                    t,
                    snapshots
                        .iter()
                        .map(|s| s.tag.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            t.to_string()
        }
        None => snapshots.last().map(|s| s.tag.clone()).unwrap(),
    };

    eprintln!("Restoring from snapshot [{}]...", restore_tag);
    xray::snapshot::restore_snapshot(backend.as_ref(), &restore_tag, config.snapshot_dir()).await?;

    println!(
        "Restored from snapshot [{}]. Container restarted.",
        restore_tag
    );
    Ok(())
}

async fn cli_upgrade_xray(config: &Config, local: bool) -> error::Result<()> {
    let backend = connect_cli_backend(config, local).await?;

    // Get current version for display
    let client = xray::client::XrayApiClient::new(backend.as_ref());
    let server_info = client.get_server_info().await?;
    eprintln!("Current version: v{}", server_info.version);

    // Check latest
    let latest = xray::snapshot::get_latest_xray_version(backend.as_ref()).await?;
    if latest == server_info.version {
        println!("Already on latest version v{}. Nothing to do.", latest);
        return Ok(());
    }
    eprintln!("Latest version:  v{}", latest);
    eprintln!();

    eprintln!("Upgrading...");
    let result = xray::snapshot::upgrade_xray(backend.as_ref(), config.snapshot_dir()).await?;

    println!();
    println!("Upgrade complete!");
    println!("  Before:   v{}", result.old_version);
    println!("  After:    v{}", result.new_version);
    println!(
        "  Snapshot: {} (use --snapshot-restore {} to rollback)",
        result.snapshot_tag, result.snapshot_tag
    );

    Ok(())
}
