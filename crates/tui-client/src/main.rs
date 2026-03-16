mod app;
mod command;
mod network;
mod script;
mod tui;

use anyhow::Result;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let mut host = "127.0.0.1".to_string();
    let mut token: Option<String> = None;
    let mut character_name: Option<String> = None;
    let mut script_mode = false;
    let mut login_username: Option<String> = None;
    let mut login_password: Option<String> = None;
    let mut auth_host = "127.0.0.1".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--script" => {
                script_mode = true;
                i += 1;
            }
            "--tui" => {
                script_mode = false;
                i += 1;
            }
            "--host" => {
                host = args.get(i + 1).cloned().unwrap_or(host);
                i += 2;
            }
            "--token" => {
                token = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--login" => {
                login_username = args.get(i + 1).cloned();
                login_password = args.get(i + 2).cloned();
                i += 3;
            }
            "--name" => {
                character_name = args.get(i + 1).cloned();
                i += 2;
            }
            "--auth-host" => {
                auth_host = args.get(i + 1).cloned().unwrap_or(auth_host);
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Resolve token (login via login-server if needed)
    let token = if let (Some(username), Some(password)) = (login_username, login_password) {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{auth_host}:8080/login"))
            .json(&serde_json::json!({"username": username, "password": password}))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("login-server returned {status}: {body}");
        }

        let body: serde_json::Value = resp.json().await?;
        body["token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no 'token' field in login response"))?
            .to_string()
    } else {
        token.unwrap_or_else(|| "test-client-token".to_string())
    };

    // Setup channels
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (action_tx, action_rx) = mpsc::unbounded_channel();

    let config = network::NetworkConfig {
        host: host.clone(),
        token,
    };

    // Spawn network task
    let net_event_tx = event_tx.clone();
    let net_handle = tokio::spawn(async move {
        if let Err(e) = network::run_network(config, net_event_tx.clone(), action_rx).await {
            let _ = net_event_tx.send(app::ServerEvent::Disconnected {
                reason: format!("{e}"),
            });
        }
    });

    // Run the appropriate mode
    if script_mode {
        // In script mode, init tracing to stderr so stdout is clean JSON
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .init();

        script::run_script(event_rx, action_tx, character_name).await?;
    } else {
        // TUI mode — no tracing to avoid corrupting terminal
        tui::run_tui(event_rx, action_tx, character_name).await?;
    }

    net_handle.abort();
    Ok(())
}
