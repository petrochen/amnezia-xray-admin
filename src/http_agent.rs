use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

pub async fn run_agent(
    port: u16,
    secret: String,
    container: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr)?;
    log::info!("HTTP agent listening on {}", addr);

    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;

        // Parse: GET /<secret>/stats HTTP/1.1
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            send_response(&mut stream, 400, "Bad Request");
            continue;
        }

        let method = parts[0];
        let path = parts[1];

        // Read and discard remaining headers
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim().is_empty() {
                break;
            }
        }

        // Auth check
        let expected_prefix = format!("/{}", secret);
        if !path.starts_with(&expected_prefix) {
            send_response(&mut stream, 404, "Not Found");
            continue;
        }

        let action = &path[expected_prefix.len()..];

        match (method, action) {
            ("GET", "/health") => {
                send_json(&mut stream, 200, r#"{"ok":true}"#);
            }
            ("GET", "/stats") => {
                let output = std::process::Command::new("docker")
                    .args([
                        "exec",
                        &container,
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
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        send_json(&mut stream, 200, &stdout);
                    }
                    Err(e) => {
                        send_json(
                            &mut stream,
                            500,
                            &format!(r#"{{"error":"{}"}}"#, e),
                        );
                    }
                }
            }
            ("GET", "/online") => {
                let output = std::process::Command::new("docker")
                    .args([
                        "exec",
                        &container,
                        "xray",
                        "api",
                        "statsquery",
                        "-s",
                        "127.0.0.1:8080",
                        "-pattern",
                        "user>>>",
                        "-reset",
                    ])
                    .output();
                match output {
                    Ok(o) => send_json(&mut stream, 200, &String::from_utf8_lossy(&o.stdout)),
                    Err(e) => send_json(
                        &mut stream,
                        500,
                        &format!(r#"{{"error":"{}"}}"#, e),
                    ),
                }
            }
            ("POST", path) if path.starts_with("/add-user/") => {
                // POST /<secret>/add-user/<uuid>/<email>
                let parts: Vec<&str> = path
                    .trim_start_matches("/add-user/")
                    .splitn(2, '/')
                    .collect();
                if parts.len() != 2 {
                    send_json(
                        &mut stream,
                        400,
                        r#"{"error":"usage: /add-user/<uuid>/<email>"}"#,
                    );
                    continue;
                }
                let uuid = parts[0];
                let email = parts[1];
                let adu_json = format!(
                    r#"{{"inboundTag":"client-in","user":{{"email":"{}","level":0,"account":{{"id":"{}","encryption":"none"}}}}}}"#,
                    email, uuid
                );
                let output = std::process::Command::new("docker")
                    .args([
                        "exec",
                        &container,
                        "xray",
                        "api",
                        "adu",
                        "-s",
                        "127.0.0.1:8080",
                        &adu_json,
                    ])
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        send_json(&mut stream, 200, r#"{"ok":true}"#);
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        send_json(
                            &mut stream,
                            500,
                            &format!(r#"{{"error":"{}"}}"#, stderr.trim()),
                        );
                    }
                    Err(e) => send_json(
                        &mut stream,
                        500,
                        &format!(r#"{{"error":"{}"}}"#, e),
                    ),
                }
            }
            ("POST", path) if path.starts_with("/del-user/") => {
                let email = path.trim_start_matches("/del-user/");
                let output = std::process::Command::new("docker")
                    .args([
                        "exec",
                        &container,
                        "xray",
                        "api",
                        "rmu",
                        "-s",
                        "127.0.0.1:8080",
                        "-tag=client-in",
                        email,
                    ])
                    .output();
                match output {
                    Ok(o) if o.status.success() => {
                        send_json(&mut stream, 200, r#"{"ok":true}"#);
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        send_json(
                            &mut stream,
                            500,
                            &format!(r#"{{"error":"{}"}}"#, stderr.trim()),
                        );
                    }
                    Err(e) => send_json(
                        &mut stream,
                        500,
                        &format!(r#"{{"error":"{}"}}"#, e),
                    ),
                }
            }
            _ => {
                send_response(&mut stream, 404, "Not Found");
            }
        }
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
