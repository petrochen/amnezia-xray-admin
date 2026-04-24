use std::io::{self, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;

use crate::backend::{self, BackendMsg, VlessUrlIntent};
use crate::config::Config;
use crate::ui::add_user::{self, AddUserResult, AddUserState};
use crate::ui::dashboard::{self, DashboardState};
use crate::ui::qr::{self, QrViewState};
use crate::ui::setup::{self, SetupState};
use crate::ui::telegram_setup::{self, TelegramSetupState};
use crate::ui::theme;
use crate::ui::user_detail::{self, DetailMode, UserDetailState};

/// Screens in the application
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Setup,
    Dashboard,
    UserDetail,
    AddUser,
    QrView,
    TelegramSetup,
}

/// Refresh interval for stats polling
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Core application state
pub struct App {
    pub screen: Screen,
    pub running: bool,
    pub last_refresh: Instant,
    pub status_message: String,
    pub setup_state: SetupState,
    pub dashboard_state: DashboardState,
    pub add_user_state: AddUserState,
    pub user_detail_state: UserDetailState,
    pub qr_view_state: QrViewState,
    pub telegram_setup_state: TelegramSetupState,
    pub config: Config,
    /// Tokio runtime handle for spawning async operations
    runtime: tokio::runtime::Handle,
    /// Channel for receiving backend operation results
    backend_rx: mpsc::Receiver<BackendMsg>,
    /// Channel sender cloned into spawned tasks
    backend_tx: mpsc::Sender<BackendMsg>,
    /// Whether a backend operation is in flight (prevents duplicate requests)
    pending_refresh: bool,
    /// Whether we've done the initial data load
    initial_load_done: bool,
    /// Whether ensure_api_enabled has already succeeded (skip on subsequent refreshes)
    api_check_done: bool,
    /// Whether we've already fetched the latest Xray release from GitHub this session.
    /// Prevents hammering the unauthenticated GitHub API (60 req/h limit) on every 5 s refresh.
    version_check_done: bool,
    /// Whether a mutation completed and we need to refresh once the current fetch finishes
    refresh_after_mutation: bool,
    /// Name of the user being added (prevents duplicate submissions and stale error routing)
    pending_add_name: Option<String>,
    /// UUID of the user being deleted (prevents duplicate submissions and stale error routing)
    pending_delete_uuid: Option<String>,
    /// Whether a connection test is in flight (prevents stale results from overwriting)
    pending_test: bool,
    /// Config snapshot taken when a connection test was started (for staleness detection)
    tested_config: Option<Config>,
    /// Whether a bot deployment is in flight
    pending_deploy: bool,
}

impl App {
    pub fn with_config(config: Config, runtime: tokio::runtime::Handle) -> Self {
        let has_config = config.has_connection_info();
        let mut dashboard_state = DashboardState::default();
        if has_config {
            if let Some(ref host) = config.host {
                dashboard_state.server_host = host.clone();
            } else if let Some(ref ssh_host) = config.ssh_host {
                dashboard_state.server_host = ssh_host.clone();
            }
        }
        let (tx, rx) = mpsc::channel();
        Self {
            screen: if has_config {
                Screen::Dashboard
            } else {
                Screen::Setup
            },
            running: true,
            last_refresh: Instant::now(),
            status_message: String::new(),
            setup_state: SetupState::from_config(&config),
            dashboard_state,
            add_user_state: AddUserState::default(),
            user_detail_state: UserDetailState::default(),
            qr_view_state: QrViewState::default(),
            telegram_setup_state: TelegramSetupState::from_config(
                config.telegram_token.as_deref(),
                config.telegram_admin_chat_id,
            ),
            config,
            runtime,
            backend_rx: rx,
            backend_tx: tx,
            pending_refresh: false,
            initial_load_done: false,
            api_check_done: false,
            version_check_done: false,
            refresh_after_mutation: false,
            pending_add_name: None,
            pending_delete_uuid: None,
            pending_test: false,
            tested_config: None,
            pending_deploy: false,
        }
    }

    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Handle a key event and update state accordingly
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Global: Ctrl+C or Ctrl+Q always quits
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && (key.code == KeyCode::Char('c') || key.code == KeyCode::Char('q'))
        {
            self.quit();
            return;
        }

        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key),
            Screen::Setup => self.handle_setup_key(key),
            Screen::UserDetail => self.handle_user_detail_key(key),
            Screen::AddUser => self.handle_add_user_key(key),
            Screen::QrView => self.handle_qr_view_key(key),
            Screen::TelegramSetup => self.handle_telegram_setup_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit(),
            KeyCode::Char('r') => {
                self.trigger_refresh();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.dashboard_state.select_next();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.dashboard_state.select_previous();
            }
            KeyCode::Enter => {
                if let Some(user) = self.dashboard_state.selected_user() {
                    let user = user.clone();
                    self.user_detail_state.open(user.clone());
                    self.screen = Screen::UserDetail;
                    // Fetch online IPs in background
                    backend::spawn_fetch_online_ips(
                        &self.runtime,
                        self.config.clone(),
                        user.uuid.clone(),
                        user.email.clone(),
                        self.backend_tx.clone(),
                    );
                }
            }
            KeyCode::Char('a') => {
                self.add_user_state.reset();
                self.screen = Screen::AddUser;
            }
            KeyCode::Char('d') => {
                if let Some(user) = self.dashboard_state.selected_user() {
                    let user = user.clone();
                    self.user_detail_state.open(user.clone());
                    self.user_detail_state.mode = DetailMode::DeleteConfirm;
                    self.screen = Screen::UserDetail;
                    // Fetch online IPs in background
                    backend::spawn_fetch_online_ips(
                        &self.runtime,
                        self.config.clone(),
                        user.uuid.clone(),
                        user.email.clone(),
                        self.backend_tx.clone(),
                    );
                }
            }
            KeyCode::Char('t') => {
                self.telegram_setup_state = TelegramSetupState::from_config(
                    self.config.telegram_token.as_deref(),
                    self.config.telegram_admin_chat_id,
                );
                self.screen = Screen::TelegramSetup;
            }
            _ => {}
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.quit();
            return;
        }

        self.setup_state.handle_key(key);

        // Check if save was requested
        if self.setup_state.save_requested {
            self.setup_state.save_requested = false;
            let mut new_config = self.setup_state.to_config();
            // Preserve Telegram credentials that aren't part of the setup wizard
            new_config.telegram_token = self.config.telegram_token.clone();
            new_config.telegram_admin_chat_id = self.config.telegram_admin_chat_id;
            if !new_config.has_connection_info() {
                self.setup_state.test_result =
                    setup::TestResult::Error("Please enter a host or SSH config alias".to_string());
                return;
            }
            match new_config.save() {
                Ok(()) => {
                    self.config = new_config;
                    // Update dashboard host display
                    if let Some(ref host) = self.config.host {
                        self.dashboard_state.server_host = host.clone();
                    } else if let Some(ref ssh_host) = self.config.ssh_host {
                        self.dashboard_state.server_host = ssh_host.clone();
                    }
                    self.dashboard_state.loading = true;
                    self.screen = Screen::Dashboard;
                    self.status_message = "Config saved. Connecting...".to_string();
                    // Trigger initial data load
                    self.trigger_refresh();
                }
                Err(e) => {
                    self.setup_state.test_result =
                        setup::TestResult::Error(format!("Save failed: {}", e));
                }
            }
        }

        // Check if test was requested (guard: only one test at a time)
        if self.setup_state.test_requested {
            self.setup_state.test_requested = false;
            if !self.pending_test {
                let test_config = self.setup_state.to_config();
                if !test_config.has_connection_info() {
                    self.setup_state.test_result = setup::TestResult::Error(
                        "Please enter a host or SSH config alias".to_string(),
                    );
                } else {
                    self.pending_test = true;
                    self.tested_config = Some(test_config.clone());
                    self.setup_state.test_result = setup::TestResult::Testing;
                    backend::spawn_test_connection(
                        &self.runtime,
                        test_config,
                        self.backend_tx.clone(),
                    );
                }
            }
        }
    }

    fn handle_user_detail_key(&mut self, key: KeyEvent) {
        // Let the detail state handle input first
        if self.user_detail_state.handle_key(key) {
            // Check if clipboard copy was requested
            if self.user_detail_state.take_clipboard_copied() {
                if let Some(ref user) = self.user_detail_state.user {
                    // Fetch real vless URL in background, but for clipboard we need it now.
                    // Use a placeholder for immediate feedback; the QR view will get the real URL.
                    if let Some(ref url) = self.user_detail_state.cached_vless_url {
                        let osc52 = user_detail::osc52_copy(url);
                        print!("{}", osc52);
                        let _ = io::stdout().flush();
                        self.status_message = "Copied to clipboard (OSC 52)".to_string();
                    } else {
                        // Spawn URL generation and copy when ready
                        self.status_message = "Generating URL...".to_string();
                        backend::spawn_vless_url(
                            &self.runtime,
                            self.config.clone(),
                            user.uuid.clone(),
                            user.name.clone(),
                            VlessUrlIntent::Clipboard,
                            self.backend_tx.clone(),
                        );
                    }
                }
            }

            // Check if delete was confirmed (guard against duplicate submissions
            // and concurrent mutations — block if an add is also in flight)
            if self.user_detail_state.take_delete_confirmed()
                && self.pending_delete_uuid.is_none()
                && self.pending_add_name.is_none()
            {
                if let Some(ref user) = self.user_detail_state.user {
                    self.pending_delete_uuid = Some(user.uuid.clone());
                    self.status_message = format!("Deleting user '{}'...", user.name);
                    backend::spawn_delete_user(
                        &self.runtime,
                        self.config.clone(),
                        user.uuid.clone(),
                        self.backend_tx.clone(),
                    );
                }
            }
            return;
        }

        // Keys not consumed by user_detail_state
        match &self.user_detail_state.mode {
            DetailMode::View => match key.code {
                KeyCode::Esc => {
                    self.user_detail_state.close();
                    self.screen = Screen::Dashboard;
                }
                KeyCode::Char('q') => {
                    if let Some(ref user) = self.user_detail_state.user {
                        if let Some(ref url) = self.user_detail_state.cached_vless_url {
                            self.qr_view_state.open(user.name.clone(), url.clone());
                            self.screen = Screen::QrView;
                        } else {
                            // Need to fetch the URL first
                            self.status_message = "Generating QR code...".to_string();
                            backend::spawn_vless_url(
                                &self.runtime,
                                self.config.clone(),
                                user.uuid.clone(),
                                user.name.clone(),
                                VlessUrlIntent::Qr,
                                self.backend_tx.clone(),
                            );
                        }
                    }
                }
                _ => {}
            },
            DetailMode::DeleteSuccess | DetailMode::DeleteError(_) => {
                // Any key returns to dashboard
                self.user_detail_state.close();
                self.screen = Screen::Dashboard;
            }
            _ => {}
        }
    }

    fn handle_add_user_key(&mut self, key: KeyEvent) {
        // Let the add_user state handle input first
        if self.add_user_state.handle_key(key) {
            // Check if user confirmed the add (guard against duplicate submissions
            // and concurrent mutations — block if a delete is also in flight)
            if self.add_user_state.is_confirmed()
                && self.pending_add_name.is_none()
                && self.pending_delete_uuid.is_none()
            {
                let name = self.add_user_state.name.trim().to_string();
                self.add_user_state.confirmed = false; // prevent re-trigger
                self.pending_add_name = Some(name.clone());
                self.status_message = format!("Adding user '{}'...", name);
                backend::spawn_add_user(
                    &self.runtime,
                    self.config.clone(),
                    name,
                    self.backend_tx.clone(),
                );
            }
            return;
        }

        // Keys not consumed by add_user state
        match key.code {
            KeyCode::Esc => {
                self.add_user_state.reset();
                self.screen = Screen::Dashboard;
            }
            KeyCode::Char('q') => {
                if let AddUserResult::Success { ref name, ref uuid } = self.add_user_state.result {
                    if let Some(ref url) = self.add_user_state.cached_vless_url {
                        self.qr_view_state.open(name.clone(), url.clone());
                        self.screen = Screen::QrView;
                    } else {
                        self.status_message = "Generating QR code...".to_string();
                        backend::spawn_vless_url(
                            &self.runtime,
                            self.config.clone(),
                            uuid.clone(),
                            name.clone(),
                            VlessUrlIntent::Qr,
                            self.backend_tx.clone(),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_qr_view_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.qr_view_state.close();
                self.screen = Screen::Dashboard;
            }
            _ => {}
        }
    }

    fn handle_telegram_setup_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.screen = Screen::Dashboard;
            return;
        }

        self.telegram_setup_state.handle_key(key);

        // Check if deploy was requested
        if self.telegram_setup_state.deploy_requested && !self.pending_deploy {
            self.telegram_setup_state.deploy_requested = false;
            let token = self.telegram_setup_state.token.trim().to_string();
            let admin_id_str = self.telegram_setup_state.admin_id.trim().to_string();

            if !telegram_setup::is_valid_token(&token) {
                self.telegram_setup_state.deploy_status = telegram_setup::DeployStatus::Error(
                    "Invalid token format (expected digits:secret)".to_string(),
                );
                return;
            }

            let admin_id: i64 = match admin_id_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    self.telegram_setup_state.deploy_status = telegram_setup::DeployStatus::Error(
                        "Invalid Admin ID (must be a number)".to_string(),
                    );
                    return;
                }
            };

            // Save token and admin ID to config
            self.config.telegram_token = Some(token.clone());
            self.config.telegram_admin_chat_id = Some(admin_id);
            if let Err(e) = self.config.save() {
                self.telegram_setup_state.deploy_status =
                    telegram_setup::DeployStatus::Error(format!("Config save failed: {}", e));
                return;
            }

            self.pending_deploy = true;
            self.telegram_setup_state.deploy_status = telegram_setup::DeployStatus::Connecting;
            backend::spawn_deploy_bot(
                &self.runtime,
                self.config.clone(),
                token,
                self.backend_tx.clone(),
            );
        }
    }

    /// Trigger a data refresh from the server
    fn trigger_refresh(&mut self) {
        if self.pending_refresh {
            return; // already in flight
        }
        self.pending_refresh = true;
        self.last_refresh = Instant::now();
        self.status_message = "Refreshing...".to_string();
        backend::spawn_fetch_dashboard(
            &self.runtime,
            self.config.clone(),
            self.backend_tx.clone(),
            self.api_check_done,
            !self.version_check_done,
        );
    }

    /// Process any pending backend messages
    fn process_backend_messages(&mut self) {
        while let Ok(msg) = self.backend_rx.try_recv() {
            match msg {
                BackendMsg::DashboardData(result) => {
                    self.pending_refresh = false;
                    // If a mutation happened while this fetch was in flight,
                    // discard the stale result and re-fetch.
                    if self.refresh_after_mutation {
                        self.refresh_after_mutation = false;
                        self.trigger_refresh();
                        continue;
                    }
                    match result {
                        Ok(data) => {
                            self.dashboard_state.set_users(data.users);
                            self.dashboard_state.server_version = data.server_info.version;
                            self.dashboard_state.container_uptime = data.container_uptime;
                            if data.version_was_fetched {
                                self.dashboard_state.latest_version = data.latest_version;
                                self.version_check_done = true;
                            }
                            self.dashboard_state.total_upload = data.server_info.uplink;
                            self.dashboard_state.total_download = data.server_info.downlink;
                            self.dashboard_state.loading = false;
                            self.initial_load_done = true;
                            self.api_check_done = true;
                            self.status_message =
                                format!("Last update: {}", chrono_free_timestamp());
                        }
                        Err(e) => {
                            self.dashboard_state.loading = false;
                            self.status_message = format!("Error: {}", truncate_msg(&e, 60));
                        }
                    }
                }
                BackendMsg::ConnectionTest(result) => {
                    self.pending_test = false;
                    let tested = self.tested_config.take();
                    // Only apply result if still on setup screen and the form
                    // hasn't changed since the test was initiated
                    if self.screen == Screen::Setup {
                        let current_config = self.setup_state.to_config();
                        let is_stale = tested
                            .as_ref()
                            .map(|tc| *tc != current_config)
                            .unwrap_or(false);
                        if is_stale {
                            // Form was edited since test started; discard result
                            continue;
                        }
                        match result {
                            Ok(version) => {
                                self.setup_state.test_result =
                                    setup::TestResult::Success(format!("Connected! {}", version));
                            }
                            Err(e) => {
                                self.setup_state.test_result =
                                    setup::TestResult::Error(truncate_msg(&e, 60));
                            }
                        }
                    }
                }
                BackendMsg::UserAdded(result) => {
                    let request_name = self.pending_add_name.take();
                    match result {
                        Ok(added) => {
                            // Only update the add dialog if we're still on AddUser
                            // and the name matches the in-flight request
                            let is_current_add = self.screen == Screen::AddUser
                                && request_name.as_deref() == Some(added.name.as_str())
                                && self.add_user_state.name.trim() == added.name;
                            if is_current_add {
                                self.add_user_state
                                    .set_success(added.name.clone(), added.uuid.clone());
                                self.add_user_state.cached_vless_url = if added.vless_url.is_empty()
                                {
                                    None
                                } else {
                                    Some(added.vless_url)
                                };
                            }
                            self.status_message =
                                format!("User '{}' added successfully", added.name);
                            // Refresh dashboard to show new user
                            if self.pending_refresh {
                                self.refresh_after_mutation = true;
                            } else {
                                self.trigger_refresh();
                            }
                        }
                        Err(e) => {
                            // Only show add error if we're still on AddUser
                            // and this error corresponds to the current request
                            let is_current_add = self.screen == Screen::AddUser
                                && request_name.as_deref() == Some(self.add_user_state.name.trim());
                            if is_current_add {
                                self.add_user_state.set_error(truncate_msg(&e, 50));
                            }
                            self.status_message = format!("Error: {}", truncate_msg(&e, 60));
                        }
                    }
                }
                BackendMsg::VlessUrl(result, intent) => match result {
                    Ok(added) => {
                        // Only apply if still on the same screen AND same user
                        let is_current_user = match self.screen {
                            Screen::UserDetail => self
                                .user_detail_state
                                .user
                                .as_ref()
                                .is_some_and(|u| u.uuid == added.uuid),
                            Screen::AddUser => matches!(
                                &self.add_user_state.result,
                                AddUserResult::Success { uuid, .. } if *uuid == added.uuid
                            ),
                            _ => false,
                        };
                        if !is_current_user {
                            // User navigated away or viewing a different user; discard
                            continue;
                        }
                        match self.screen {
                            Screen::AddUser => {
                                self.add_user_state.cached_vless_url =
                                    Some(added.vless_url.clone());
                            }
                            _ => {
                                self.user_detail_state.cached_vless_url =
                                    Some(added.vless_url.clone());
                            }
                        }
                        match intent {
                            VlessUrlIntent::Clipboard => {
                                let osc52 = user_detail::osc52_copy(&added.vless_url);
                                print!("{}", osc52);
                                let _ = io::stdout().flush();
                                self.status_message = "Copied to clipboard (OSC 52)".to_string();
                            }
                            VlessUrlIntent::Qr => {
                                self.qr_view_state.open(added.name.clone(), added.vless_url);
                                self.screen = Screen::QrView;
                            }
                        }
                    }
                    Err(e) => {
                        self.status_message = format!("Error: {}", truncate_msg(&e, 60));
                    }
                },
                BackendMsg::UserDeleted(result) => {
                    let request_uuid = self.pending_delete_uuid.take();
                    match result {
                        Ok(ref deleted_uuid) => {
                            // Only update the detail view if we're still viewing the same user
                            let is_same_user = self.screen == Screen::UserDetail
                                && self
                                    .user_detail_state
                                    .user
                                    .as_ref()
                                    .is_some_and(|u| u.uuid == *deleted_uuid);
                            if is_same_user {
                                self.user_detail_state.mode = DetailMode::DeleteSuccess;
                            }
                            self.status_message = "User deleted".to_string();
                            // Only mark stale if a fetch is already in flight
                            if self.pending_refresh {
                                self.refresh_after_mutation = true;
                            } else {
                                self.trigger_refresh();
                            }
                        }
                        Err(e) => {
                            // Only show delete error if still on the detail screen
                            // AND this error corresponds to the user we're viewing
                            let is_same_user = self.screen == Screen::UserDetail
                                && request_uuid.is_some()
                                && self.user_detail_state.user.as_ref().is_some_and(|u| {
                                    Some(u.uuid.as_str()) == request_uuid.as_deref()
                                });
                            if is_same_user {
                                self.user_detail_state.mode =
                                    DetailMode::DeleteError(truncate_msg(&e, 50));
                            }
                            self.status_message =
                                format!("Delete failed: {}", truncate_msg(&e, 60));
                        }
                    }
                }
                BackendMsg::DeployProgress(status) => {
                    if self.screen == Screen::TelegramSetup {
                        self.telegram_setup_state.deploy_status = status;
                    }
                }
                BackendMsg::DeployBot(result) => {
                    self.pending_deploy = false;
                    if self.screen == Screen::TelegramSetup {
                        match result {
                            Ok(msg) => {
                                self.telegram_setup_state.deploy_status =
                                    telegram_setup::DeployStatus::Success(msg.clone());
                                self.status_message = msg;
                            }
                            Err(e) => {
                                self.telegram_setup_state.deploy_status =
                                    telegram_setup::DeployStatus::Error(truncate_msg(&e, 60));
                                self.status_message =
                                    format!("Deploy failed: {}", truncate_msg(&e, 60));
                            }
                        }
                    }
                }
                BackendMsg::OnlineIps(result) => {
                    let is_detail_view = self.screen == Screen::UserDetail;
                    match result {
                        Ok((uuid, ips)) => {
                            if is_detail_view
                                && self
                                    .user_detail_state
                                    .user
                                    .as_ref()
                                    .is_some_and(|u| u.uuid == uuid)
                            {
                                self.user_detail_state.online_ips = ips;
                                self.user_detail_state.online_ips_error = None;
                            }
                        }
                        Err((uuid, e)) => {
                            if is_detail_view
                                && self
                                    .user_detail_state
                                    .user
                                    .as_ref()
                                    .is_some_and(|u| u.uuid == uuid)
                            {
                                self.user_detail_state.online_ips_error =
                                    Some(truncate_msg(&e, 40));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check if a periodic refresh is due
    pub fn should_refresh(&self) -> bool {
        self.last_refresh.elapsed() >= REFRESH_INTERVAL
    }

    /// Draw the UI
    pub fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        // Tick spinner when loading
        if self.dashboard_state.loading {
            self.dashboard_state.tick_spinner();
        }

        let screen = self.screen;
        let screen_label = self.screen_label();
        let keybinds = self.keybind_hints();
        let status_message = self.status_message.clone();

        terminal.draw(|frame| {
            let size = frame.area();

            // Main layout: header (3 lines), body (flex), status bar (1 line)
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // header
                    Constraint::Min(1),    // body
                    Constraint::Length(1), // status bar
                ])
                .split(size);

            Self::draw_header_static(frame, chunks[0], screen_label);

            match screen {
                Screen::Dashboard => dashboard::draw(&mut self.dashboard_state, frame, chunks[1]),
                Screen::Setup => setup::draw(&self.setup_state, frame, chunks[1]),
                Screen::AddUser => {
                    // Draw dashboard underneath, then overlay the dialog
                    dashboard::draw(&mut self.dashboard_state, frame, chunks[1]);
                    add_user::draw(&self.add_user_state, frame, chunks[1]);
                }
                Screen::UserDetail => {
                    user_detail::draw(&self.user_detail_state, frame, chunks[1]);
                }
                Screen::QrView => {
                    qr::draw(&self.qr_view_state, frame, chunks[1]);
                }
                Screen::TelegramSetup => {
                    telegram_setup::draw(&self.telegram_setup_state, frame, chunks[1]);
                }
            }

            Self::draw_status_bar_static(frame, chunks[2], &status_message, &keybinds);
        })?;
        Ok(())
    }

    fn draw_header_static(frame: &mut ratatui::Frame, area: Rect, screen_label: &str) {
        let header_block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme::border_style())
            .style(theme::header_style());

        let title_text = format!(
            " {} {} | {}",
            theme::APP_NAME,
            theme::APP_VERSION,
            screen_label
        );
        let header = Paragraph::new(Line::from(vec![Span::styled(
            title_text,
            theme::title_style(),
        )]))
        .block(header_block);

        frame.render_widget(header, area);
    }

    fn draw_status_bar_static(
        frame: &mut ratatui::Frame,
        area: Rect,
        status_message: &str,
        keybinds: &str,
    ) {
        let status = if status_message.is_empty() {
            keybinds.to_string()
        } else {
            format!("{} | {}", status_message, keybinds)
        };

        let bar = Paragraph::new(Line::from(Span::styled(status, theme::status_style())));
        frame.render_widget(bar, area);
    }

    fn screen_label(&self) -> &'static str {
        match self.screen {
            Screen::Setup => "Setup",
            Screen::Dashboard => "Dashboard",
            Screen::UserDetail => "User Detail",
            Screen::AddUser => "Add User",
            Screen::QrView => "QR Code",
            Screen::TelegramSetup => "Telegram Bot",
        }
    }

    fn keybind_hints(&self) -> String {
        match self.screen {
            Screen::Dashboard => {
                "[a]dd user  [d]elete  [r]efresh  [t]elegram bot  [q]uit  [Enter] detail"
                    .to_string()
            }
            Screen::Setup => "[Tab] next field  [Enter] confirm  [Esc] quit".to_string(),
            Screen::UserDetail => "[Esc] back  [d]elete  [c]opy URL  [q] QR code".to_string(),
            Screen::AddUser => "[Enter] confirm  [Esc] cancel".to_string(),
            Screen::QrView => "[Esc/q] back".to_string(),
            Screen::TelegramSetup => "[Tab] next field  [Enter] confirm  [Esc] back".to_string(),
        }
    }
}

/// Initialize the terminal for TUI rendering
pub fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

/// Restore terminal to normal state
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Run the main event loop
pub fn run(app: &mut App, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    // Trigger initial data load if we have config
    if app.screen == Screen::Dashboard && !app.initial_load_done {
        app.trigger_refresh();
    }

    while app.running {
        app.draw(terminal)?;

        // Poll for events with a timeout so we can do periodic refresh
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        // Process any completed backend operations
        app.process_backend_messages();

        // Periodic refresh check (only on dashboard, when not already loading)
        if app.screen == Screen::Dashboard && app.should_refresh() && !app.pending_refresh {
            // Auto-retry initial load on failure, or periodic refresh after success
            app.trigger_refresh();
        }
    }

    Ok(())
}

/// Simple timestamp without chrono dependency
fn chrono_free_timestamp() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = elapsed.as_secs();
    // HH:MM:SS from unix timestamp (UTC)
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02} UTC", h, m, s)
}

/// Truncate a message to a given length
fn truncate_msg(msg: &str, max: usize) -> String {
    if msg.len() <= max {
        msg.to_string()
    } else {
        // Find a char boundary at or before `max` to avoid panicking on multi-byte UTF-8
        let boundary = msg.floor_char_boundary(max);
        format!("{}...", &msg[..boundary])
    }
}

#[cfg(test)]
impl App {
    pub fn new(has_config: bool, runtime: tokio::runtime::Handle) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            screen: if has_config {
                Screen::Dashboard
            } else {
                Screen::Setup
            },
            running: true,
            last_refresh: Instant::now(),
            status_message: String::new(),
            setup_state: SetupState::default(),
            dashboard_state: DashboardState::default(),
            add_user_state: AddUserState::default(),
            user_detail_state: UserDetailState::default(),
            qr_view_state: QrViewState::default(),
            telegram_setup_state: TelegramSetupState::default(),
            config: Config::default(),
            runtime,
            backend_rx: rx,
            backend_tx: tx,
            pending_refresh: false,
            initial_load_done: false,
            api_check_done: false,
            version_check_done: false,
            refresh_after_mutation: false,
            pending_add_name: None,
            pending_delete_uuid: None,
            pending_test: false,
            tested_config: None,
            pending_deploy: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn test_runtime() -> tokio::runtime::Handle {
        tokio::runtime::Handle::try_current().unwrap_or_else(|_| {
            // Create a runtime for tests that don't have one
            let rt = tokio::runtime::Runtime::new().unwrap();
            let handle = rt.handle().clone();
            // Leak to keep alive for test duration
            std::mem::forget(rt);
            handle
        })
    }

    #[test]
    fn test_new_with_config() {
        let app = App::new(true, test_runtime());
        assert_eq!(app.screen, Screen::Dashboard);
        assert!(app.running);
    }

    #[test]
    fn test_new_without_config() {
        let app = App::new(false, test_runtime());
        assert_eq!(app.screen, Screen::Setup);
        assert!(app.running);
    }

    #[test]
    fn test_quit() {
        let mut app = App::new(true, test_runtime());
        assert!(app.running);
        app.quit();
        assert!(!app.running);
    }

    #[test]
    fn test_ctrl_c_quits() {
        let mut app = App::new(true, test_runtime());
        app.handle_key(make_ctrl_key(KeyCode::Char('c')));
        assert!(!app.running);
    }

    #[test]
    fn test_ctrl_q_quits() {
        let mut app = App::new(true, test_runtime());
        app.handle_key(make_ctrl_key(KeyCode::Char('q')));
        assert!(!app.running);
    }

    #[test]
    fn test_q_quits_from_dashboard() {
        let mut app = App::new(true, test_runtime());
        app.handle_key(make_key(KeyCode::Char('q')));
        assert!(!app.running);
    }

    #[test]
    fn test_esc_quits_from_setup() {
        let mut app = App::new(false, test_runtime());
        app.handle_key(make_key(KeyCode::Esc));
        assert!(!app.running);
    }

    #[test]
    fn test_a_opens_add_user() {
        let mut app = App::new(true, test_runtime());
        app.handle_key(make_key(KeyCode::Char('a')));
        assert_eq!(app.screen, Screen::AddUser);
    }

    #[test]
    fn test_esc_from_add_user_returns_to_dashboard() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::AddUser;
        app.handle_key(make_key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn test_esc_from_qr_view_returns_to_dashboard() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::QrView;
        app.handle_key(make_key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn test_esc_from_user_detail_returns_to_dashboard() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::UserDetail;
        app.user_detail_state.mode = DetailMode::View;
        app.handle_key(make_key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn test_should_refresh() {
        let mut app = App::new(true, test_runtime());
        app.last_refresh = Instant::now() - Duration::from_secs(10);
        assert!(app.should_refresh());
    }

    #[test]
    fn test_should_not_refresh_too_soon() {
        let app = App::new(true, test_runtime());
        assert!(!app.should_refresh());
    }

    #[test]
    fn test_screen_labels() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::Dashboard;
        assert_eq!(app.screen_label(), "Dashboard");
        app.screen = Screen::Setup;
        assert_eq!(app.screen_label(), "Setup");
        app.screen = Screen::UserDetail;
        assert_eq!(app.screen_label(), "User Detail");
        app.screen = Screen::AddUser;
        assert_eq!(app.screen_label(), "Add User");
        app.screen = Screen::QrView;
        assert_eq!(app.screen_label(), "QR Code");
        app.screen = Screen::TelegramSetup;
        assert_eq!(app.screen_label(), "Telegram Bot");
    }

    #[test]
    fn test_keybind_hints_nonempty() {
        let mut app = App::new(true, test_runtime());
        for screen in [
            Screen::Dashboard,
            Screen::Setup,
            Screen::UserDetail,
            Screen::AddUser,
            Screen::QrView,
            Screen::TelegramSetup,
        ] {
            app.screen = screen;
            assert!(!app.keybind_hints().is_empty());
        }
    }

    #[test]
    fn test_with_config_sets_host() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: None,
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let app = App::with_config(config, test_runtime());
        assert_eq!(app.screen, Screen::Dashboard);
        assert_eq!(app.dashboard_state.server_host, "1.2.3.4");
    }

    #[test]
    fn test_with_config_ssh_host_sets_host() {
        let config = Config {
            host: None,
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
        let app = App::with_config(config, test_runtime());
        assert_eq!(app.dashboard_state.server_host, "vps-vpn");
    }

    #[test]
    fn test_truncate_msg() {
        assert_eq!(truncate_msg("short", 10), "short");
        assert_eq!(truncate_msg("a long message here", 6), "a long...");
    }

    #[test]
    fn test_chrono_free_timestamp() {
        let ts = chrono_free_timestamp();
        assert!(ts.ends_with("UTC"));
        assert!(ts.contains(':'));
    }

    #[test]
    fn test_process_backend_messages_empty() {
        let mut app = App::new(true, test_runtime());
        // Should not panic with no messages
        app.process_backend_messages();
    }

    #[test]
    fn test_process_dashboard_data() {
        let mut app = App::new(true, test_runtime());
        app.pending_refresh = true;
        app.dashboard_state.loading = true;

        // Send a dashboard data message
        let _ = app.backend_tx.send(BackendMsg::DashboardData(Ok(
            crate::backend::DashboardData {
                users: vec![],
                server_info: crate::xray::client::ServerInfo {
                    version: "Xray 25.8.3".to_string(),
                    uplink: 1000,
                    downlink: 2000,
                },
                container_uptime: "Up 2 hours".to_string(),
                latest_version: Some("25.8.3".to_string()),
                version_was_fetched: true,
            },
        )));

        app.process_backend_messages();

        assert!(!app.pending_refresh);
        assert!(!app.dashboard_state.loading);
        assert_eq!(app.dashboard_state.server_version, "Xray 25.8.3");
        assert_eq!(app.dashboard_state.total_upload, 1000);
        assert_eq!(app.dashboard_state.total_download, 2000);
    }

    #[test]
    fn test_process_dashboard_data_error() {
        let mut app = App::new(true, test_runtime());
        app.pending_refresh = true;
        app.dashboard_state.loading = true;

        let _ = app
            .backend_tx
            .send(BackendMsg::DashboardData(Err("connection failed".into())));

        app.process_backend_messages();

        assert!(!app.pending_refresh);
        assert!(!app.dashboard_state.loading);
        assert!(app.status_message.contains("Error"));
    }

    #[test]
    fn test_process_connection_test_success() {
        let mut app = App::new(false, test_runtime());
        app.setup_state.test_result = setup::TestResult::Testing;

        let _ = app
            .backend_tx
            .send(BackendMsg::ConnectionTest(Ok("Xray 25.8.3".to_string())));

        app.process_backend_messages();

        match &app.setup_state.test_result {
            setup::TestResult::Success(msg) => assert!(msg.contains("Xray 25.8.3")),
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_process_connection_test_error() {
        let mut app = App::new(false, test_runtime());

        let _ = app
            .backend_tx
            .send(BackendMsg::ConnectionTest(Err("connection refused".into())));

        app.process_backend_messages();

        match &app.setup_state.test_result {
            setup::TestResult::Error(msg) => assert!(msg.contains("connection refused")),
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn test_process_user_added() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::AddUser;
        app.add_user_state.name = "alice".to_string();
        app.pending_add_name = Some("alice".to_string());

        let _ = app
            .backend_tx
            .send(BackendMsg::UserAdded(Ok(crate::backend::AddedUser {
                name: "alice".to_string(),
                uuid: "test-uuid".to_string(),
                vless_url: "vless://test@host:443#alice".to_string(),
            })));

        app.process_backend_messages();

        match &app.add_user_state.result {
            AddUserResult::Success { name, uuid } => {
                assert_eq!(name, "alice");
                assert_eq!(uuid, "test-uuid");
            }
            _ => panic!("Expected Success"),
        }
        assert!(app.add_user_state.cached_vless_url.is_some());
    }

    #[test]
    fn test_process_user_deleted() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::UserDetail;
        // Set up the user detail state with the matching UUID
        app.user_detail_state.open(crate::xray::types::XrayUser {
            uuid: "deleted-uuid".to_string(),
            name: "testuser".to_string(),
            email: "testuser@vpn".to_string(),
            flow: "xtls-rprx-vision".to_string(),
            stats: Default::default(),
            online_count: 0,
        });

        let _ = app
            .backend_tx
            .send(BackendMsg::UserDeleted(Ok("deleted-uuid".to_string())));

        app.process_backend_messages();

        assert_eq!(app.user_detail_state.mode, DetailMode::DeleteSuccess);
    }

    #[test]
    fn test_t_opens_telegram_setup() {
        let mut app = App::new(true, test_runtime());
        app.handle_key(make_key(KeyCode::Char('t')));
        assert_eq!(app.screen, Screen::TelegramSetup);
    }

    #[test]
    fn test_esc_from_telegram_setup_returns_to_dashboard() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::TelegramSetup;
        app.handle_key(make_key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Dashboard);
    }

    #[test]
    fn test_telegram_setup_preserves_token_from_config() {
        let mut app = App::new(true, test_runtime());
        app.config.telegram_token = Some("123:abc".to_string());
        app.config.telegram_admin_chat_id = Some(987654321);
        app.handle_key(make_key(KeyCode::Char('t')));
        assert_eq!(app.screen, Screen::TelegramSetup);
        assert_eq!(app.telegram_setup_state.token, "123:abc");
        assert_eq!(app.telegram_setup_state.admin_id, "987654321");
    }

    #[test]
    fn test_process_deploy_bot_success() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::TelegramSetup;
        app.pending_deploy = true;

        let _ = app.backend_tx.send(BackendMsg::DeployBot(Ok(
            "Bot deployed and running!".to_string()
        )));

        app.process_backend_messages();

        assert!(!app.pending_deploy);
        match &app.telegram_setup_state.deploy_status {
            telegram_setup::DeployStatus::Success(msg) => {
                assert!(msg.contains("deployed"));
            }
            other => panic!("Expected Success, got {:?}", other),
        }
    }

    #[test]
    fn test_process_deploy_bot_error() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::TelegramSetup;
        app.pending_deploy = true;

        let _ = app
            .backend_tx
            .send(BackendMsg::DeployBot(Err("connection refused".to_string())));

        app.process_backend_messages();

        assert!(!app.pending_deploy);
        match &app.telegram_setup_state.deploy_status {
            telegram_setup::DeployStatus::Error(msg) => {
                assert!(msg.contains("connection refused"));
            }
            other => panic!("Expected Error, got {:?}", other),
        }
    }

    #[test]
    fn test_process_user_delete_error() {
        let mut app = App::new(true, test_runtime());
        app.screen = Screen::UserDetail;
        // Set up a user in the detail view matching the pending request
        app.user_detail_state.open(crate::xray::types::XrayUser {
            uuid: "some-uuid".to_string(),
            name: "testuser".to_string(),
            email: "testuser@vpn".to_string(),
            flow: "xtls-rprx-vision".to_string(),
            stats: Default::default(),
            online_count: 0,
        });
        app.pending_delete_uuid = Some("some-uuid".to_string());

        let _ = app
            .backend_tx
            .send(BackendMsg::UserDeleted(Err("not found".to_string())));

        app.process_backend_messages();

        match &app.user_detail_state.mode {
            DetailMode::DeleteError(msg) => assert!(msg.contains("not found")),
            other => panic!("Expected DeleteError, got {:?}", other),
        }
    }
}
