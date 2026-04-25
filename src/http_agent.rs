use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Default config path for the bridge Xray container (bind-mounted from host).
const BRIDGE_CONFIG_PATH: &str = "/etc/xray-bridge/config.json";

pub async fn run_agent(
    port: u16,
    secret: String,
    container: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr)?;
    log::info!("HTTP agent listening on {}", addr);

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Set timeouts to prevent blocking the single thread
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .ok();

        let secret = secret.clone();
        let container = container.clone();

        std::thread::spawn(move || {
            if let Err(e) = handle_connection(&mut stream, &secret, &container) {
                log::warn!("connection error: {}", e);
            }
        });
    }
    Ok(())
}

fn handle_connection(
    stream: &mut std::net::TcpStream,
    secret: &str,
    container: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        send_response(stream, 400, "Bad Request");
        return Ok(());
    }

    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Read and discard remaining headers
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.trim().is_empty() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let expected_prefix = format!("/{}", secret);
    if !path.starts_with(&expected_prefix) {
        send_response(stream, 404, "Not Found");
        return Ok(());
    }

    let action = &path[expected_prefix.len()..];

    match (method.as_str(), action) {
        ("GET", "/health") => {
            send_json(stream, 200, r#"{"ok":true}"#);
        }
        ("GET", "/stats") => {
            let output = std::process::Command::new("docker")
                .args([
                    "exec", container, "xray", "api", "statsquery", "-s",
                    "127.0.0.1:8080", "-pattern", "user>>>",
                ])
                .output();
            match output {
                Ok(o) => send_json(stream, 200, &String::from_utf8_lossy(&o.stdout)),
                Err(e) => send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e)),
            }
        }
        ("POST", action) if action.starts_with("/add-user/") => {
            let remainder = action.trim_start_matches("/add-user/");
            let parts: Vec<&str> = remainder.splitn(2, '/').collect();
            if parts.len() != 2 {
                send_json(stream, 400, r#"{"error":"usage: /add-user/<uuid>/<email>"}"#);
                return Ok(());
            }
            let (uuid, email) = (parts[0], parts[1]);
            // 1. Add to runtime via Xray API (pass JSON via stdin using echo pipe)
            let adu_json = format!(
                r#"{{"inboundTag":"client-in","user":{{"email":"{}","level":0,"account":{{"id":"{}","encryption":"none"}}}}}}"#,
                email, uuid
            );
            let output = std::process::Command::new("docker")
                .args(["exec", "-i", container, "sh", "-c",
                    &format!("echo '{}' | xray api adu -s 127.0.0.1:8080", adu_json)])
                .output();
            let api_ok = matches!(&output, Ok(o) if o.status.success());
            // 2. Persist to config.json (survives container restart)
            if api_ok {
                if let Err(e) = persist_add_user(uuid, email) {
                    log::warn!("API add OK but config persist failed: {}", e);
                }
            }
            match output {
                Ok(o) if o.status.success() => send_json(stream, 200, r#"{"ok":true}"#),
                Ok(o) => send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e)),
            }
        }
        ("POST", action) if action.starts_with("/del-user/") => {
            let email = action.trim_start_matches("/del-user/");
            // 1. Remove from runtime via Xray API
            let output = std::process::Command::new("docker")
                .args(["exec", container, "xray", "api", "rmu", "-s", "127.0.0.1:8080", "-tag=client-in", email])
                .output();
            let api_ok = matches!(&output, Ok(o) if o.status.success());
            // 2. Remove from config.json (survives container restart)
            if api_ok {
                if let Err(e) = persist_del_user(email) {
                    log::warn!("API del OK but config persist failed: {}", e);
                }
            }
            match output {
                Ok(o) if o.status.success() => send_json(stream, 200, r#"{"ok":true}"#),
                Ok(o) => send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e)),
            }
        }
        _ => {
            send_response(stream, 404, "Not Found");
        }
    }
    Ok(())
}

/// Add user to bridge config.json on disk.
fn persist_add_user(uuid: &str, email: &str) -> Result<(), String> {
    let data = std::fs::read_to_string(BRIDGE_CONFIG_PATH)
        .map_err(|e| format!("read config: {}", e))?;
    let mut config: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("parse config: {}", e))?;

    let clients = config
        .pointer_mut("/inbounds/0/settings/clients")
        .and_then(|c| c.as_array_mut())
        .ok_or("no clients array in config")?;

    // Don't add duplicate
    if clients.iter().any(|c| c.get("id").and_then(|v| v.as_str()) == Some(uuid)) {
        return Ok(());
    }

    clients.push(serde_json::json!({"id": uuid, "email": email}));

    let output = serde_json::to_string_pretty(&config).map_err(|e| format!("serialize: {}", e))?;
    std::fs::write(BRIDGE_CONFIG_PATH, output).map_err(|e| format!("write config: {}", e))?;
    log::info!("Persisted add-user {} to {}", email, BRIDGE_CONFIG_PATH);
    Ok(())
}

/// Remove user from bridge config.json on disk.
fn persist_del_user(email: &str) -> Result<(), String> {
    let data = std::fs::read_to_string(BRIDGE_CONFIG_PATH)
        .map_err(|e| format!("read config: {}", e))?;
    let mut config: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("parse config: {}", e))?;

    let clients = config
        .pointer_mut("/inbounds/0/settings/clients")
        .and_then(|c| c.as_array_mut())
        .ok_or("no clients array in config")?;

    let before = clients.len();
    clients.retain(|c| c.get("email").and_then(|v| v.as_str()) != Some(email));

    if clients.len() < before {
        let output = serde_json::to_string_pretty(&config).map_err(|e| format!("serialize: {}", e))?;
        std::fs::write(BRIDGE_CONFIG_PATH, output).map_err(|e| format!("write config: {}", e))?;
        log::info!("Persisted del-user {} from {}", email, BRIDGE_CONFIG_PATH);
    }
    Ok(())
}

fn send_response(stream: &mut impl Write, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text(status),
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn send_json(stream: &mut impl Write, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text(status),
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}
