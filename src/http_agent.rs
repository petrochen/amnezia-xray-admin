use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default config path for the bridge Xray container (bind-mounted from host).
const BRIDGE_CONFIG_PATH: &str = "/etc/xray-bridge/config.json";

pub async fn run_agent(
    port: u16,
    secret: String,
    container: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr)?;

    let running = Arc::new(AtomicBool::new(true));

    // Graceful shutdown on SIGTERM (systemd stop) / SIGINT (Ctrl+C)
    let running_signal = running.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        log::info!("Received shutdown signal");
        running_signal.store(false, Ordering::Relaxed);
    });

    log::info!("HTTP agent listening on {}", addr);

    while running.load(Ordering::Relaxed) {
        // Non-blocking accept so we can check the shutdown flag
        listener.set_nonblocking(true).ok();
        let stream = listener.accept();
        listener.set_nonblocking(false).ok();

        let mut stream = match stream {
            Ok((s, _addr)) => s,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(_) => continue,
        };

        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();

        let secret = secret.clone();
        let container = container.clone();

        std::thread::spawn(move || {
            if let Err(e) = handle_connection(&mut stream, &secret, &container) {
                log::warn!("connection error: {}", e);
            }
        });
    }

    log::info!("HTTP agent shutting down gracefully");
    Ok(())
}

/// Wait for SIGTERM or SIGINT.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
    }
}

fn handle_connection(
    stream: &mut std::net::TcpStream,
    secret: &str,
    container: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        log_request("???", "???", 400, start.elapsed());
        send_response(stream, 400, "Bad Request");
        return Ok(());
    }

    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Read headers, capturing Content-Length
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                let lower = trimmed.to_ascii_lowercase();
                if lower.starts_with("content-length:") {
                    content_length = lower["content-length:".len()..].trim().parse().unwrap_or(0);
                }
            }
            Err(_) => break,
        }
    }

    // Read body for POST requests (capped at 1 MB)
    let body = if method == "POST" && content_length > 0 {
        let cap = content_length.min(1024 * 1024);
        let mut buf = vec![0u8; cap];
        let mut total = 0;
        while total < cap {
            match reader.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        buf.truncate(total);
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::new()
    };

    let expected_prefix = format!("/{}", secret);
    if !path.starts_with(&expected_prefix) {
        log_request(&method, "<unauthorized>", 404, start.elapsed());
        send_response(stream, 404, "Not Found");
        return Ok(());
    }

    let action = &path[expected_prefix.len()..];

    let status = match (method.as_str(), action) {
        ("GET", "/health") => {
            send_json(stream, 200, r#"{"ok":true}"#);
            200
        }
        ("GET", "/backup") => match std::fs::read_to_string(BRIDGE_CONFIG_PATH) {
            Ok(config) => {
                send_json(stream, 200, &config);
                200
            }
            Err(e) => {
                send_json(stream, 500, &format!(r#"{{"error":"read config: {}"}}"#, e));
                500
            }
        },
        ("POST", "/restore") => {
            if body.is_empty() {
                send_json(stream, 400, r#"{"error":"empty body"}"#);
                400
            } else if serde_json::from_str::<serde_json::Value>(&body).is_err() {
                send_json(stream, 400, r#"{"error":"invalid JSON"}"#);
                400
            } else {
                match do_restore_bridge_config(&body, container) {
                    Ok(()) => {
                        send_json(stream, 200, r#"{"ok":true}"#);
                        200
                    }
                    Err(e) => {
                        send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e));
                        500
                    }
                }
            }
        }
        ("GET", "/stats") => {
            let output = std::process::Command::new("docker")
                .args([
                    "exec",
                    container,
                    "xray",
                    "api",
                    "statsquery",
                    "-s",
                    "127.0.0.1:8080",
                    "-pattern",
                    "user>>>",
                ])
                .output();
            match output {
                Ok(o) => {
                    send_json(stream, 200, &String::from_utf8_lossy(&o.stdout));
                    200
                }
                Err(e) => {
                    send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e));
                    500
                }
            }
        }
        ("GET", "/online") => {
            let emails = user_emails_from_config();
            let mut entries = Vec::new();
            for email in &emails {
                let out = std::process::Command::new("docker")
                    .args([
                        "exec",
                        container,
                        "xray",
                        "api",
                        "statsonline",
                        "-s",
                        "127.0.0.1:8080",
                        "-email",
                        email,
                    ])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .unwrap_or_default();
                let count = parse_statsonline(&out);
                entries.push(format!(r#"{{"email":"{}","online":{}}}"#, email, count));
            }
            let json = format!(r#"{{"users":[{}]}}"#, entries.join(","));
            send_json(stream, 200, &json);
            200
        }
        ("POST", action) if action.starts_with("/add-user/") => {
            let remainder = action.trim_start_matches("/add-user/");
            let parts: Vec<&str> = remainder.splitn(2, '/').collect();
            if parts.len() != 2 {
                send_json(
                    stream,
                    400,
                    r#"{"error":"usage: /add-user/<uuid>/<email>"}"#,
                );
                log_request(&method, action, 400, start.elapsed());
                return Ok(());
            }
            let (uuid, email) = (parts[0], parts[1]);
            let adu_json = format!(
                r#"{{"inboundTag":"client-in","user":{{"email":"{}","level":0,"account":{{"id":"{}","encryption":"none"}}}}}}"#,
                email, uuid
            );
            let output = std::process::Command::new("docker")
                .args([
                    "exec",
                    "-i",
                    container,
                    "sh",
                    "-c",
                    &format!("echo '{}' | xray api adu -s 127.0.0.1:8080", adu_json),
                ])
                .output();
            let api_ok = matches!(&output, Ok(o) if o.status.success());
            if api_ok {
                if let Err(e) = persist_add_user(uuid, email) {
                    log::warn!("API add OK but config persist failed: {}", e);
                }
            }
            match output {
                Ok(o) if o.status.success() => {
                    send_json(stream, 200, r#"{"ok":true}"#);
                    200
                }
                Ok(o) => {
                    send_json(
                        stream,
                        500,
                        &format!(
                            r#"{{"error":"{}"}}"#,
                            String::from_utf8_lossy(&o.stderr).trim()
                        ),
                    );
                    500
                }
                Err(e) => {
                    send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e));
                    500
                }
            }
        }
        ("POST", action) if action.starts_with("/del-user/") => {
            let email = action.trim_start_matches("/del-user/");
            let output = std::process::Command::new("docker")
                .args([
                    "exec",
                    container,
                    "xray",
                    "api",
                    "rmu",
                    "-s",
                    "127.0.0.1:8080",
                    "-tag=client-in",
                    email,
                ])
                .output();
            let api_ok = matches!(&output, Ok(o) if o.status.success());
            if api_ok {
                if let Err(e) = persist_del_user(email) {
                    log::warn!("API del OK but config persist failed: {}", e);
                }
            }
            match output {
                Ok(o) if o.status.success() => {
                    send_json(stream, 200, r#"{"ok":true}"#);
                    200
                }
                Ok(o) => {
                    send_json(
                        stream,
                        500,
                        &format!(
                            r#"{{"error":"{}"}}"#,
                            String::from_utf8_lossy(&o.stderr).trim()
                        ),
                    );
                    500
                }
                Err(e) => {
                    send_json(stream, 500, &format!(r#"{{"error":"{}"}}"#, e));
                    500
                }
            }
        }
        _ => {
            send_response(stream, 404, "Not Found");
            404
        }
    };

    log_request(&method, action, status, start.elapsed());
    Ok(())
}

/// Log request with method, action, status code, and duration.
fn log_request(method: &str, action: &str, status: u16, duration: Duration) {
    log::info!(
        "{} {} → {} ({:.1}ms)",
        method,
        action,
        status,
        duration.as_secs_f64() * 1000.0
    );
}

/// Read user emails from bridge config.json.
fn user_emails_from_config() -> Vec<String> {
    let data = match std::fs::read_to_string(BRIDGE_CONFIG_PATH) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let config: serde_json::Value = match serde_json::from_str(&data) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    config
        .pointer("/inbounds/0/settings/clients")
        .and_then(|c| c.as_array())
        .map(|clients| {
            clients
                .iter()
                .filter_map(|c| c.get("email").and_then(|e| e.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse online count from `xray api statsonline` JSON output.
fn parse_statsonline(output: &str) -> u32 {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|v| {
            v.get("stat")
                .and_then(|s| s.get("value"))
                .and_then(|v| v.as_i64())
        })
        .map(|v| v.max(0) as u32)
        .unwrap_or(0)
}

/// Atomically overwrite bridge config and restart the xray container.
fn do_restore_bridge_config(config: &str, container: &str) -> Result<(), String> {
    let tmp = format!("{}.tmp", BRIDGE_CONFIG_PATH);
    std::fs::write(&tmp, config).map_err(|e| format!("write temp: {}", e))?;
    std::fs::rename(&tmp, BRIDGE_CONFIG_PATH).map_err(|e| format!("rename: {}", e))?;
    let output = std::process::Command::new("docker")
        .args(["restart", container])
        .output()
        .map_err(|e| format!("docker restart: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "docker restart failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    log::info!("Restored bridge config and restarted {}", container);
    Ok(())
}

/// Add user to bridge config.json on disk.
fn persist_add_user(uuid: &str, email: &str) -> Result<(), String> {
    let data =
        std::fs::read_to_string(BRIDGE_CONFIG_PATH).map_err(|e| format!("read config: {}", e))?;
    let mut config: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("parse config: {}", e))?;

    let clients = config
        .pointer_mut("/inbounds/0/settings/clients")
        .and_then(|c| c.as_array_mut())
        .ok_or("no clients array in config")?;

    // Don't add duplicate
    if clients
        .iter()
        .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(uuid))
    {
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
    let data =
        std::fs::read_to_string(BRIDGE_CONFIG_PATH).map_err(|e| format!("read config: {}", e))?;
    let mut config: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("parse config: {}", e))?;

    let clients = config
        .pointer_mut("/inbounds/0/settings/clients")
        .and_then(|c| c.as_array_mut())
        .ok_or("no clients array in config")?;

    let before = clients.len();
    clients.retain(|c| c.get("email").and_then(|v| v.as_str()) != Some(email));

    if clients.len() < before {
        let output =
            serde_json::to_string_pretty(&config).map_err(|e| format!("serialize: {}", e))?;
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
