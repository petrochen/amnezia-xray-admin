//! HTTP client for the bridge agent API.
//!
//! Uses `ureq` for HTTP requests and `serde` for JSON parsing.

use std::time::Duration;

/// Host system metrics from the bridge server.
#[derive(serde::Deserialize, Default)]
pub struct BridgeSysinfo {
    #[serde(default)]
    pub load_1min: String,
    #[serde(default)]
    pub mem_used_mb: u64,
    #[serde(default)]
    pub mem_total_mb: u64,
    #[serde(default)]
    pub disk_used_gb: u64,
    #[serde(default)]
    pub disk_total_gb: u64,
    #[serde(default)]
    pub uptime_secs: u64,
}

impl BridgeSysinfo {
    pub fn mem_percent(&self) -> u64 {
        if self.mem_total_mb == 0 {
            return 0;
        }
        self.mem_used_mb * 100 / self.mem_total_mb
    }
    pub fn disk_percent(&self) -> u64 {
        if self.disk_total_gb == 0 {
            return 0;
        }
        self.disk_used_gb * 100 / self.disk_total_gb
    }
    pub fn uptime_human(&self) -> String {
        let s = self.uptime_secs;
        let days = s / 86400;
        let hours = (s % 86400) / 3600;
        let mins = (s % 3600) / 60;
        if days > 0 {
            format!("{}d {}h", days, hours)
        } else if hours > 0 {
            format!("{}h {}m", hours, mins)
        } else {
            format!("{}m", mins)
        }
    }
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct BridgeClient {
    base_url: String, // e.g. "http://51.250.73.78:9090/secret-key"
    agent: ureq::Agent,
}

impl BridgeClient {
    pub fn new(url: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .new_agent();
        Self {
            base_url: url.trim_end_matches('/').to_string(),
            agent,
        }
    }

    /// GET /<secret>/stats — returns raw JSON string
    pub fn get_stats(&self) -> Result<String, String> {
        self.http_get("/stats")
    }

    /// GET /<secret>/online — returns per-user online counts from bridge
    pub fn get_online(&self) -> Result<Vec<(String, u32)>, String> {
        let json = self.http_get("/online")?;
        Ok(parse_bridge_online(&json))
    }

    /// GET /<secret>/health
    pub fn health(&self) -> Result<bool, String> {
        let body = self.http_get("/health")?;
        Ok(body.contains("true"))
    }

    /// POST /<secret>/add-user/<uuid>/<email>
    pub fn add_user(&self, uuid: &str, email: &str) -> Result<(), String> {
        let path = format!("/add-user/{}/{}", uuid, email);
        let body = self.http_post(&path)?;
        if body.contains("true") {
            Ok(())
        } else {
            Err(body)
        }
    }

    /// POST /<secret>/del-user/<email>
    pub fn del_user(&self, email: &str) -> Result<(), String> {
        let path = format!("/del-user/{}", email);
        let body = self.http_post(&path)?;
        if body.contains("true") {
            Ok(())
        } else {
            Err(body)
        }
    }

    /// GET /<secret>/sysinfo — returns host metrics from bridge server
    pub fn get_sysinfo(&self) -> Result<BridgeSysinfo, String> {
        let json = self.http_get("/sysinfo")?;
        serde_json::from_str(&json).map_err(|e| format!("parse sysinfo: {}", e))
    }

    /// GET /<secret>/backup — returns raw config.json content
    pub fn get_backup(&self) -> Result<String, String> {
        self.http_get("/backup")
    }

    /// POST /<secret>/restore — sends config JSON, triggers container restart on bridge
    pub fn restore_backup(&self, config: &str) -> Result<(), String> {
        let body = self.http_post_body("/restore", config.as_bytes())?;
        if body.contains("true") {
            Ok(())
        } else {
            Err(body)
        }
    }

    fn http_get(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url, path);
        let body = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("HTTP GET {}: {}", url, e))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read body: {}", e))?;
        Ok(body)
    }

    fn http_post(&self, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url, path);
        let body = self
            .agent
            .post(&url)
            .send_empty()
            .map_err(|e| format!("HTTP POST {}: {}", url, e))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read body: {}", e))?;
        Ok(body)
    }

    fn http_post_body(&self, path: &str, body: &[u8]) -> Result<String, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .agent
            .post(&url)
            .send(body)
            .map_err(|e| format!("HTTP POST {}: {}", url, e))?
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("read body: {}", e))?;
        Ok(resp)
    }
}

/// Xray stats API response format.
#[derive(serde::Deserialize, Default)]
struct StatsResponse {
    #[serde(default)]
    stat: Vec<StatEntry>,
}

#[derive(serde::Deserialize)]
struct StatEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

impl StatEntry {
    fn numeric_value(&self) -> u64 {
        match &self.value {
            Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
            _ => 0,
        }
    }
}

/// Parse bridge stats JSON and return Vec<(email, uplink_bytes, downlink_bytes)>.
///
/// The xray stats API returns entries like:
/// `{"stat":[{"name":"user>>>email@vpn>>>traffic>>>uplink","value":"12345"}]}`
/// This function extracts email, uplink and downlink for each user.
pub fn parse_bridge_stats(json: &str) -> Vec<(String, u64, u64)> {
    let response: StatsResponse = serde_json::from_str(json).unwrap_or_default();

    let mut map: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    for entry in &response.stat {
        let parts: Vec<&str> = entry.name.split(">>>").collect();
        if parts.len() >= 4 && parts[0] == "user" && parts[2] == "traffic" {
            let email = parts[1].to_string();
            let value = entry.numeric_value();
            let e = map.entry(email).or_insert((0, 0));
            if parts[3] == "uplink" {
                e.0 = value;
            } else if parts[3] == "downlink" {
                e.1 = value;
            }
        }
    }

    map.into_iter()
        .map(|(email, (up, down))| (email, up, down))
        .collect()
}

/// Parse bridge online JSON and return Vec<(email, online_count)>.
///
/// Expected format from bridge agent GET /online:
/// `{"users":[{"email":"alex@vpn","online":2},...]}`
pub fn parse_bridge_online(json: &str) -> Vec<(String, u32)> {
    #[derive(serde::Deserialize, Default)]
    struct OnlineResponse {
        #[serde(default)]
        users: Vec<OnlineEntry>,
    }
    #[derive(serde::Deserialize)]
    struct OnlineEntry {
        email: String,
        online: u32,
    }
    let response: OnlineResponse = serde_json::from_str(json).unwrap_or_default();
    response
        .users
        .into_iter()
        .map(|e| (e.email, e.online))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bridge_online_basic() {
        let json =
            r#"{"users":[{"email":"alex@vpn","online":2},{"email":"kostya@vpn","online":0}]}"#;
        let result = parse_bridge_online(json);
        assert_eq!(result.len(), 2);
        let alex = result.iter().find(|(e, _)| e == "alex@vpn").unwrap();
        assert_eq!(alex.1, 2);
        let kostya = result.iter().find(|(e, _)| e == "kostya@vpn").unwrap();
        assert_eq!(kostya.1, 0);
    }

    #[test]
    fn test_parse_bridge_online_empty() {
        let result = parse_bridge_online(r#"{"users":[]}"#);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_bridge_online_malformed() {
        let result = parse_bridge_online("not json");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_bridge_stats_empty() {
        let result = parse_bridge_stats("{}");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_bridge_stats_basic() {
        let json = r#"{"stat":[
            {"name":"user>>>alice@vpn>>>traffic>>>uplink","value":"1024"},
            {"name":"user>>>alice@vpn>>>traffic>>>downlink","value":"4096"},
            {"name":"user>>>bob@vpn>>>traffic>>>uplink","value":"512"}
        ]}"#;
        let mut result = parse_bridge_stats(json);
        result.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(result.len(), 2);
        let alice = result.iter().find(|(e, _, _)| e == "alice@vpn").unwrap();
        assert_eq!(alice.1, 1024);
        assert_eq!(alice.2, 4096);
        let bob = result.iter().find(|(e, _, _)| e == "bob@vpn").unwrap();
        assert_eq!(bob.1, 512);
        assert_eq!(bob.2, 0);
    }

    #[test]
    fn test_parse_bridge_stats_numeric_values() {
        let json = r#"{"stat":[
            {"name":"user>>>test@vpn>>>traffic>>>uplink","value":999}
        ]}"#;
        let result = parse_bridge_stats(json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, 999);
    }

    #[test]
    fn test_parse_bridge_stats_pretty_printed() {
        let json = r#"{
  "stat": [
    {
      "name": "user>>>alice@vpn>>>traffic>>>uplink",
      "value": "2048"
    },
    {
      "name": "user>>>alice@vpn>>>traffic>>>downlink",
      "value": "8192"
    }
  ]
}"#;
        let result = parse_bridge_stats(json);
        assert_eq!(result.len(), 1);
        let alice = &result[0];
        assert_eq!(alice.0, "alice@vpn");
        assert_eq!(alice.1, 2048);
        assert_eq!(alice.2, 8192);
    }

    #[test]
    fn test_parse_bridge_stats_malformed_json() {
        let result = parse_bridge_stats("not json at all");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_bridge_stats_missing_value() {
        let json = r#"{"stat":[
            {"name":"user>>>test@vpn>>>traffic>>>uplink"}
        ]}"#;
        let result = parse_bridge_stats(json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, 0);
    }
}
