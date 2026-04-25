//! Telegram bot for managing Xray users via Telegram commands.
//!
//! Admin is set at deploy time via `--admin-id`. Only the configured admin
//! can interact with the bot; all other users get "Access denied".

use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommandScope, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode,
    Recipient,
};
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;

use crate::backend;
use crate::backend_trait::XrayBackend;
use crate::config::Config;
use crate::error::Result;
use crate::ui::dashboard::format_bytes;
use crate::ui::qr::render_qr_to_png;
use crate::xray::client::{ServerInfo, XrayApiClient};
use crate::xray::snapshot;
use crate::xray::types::{TrafficStats, XrayUser};

/// State shared across all Telegram handlers.
pub struct BotState {
    pub backend: Box<dyn XrayBackend>,
    pub config: Mutex<Config>,
}

/// Callback query prefix for URL/config inline buttons.
const URL_PREFIX: &str = "url:";
/// Callback query prefix for config inline buttons.
const CONFIG_PREFIX: &str = "config:";
/// Callback query prefix for QR inline buttons.
const QR_PREFIX: &str = "qr:";
/// Callback query prefix for delete user selection buttons.
const DELETE_PREFIX: &str = "delete:";
/// Callback query prefix for delete confirmation buttons.
const DELETE_CONFIRM_PREFIX: &str = "delete_confirm:";
/// Callback query prefix for delete cancel buttons.
const DELETE_CANCEL_PREFIX: &str = "delete_cancel:";
/// Callback query prefix for restore snapshot buttons.
const RESTORE_PREFIX: &str = "restore:";
/// Callback query prefix for upgrade confirmation buttons.
const UPGRADE_CONFIRM_PREFIX: &str = "upgrade_confirm";
/// Callback query prefix for upgrade cancel buttons.
const UPGRADE_CANCEL_PREFIX: &str = "upgrade_cancel";

/// Commands recognized by the bot.
#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(description = "Welcome message")]
    Start,
    #[command(description = "Show help")]
    Help,
    #[command(description = "List users with stats")]
    Users,
    #[command(description = "Server info")]
    Status,
    #[command(description = "Add user: /add <name>")]
    Add(String),
    #[command(description = "Delete user: /del <name>")]
    Del(String),
    #[command(description = "Get config: /config <name>")]
    Config(String),
    #[command(description = "Get QR code: /qr <name>")]
    Qr(String),
    #[command(description = "Create snapshot")]
    Snapshot,
    #[command(description = "List snapshots")]
    Snapshots,
    #[command(description = "Restore snapshot")]
    Restore(String),
    #[command(description = "Upgrade Xray")]
    Upgrade,
}

/// Check if a chat ID matches the configured admin.
fn is_admin(config: &Config, chat_id: ChatId) -> bool {
    config.telegram_admin_chat_id == Some(chat_id.0)
}

/// Format the /help response text.
pub fn help_text() -> String {
    [
        "Commands:",
        "/add <name> - Add user",
        "/del <name> - Delete user",
        "/config <name> - Get connection config",
        "/qr <name> - Get QR code",
        "/users - List users",
        "/status - Server info",
        "/snapshot - Create backup",
        "/upgrade - Upgrade Xray",
    ]
    .join("\n")
}

/// Format the welcome message shown after /start for the admin.
pub fn welcome_text() -> String {
    [
        "Welcome, admin!",
        "",
        "Use /help to see available commands.",
    ]
    .join("\n")
}

/// Format the access denied message for non-admin users.
pub fn access_denied_text() -> String {
    "Access denied. Contact the server administrator.".to_string()
}

/// Format the /users response: list users with traffic stats.
pub fn format_users_message(users: &[(XrayUser, TrafficStats, u32)]) -> String {
    if users.is_empty() {
        return "No users found.".to_string();
    }

    let mut lines = Vec::new();
    lines.push("👥 Users:".to_string());
    lines.push(String::new());

    for (user, stats, online_count) in users {
        let name = if user.name.is_empty() {
            &user.uuid[..std::cmp::min(8, user.uuid.len())]
        } else {
            &user.name
        };
        let online_indicator = if *online_count > 0 {
            format!("🟢 {}", online_count)
        } else {
            "⚪".to_string()
        };
        lines.push(format!(
            "{} {} ↑{} ↓{}",
            online_indicator,
            name,
            format_bytes(stats.uplink),
            format_bytes(stats.downlink),
        ));
    }

    lines.join("\n")
}

/// Format the /users response annotated with bridge traffic stats.
///
/// Each user line shows egress stats plus bridge stats (if available for that user).
pub fn format_users_message_with_bridge(
    users: &[(XrayUser, TrafficStats, u32)],
    bridge_stats: &[(String, u64, u64)],
) -> String {
    let mut lines = Vec::new();
    lines.push("👥 Users:".to_string());
    lines.push(String::new());

    // Collect all known emails from both sources
    let mut seen_emails: std::collections::HashSet<String> = std::collections::HashSet::new();

    // First: egress users (with bridge stats merged)
    for (user, stats, online_count) in users {
        seen_emails.insert(user.email.clone());
        // Skip bridge@vpn — it's the aggregate, not a real user
        if user.email == "bridge@vpn" {
            continue;
        }
        let name = if user.name.is_empty() {
            &user.uuid[..std::cmp::min(8, user.uuid.len())]
        } else {
            &user.name
        };
        let online_indicator = if *online_count > 0 {
            format!("🟢 {}", online_count)
        } else {
            "⚪".to_string()
        };

        let bridge = bridge_stats
            .iter()
            .find(|(email, _, _)| email == &user.email);
        let has_direct_traffic = stats.uplink > 0 || stats.downlink > 0;
        let bridge_online = !bridge_stats.is_empty();

        let traffic = if let Some((_, bup, bdown)) = bridge {
            format!("↑{} ↓{}", format_bytes(*bup), format_bytes(*bdown))
        } else if has_direct_traffic {
            format!("↑{} ↓{}", format_bytes(stats.uplink), format_bytes(stats.downlink))
        } else {
            "—".to_string()
        };

        // Icon shows where user is REGISTERED, not where traffic exists
        let servers = if bridge_online {
            " 🌐" // user exists on both servers
        } else {
            " ✈️" // bridge offline, only egress known
        };

        lines.push(format!("{} {}: {}{}", online_indicator, name, traffic, servers));
    }

    // Second: bridge-only users (not on egress)
    for (email, up, down) in bridge_stats {
        if seen_emails.contains(email) {
            continue;
        }
        let name = email.strip_suffix("@vpn").unwrap_or(email);
        if name == "bridge" {
            continue;
        }
        lines.push(format!(
            "⚪ {}: ↑{} ↓{} 🏠",
            name,
            format_bytes(*up),
            format_bytes(*down),
        ));
    }

    if lines.len() <= 2 {
        return "No users found.".to_string();
    }

    lines.push(String::new());
    lines.push("🏠 bridge  ✈️ direct  🌐 both".to_string());
    lines.join("\n")
}

/// Format the /add success response.
pub fn format_add_message(name: &str, uuid: &str, bridge_url: &str, direct_url: &str) -> String {
    format!(
        "✅ User '{}' added.\n\nUUID: {}\n\n🏠 Для России:\n<pre>{}</pre>\n\n✈️ Из-за рубежа:\n<pre>{}</pre>",
        name, uuid, bridge_url, direct_url
    )
}

/// Format the /delete confirmation prompt.
pub fn format_delete_confirm_message(name: &str) -> String {
    format!(
        "⚠️ Delete user '{}'?\n\nThis will revoke their access immediately.",
        name
    )
}

/// Format the /delete success response.
pub fn format_delete_success_message(name: &str) -> String {
    format!("🗑 User '{}' deleted.", name)
}

/// Validate a user name for /add command.
/// Returns an error message if invalid, None if valid.
pub fn validate_user_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some("Usage: /add <name>".to_string());
    }
    // Telegram callback_data has a 64-byte limit. With the longest prefix
    // "delete:" (7 bytes), names must stay under 57 bytes. Use 50 for margin.
    if trimmed.len() > 50 {
        return Some("Name too long (max 50 bytes).".to_string());
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Some("Name must not contain control characters.".to_string());
    }
    if trimmed.contains('@') || trimmed.contains(">>>") {
        return Some("Name must not contain '@' or '>>>'.".to_string());
    }
    None
}

/// Result of building an inline keyboard for user selection.
pub struct UserKeyboardResult {
    pub keyboard: InlineKeyboardMarkup,
    pub skipped_names: Vec<String>,
    pub unnamed_count: usize,
}

/// Build an inline keyboard listing users, with each button using the given callback prefix.
///
/// Each button shows the user's name and has callback data `"{prefix}{name}"`.
/// Users with empty names or callback data exceeding Telegram's 64-byte limit are skipped.
/// Returns the keyboard markup, skipped names, and count of unnamed users.
pub fn build_user_keyboard(users: &[XrayUser], callback_prefix: &str) -> UserKeyboardResult {
    /// Telegram's maximum callback_data size in bytes.
    const MAX_CALLBACK_DATA_BYTES: usize = 64;

    let mut skipped_names: Vec<String> = Vec::new();
    let unnamed_count = users.iter().filter(|u| u.name.is_empty()).count();

    let buttons: Vec<Vec<InlineKeyboardButton>> = users
        .iter()
        .filter(|u| !u.name.is_empty())
        .filter(|u| {
            if callback_prefix.len() + u.name.len() > MAX_CALLBACK_DATA_BYTES {
                skipped_names.push(u.name.clone());
                false
            } else {
                true
            }
        })
        .map(|u| {
            vec![InlineKeyboardButton::callback(
                &u.name,
                format!("{}{}", callback_prefix, u.name),
            )]
        })
        .collect();
    UserKeyboardResult {
        keyboard: InlineKeyboardMarkup::new(buttons),
        skipped_names,
        unnamed_count,
    }
}

/// Format a truncated list of skipped user names, keeping the total under a safe length.
/// Shows as many names as fit, then "and N more" if truncated.
fn format_skipped_names(skipped: &[String], max_bytes: usize) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let mut result = String::new();
    for (shown, name) in skipped.iter().enumerate() {
        let separator = if shown == 0 { "" } else { ", " };
        let remaining = skipped.len() - shown;
        let suffix = format!(" and {} more", remaining - 1);
        let needed = separator.len() + name.len();
        let budget_with_suffix = if remaining > 1 {
            needed + suffix.len()
        } else {
            needed
        };
        if result.len() + budget_with_suffix > max_bytes {
            if result.is_empty() {
                // First name already exceeds budget — truncate it
                let ellipsis = "...";
                let avail = max_bytes.saturating_sub(ellipsis.len());
                // Truncate at a char boundary
                let truncated = &name[..name.floor_char_boundary(avail)];
                return format!("{}{}", truncated, ellipsis);
            }
            result.push_str(&format!(" and {} more", remaining));
            return result;
        }
        result.push_str(separator);
        result.push_str(name);
    }
    result
}

/// Format a user-selection prompt, appending notes about skipped or unnamed users if any.
fn format_selection_message(base_msg: &str, skipped: &[String], unnamed_count: usize) -> String {
    let mut notes = Vec::new();
    if !skipped.is_empty() {
        let template_overhead = base_msg.len() + 100;
        let max_names = 3500usize.saturating_sub(template_overhead);
        let names = format_skipped_names(skipped, max_names);
        notes.push(format!(
            "{} user(s) have names too long for inline buttons. Use the command with the name directly: {}",
            skipped.len(),
            names
        ));
    }
    if unnamed_count > 0 {
        notes.push(format!(
            "{} unnamed user(s) not shown. Use /users to see them.",
            unnamed_count
        ));
    }
    if notes.is_empty() {
        base_msg.to_string()
    } else {
        format!("{}\n\nNote: {}", base_msg, notes.join("\n"))
    }
}

/// Format the message when no inline buttons are available.
fn format_empty_keyboard_message(
    command_hint: &str,
    skipped: &[String],
    unnamed_count: usize,
) -> String {
    let mut parts = Vec::new();
    if !skipped.is_empty() {
        let names = format_skipped_names(skipped, 3900);
        parts.push(format!(
            "{} user(s) have names too long for inline buttons. Use {} directly for: {}",
            skipped.len(),
            command_hint,
            names
        ));
    }
    if unnamed_count > 0 {
        parts.push(format!(
            "{} user(s) have no name set. Use /users to see them.",
            unnamed_count
        ));
    }
    if parts.is_empty() {
        "No users found.".to_string()
    } else {
        parts.join("\n\n")
    }
}

/// Build an inline keyboard for delete confirmation.
pub fn delete_confirmation_keyboard(user_uuid: &str) -> InlineKeyboardMarkup {
    let confirm = InlineKeyboardButton::callback(
        "Yes, delete",
        format!("{}{}", DELETE_CONFIRM_PREFIX, user_uuid),
    );
    let cancel =
        InlineKeyboardButton::callback("Cancel", format!("{}{}", DELETE_CANCEL_PREFIX, user_uuid));
    InlineKeyboardMarkup::new(vec![vec![confirm, cancel]])
}

/// Format the /status response: server info and online users summary.
pub fn format_status_message(
    server_info: &ServerInfo,
    user_count: usize,
    online_count: usize,
    uptime: &str,
    latest_version: Option<&str>,
) -> String {
    let mut lines = Vec::new();
    lines.push("📊 Server Status:".to_string());
    lines.push(String::new());

    let version_line = match latest_version {
        Some(latest) if latest != server_info.version => {
            format!("Xray: v{} (⬆️ v{} available)", server_info.version, latest)
        }
        Some(_) => format!("Xray: v{} ✅", server_info.version),
        None => format!("Xray: v{}", server_info.version),
    };
    lines.push(version_line);

    if !uptime.is_empty() {
        lines.push(format!("Uptime: {}", uptime));
    }
    lines.push(format!("Users: {} ({} online)", user_count, online_count));
    lines.push(format!("Upload: {}", format_bytes(server_info.uplink)));
    lines.push(format!("Download: {}", format_bytes(server_info.downlink)));

    lines.join("\n")
}

/// Handle incoming bot commands.
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<BotState>,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    // Check admin access
    {
        let config = state.config.lock().await;

        if let Command::Start = &cmd {
            if is_admin(&config, chat_id) {
                bot.send_message(chat_id, welcome_text()).await?;
            } else {
                bot.send_message(chat_id, access_denied_text()).await?;
            }
            return Ok(());
        }

        if !is_admin(&config, chat_id) {
            bot.send_message(chat_id, access_denied_text()).await?;
            return Ok(());
        }
    }
    // Config lock is dropped here — safe to do async backend work

    match cmd {
        Command::Start => unreachable!(), // handled above
        Command::Help => {
            bot.send_message(chat_id, help_text()).await?;
        }
        Command::Users => {
            let text = match cmd_users(&state).await {
                Ok(t) => t,
                Err(e) => format!("Error: {}", e),
            };
            bot.send_message(chat_id, text).await?;
        }
        Command::Status => {
            let text = match cmd_status(&state).await {
                Ok(t) => t,
                Err(e) => format!("Error: {}", e),
            };
            bot.send_message(chat_id, text).await?;
        }
        Command::Add(name) => {
            let name = name.trim().to_string();
            if let Some(err) = validate_user_name(&name) {
                bot.send_message(chat_id, err).await?;
            } else {
                let text = match cmd_add(&state, &name).await {
                    Ok(t) => t,
                    Err(e) => format!("Error: {}", e),
                };
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
        }
        Command::Del(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                match cmd_user_keyboard(&state, DELETE_PREFIX).await {
                    Ok(result) => {
                        if result.keyboard.inline_keyboard.is_empty() {
                            let msg = format_empty_keyboard_message(
                                "/del <name>",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg).await?;
                        } else {
                            let msg = format_selection_message(
                                "Select a user to delete:",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg)
                                .reply_markup(result.keyboard)
                                .await?;
                        }
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            } else {
                match cmd_delete_prompt(&state, &name).await {
                    Ok((text, keyboard)) => {
                        bot.send_message(chat_id, text)
                            .reply_markup(keyboard)
                            .await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            }
        }
        Command::Config(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                match cmd_user_keyboard(&state, CONFIG_PREFIX).await {
                    Ok(result) => {
                        if result.keyboard.inline_keyboard.is_empty() {
                            let msg = format_empty_keyboard_message(
                                "/config <name>",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg).await?;
                        } else {
                            let msg = format_selection_message(
                                "Select a user:",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg)
                                .reply_markup(result.keyboard)
                                .await?;
                        }
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            } else {
                let text = match cmd_config(&state, &name).await {
                    Ok(t) => t,
                    Err(e) => format!("Error: {}", e),
                };
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
        }
        Command::Qr(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                match cmd_user_keyboard(&state, QR_PREFIX).await {
                    Ok(result) => {
                        if result.keyboard.inline_keyboard.is_empty() {
                            let msg = format_empty_keyboard_message(
                                "/qr <name>",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg).await?;
                        } else {
                            let msg = format_selection_message(
                                "Select a user:",
                                &result.skipped_names,
                                result.unnamed_count,
                            );
                            bot.send_message(chat_id, msg)
                                .reply_markup(result.keyboard)
                                .await?;
                        }
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            } else {
                match cmd_qr(&state, &name).await {
                    Ok(qr_images) => {
                        for (png_bytes, caption) in qr_images {
                            let input = InputFile::memory(png_bytes).file_name("qr.png");
                            bot.send_photo(chat_id, input).caption(caption).await?;
                        }
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            }
        }
        Command::Snapshot => {
            match cmd_snapshot(&bot, chat_id, &state).await {
                Ok(()) => {} // message already sent inside
                Err(e) => {
                    bot.send_message(chat_id, format!("Error: {}", e)).await?;
                }
            }
        }
        Command::Snapshots => {
            let text = match cmd_snapshots(&state).await {
                Ok(t) => t,
                Err(e) => format!("Error: {}", e),
            };
            bot.send_message(chat_id, text).await?;
        }
        Command::Restore(tag) => {
            let tag = tag.trim().to_string();
            if tag.is_empty() {
                match cmd_restore_keyboard(&state).await {
                    Ok(Some((text, keyboard))) => {
                        bot.send_message(chat_id, text)
                            .reply_markup(keyboard)
                            .await?;
                    }
                    Ok(None) => {
                        bot.send_message(chat_id, "No snapshots found.").await?;
                    }
                    Err(e) => {
                        bot.send_message(chat_id, format!("Error: {}", e)).await?;
                    }
                }
            } else {
                let text = match cmd_restore(&state, &tag).await {
                    Ok(t) => t,
                    Err(e) => format!("Error: {}", e),
                };
                bot.send_message(chat_id, text).await?;
            }
        }
        Command::Upgrade => {
            match cmd_upgrade_check(&bot, chat_id, &state).await {
                Ok(()) => {} // messages already sent inside
                Err(e) => {
                    bot.send_message(chat_id, format!("Error: {}", e)).await?;
                }
            }
        }
    }

    Ok(())
}

/// Execute /users command: list users with stats, optionally annotated with bridge traffic.
async fn cmd_users(state: &BotState) -> std::result::Result<String, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let users = client.list_users().await?;

    let mut user_data = Vec::new();
    for user in users {
        let stats = client.get_user_stats(&user.email).await.unwrap_or_default();
        let online = client.get_online_count(&user.email).await.unwrap_or(0);
        user_data.push((user, stats, online));
    }

    // Fetch bridge stats if configured and annotate the output
    let bridge_url = state.config.lock().await.bridge_agent_url.clone();
    if let Some(url) = bridge_url {
        let bridge_client = crate::bridge_client::BridgeClient::new(url);
        if let Ok(json) = bridge_client.get_stats() {
            let bridge_stats = crate::bridge_client::parse_bridge_stats(&json);
            return Ok(format_users_message_with_bridge(&user_data, &bridge_stats));
        }
    }

    Ok(format_users_message(&user_data))
}

/// Execute /add command: add a user, return UUID + bridge + direct URLs.
async fn cmd_add(
    state: &BotState,
    name: &str,
) -> std::result::Result<String, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let uuid = client.add_user(name).await?;
    let bridge_url = backend::build_bridge_vless_url(state.backend.as_ref(), &uuid).await?;
    let direct_url = backend::build_direct_vless_url(state.backend.as_ref(), &uuid).await?;

    // Also register user on bridge agent if configured
    let bridge_agent_url = state.config.lock().await.bridge_agent_url.clone();
    if let Some(agent_url) = bridge_agent_url {
        let email = XrayUser::email_from_name(name);
        let agent = crate::bridge_client::BridgeClient::new(agent_url);
        if let Err(e) = agent.add_user(&uuid, &email) {
            log::warn!("Failed to add user '{}' to bridge agent: {}", name, e);
            // Non-fatal: user is already added to egress
        }
    }

    Ok(format_add_message(name, &uuid, &bridge_url, &direct_url))
}

/// Execute /delete prompt: find user and return confirmation message with inline keyboard.
async fn cmd_delete_prompt(
    state: &BotState,
    name: &str,
) -> std::result::Result<(String, InlineKeyboardMarkup), crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let users = client.list_users().await?;

    let user = users
        .iter()
        .find(|u| u.name == name)
        .ok_or_else(|| crate::error::AppError::Xray(format!("user '{}' not found", name)))?;

    let text = format_delete_confirm_message(name);
    let keyboard = delete_confirmation_keyboard(&user.uuid);
    Ok((text, keyboard))
}

/// Execute the actual user deletion by UUID.
async fn cmd_delete_execute(
    state: &BotState,
    uuid: &str,
) -> std::result::Result<String, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());

    // Look up user name and email before deletion for the response message and bridge call
    let users = client.list_users().await?;
    let (name, email) = users
        .iter()
        .find(|u| u.uuid == uuid)
        .map(|u| (u.name.clone(), u.email.clone()))
        .unwrap_or_else(|| {
            let short = uuid[..std::cmp::min(8, uuid.len())].to_string();
            (short.clone(), short)
        });

    client.remove_user(uuid).await?;

    // Also remove from bridge agent if configured
    let bridge_agent_url = state.config.lock().await.bridge_agent_url.clone();
    if let Some(agent_url) = bridge_agent_url {
        let agent = crate::bridge_client::BridgeClient::new(agent_url);
        let _ = agent.del_user(&email);
    }

    Ok(format_delete_success_message(&name))
}

/// Build an inline keyboard listing all users, for use by /url, /qr, /delete without arguments.
async fn cmd_user_keyboard(
    state: &BotState,
    callback_prefix: &str,
) -> std::result::Result<UserKeyboardResult, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let users = client.list_users().await?;
    Ok(build_user_keyboard(&users, callback_prefix))
}

/// Execute /config command: get both bridge and direct vless:// URLs for a user.
async fn cmd_config(
    state: &BotState,
    name: &str,
) -> std::result::Result<String, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let users = client.list_users().await?;

    let user = users
        .iter()
        .find(|u| u.name == name)
        .ok_or_else(|| crate::error::AppError::Xray(format!("user '{}' not found", name)))?;

    let bridge_url = backend::build_bridge_vless_url(state.backend.as_ref(), &user.uuid).await?;
    let direct_url = backend::build_direct_vless_url(state.backend.as_ref(), &user.uuid).await?;
    Ok(format!(
        "🏠 Для России:\n<pre>{}</pre>\n\n✈️ Из-за рубежа:\n<pre>{}</pre>",
        bridge_url, direct_url
    ))
}

/// Execute /qr command: generate QR code PNGs for bridge and direct vless:// URLs.
async fn cmd_qr(
    state: &BotState,
    name: &str,
) -> std::result::Result<Vec<(Vec<u8>, String)>, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let users = client.list_users().await?;

    let user = users
        .iter()
        .find(|u| u.name == name)
        .ok_or_else(|| crate::error::AppError::Xray(format!("user '{}' not found", name)))?;

    let bridge_url = backend::build_bridge_vless_url(state.backend.as_ref(), &user.uuid).await?;
    let direct_url = backend::build_direct_vless_url(state.backend.as_ref(), &user.uuid).await?;

    let mut results = Vec::new();

    let bridge_png = render_qr_to_png(&bridge_url, 8)
        .map_err(|e| crate::error::AppError::Xray(format!("QR generation failed: {}", e)))?;
    results.push((
        bridge_png,
        format!("🏠 Bridge (censored regions) — {}", name),
    ));

    let direct_png = render_qr_to_png(&direct_url, 8)
        .map_err(|e| crate::error::AppError::Xray(format!("QR generation failed: {}", e)))?;
    results.push((
        direct_png,
        format!("🌍 Direct (abroad) — {}", name),
    ));

    Ok(results)
}

/// Execute /snapshot command: create snapshot and send archive as document.
async fn cmd_snapshot(
    bot: &Bot,
    chat_id: ChatId,
    state: &BotState,
) -> std::result::Result<(), crate::error::AppError> {
    bot.send_message(chat_id, "Creating snapshot...").await.ok();

    let snapshot_dir = state.config.lock().await.snapshot_dir().to_string();
    let info = snapshot::create_snapshot(state.backend.as_ref(), &snapshot_dir).await?;
    let zip_bytes =
        snapshot::pack_snapshot_zip(state.backend.as_ref(), &info.tag, &snapshot_dir).await?;

    let file_name = format!("snapshot-{}.tar.gz", info.tag);
    let caption = format!(
        "\u{1f4e6} Snapshot {} | v{} | {} users",
        info.tag, info.version, info.users_count
    );
    let input = InputFile::memory(zip_bytes).file_name(file_name);
    bot.send_document(chat_id, input)
        .caption(caption)
        .await
        .map_err(|e| crate::error::AppError::Xray(format!("failed to send document: {}", e)))?;

    Ok(())
}

/// Execute /snapshots command: list available snapshots.
async fn cmd_snapshots(state: &BotState) -> std::result::Result<String, crate::error::AppError> {
    let snapshot_dir = state.config.lock().await.snapshot_dir().to_string();
    let snapshots = snapshot::list_snapshots(state.backend.as_ref(), &snapshot_dir).await?;

    if snapshots.is_empty() {
        return Ok("No snapshots found.".to_string());
    }

    let mut lines = Vec::new();
    lines.push("\u{1f4cb} Snapshots:".to_string());
    lines.push(String::new());
    for s in &snapshots {
        lines.push(format!(
            "  {} | v{} | {} users",
            s.tag, s.version, s.users_count
        ));
    }
    Ok(lines.join("\n"))
}

/// Build inline keyboard for restore (last 5 snapshots).
async fn cmd_restore_keyboard(
    state: &BotState,
) -> std::result::Result<Option<(String, InlineKeyboardMarkup)>, crate::error::AppError> {
    let snapshot_dir = state.config.lock().await.snapshot_dir().to_string();
    let snapshots = snapshot::list_snapshots(state.backend.as_ref(), &snapshot_dir).await?;

    if snapshots.is_empty() {
        return Ok(None);
    }

    // Take last 5 (most recent)
    let recent: Vec<_> = snapshots.iter().rev().take(5).collect();
    let buttons: Vec<Vec<InlineKeyboardButton>> = recent
        .iter()
        .map(|s| {
            vec![InlineKeyboardButton::callback(
                format!("{} | v{}", s.tag, s.version),
                format!("{}{}", RESTORE_PREFIX, s.tag),
            )]
        })
        .collect();

    let keyboard = InlineKeyboardMarkup::new(buttons);
    Ok(Some((
        "Select a snapshot to restore:".to_string(),
        keyboard,
    )))
}

/// Execute restore from a specific snapshot tag.
async fn cmd_restore(
    state: &BotState,
    tag: &str,
) -> std::result::Result<String, crate::error::AppError> {
    let snapshot_dir = state.config.lock().await.snapshot_dir().to_string();
    snapshot::restore_snapshot(state.backend.as_ref(), tag, &snapshot_dir).await?;
    Ok(format!(
        "\u{2705} Restored from snapshot [{}]. Container restarted.",
        tag
    ))
}

/// Execute /upgrade command: show confirmation before proceeding.
async fn cmd_upgrade_check(
    bot: &Bot,
    chat_id: ChatId,
    state: &BotState,
) -> std::result::Result<(), crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let server_info = client.get_server_info().await?;
    let latest = snapshot::get_latest_xray_version(state.backend.as_ref()).await?;

    if latest == server_info.version {
        bot.send_message(
            chat_id,
            format!(
                "\u{2705} Already on latest version v{}. Nothing to do.",
                latest
            ),
        )
        .await
        .ok();
        return Ok(());
    }

    let text = format!(
        "\u{26a0}\u{fe0f} Upgrade Xray v{} \u{2192} v{}?\n\n\
         This will restart the container and briefly drop all VPN connections.\n\
         A snapshot will be created automatically before upgrading.",
        server_info.version, latest
    );
    let confirm = InlineKeyboardButton::callback("Yes, upgrade", UPGRADE_CONFIRM_PREFIX);
    let cancel = InlineKeyboardButton::callback("Cancel", UPGRADE_CANCEL_PREFIX);
    let keyboard = InlineKeyboardMarkup::new(vec![vec![confirm, cancel]]);
    bot.send_message(chat_id, text)
        .reply_markup(keyboard)
        .await
        .ok();

    Ok(())
}

/// Execute the actual upgrade after confirmation.
async fn cmd_upgrade_execute(
    bot: &Bot,
    chat_id: ChatId,
    state: &BotState,
) -> std::result::Result<(), crate::error::AppError> {
    bot.send_message(chat_id, "\u{23f3} Upgrading...")
        .await
        .ok();

    let snapshot_dir = state.config.lock().await.snapshot_dir().to_string();
    let result = snapshot::upgrade_xray(state.backend.as_ref(), &snapshot_dir).await?;

    // Send pre-upgrade backup as document
    match snapshot::pack_snapshot_zip(state.backend.as_ref(), &result.snapshot_tag, &snapshot_dir)
        .await
    {
        Ok(zip_bytes) => {
            let file_name = format!("pre-upgrade-{}.tar.gz", result.snapshot_tag);
            let caption = format!("\u{1f4e6} Pre-upgrade backup (v{})", result.old_version);
            let input = InputFile::memory(zip_bytes).file_name(file_name);
            bot.send_document(chat_id, input)
                .caption(caption)
                .await
                .ok();
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Warning: could not pack backup: {}", e))
                .await
                .ok();
        }
    }

    bot.send_message(
        chat_id,
        format!(
            "\u{2705} Upgraded! v{} \u{2192} v{}\nSnapshot: {}",
            result.old_version, result.new_version, result.snapshot_tag
        ),
    )
    .await
    .ok();

    Ok(())
}

/// Format the /url response: vless:// URL as a copyable message.
#[allow(dead_code)]
pub fn format_url_message(name: &str, vless_url: &str) -> String {
    format!("🔗 {} URL:\n\n<pre>{}</pre>", name, vless_url)
}

/// Handle callback queries from inline keyboard buttons (e.g., delete confirmation).
async fn handle_callback(bot: Bot, q: CallbackQuery, state: Arc<BotState>) -> ResponseResult<()> {
    let data = match q.data {
        Some(ref d) => d.as_str(),
        None => return Ok(()),
    };

    // Use q.from.id for the admin check (the user who pressed the button),
    // not q.message.chat().id (which would be the group ID in group chats).
    let caller_id = ChatId(q.from.id.0 as i64);
    let chat_id = match q.message {
        Some(ref msg) => msg.chat().id,
        None => return Ok(()),
    };

    // Check admin access using caller_id (who pressed the button)
    {
        let config = state.config.lock().await;
        if !is_admin(&config, caller_id) {
            bot.answer_callback_query(q.id.clone())
                .text("Access denied.")
                .await?;
            return Ok(());
        }
    }

    if let Some(user_name) = data.strip_prefix(CONFIG_PREFIX) {
        let text = match cmd_config(&state, user_name).await {
            Ok(t) => t,
            Err(e) => format!("Error: {}", e),
        };
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), &text)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    } else if let Some(user_name) = data.strip_prefix(URL_PREFIX) {
        let text = match cmd_config(&state, user_name).await {
            Ok(t) => t,
            Err(e) => format!("Error: {}", e),
        };
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), &text)
                .parse_mode(ParseMode::Html)
                .await?;
        }
    } else if let Some(user_name) = data.strip_prefix(QR_PREFIX) {
        bot.answer_callback_query(q.id.clone()).await?;
        match cmd_qr(&state, user_name).await {
            Ok(qr_images) => {
                for (png_bytes, caption) in qr_images {
                    let input = InputFile::memory(png_bytes).file_name("qr.png");
                    bot.send_photo(chat_id, input).caption(caption).await?;
                }
                // Remove the inline keyboard from the original message
                if let Some(ref msg) = q.message {
                    bot.edit_message_text(chat_id, msg.id(), format!("QR for {}", user_name))
                        .await?;
                }
            }
            Err(e) => {
                bot.send_message(chat_id, format!("Error: {}", e)).await?;
            }
        }
    } else if let Some(user_name) = data.strip_prefix(DELETE_PREFIX) {
        // User selected from /delete inline keyboard — show confirmation
        match cmd_delete_prompt(&state, user_name).await {
            Ok((text, keyboard)) => {
                bot.answer_callback_query(q.id.clone()).await?;
                if let Some(ref msg) = q.message {
                    bot.edit_message_text(chat_id, msg.id(), &text)
                        .reply_markup(keyboard)
                        .await?;
                }
            }
            Err(e) => {
                bot.answer_callback_query(q.id.clone())
                    .text(format!("Error: {}", e))
                    .await?;
            }
        }
    } else if let Some(uuid) = data.strip_prefix(DELETE_CONFIRM_PREFIX) {
        let text = match cmd_delete_execute(&state, uuid).await {
            Ok(t) => t,
            Err(e) => format!("Error: {}", e),
        };
        bot.answer_callback_query(q.id.clone()).await?;
        // Edit the original message to show the result
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), &text).await?;
        }
    } else if data.starts_with(DELETE_CANCEL_PREFIX) {
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), "Deletion cancelled.")
                .await?;
        }
    } else if let Some(tag) = data.strip_prefix(RESTORE_PREFIX) {
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(
                chat_id,
                msg.id(),
                format!("Restoring from snapshot [{}]...", tag),
            )
            .await?;
        }
        let text = match cmd_restore(&state, tag).await {
            Ok(t) => t,
            Err(e) => format!("Error: {}", e),
        };
        bot.send_message(chat_id, text).await?;
    } else if data == UPGRADE_CONFIRM_PREFIX {
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), "\u{23f3} Upgrading...")
                .await?;
        }
        match cmd_upgrade_execute(&bot, chat_id, &state).await {
            Ok(()) => {} // messages already sent inside
            Err(e) => {
                bot.send_message(chat_id, format!("Error: {}", e)).await?;
            }
        }
    } else if data == UPGRADE_CANCEL_PREFIX {
        bot.answer_callback_query(q.id.clone()).await?;
        if let Some(ref msg) = q.message {
            bot.edit_message_text(chat_id, msg.id(), "Upgrade cancelled.")
                .await?;
        }
    }

    Ok(())
}

/// Execute /status command: server info + online summary + optional bridge health.
async fn cmd_status(state: &BotState) -> std::result::Result<String, crate::error::AppError> {
    let client = XrayApiClient::new(state.backend.as_ref());
    let server_info = client.get_server_info().await?;
    let users = client.list_users().await?;

    let mut online_total = 0usize;
    for user in &users {
        let count = client.get_online_count(&user.email).await.unwrap_or(0);
        online_total += count as usize;
    }

    let uptime = crate::backend::fetch_container_uptime(state.backend.as_ref()).await;
    let latest_version = crate::backend::fetch_latest_xray_version(state.backend.as_ref()).await;

    let mut text = format_status_message(
        &server_info,
        users.len(),
        online_total,
        &uptime,
        latest_version.as_deref(),
    );

    // Append bridge agent health status if configured
    let bridge_agent_url = state.config.lock().await.bridge_agent_url.clone();
    if let Some(agent_url) = bridge_agent_url {
        let agent = crate::bridge_client::BridgeClient::new(agent_url);
        let bridge_status = match agent.health() {
            Ok(true) => "Bridge: ✅ online",
            _ => "Bridge: ❌ offline",
        };
        text.push('\n');
        text.push_str(bridge_status);
    }

    Ok(text)
}

/// Start the Telegram bot and block until shutdown.
pub async fn run_bot(token: &str, backend: Box<dyn XrayBackend>, config: Config) -> Result<()> {
    log::info!("Starting Telegram bot...");

    let bot = Bot::new(token);

    // Register public commands visible to all users
    let public_cmds = vec![
        teloxide::types::BotCommand::new("start", "Welcome message"),
        teloxide::types::BotCommand::new("help", "Show help"),
    ];
    if let Err(e) = bot.set_my_commands(public_cmds).await {
        log::warn!("Failed to register public bot commands: {}", e);
    }

    // Register full command list scoped to admin chat only
    if let Some(admin_id) = config.telegram_admin_chat_id {
        let scope = BotCommandScope::Chat {
            chat_id: Recipient::Id(ChatId(admin_id)),
        };
        if let Err(e) = bot
            .set_my_commands(Command::bot_commands())
            .scope(scope)
            .await
        {
            log::warn!("Failed to register admin bot commands: {}", e);
        }
    }

    let state = Arc::new(BotState {
        backend,
        config: Mutex::new(config),
    });

    let state_cmd = Arc::clone(&state);
    let state_cb = Arc::clone(&state);

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(move |bot: Bot, msg: Message, cmd: Command| {
                    let state = Arc::clone(&state_cmd);
                    async move { handle_command(bot, msg, cmd, state).await }
                }),
        )
        .branch(
            Update::filter_callback_query().endpoint(move |bot: Bot, q: CallbackQuery| {
                let state = Arc::clone(&state_cb);
                async move { handle_callback(bot, q, state).await }
            }),
        );

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_help_text_contains_all_commands() {
        let text = help_text();
        assert!(text.contains("/add"));
        assert!(text.contains("/del"));
        assert!(text.contains("/config"));
        assert!(text.contains("/qr"));
        assert!(text.contains("/users"));
        assert!(text.contains("/status"));
        assert!(text.contains("/snapshot"));
        assert!(text.contains("/upgrade"));
    }

    #[test]
    fn test_welcome_text() {
        let text = welcome_text();
        assert!(text.contains("Welcome, admin"));
        assert!(text.contains("/help"));
    }

    #[test]
    fn test_access_denied_text() {
        let text = access_denied_text();
        assert!(text.contains("Access denied"));
        assert!(text.contains("administrator"));
    }

    #[test]
    fn test_is_admin_no_admin_configured() {
        let config = Config::default();
        assert!(!is_admin(&config, ChatId(12345)));
    }

    #[test]
    fn test_is_admin_matching() {
        let mut config = Config::default();
        config.telegram_admin_chat_id = Some(12345);
        assert!(is_admin(&config, ChatId(12345)));
    }

    #[test]
    fn test_is_admin_wrong_user() {
        let mut config = Config::default();
        config.telegram_admin_chat_id = Some(12345);
        assert!(!is_admin(&config, ChatId(99999)));
    }

    #[test]
    fn test_bot_commands_parse() {
        let cmds = Command::bot_commands();
        let descriptions: String = cmds
            .iter()
            .map(|c| c.command.as_str())
            .collect::<Vec<_>>()
            .join(",");
        // teloxide BotCommand.command contains just the name (no slash)
        assert!(descriptions.contains("start"), "commands: {}", descriptions);
        assert!(descriptions.contains("help"), "commands: {}", descriptions);
        assert!(descriptions.contains("users"), "commands: {}", descriptions);
        assert!(
            descriptions.contains("status"),
            "commands: {}",
            descriptions
        );
        assert!(descriptions.contains("add"), "commands: {}", descriptions);
        assert!(descriptions.contains("del"), "commands: {}", descriptions);
        assert!(descriptions.contains("config"), "commands: {}", descriptions);
    }

    #[test]
    fn test_help_text_not_empty() {
        let text = help_text();
        assert!(!text.is_empty());
        assert!(text.lines().count() >= 5);
    }

    #[test]
    fn test_format_users_message_empty() {
        let text = format_users_message(&[]);
        assert_eq!(text, "No users found.");
    }

    #[test]
    fn test_format_users_message_with_users() {
        let users = vec![
            (
                XrayUser {
                    uuid: "aaaa-bbbb-cccc".to_string(),
                    name: "Alice".to_string(),
                    email: "Alice@vpn".to_string(),
                    flow: "xtls-rprx-vision".to_string(),
                    stats: TrafficStats::default(),
                    online_count: 0,
                },
                TrafficStats {
                    uplink: 1024 * 1024 * 100,
                    downlink: 1024 * 1024 * 1024 * 2,
                },
                1,
            ),
            (
                XrayUser {
                    uuid: "dddd-eeee-ffff".to_string(),
                    name: "Bob".to_string(),
                    email: "Bob@vpn".to_string(),
                    flow: "xtls-rprx-vision".to_string(),
                    stats: TrafficStats::default(),
                    online_count: 0,
                },
                TrafficStats {
                    uplink: 0,
                    downlink: 0,
                },
                0,
            ),
        ];
        let text = format_users_message(&users);
        assert!(text.contains("Alice"), "text: {}", text);
        assert!(text.contains("Bob"), "text: {}", text);
        // Alice is online (count=1)
        assert!(text.contains("🟢 1"), "text: {}", text);
        // Bob is offline (count=0)
        assert!(text.contains("⚪"), "text: {}", text);
        // Traffic stats present
        assert!(text.contains("100.0 MB"), "text: {}", text);
        assert!(text.contains("2.0 GB"), "text: {}", text);
    }

    #[test]
    fn test_format_users_message_unnamed_user() {
        let users = vec![(
            XrayUser {
                uuid: "12345678-abcd-efgh".to_string(),
                name: String::new(),
                email: "@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            TrafficStats::default(),
            0,
        )];
        let text = format_users_message(&users);
        // Should show truncated UUID for unnamed users
        assert!(text.contains("12345678"), "text: {}", text);
    }

    #[test]
    fn test_format_status_message() {
        let info = ServerInfo {
            version: "1.8.4".to_string(),
            uplink: 1024 * 1024 * 500,
            downlink: 1024 * 1024 * 1024 * 10,
        };
        let text = format_status_message(&info, 5, 2, "Up 2 hours", Some("1.8.4"));
        assert!(text.contains("v1.8.4"), "text: {}", text);
        assert!(text.contains("✅"), "text: {}", text);
        assert!(text.contains("Up 2 hours"), "text: {}", text);
        assert!(text.contains("5"), "text: {}", text);
        assert!(text.contains("2 online"), "text: {}", text);
        assert!(text.contains("500.0 MB"), "text: {}", text);
        assert!(text.contains("10.0 GB"), "text: {}", text);
    }

    #[test]
    fn test_format_status_message_update_available() {
        let info = ServerInfo {
            version: "1.8.0".to_string(),
            uplink: 0,
            downlink: 0,
        };
        let text = format_status_message(&info, 3, 0, "Up 5 minutes", Some("1.9.0"));
        assert!(text.contains("⬆️ v1.9.0"), "text: {}", text);
    }

    #[test]
    fn test_format_status_message_zero_online() {
        let info = ServerInfo {
            version: "1.8.0".to_string(),
            uplink: 0,
            downlink: 0,
        };
        let text = format_status_message(&info, 3, 0, "", None);
        assert!(text.contains("0 online"), "text: {}", text);
        assert!(text.contains("3"), "text: {}", text);
    }

    #[test]
    fn test_config_telegram_admin_serialization() {
        let mut config = Config::default();
        config.telegram_admin_chat_id = Some(123456789);
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("telegram_admin_chat_id = 123456789"));

        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.telegram_admin_chat_id, Some(123456789));
    }

    #[test]
    fn test_config_telegram_admin_none_not_serialized() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(!toml_str.contains("telegram_admin_chat_id"));
    }

    // -- /add and /delete formatting tests --

    #[test]
    fn test_format_add_message() {
        let text = format_add_message(
            "Alice",
            "uuid-123",
            "vless://uuid-123@bridge:443?...",
            "vless://uuid-123@direct:443?...",
        );
        assert!(text.contains("Alice"), "text: {}", text);
        assert!(text.contains("uuid-123"), "text: {}", text);
        assert!(text.contains("vless://"), "text: {}", text);
        assert!(text.contains("✅"), "text: {}", text);
        assert!(text.contains("Для России"), "text: {}", text);
        assert!(text.contains("Из-за рубежа"), "text: {}", text);
    }

    #[test]
    fn test_format_add_message_special_name() {
        let text = format_add_message("Bob's Phone [iOS]", "uuid-456", "vless://bridge...", "vless://direct...");
        assert!(text.contains("Bob's Phone [iOS]"), "text: {}", text);
    }

    #[test]
    fn test_format_delete_confirm_message() {
        let text = format_delete_confirm_message("Alice");
        assert!(text.contains("Alice"), "text: {}", text);
        assert!(text.contains("⚠️"), "text: {}", text);
        assert!(text.contains("Delete"), "text: {}", text);
    }

    #[test]
    fn test_format_delete_success_message() {
        let text = format_delete_success_message("Alice");
        assert!(text.contains("Alice"), "text: {}", text);
        assert!(text.contains("deleted"), "text: {}", text);
    }

    #[test]
    fn test_validate_user_name_valid() {
        assert!(validate_user_name("Alice").is_none());
        assert!(validate_user_name("Bob's Phone").is_none());
        assert!(validate_user_name("Admin [macOS]").is_none());
        assert!(validate_user_name("a").is_none());
    }

    #[test]
    fn test_validate_user_name_empty() {
        assert!(validate_user_name("").is_some());
        assert!(validate_user_name("   ").is_some());
    }

    #[test]
    fn test_add_without_argument_shows_usage_hint() {
        // /add without argument passes empty string to validate_user_name,
        // which returns a usage hint instead of proceeding with add
        let result = validate_user_name("");
        assert_eq!(result, Some("Usage: /add <name>".to_string()));

        let result_whitespace = validate_user_name("   ");
        assert_eq!(result_whitespace, Some("Usage: /add <name>".to_string()));
    }

    #[test]
    fn test_validate_user_name_too_long() {
        let long_name = "a".repeat(51);
        let result = validate_user_name(&long_name);
        assert!(result.is_some());
        assert!(result.unwrap().contains("too long"));
    }

    #[test]
    fn test_delete_confirmation_keyboard() {
        let keyboard = delete_confirmation_keyboard("uuid-123");
        let buttons = &keyboard.inline_keyboard;
        assert_eq!(buttons.len(), 1, "should have one row");
        assert_eq!(buttons[0].len(), 2, "should have two buttons");

        // Check button text
        assert_eq!(buttons[0][0].text, "Yes, delete");
        assert_eq!(buttons[0][1].text, "Cancel");
    }

    #[test]
    fn test_delete_confirmation_keyboard_callback_data() {
        let keyboard = delete_confirmation_keyboard("test-uuid-abc");
        let buttons = &keyboard.inline_keyboard;

        // Extract callback data from buttons
        let confirm_data = match &buttons[0][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        let cancel_data = match &buttons[0][1].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };

        assert_eq!(confirm_data, "delete_confirm:test-uuid-abc");
        assert_eq!(cancel_data, "delete_cancel:test-uuid-abc");
    }

    #[test]
    fn test_validate_user_name_at_sign() {
        assert!(validate_user_name("foo@bar").is_some());
    }

    #[test]
    fn test_validate_user_name_triple_arrow() {
        assert!(validate_user_name("foo>>>bar").is_some());
    }

    #[test]
    fn test_validate_user_name_usage_hint() {
        let result = validate_user_name("").unwrap();
        assert!(result.contains("/add"), "result: {}", result);
    }

    // -- /url formatting tests --

    #[test]
    fn test_format_url_message() {
        let text = format_url_message("Alice", "vless://uuid@1.2.3.4:443?test=1#Alice");
        assert!(text.contains("Alice"), "text: {}", text);
        assert!(text.contains("vless://"), "text: {}", text);
        assert!(text.contains("🔗"), "text: {}", text);
    }

    #[test]
    fn test_format_url_message_special_name() {
        let text = format_url_message("Bob's Phone [iOS]", "vless://...");
        assert!(text.contains("Bob's Phone [iOS]"), "text: {}", text);
    }

    #[test]
    fn test_bot_commands_include_config_and_qr() {
        let cmds = Command::bot_commands();
        let descriptions: String = cmds
            .iter()
            .map(|c| c.command.as_str())
            .collect::<Vec<_>>()
            .join(",");
        assert!(descriptions.contains("config"), "commands: {}", descriptions);
        assert!(descriptions.contains("qr"), "commands: {}", descriptions);
    }

    // -- Inline keyboard / callback data tests --

    #[test]
    fn test_build_user_keyboard_url_prefix() {
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: "Alice".to_string(),
                email: "Alice@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: "Bob".to_string(),
                email: "Bob@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, URL_PREFIX);
        assert!(result.skipped_names.is_empty());
        assert_eq!(result.unnamed_count, 0);
        let buttons = &result.keyboard.inline_keyboard;
        assert_eq!(buttons.len(), 2, "should have two rows");
        assert_eq!(buttons[0][0].text, "Alice");
        assert_eq!(buttons[1][0].text, "Bob");

        // Verify callback data
        let alice_data = match &buttons[0][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        let bob_data = match &buttons[1][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        assert_eq!(alice_data, "url:Alice");
        assert_eq!(bob_data, "url:Bob");
    }

    #[test]
    fn test_build_user_keyboard_skips_empty_names() {
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: "Alice".to_string(),
                email: "Alice@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: String::new(),
                email: "@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, "url:");
        let buttons = &result.keyboard.inline_keyboard;
        assert_eq!(buttons.len(), 1, "should skip unnamed user");
        assert_eq!(buttons[0][0].text, "Alice");
        assert_eq!(result.unnamed_count, 1);
    }

    #[test]
    fn test_build_user_keyboard_empty_list() {
        let result = build_user_keyboard(&[], "url:");
        assert!(result.keyboard.inline_keyboard.is_empty());
        assert!(result.skipped_names.is_empty());
        assert_eq!(result.unnamed_count, 0);
    }

    #[test]
    fn test_build_user_keyboard_different_prefixes() {
        let users = vec![XrayUser {
            uuid: "uuid-1".to_string(),
            name: "Alice".to_string(),
            email: "Alice@vpn".to_string(),
            flow: String::new(),
            stats: TrafficStats::default(),
            online_count: 0,
        }];

        for prefix in &["url:", "qr:", "delete:"] {
            let result = build_user_keyboard(&users, prefix);
            let data = match &result.keyboard.inline_keyboard[0][0].kind {
                teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
                _ => panic!("expected callback data"),
            };
            assert_eq!(data, format!("{}Alice", prefix));
        }
    }

    #[test]
    fn test_url_callback_data_parsing() {
        // Verify that URL_PREFIX correctly strips from callback data
        let callback_data = "url:Alice";
        let user_name = callback_data.strip_prefix(URL_PREFIX);
        assert_eq!(user_name, Some("Alice"));

        let callback_data = "url:Bob's Phone [iOS]";
        let user_name = callback_data.strip_prefix(URL_PREFIX);
        assert_eq!(user_name, Some("Bob's Phone [iOS]"));
    }

    #[test]
    fn test_url_callback_data_no_match() {
        let callback_data = "qr:Alice";
        assert!(callback_data.strip_prefix(URL_PREFIX).is_none());
    }

    #[test]
    fn test_qr_callback_data_parsing() {
        let callback_data = "qr:Alice";
        let user_name = callback_data.strip_prefix(QR_PREFIX);
        assert_eq!(user_name, Some("Alice"));

        let callback_data = "qr:Bob's Phone [iOS]";
        let user_name = callback_data.strip_prefix(QR_PREFIX);
        assert_eq!(user_name, Some("Bob's Phone [iOS]"));
    }

    #[test]
    fn test_qr_callback_data_no_match() {
        let callback_data = "url:Alice";
        assert!(callback_data.strip_prefix(QR_PREFIX).is_none());

        let callback_data = "delete:Alice";
        assert!(callback_data.strip_prefix(QR_PREFIX).is_none());
    }

    #[test]
    fn test_build_user_keyboard_qr_prefix() {
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: "Alice".to_string(),
                email: "Alice@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: "Bob".to_string(),
                email: "Bob@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, QR_PREFIX);
        assert!(result.skipped_names.is_empty());
        let buttons = &result.keyboard.inline_keyboard;
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0][0].text, "Alice");
        assert_eq!(buttons[1][0].text, "Bob");

        let alice_data = match &buttons[0][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        let bob_data = match &buttons[1][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        assert_eq!(alice_data, "qr:Alice");
        assert_eq!(bob_data, "qr:Bob");
    }

    #[test]
    fn test_is_admin_negative_id() {
        // Telegram group chat IDs can be negative
        let mut config = Config::default();
        config.telegram_admin_chat_id = Some(-100123456);
        assert!(is_admin(&config, ChatId(-100123456)));
        assert!(!is_admin(&config, ChatId(100123456)));
    }

    #[test]
    fn test_admin_id_from_cli() {
        use clap::Parser;
        let cli = crate::config::Cli::parse_from([
            "app",
            "--telegram-bot",
            "--local",
            "--admin-id",
            "123456789",
        ]);
        assert_eq!(cli.admin_id, Some(123456789));
    }

    #[test]
    fn test_admin_id_merged_into_config() {
        let mut config = Config::default();
        assert_eq!(config.telegram_admin_chat_id, None);

        use clap::Parser;
        let cli = crate::config::Cli::parse_from(["app", "--admin-id", "999"]);
        config.merge_cli(&cli);
        assert_eq!(config.telegram_admin_chat_id, Some(999));
    }

    // -- /delete inline buttons tests --

    #[test]
    fn test_build_user_keyboard_delete_prefix() {
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: "Alice".to_string(),
                email: "Alice@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: "Bob".to_string(),
                email: "Bob@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, DELETE_PREFIX);
        assert!(result.skipped_names.is_empty());
        let buttons = &result.keyboard.inline_keyboard;
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0][0].text, "Alice");
        assert_eq!(buttons[1][0].text, "Bob");

        let alice_data = match &buttons[0][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        let bob_data = match &buttons[1][0].kind {
            teloxide::types::InlineKeyboardButtonKind::CallbackData(d) => d.clone(),
            _ => panic!("expected callback data"),
        };
        assert_eq!(alice_data, "delete:Alice");
        assert_eq!(bob_data, "delete:Bob");
    }

    #[test]
    fn test_delete_callback_data_parsing() {
        // Verify DELETE_PREFIX correctly strips from callback data
        let callback_data = "delete:Alice";
        let user_name = callback_data.strip_prefix(DELETE_PREFIX);
        assert_eq!(user_name, Some("Alice"));

        let callback_data = "delete:Bob's Phone [iOS]";
        let user_name = callback_data.strip_prefix(DELETE_PREFIX);
        assert_eq!(user_name, Some("Bob's Phone [iOS]"));
    }

    #[test]
    fn test_delete_callback_data_no_match() {
        let callback_data = "url:Alice";
        assert!(callback_data.strip_prefix(DELETE_PREFIX).is_none());

        let callback_data = "qr:Alice";
        assert!(callback_data.strip_prefix(DELETE_PREFIX).is_none());
    }

    #[test]
    fn test_delete_prefix_does_not_match_confirm_cancel() {
        // "delete:" should NOT match "delete_confirm:" or "delete_cancel:"
        let confirm_data = "delete_confirm:uuid-123";
        assert!(confirm_data.strip_prefix(DELETE_PREFIX).is_none());

        let cancel_data = "delete_cancel:uuid-123";
        assert!(cancel_data.strip_prefix(DELETE_PREFIX).is_none());
    }

    #[test]
    fn test_delete_confirm_cancel_prefixes_distinct() {
        // Verify all delete-related prefixes are distinct
        assert_ne!(DELETE_PREFIX, DELETE_CONFIRM_PREFIX);
        assert_ne!(DELETE_PREFIX, DELETE_CANCEL_PREFIX);
        assert_ne!(DELETE_CONFIRM_PREFIX, DELETE_CANCEL_PREFIX);

        // None is a prefix of another (important for strip_prefix correctness)
        assert!(!DELETE_CONFIRM_PREFIX.starts_with(DELETE_PREFIX));
        assert!(!DELETE_CANCEL_PREFIX.starts_with(DELETE_PREFIX));
    }

    #[test]
    fn test_build_user_keyboard_skips_long_names() {
        // "delete:" is 7 bytes, so max name is 57 bytes
        let long_name = "a".repeat(58);
        let short_name = "Alice".to_string();
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: short_name.clone(),
                email: "Alice@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: long_name.clone(),
                email: "long@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, DELETE_PREFIX);
        assert_eq!(result.keyboard.inline_keyboard.len(), 1);
        assert_eq!(result.keyboard.inline_keyboard[0][0].text, "Alice");
        assert_eq!(result.skipped_names, vec![long_name]);
    }

    #[test]
    fn test_build_user_keyboard_all_skipped() {
        let long_name = "a".repeat(61); // exceeds 64 bytes with "url:" prefix
        let users = vec![XrayUser {
            uuid: "uuid-1".to_string(),
            name: long_name.clone(),
            email: "long@vpn".to_string(),
            flow: String::new(),
            stats: TrafficStats::default(),
            online_count: 0,
        }];
        let result = build_user_keyboard(&users, URL_PREFIX);
        assert!(result.keyboard.inline_keyboard.is_empty());
        assert_eq!(result.skipped_names, vec![long_name]);
    }

    #[test]
    fn test_build_user_keyboard_all_unnamed() {
        let users = vec![
            XrayUser {
                uuid: "uuid-1".to_string(),
                name: String::new(),
                email: "@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
            XrayUser {
                uuid: "uuid-2".to_string(),
                name: String::new(),
                email: "@vpn".to_string(),
                flow: String::new(),
                stats: TrafficStats::default(),
                online_count: 0,
            },
        ];
        let result = build_user_keyboard(&users, URL_PREFIX);
        assert!(result.keyboard.inline_keyboard.is_empty());
        assert!(result.skipped_names.is_empty());
        assert_eq!(result.unnamed_count, 2);
    }

    #[test]
    fn test_format_empty_keyboard_message_no_users() {
        let msg = format_empty_keyboard_message("/url <name>", &[], 0);
        assert_eq!(msg, "No users found.");
    }

    #[test]
    fn test_format_empty_keyboard_message_unnamed_only() {
        let msg = format_empty_keyboard_message("/url <name>", &[], 3);
        assert!(msg.contains("3 user(s) have no name set"));
        assert!(msg.contains("/users"));
    }

    #[test]
    fn test_format_empty_keyboard_message_skipped_and_unnamed() {
        let skipped = vec!["LongName".to_string()];
        let msg = format_empty_keyboard_message("/delete <name>", &skipped, 2);
        assert!(msg.contains("1 user(s) have names too long"));
        assert!(msg.contains("LongName"));
        assert!(msg.contains("2 user(s) have no name set"));
    }

    #[test]
    fn test_format_selection_message_no_skipped() {
        let msg = format_selection_message("Select a user:", &[], 0);
        assert_eq!(msg, "Select a user:");
    }

    #[test]
    fn test_format_selection_message_with_skipped() {
        let skipped = vec!["LongUserName".to_string()];
        let msg = format_selection_message("Select a user:", &skipped, 0);
        assert!(msg.contains("Select a user:"));
        assert!(msg.contains("LongUserName"));
        assert!(msg.contains("1 user(s)"));
    }

    #[test]
    fn test_format_selection_message_with_unnamed() {
        let msg = format_selection_message("Select a user:", &[], 3);
        assert!(msg.contains("Select a user:"));
        assert!(msg.contains("3 unnamed user(s) not shown"));
        assert!(msg.contains("/users"));
    }

    #[test]
    fn test_format_selection_message_with_skipped_and_unnamed() {
        let skipped = vec!["LongName".to_string()];
        let msg = format_selection_message("Select a user:", &skipped, 2);
        assert!(msg.contains("Select a user:"));
        assert!(msg.contains("LongName"));
        assert!(msg.contains("2 unnamed user(s) not shown"));
    }
}
