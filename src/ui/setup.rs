use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme;
use crate::config::Config;

/// Index of each field in the setup form
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupField {
    SshHost,
    Host,
    Port,
    User,
    KeyPath,
    Container,
    TestConnection,
    SaveAndStart,
}

impl SetupField {
    pub const ALL: [SetupField; 8] = [
        SetupField::SshHost,
        SetupField::Host,
        SetupField::Port,
        SetupField::User,
        SetupField::KeyPath,
        SetupField::Container,
        SetupField::TestConnection,
        SetupField::SaveAndStart,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            SetupField::SshHost => "SSH Config Host",
            SetupField::Host => "Host",
            SetupField::Port => "Port",
            SetupField::User => "User",
            SetupField::KeyPath => "SSH Key Path",
            SetupField::Container => "Container Name",
            SetupField::TestConnection => "[ Test Connection ]",
            SetupField::SaveAndStart => "[ Save & Start ]",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            SetupField::SshHost => {
                "SSH config alias (e.g. vps-vpn). If set, overrides host/port/user/key."
            }
            SetupField::Host => "Server IP or hostname (e.g. 203.0.113.42)",
            SetupField::Port => "SSH port (default: 22)",
            SetupField::User => "SSH user (default: root)",
            SetupField::KeyPath => "Path to SSH private key (e.g. ~/.ssh/id_ed25519)",
            SetupField::Container => "Docker container name (default: amnezia-xray)",
            SetupField::TestConnection => "Press Enter to test SSH connection",
            SetupField::SaveAndStart => "Press Enter to save config and start dashboard",
        }
    }

    pub fn is_button(&self) -> bool {
        matches!(self, SetupField::TestConnection | SetupField::SaveAndStart)
    }
}

/// Result of a connection test
#[derive(Debug, Clone)]
pub enum TestResult {
    None,
    Testing,
    Success(String),
    Error(String),
}

/// State for the setup wizard
#[derive(Debug, Clone)]
pub struct SetupState {
    pub ssh_host: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub key_path: String,
    pub container: String,
    pub focused: usize,
    pub test_result: TestResult,
    pub save_requested: bool,
    pub test_requested: bool,
}

impl Default for SetupState {
    fn default() -> Self {
        Self {
            ssh_host: String::new(),
            host: String::new(),
            port: "22".to_string(),
            user: "root".to_string(),
            key_path: String::new(),
            container: "amnezia-xray".to_string(),
            focused: 0,
            test_result: TestResult::None,
            save_requested: false,
            test_requested: false,
        }
    }
}

impl SetupState {
    /// Create SetupState pre-filled from an existing config
    pub fn from_config(config: &Config) -> Self {
        Self {
            ssh_host: config.ssh_host.clone().unwrap_or_default(),
            host: config.host.clone().unwrap_or_default(),
            port: config.port.to_string(),
            user: config.user.clone(),
            key_path: config
                .key_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            container: config.container.clone(),
            ..Default::default()
        }
    }

    /// Convert setup state to a Config
    pub fn to_config(&self) -> Config {
        Config {
            host: if self.host.is_empty() {
                None
            } else {
                Some(self.host.clone())
            },
            port: self.port.parse().unwrap_or(22),
            user: if self.user.is_empty() {
                "root".to_string()
            } else {
                self.user.clone()
            },
            key_path: if self.key_path.is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(&self.key_path))
            },
            ssh_host: if self.ssh_host.is_empty() {
                None
            } else {
                Some(self.ssh_host.clone())
            },
            container: if self.container.is_empty() {
                "amnezia-xray".to_string()
            } else {
                self.container.clone()
            },
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        }
    }

    /// Get the currently focused field
    pub fn focused_field(&self) -> SetupField {
        SetupField::ALL[self.focused]
    }

    /// Get mutable reference to the field value for the focused field
    fn focused_value_mut(&mut self) -> Option<&mut String> {
        match self.focused_field() {
            SetupField::SshHost => Some(&mut self.ssh_host),
            SetupField::Host => Some(&mut self.host),
            SetupField::Port => Some(&mut self.port),
            SetupField::User => Some(&mut self.user),
            SetupField::KeyPath => Some(&mut self.key_path),
            SetupField::Container => Some(&mut self.container),
            _ => None,
        }
    }

    /// Move focus to the next field
    pub fn focus_next(&mut self) {
        self.focused = (self.focused + 1) % SetupField::ALL.len();
    }

    /// Move focus to the previous field
    pub fn focus_prev(&mut self) {
        if self.focused == 0 {
            self.focused = SetupField::ALL.len() - 1;
        } else {
            self.focused -= 1;
        }
    }

    /// Handle a key event, return true if the event was consumed
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.focus_prev();
                } else {
                    self.focus_next();
                }
                true
            }
            KeyCode::BackTab => {
                self.focus_prev();
                true
            }
            KeyCode::Down => {
                self.focus_next();
                true
            }
            KeyCode::Up => {
                self.focus_prev();
                true
            }
            KeyCode::Enter => {
                match self.focused_field() {
                    SetupField::TestConnection => {
                        self.test_requested = true;
                    }
                    SetupField::SaveAndStart => {
                        self.save_requested = true;
                    }
                    _ => {
                        // Enter on a text field moves to next field
                        self.focus_next();
                    }
                }
                true
            }
            KeyCode::Char(c) => {
                let field = self.focused_field();
                if let Some(value) = self.focused_value_mut() {
                    // For port field, only allow digits
                    if field == SetupField::Port && !c.is_ascii_digit() {
                        return true;
                    }
                    // For container field, only allow Docker-safe characters [a-zA-Z0-9_.-]
                    if field == SetupField::Container
                        && !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
                    {
                        return true;
                    }
                    value.push(c);
                    // Invalidate any stale connection test result when form is edited
                    self.test_result = TestResult::None;
                    true
                } else {
                    false
                }
            }
            KeyCode::Backspace => {
                if let Some(value) = self.focused_value_mut() {
                    value.pop();
                    // Invalidate any stale connection test result when form is edited
                    self.test_result = TestResult::None;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}

#[cfg(test)]
impl SetupState {
    pub fn has_connection_info(&self) -> bool {
        !self.ssh_host.is_empty() || !self.host.is_empty()
    }
}

/// Draw the setup wizard
pub fn draw(state: &SetupState, frame: &mut ratatui::Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_style())
        .title(Span::styled(" First-Run Setup ", theme::accent_style()))
        .style(theme::text_style());

    // Vertical layout: logo, fields, test result, hint
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // logo
            Constraint::Length(1), // welcome text
            Constraint::Length(1), // spacer
            Constraint::Min(12),   // form fields
            Constraint::Length(3), // test result
            Constraint::Length(2), // hint
        ])
        .split(inner);

    // Logo
    let logo_lines: Vec<Line> = theme::LOGO
        .lines()
        .map(|l| Line::from(Span::styled(l, theme::title_style())))
        .collect();
    frame.render_widget(Paragraph::new(logo_lines), chunks[0]);

    // Welcome text
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "  Configure your SSH connection to get started.",
            theme::secondary_style(),
        ))),
        chunks[1],
    );

    // Form fields
    draw_form_fields(state, frame, chunks[3]);

    // Test result
    draw_test_result(state, frame, chunks[4]);

    // Hint for focused field
    let hint = state.focused_field().hint();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ", theme::muted_style()),
            Span::styled(hint, theme::muted_style()),
        ])),
        chunks[5],
    );
}

fn draw_form_fields(state: &SetupState, frame: &mut ratatui::Frame, area: Rect) {
    let field_constraints: Vec<Constraint> = SetupField::ALL
        .iter()
        .map(|_| Constraint::Length(1))
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(field_constraints)
        .split(area);

    for (i, field) in SetupField::ALL.iter().enumerate() {
        let is_focused = i == state.focused;
        let row = rows[i];

        if field.is_button() {
            draw_button(state, frame, row, *field, is_focused);
        } else {
            draw_input_field(state, frame, row, *field, is_focused);
        }
    }
}

fn draw_input_field(
    state: &SetupState,
    frame: &mut ratatui::Frame,
    area: Rect,
    field: SetupField,
    focused: bool,
) {
    // Layout: label (20 chars) + input (rest)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),  // left margin
            Constraint::Length(18), // label
            Constraint::Length(2),  // separator
            Constraint::Min(20),    // input
        ])
        .split(area);

    let label_style = if focused {
        theme::secondary_style()
    } else {
        theme::muted_style()
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(field.label(), label_style))),
        cols[1],
    );

    let separator = if focused { "> " } else { ": " };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(separator, label_style))),
        cols[2],
    );

    let value = match field {
        SetupField::SshHost => &state.ssh_host,
        SetupField::Host => &state.host,
        SetupField::Port => &state.port,
        SetupField::User => &state.user,
        SetupField::KeyPath => &state.key_path,
        SetupField::Container => &state.container,
        _ => unreachable!(),
    };

    let display_value = if focused {
        format!("{}_", value)
    } else {
        value.clone()
    };

    let value_style = if focused {
        theme::title_style()
    } else {
        theme::text_style()
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(display_value, value_style))),
        cols[3],
    );
}

fn draw_button(
    _state: &SetupState,
    frame: &mut ratatui::Frame,
    area: Rect,
    field: SetupField,
    focused: bool,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(2), Constraint::Min(20)])
        .split(area);

    let style = if focused {
        theme::selected_style()
    } else {
        theme::secondary_style()
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(field.label(), style))),
        cols[1],
    );
}

fn draw_test_result(state: &SetupState, frame: &mut ratatui::Frame, area: Rect) {
    let (text, style) = match &state.test_result {
        TestResult::None => return,
        TestResult::Testing => (
            "  Testing connection...".to_string(),
            theme::secondary_style(),
        ),
        TestResult::Success(msg) => (format!("  OK: {}", msg), theme::title_style()),
        TestResult::Error(msg) => (format!("  Error: {}", msg), theme::alert_style()),
    };

    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn make_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn test_default_state() {
        let state = SetupState::default();
        assert_eq!(state.focused, 0);
        assert_eq!(state.ssh_host, "");
        assert_eq!(state.host, "");
        assert_eq!(state.port, "22");
        assert_eq!(state.user, "root");
        assert_eq!(state.key_path, "");
        assert_eq!(state.container, "amnezia-xray");
        assert!(!state.save_requested);
        assert!(!state.test_requested);
        assert!(matches!(state.test_result, TestResult::None));
    }

    #[test]
    fn test_from_config() {
        let config = Config {
            host: Some("1.2.3.4".to_string()),
            port: 2222,
            user: "admin".to_string(),
            key_path: Some(std::path::PathBuf::from("/home/.ssh/key")),
            ssh_host: Some("vps-vpn".to_string()),
            container: "my-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let state = SetupState::from_config(&config);
        assert_eq!(state.ssh_host, "vps-vpn");
        assert_eq!(state.host, "1.2.3.4");
        assert_eq!(state.port, "2222");
        assert_eq!(state.user, "admin");
        assert_eq!(state.key_path, "/home/.ssh/key");
        assert_eq!(state.container, "my-xray");
    }

    #[test]
    fn test_from_config_defaults() {
        let config = Config::default();
        let state = SetupState::from_config(&config);
        assert_eq!(state.ssh_host, "");
        assert_eq!(state.host, "");
        assert_eq!(state.port, "22");
        assert_eq!(state.user, "root");
        assert_eq!(state.key_path, "");
        assert_eq!(state.container, "amnezia-xray");
    }

    #[test]
    fn test_to_config() {
        let state = SetupState {
            ssh_host: "vps-vpn".to_string(),
            host: "1.2.3.4".to_string(),
            port: "2222".to_string(),
            user: "admin".to_string(),
            key_path: "/home/.ssh/key".to_string(),
            container: "my-xray".to_string(),
            ..Default::default()
        };
        let config = state.to_config();
        assert_eq!(config.host.as_deref(), Some("1.2.3.4"));
        assert_eq!(config.port, 2222);
        assert_eq!(config.user, "admin");
        assert_eq!(
            config.key_path,
            Some(std::path::PathBuf::from("/home/.ssh/key"))
        );
        assert_eq!(config.ssh_host.as_deref(), Some("vps-vpn"));
        assert_eq!(config.container, "my-xray");
    }

    #[test]
    fn test_to_config_empty_fields() {
        let state = SetupState {
            ssh_host: String::new(),
            host: String::new(),
            port: String::new(),
            user: String::new(),
            key_path: String::new(),
            container: String::new(),
            ..Default::default()
        };
        let config = state.to_config();
        assert_eq!(config.host, None);
        assert_eq!(config.port, 22); // invalid parse falls back to 22
        assert_eq!(config.user, "root");
        assert_eq!(config.key_path, None);
        assert_eq!(config.ssh_host, None);
        assert_eq!(config.container, "amnezia-xray");
    }

    #[test]
    fn test_to_config_roundtrip() {
        let config = Config {
            host: Some("10.0.0.1".to_string()),
            port: 22,
            user: "root".to_string(),
            key_path: None,
            ssh_host: Some("myhost".to_string()),
            container: "amnezia-xray".to_string(),
            telegram_token: None,
            telegram_admin_chat_id: None,
            bot_image: Default::default(),
            snapshot_dir: None,
            bridge_agent_url: None,
        };
        let state = SetupState::from_config(&config);
        let result = state.to_config();
        assert_eq!(config, result);
    }

    #[test]
    fn test_focus_next() {
        let mut state = SetupState::default();
        assert_eq!(state.focused, 0);
        state.focus_next();
        assert_eq!(state.focused, 1);
        state.focus_next();
        assert_eq!(state.focused, 2);
    }

    #[test]
    fn test_focus_next_wraps() {
        let mut state = SetupState::default();
        state.focused = SetupField::ALL.len() - 1;
        state.focus_next();
        assert_eq!(state.focused, 0);
    }

    #[test]
    fn test_focus_prev() {
        let mut state = SetupState::default();
        state.focused = 2;
        state.focus_prev();
        assert_eq!(state.focused, 1);
        state.focus_prev();
        assert_eq!(state.focused, 0);
    }

    #[test]
    fn test_focus_prev_wraps() {
        let mut state = SetupState::default();
        assert_eq!(state.focused, 0);
        state.focus_prev();
        assert_eq!(state.focused, SetupField::ALL.len() - 1);
    }

    #[test]
    fn test_tab_moves_focus_forward() {
        let mut state = SetupState::default();
        state.handle_key(make_key(KeyCode::Tab));
        assert_eq!(state.focused, 1);
    }

    #[test]
    fn test_shift_tab_moves_focus_backward() {
        let mut state = SetupState::default();
        state.focused = 2;
        state.handle_key(make_shift_key(KeyCode::Tab));
        assert_eq!(state.focused, 1);
    }

    #[test]
    fn test_backtab_moves_focus_backward() {
        let mut state = SetupState::default();
        state.focused = 2;
        state.handle_key(make_key(KeyCode::BackTab));
        assert_eq!(state.focused, 1);
    }

    #[test]
    fn test_down_moves_focus_forward() {
        let mut state = SetupState::default();
        state.handle_key(make_key(KeyCode::Down));
        assert_eq!(state.focused, 1);
    }

    #[test]
    fn test_up_moves_focus_backward() {
        let mut state = SetupState::default();
        state.focused = 3;
        state.handle_key(make_key(KeyCode::Up));
        assert_eq!(state.focused, 2);
    }

    #[test]
    fn test_char_input_on_text_field() {
        let mut state = SetupState::default();
        // Focus is on SshHost (index 0)
        state.handle_key(make_key(KeyCode::Char('v')));
        state.handle_key(make_key(KeyCode::Char('p')));
        state.handle_key(make_key(KeyCode::Char('s')));
        assert_eq!(state.ssh_host, "vps");
    }

    #[test]
    fn test_backspace_deletes_char() {
        let mut state = SetupState::default();
        state.ssh_host = "vps".to_string();
        state.handle_key(make_key(KeyCode::Backspace));
        assert_eq!(state.ssh_host, "vp");
    }

    #[test]
    fn test_backspace_on_empty_field() {
        let mut state = SetupState::default();
        assert_eq!(state.ssh_host, "");
        state.handle_key(make_key(KeyCode::Backspace));
        assert_eq!(state.ssh_host, "");
    }

    #[test]
    fn test_port_field_only_digits() {
        let mut state = SetupState::default();
        state.focused = 2; // Port field
        assert_eq!(state.focused_field(), SetupField::Port);
        state.port = String::new();
        state.handle_key(make_key(KeyCode::Char('2')));
        state.handle_key(make_key(KeyCode::Char('a'))); // rejected
        state.handle_key(make_key(KeyCode::Char('2')));
        assert_eq!(state.port, "22");
    }

    #[test]
    fn test_enter_on_text_field_moves_next() {
        let mut state = SetupState::default();
        assert_eq!(state.focused, 0);
        state.handle_key(make_key(KeyCode::Enter));
        assert_eq!(state.focused, 1);
    }

    #[test]
    fn test_enter_on_test_connection() {
        let mut state = SetupState::default();
        state.focused = 6; // TestConnection
        assert_eq!(state.focused_field(), SetupField::TestConnection);
        state.handle_key(make_key(KeyCode::Enter));
        assert!(state.test_requested);
        assert!(!state.save_requested);
    }

    #[test]
    fn test_enter_on_save_and_start() {
        let mut state = SetupState::default();
        state.focused = 7; // SaveAndStart
        assert_eq!(state.focused_field(), SetupField::SaveAndStart);
        state.handle_key(make_key(KeyCode::Enter));
        assert!(state.save_requested);
        assert!(!state.test_requested);
    }

    #[test]
    fn test_char_input_on_button_not_consumed() {
        let mut state = SetupState::default();
        state.focused = 6; // TestConnection button
        let consumed = state.handle_key(make_key(KeyCode::Char('x')));
        assert!(!consumed);
    }

    #[test]
    fn test_focused_field() {
        let mut state = SetupState::default();
        assert_eq!(state.focused_field(), SetupField::SshHost);
        state.focused = 1;
        assert_eq!(state.focused_field(), SetupField::Host);
        state.focused = 7;
        assert_eq!(state.focused_field(), SetupField::SaveAndStart);
    }

    #[test]
    fn test_has_connection_info() {
        let mut state = SetupState::default();
        assert!(!state.has_connection_info());

        state.ssh_host = "vps-vpn".to_string();
        assert!(state.has_connection_info());

        state.ssh_host = String::new();
        state.host = "1.2.3.4".to_string();
        assert!(state.has_connection_info());
    }

    #[test]
    fn test_field_labels_nonempty() {
        for field in SetupField::ALL {
            assert!(!field.label().is_empty());
        }
    }

    #[test]
    fn test_field_hints_nonempty() {
        for field in SetupField::ALL {
            assert!(!field.hint().is_empty());
        }
    }

    #[test]
    fn test_field_is_button() {
        assert!(!SetupField::SshHost.is_button());
        assert!(!SetupField::Host.is_button());
        assert!(!SetupField::Port.is_button());
        assert!(!SetupField::User.is_button());
        assert!(!SetupField::KeyPath.is_button());
        assert!(!SetupField::Container.is_button());
        assert!(SetupField::TestConnection.is_button());
        assert!(SetupField::SaveAndStart.is_button());
    }

    #[test]
    fn test_typing_in_each_text_field() {
        let mut state = SetupState::default();

        // SshHost (0)
        state.focused = 0;
        state.handle_key(make_key(KeyCode::Char('a')));
        assert_eq!(state.ssh_host, "a");

        // Host (1)
        state.focused = 1;
        state.handle_key(make_key(KeyCode::Char('b')));
        assert_eq!(state.host, "b");

        // Port (2)
        state.focused = 2;
        state.port.clear();
        state.handle_key(make_key(KeyCode::Char('8')));
        assert_eq!(state.port, "8");

        // User (3)
        state.focused = 3;
        state.user.clear();
        state.handle_key(make_key(KeyCode::Char('r')));
        assert_eq!(state.user, "r");

        // KeyPath (4)
        state.focused = 4;
        state.handle_key(make_key(KeyCode::Char('/')));
        assert_eq!(state.key_path, "/");

        // Container (5)
        state.focused = 5;
        state.container.clear();
        state.handle_key(make_key(KeyCode::Char('x')));
        assert_eq!(state.container, "x");
    }

    #[test]
    fn test_typing_clears_test_result() {
        let mut state = SetupState::default();
        state.test_result = TestResult::Success("Connected!".to_string());
        // Typing in a field should clear stale test result
        state.handle_key(make_key(KeyCode::Char('a')));
        assert!(matches!(state.test_result, TestResult::None));
    }

    #[test]
    fn test_backspace_clears_test_result() {
        let mut state = SetupState::default();
        state.ssh_host = "vps".to_string();
        state.test_result = TestResult::Success("Connected!".to_string());
        state.handle_key(make_key(KeyCode::Backspace));
        assert!(matches!(state.test_result, TestResult::None));
    }

    #[test]
    fn test_navigation_preserves_test_result() {
        let mut state = SetupState::default();
        state.test_result = TestResult::Success("Connected!".to_string());
        // Tab navigation should NOT clear test result
        state.handle_key(make_key(KeyCode::Tab));
        assert!(matches!(state.test_result, TestResult::Success(_)));
    }
}
