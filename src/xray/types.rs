use serde::{Deserialize, Serialize};

/// A user as seen in the TUI — merged from server.json + clientsTable
#[derive(Debug, Clone, PartialEq)]
pub struct XrayUser {
    pub uuid: String,
    pub name: String,
    pub email: String,
    pub flow: String,
    pub stats: TrafficStats,
    pub online_count: u32,
}

impl XrayUser {
    pub fn email_from_name(name: &str) -> String {
        format!("{}@vpn", name)
    }
}

/// Traffic statistics for a user
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrafficStats {
    pub uplink: u64,
    pub downlink: u64,
}

/// User data within a clientsTable entry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientUserData {
    #[serde(rename = "clientName", default)]
    pub client_name: String,
    #[serde(rename = "creationDate", default)]
    pub creation_date: String,
}

/// An entry in the clientsTable JSON file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientEntry {
    #[serde(rename = "clientId")]
    pub client_id: String,
    #[serde(rename = "userData")]
    pub user_data: ClientUserData,
}

/// A client entry within a server.json inbound's settings.clients array
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerJsonClient {
    pub id: String,
    #[serde(default)]
    pub flow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

/// Reality protocol parameters extracted from server.json
#[derive(Debug, Clone, PartialEq)]
pub struct RealityParams {
    pub sni: String,
    pub short_id: String,
}

/// Parameters needed to construct a vless:// URL
#[derive(Debug, Clone)]
pub struct VlessUrlParams {
    pub uuid: String,
    pub host: String,
    pub port: u16,
    pub sni: String,
    pub public_key: String,
    pub short_id: String,
    pub path: String,
}

/// Parsed representation of server.json — we keep it as serde_json::Value
/// to preserve unknown fields, and provide typed access to what we need.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub raw: serde_json::Value,
}

impl ServerConfig {
    /// Parse from JSON string
    pub fn parse(json: &str) -> crate::error::Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(json)?;
        Ok(Self { raw })
    }

    /// Serialize back to pretty JSON.
    /// Safe: serde_json::Value from JSON deserialization always serializes successfully.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.raw).expect("BUG: Value with non-string key")
    }

    /// Find the main VLESS inbound and return its clients
    pub fn clients(&self) -> Vec<ServerJsonClient> {
        self.find_vless_inbound()
            .and_then(|inbound| {
                inbound
                    .get("settings")
                    .and_then(|s| s.get("clients"))
                    .and_then(|c| serde_json::from_value(c.clone()).ok())
            })
            .unwrap_or_default()
    }

    /// Check if a client with the given email already exists in the VLESS inbound.
    pub fn has_client_email(&self, email: &str) -> bool {
        self.clients()
            .iter()
            .any(|c| c.email.as_deref() == Some(email))
    }

    /// Add a client to the VLESS inbound
    pub fn add_client(&mut self, client: &ServerJsonClient) -> crate::error::Result<()> {
        let inbound = self
            .find_vless_inbound_mut()
            .ok_or_else(|| crate::error::AppError::Xray("no VLESS inbound found".into()))?;

        let clients = inbound
            .get_mut("settings")
            .and_then(|s| s.get_mut("clients"))
            .and_then(|c| c.as_array_mut())
            .ok_or_else(|| crate::error::AppError::Xray("no clients array found".into()))?;

        clients.push(serde_json::to_value(client)?);
        Ok(())
    }

    /// Update a client's email by UUID in the VLESS inbound
    pub fn update_client_email(
        &mut self,
        uuid: &str,
        new_email: &str,
    ) -> crate::error::Result<bool> {
        let inbound = self
            .find_vless_inbound_mut()
            .ok_or_else(|| crate::error::AppError::Xray("no VLESS inbound found".into()))?;

        let clients = inbound
            .get_mut("settings")
            .and_then(|s| s.get_mut("clients"))
            .and_then(|c| c.as_array_mut())
            .ok_or_else(|| crate::error::AppError::Xray("no clients array found".into()))?;

        for client in clients.iter_mut() {
            if client.get("id").and_then(|v| v.as_str()) == Some(uuid) {
                client["email"] = serde_json::Value::String(new_email.to_string());
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Remove a client by UUID from the VLESS inbound
    pub fn remove_client(&mut self, uuid: &str) -> crate::error::Result<bool> {
        let inbound = self
            .find_vless_inbound_mut()
            .ok_or_else(|| crate::error::AppError::Xray("no VLESS inbound found".into()))?;

        let clients = inbound
            .get_mut("settings")
            .and_then(|s| s.get_mut("clients"))
            .and_then(|c| c.as_array_mut())
            .ok_or_else(|| crate::error::AppError::Xray("no clients array found".into()))?;

        let before = clients.len();
        clients.retain(|c| c.get("id").and_then(|v| v.as_str()) != Some(uuid));
        Ok(clients.len() < before)
    }

    /// Check if the API section is correctly configured with required tag and services
    pub fn has_api(&self) -> bool {
        let Some(api) = self.raw.get("api") else {
            return false;
        };
        let has_tag = api.get("tag").and_then(|t| t.as_str()) == Some("api");
        let Some(services) = api.get("services").and_then(|s| s.as_array()) else {
            return false;
        };
        let has_handler = services
            .iter()
            .any(|s| s.as_str() == Some("HandlerService"));
        let has_stats = services.iter().any(|s| s.as_str() == Some("StatsService"));
        has_tag && has_handler && has_stats
    }

    pub(crate) fn find_vless_inbound(&self) -> Option<&serde_json::Value> {
        self.raw
            .get("inbounds")
            .and_then(|i| i.as_array())
            .and_then(|inbounds| {
                inbounds.iter().find(|ib| {
                    ib.get("protocol")
                        .and_then(|p| p.as_str())
                        .map(|p| p == "vless")
                        .unwrap_or(false)
                })
            })
    }

    /// Extract Reality settings (SNI, public key, short ID) from the VLESS inbound.
    pub fn reality_settings(&self) -> Option<RealityParams> {
        let inbound = self.find_vless_inbound()?;
        let reality = inbound
            .get("streamSettings")
            .and_then(|ss| ss.get("realitySettings"))?;

        let sni = reality
            .get("serverNames")
            .and_then(|sn| sn.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?
            .to_string();

        let short_id = reality
            .get("shortIds")
            .and_then(|si| si.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?
            .to_string();

        Some(RealityParams { sni, short_id })
    }

    /// Get the port from the VLESS inbound.
    pub fn vless_port(&self) -> Option<u16> {
        self.find_vless_inbound()
            .and_then(|ib| ib.get("port"))
            .and_then(|p| p.as_u64())
            .filter(|&p| p <= 65535)
            .map(|p| p as u16)
    }

    pub(crate) fn find_vless_inbound_mut(&mut self) -> Option<&mut serde_json::Value> {
        self.raw
            .get_mut("inbounds")
            .and_then(|i| i.as_array_mut())
            .and_then(|inbounds| {
                inbounds.iter_mut().find(|ib| {
                    ib.get("protocol")
                        .and_then(|p| p.as_str())
                        .map(|p| p == "vless")
                        .unwrap_or(false)
                })
            })
    }

    /// List user routing rules.
    ///
    /// Returns (user_email, outbound_tag) pairs for routing rules that have
    /// a `user` field (custom per-user routes, not system routes like api).
    #[allow(dead_code)]
    pub fn list_user_routes(&self) -> Vec<(String, String)> {
        let mut routes = Vec::new();
        if let Some(routing) = self.raw.get("routing") {
            if let Some(rules) = routing.get("rules").and_then(|r| r.as_array()) {
                for rule in rules {
                    if let (Some(users), Some(outbound)) = (
                        rule.get("user").and_then(|u| u.as_array()),
                        rule.get("outboundTag").and_then(|t| t.as_str()),
                    ) {
                        for user in users {
                            if let Some(email) = user.as_str() {
                                routes.push((email.to_string(), outbound.to_string()));
                            }
                        }
                    }
                }
            }
        }
        routes
    }

    /// Add a per-user routing rule.
    ///
    /// Creates a routing rule that matches the user's email and directs
    /// traffic to the specified outbound tag.
    #[allow(dead_code)]
    pub fn add_user_route(&mut self, user: &str, outbound: &str) {
        let email = if user.contains('@') {
            user.to_string()
        } else {
            XrayUser::email_from_name(user)
        };

        let rule = serde_json::json!({
            "type": "field",
            "user": [email],
            "outboundTag": outbound
        });

        let routing = self
            .raw
            .as_object_mut()
            .unwrap()
            .entry("routing")
            .or_insert_with(|| serde_json::json!({"rules": []}));
        let rules = routing
            .as_object_mut()
            .unwrap()
            .entry("rules")
            .or_insert_with(|| serde_json::json!([]));
        if let Some(arr) = rules.as_array_mut() {
            arr.push(rule);
        }
    }

    /// Remove a per-user routing rule.
    ///
    /// Returns true if a rule was found and removed.
    #[allow(dead_code)]
    pub fn remove_user_route(&mut self, user: &str) -> bool {
        let email = if user.contains('@') {
            user.to_string()
        } else {
            XrayUser::email_from_name(user)
        };

        if let Some(routing) = self.raw.get_mut("routing") {
            if let Some(rules) = routing.get_mut("rules").and_then(|r| r.as_array_mut()) {
                let before = rules.len();
                rules.retain(|rule| {
                    if let Some(users) = rule.get("user").and_then(|u| u.as_array()) {
                        // Remove rules where the user list contains this email
                        !users.iter().any(|u| u.as_str() == Some(&email))
                    } else {
                        true // keep non-user rules
                    }
                });
                return rules.len() < before;
            }
        }
        false
    }
}

/// Parsed clientsTable — a simple JSON array of ClientEntry
#[derive(Debug, Clone)]
pub struct ClientsTable {
    pub entries: Vec<ClientEntry>,
}

impl ClientsTable {
    /// Parse from JSON string
    pub fn parse(json: &str) -> crate::error::Result<Self> {
        let entries: Vec<ClientEntry> = serde_json::from_str(json)?;
        Ok(Self { entries })
    }

    /// Serialize back to JSON string.
    /// Safe: Vec<ClientEntry> with Serialize always succeeds.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.entries).expect("BUG: ClientEntry serialization failed")
    }

    /// Check if a client with the given name exists
    pub fn has_name(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.user_data.client_name == name)
    }

    /// Find a name for a given UUID
    pub fn name_for_uuid(&self, uuid: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.client_id == uuid)
            .map(|e| e.user_data.client_name.as_str())
    }

    /// Add an entry
    pub fn add(&mut self, client_id: String, name: String) {
        self.entries.push(ClientEntry {
            client_id,
            user_data: ClientUserData {
                client_name: name,
                creation_date: String::new(),
            },
        });
    }

    /// Rename a client by UUID, returns true if found
    pub fn rename(&mut self, uuid: &str, new_name: &str) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.client_id == uuid) {
            entry.user_data.client_name = new_name.to_string();
            return true;
        }
        false
    }

    /// Remove an entry by UUID, returns true if found
    pub fn remove(&mut self, uuid: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.client_id != uuid);
        self.entries.len() < before
    }
}

/// Merge server.json clients with clientsTable names to produce XrayUser list
pub fn merge_users(config: &ServerConfig, table: &ClientsTable) -> Vec<XrayUser> {
    config
        .clients()
        .into_iter()
        .map(|client| {
            let name = table
                .name_for_uuid(&client.id)
                .unwrap_or("unknown")
                .to_string();
            let email = client
                .email
                .unwrap_or_else(|| XrayUser::email_from_name(&name));
            XrayUser {
                uuid: client.id,
                name,
                email,
                flow: client.flow,
                stats: TrafficStats::default(),
                online_count: 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_server_json() -> &'static str {
        r#"{
            "inbounds": [
                {
                    "listen": "0.0.0.0",
                    "port": 443,
                    "protocol": "vless",
                    "settings": {
                        "clients": [
                            {
                                "id": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb",
                                "flow": "xtls-rprx-vision"
                            },
                            {
                                "id": "cccccccc-4444-5555-6666-dddddddddddd",
                                "flow": "xtls-rprx-vision",
                                "email": "alice@vpn"
                            }
                        ],
                        "decryption": "none"
                    },
                    "streamSettings": {
                        "network": "tcp",
                        "security": "reality"
                    }
                }
            ],
            "outbounds": [
                {"protocol": "freedom", "tag": "direct"}
            ]
        }"#
    }

    fn sample_clients_table() -> &'static str {
        r#"[
            {"clientId": "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb", "userData": {"clientName": "bob", "creationDate": ""}},
            {"clientId": "cccccccc-4444-5555-6666-dddddddddddd", "userData": {"clientName": "alice", "creationDate": ""}}
        ]"#
    }

    #[test]
    fn test_server_config_parse_and_clients() {
        let config = ServerConfig::parse(sample_server_json()).unwrap();
        let clients = config.clients();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].id, "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb");
        assert_eq!(clients[0].flow, "xtls-rprx-vision");
        assert_eq!(clients[1].email, Some("alice@vpn".to_string()));
    }

    #[test]
    fn test_server_config_roundtrip() {
        let config = ServerConfig::parse(sample_server_json()).unwrap();
        let json = config.to_json();
        let config2 = ServerConfig::parse(&json).unwrap();
        assert_eq!(config2.clients().len(), 2);
    }

    #[test]
    fn test_server_config_add_client() {
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let new_client = ServerJsonClient {
            id: "eeeeeeee-7777-8888-9999-ffffffffffff".to_string(),
            flow: "xtls-rprx-vision".to_string(),
            email: Some("charlie@vpn".to_string()),
            level: Some(0),
        };
        config.add_client(&new_client).unwrap();
        let clients = config.clients();
        assert_eq!(clients.len(), 3);
        assert_eq!(clients[2].id, "eeeeeeee-7777-8888-9999-ffffffffffff");
        assert_eq!(clients[2].email, Some("charlie@vpn".to_string()));
    }

    #[test]
    fn test_server_config_remove_client() {
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let removed = config
            .remove_client("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb")
            .unwrap();
        assert!(removed);
        assert_eq!(config.clients().len(), 1);
        assert_eq!(
            config.clients()[0].id,
            "cccccccc-4444-5555-6666-dddddddddddd"
        );
    }

    #[test]
    fn test_server_config_update_client_email() {
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let updated = config
            .update_client_email("cccccccc-4444-5555-6666-dddddddddddd", "newalice@vpn")
            .unwrap();
        assert!(updated);
        let clients = config.clients();
        assert_eq!(clients[1].email, Some("newalice@vpn".to_string()));
        // First client should be unchanged
        assert_eq!(clients[0].email, None);
    }

    #[test]
    fn test_server_config_update_client_email_nonexistent() {
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let updated = config
            .update_client_email("nonexistent-uuid", "test@vpn")
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn test_server_config_update_client_email_adds_email_field() {
        // First client has no email field — updating should add one
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let updated = config
            .update_client_email("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb", "bob@vpn")
            .unwrap();
        assert!(updated);
        let clients = config.clients();
        assert_eq!(clients[0].email, Some("bob@vpn".to_string()));
    }

    #[test]
    fn test_server_config_remove_nonexistent() {
        let mut config = ServerConfig::parse(sample_server_json()).unwrap();
        let removed = config.remove_client("nonexistent-uuid").unwrap();
        assert!(!removed);
        assert_eq!(config.clients().len(), 2);
    }

    #[test]
    fn test_server_config_has_api() {
        let config = ServerConfig::parse(sample_server_json()).unwrap();
        assert!(!config.has_api());

        // Incomplete api section (missing services) should return false
        let incomplete = ServerConfig::parse(r#"{"api":{"tag":"api"},"inbounds":[]}"#).unwrap();
        assert!(!incomplete.has_api());

        // Missing one service should return false
        let partial = ServerConfig::parse(
            r#"{"api":{"tag":"api","services":["HandlerService"]},"inbounds":[]}"#,
        )
        .unwrap();
        assert!(!partial.has_api());

        // Complete api section should return true
        let with_api = ServerConfig::parse(
            r#"{"api":{"tag":"api","services":["HandlerService","StatsService"]},"inbounds":[]}"#,
        )
        .unwrap();
        assert!(with_api.has_api());

        // Wrong tag should return false even with correct services
        let wrong_tag = ServerConfig::parse(
            r#"{"api":{"tag":"wrong","services":["HandlerService","StatsService"]},"inbounds":[]}"#,
        )
        .unwrap();
        assert!(!wrong_tag.has_api());

        // Missing tag should return false
        let no_tag = ServerConfig::parse(
            r#"{"api":{"services":["HandlerService","StatsService"]},"inbounds":[]}"#,
        )
        .unwrap();
        assert!(!no_tag.has_api());
    }

    #[test]
    fn test_clients_table_parse() {
        let table = ClientsTable::parse(sample_clients_table()).unwrap();
        assert_eq!(table.entries.len(), 2);
        assert_eq!(
            table.entries[0].client_id,
            "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb"
        );
        assert_eq!(table.entries[0].user_data.client_name, "bob");
    }

    #[test]
    fn test_clients_table_name_for_uuid() {
        let table = ClientsTable::parse(sample_clients_table()).unwrap();
        assert_eq!(
            table.name_for_uuid("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb"),
            Some("bob")
        );
        assert_eq!(table.name_for_uuid("nonexistent"), None);
    }

    #[test]
    fn test_clients_table_add() {
        let mut table = ClientsTable::parse(sample_clients_table()).unwrap();
        table.add("new-uuid".to_string(), "charlie".to_string());
        assert_eq!(table.entries.len(), 3);
        assert_eq!(table.name_for_uuid("new-uuid"), Some("charlie"));
    }

    #[test]
    fn test_clients_table_remove() {
        let mut table = ClientsTable::parse(sample_clients_table()).unwrap();
        let removed = table.remove("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb");
        assert!(removed);
        assert_eq!(table.entries.len(), 1);
    }

    #[test]
    fn test_clients_table_rename() {
        let mut table = ClientsTable::parse(sample_clients_table()).unwrap();
        let renamed = table.rename("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb", "robert");
        assert!(renamed);
        assert_eq!(
            table.name_for_uuid("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb"),
            Some("robert")
        );
    }

    #[test]
    fn test_clients_table_rename_nonexistent() {
        let mut table = ClientsTable::parse(sample_clients_table()).unwrap();
        let renamed = table.rename("nonexistent", "newname");
        assert!(!renamed);
    }

    #[test]
    fn test_clients_table_remove_nonexistent() {
        let mut table = ClientsTable::parse(sample_clients_table()).unwrap();
        let removed = table.remove("nonexistent");
        assert!(!removed);
        assert_eq!(table.entries.len(), 2);
    }

    #[test]
    fn test_clients_table_roundtrip() {
        let table = ClientsTable::parse(sample_clients_table()).unwrap();
        let json = table.to_json();
        let table2 = ClientsTable::parse(&json).unwrap();
        assert_eq!(table.entries, table2.entries);
    }

    #[test]
    fn test_merge_users() {
        let config = ServerConfig::parse(sample_server_json()).unwrap();
        let table = ClientsTable::parse(sample_clients_table()).unwrap();
        let users = merge_users(&config, &table);

        assert_eq!(users.len(), 2);

        // First user: bob (no email in server.json, derived from name)
        assert_eq!(users[0].name, "bob");
        assert_eq!(users[0].uuid, "aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb");
        assert_eq!(users[0].email, "bob@vpn");
        assert_eq!(users[0].flow, "xtls-rprx-vision");

        // Second user: alice (has email in server.json)
        assert_eq!(users[1].name, "alice");
        assert_eq!(users[1].email, "alice@vpn");
    }

    #[test]
    fn test_merge_users_unknown_uuid() {
        // Client in server.json but not in clientsTable
        let json = r#"{
            "inbounds": [{
                "protocol": "vless",
                "settings": {
                    "clients": [{"id": "unknown-uuid", "flow": "xtls-rprx-vision"}]
                }
            }]
        }"#;
        let config = ServerConfig::parse(json).unwrap();
        let table = ClientsTable::parse("[]").unwrap();
        let users = merge_users(&config, &table);

        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "unknown");
        assert_eq!(users[0].email, "unknown@vpn");
    }

    #[test]
    fn test_email_from_name() {
        assert_eq!(XrayUser::email_from_name("alice"), "alice@vpn");
        assert_eq!(XrayUser::email_from_name("bob"), "bob@vpn");
    }

    #[test]
    fn test_traffic_stats_default() {
        let stats = TrafficStats::default();
        assert_eq!(stats.uplink, 0);
        assert_eq!(stats.downlink, 0);
    }

    #[test]
    fn test_client_entry_serde() {
        let entry = ClientEntry {
            client_id: "test-uuid".to_string(),
            user_data: ClientUserData {
                client_name: "test-name".to_string(),
                creation_date: String::new(),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("clientId"));
        assert!(json.contains("userData"));
        let parsed: ClientEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_server_json_client_serde_minimal() {
        let json = r#"{"id": "some-uuid", "flow": "xtls-rprx-vision"}"#;
        let client: ServerJsonClient = serde_json::from_str(json).unwrap();
        assert_eq!(client.id, "some-uuid");
        assert_eq!(client.email, None);
        assert_eq!(client.level, None);

        // Serialization should skip None fields
        let serialized = serde_json::to_string(&client).unwrap();
        assert!(!serialized.contains("email"));
        assert!(!serialized.contains("level"));
    }

    #[test]
    fn test_reality_settings_extraction() {
        let json = r#"{
            "inbounds": [{
                "protocol": "vless",
                "settings": {"clients": []},
                "streamSettings": {
                    "network": "tcp",
                    "security": "reality",
                    "realitySettings": {
                        "dest": "www.googletagmanager.com:443",
                        "serverNames": ["www.googletagmanager.com"],
                        "privateKey": "secret",
                        "shortIds": ["abcd1234", "ef567890"]
                    }
                }
            }]
        }"#;
        let config = ServerConfig::parse(json).unwrap();
        let reality = config.reality_settings().unwrap();
        assert_eq!(reality.sni, "www.googletagmanager.com");
        assert_eq!(reality.short_id, "abcd1234");
    }

    #[test]
    fn test_reality_settings_missing() {
        let json = r#"{
            "inbounds": [{
                "protocol": "vless",
                "settings": {"clients": []},
                "streamSettings": {"network": "tcp"}
            }]
        }"#;
        let config = ServerConfig::parse(json).unwrap();
        assert!(config.reality_settings().is_none());
    }

    #[test]
    fn test_vless_port() {
        let config = ServerConfig::parse(sample_server_json()).unwrap();
        assert_eq!(config.vless_port(), Some(443));
    }

    #[test]
    fn test_vless_port_missing() {
        let json = r#"{"inbounds": [{"protocol": "vless", "settings": {"clients": []}}]}"#;
        let config = ServerConfig::parse(json).unwrap();
        assert_eq!(config.vless_port(), None);
    }

    #[test]
    fn test_no_vless_inbound_errors() {
        let json = r#"{"inbounds": [{"protocol": "vmess", "settings": {"clients": []}}]}"#;
        let mut config = ServerConfig::parse(json).unwrap();
        assert!(config.clients().is_empty());
        assert!(config
            .add_client(&ServerJsonClient {
                id: "x".into(),
                flow: "".into(),
                email: None,
                level: None,
            })
            .is_err());
        assert!(config.remove_client("x").is_err());
    }
}
