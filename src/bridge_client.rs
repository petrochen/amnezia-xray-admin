//! Simple synchronous HTTP client for the bridge agent API.
//!
//! Uses only std::net::TcpStream — no external HTTP dependencies.


pub struct BridgeClient {
    base_url: String, // e.g. "http://51.250.73.78:9090/secret-key"
}

impl BridgeClient {
    pub fn new(url: String) -> Self {
        Self {
            base_url: url.trim_end_matches('/').to_string(),
        }
    }

    /// GET /<secret>/stats — returns raw JSON string
    pub fn get_stats(&self) -> Result<String, String> {
        self.http_get("/stats")
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

    fn parse_url(&self) -> Result<(String, u16, String), String> {
        // Parse "http://host:port/path"
        let url = self
            .base_url
            .strip_prefix("http://")
            .unwrap_or(&self.base_url);
        let (host_port, base_path) = url.split_once('/').unwrap_or((url, ""));
        let (host, port) = if host_port.contains(':') {
            let parts: Vec<&str> = host_port.splitn(2, ':').collect();
            (
                parts[0].to_string(),
                parts[1]
                    .parse::<u16>()
                    .map_err(|e| format!("invalid port: {}", e))?,
            )
        } else {
            (host_port.to_string(), 80)
        };
        Ok((host, port, format!("/{}", base_path)))
    }

    fn http_get(&self, path: &str) -> Result<String, String> {
        self.do_curl("GET", path)
    }

    fn http_post(&self, path: &str) -> Result<String, String> {
        self.do_curl("POST", path)
    }

    fn do_request(&self, _host: &str, _port: u16, _request: &str) -> Result<String, String> {
        unreachable!("use do_curl instead")
    }

    fn do_curl(&self, method: &str, path: &str) -> Result<String, String> {
        let url = format!("{}{}", self.base_url, path);
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-sf", "-4", "--max-time", "5", "--connect-timeout", "3"]);
        if method == "POST" {
            cmd.args(["-X", "POST"]);
        }
        cmd.arg(&url);
        let output = cmd.output().map_err(|e| format!("curl exec: {}", e))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(format!("curl failed: {}", String::from_utf8_lossy(&output.stderr)))
        }
    }
}

/// Parse bridge stats JSON and return Vec<(email, uplink_bytes, downlink_bytes)>.
///
/// The xray stats API returns entries like:
/// `{"stat":{"name":"user>>>email@vpn>>>traffic>>>uplink","value":"12345"}}`
/// This function extracts email, uplink and downlink for each user.
pub fn parse_bridge_stats(json: &str) -> Vec<(String, u64, u64)> {
    // Collect all name/value pairs from the stats JSON
    let mut entries: Vec<(String, u64)> = Vec::new();

    // Simple manual extraction: find all "name":"..." and "value":"..." pairs
    let mut remaining = json;
    while let Some(name_pos) = remaining.find("\"name\"") {
        remaining = &remaining[name_pos + 6..];
        // Skip ': "' or ':"'
        let remaining_trimmed = remaining.trim_start_matches(|c: char| c == ':' || c == ' ' || c == '"');
        let skipped = remaining.len() - remaining_trimmed.len();
        remaining = &remaining[skipped..];
        let end = match remaining.find('"') {
            Some(e) => e,
            None => break,
        };
        let name = remaining[..end].to_string();
        remaining = &remaining[end..];

        // Look for a "value" field nearby (within next 200 chars)
        let search_window = &remaining[..remaining.len().min(200)];
        let value: u64 = if let Some(val_pos) = search_window.find("\"value\"") {
            let after_key = &search_window[val_pos + 7..];
            // Skip ": " or ":" or ": \""
            let val_str = after_key.trim_start_matches(|c: char| c == ':' || c == ' ' || c == '"');
            let val_end = val_str
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(val_str.len());
            val_str[..val_end].parse().unwrap_or(0)
        } else {
            0
        };

        entries.push((name, value));
    }

    // Group by email: name format is "user>>>email@vpn>>>traffic>>>uplink"
    let mut map: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
    for (name, value) in &entries {
        // Extract email and direction from the name
        let parts: Vec<&str> = name.split(">>>").collect();
        if parts.len() >= 4 && parts[0] == "user" && parts[2] == "traffic" {
            let email = parts[1].to_string();
            let direction = parts[3];
            let entry = map.entry(email).or_insert((0, 0));
            if direction == "uplink" {
                entry.0 = *value;
            } else if direction == "downlink" {
                entry.1 = *value;
            }
        }
    }

    map.into_iter()
        .map(|(email, (up, down))| (email, up, down))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_url_with_port_and_path() {
        let client = BridgeClient::new("http://51.250.73.78:9090/my-secret".to_string());
        let (host, port, path) = client.parse_url().unwrap();
        assert_eq!(host, "51.250.73.78");
        assert_eq!(port, 9090);
        assert_eq!(path, "/my-secret");
    }

    #[test]
    fn test_parse_url_no_port() {
        let client = BridgeClient::new("http://example.com/secret".to_string());
        let (host, port, path) = client.parse_url().unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
        assert_eq!(path, "/secret");
    }

    #[test]
    fn test_parse_url_trailing_slash_trimmed() {
        let client = BridgeClient::new("http://host:9090/secret/".to_string());
        let (host, port, path) = client.parse_url().unwrap();
        assert_eq!(host, "host");
        assert_eq!(port, 9090);
        assert_eq!(path, "/secret");
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
}
