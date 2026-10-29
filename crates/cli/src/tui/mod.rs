//! Ratatui-based TUI chat client for the Workforce gateway.

mod app;
mod chat_view;
mod event;
mod input_area;
mod markdown;
mod status_bar;
mod terminal;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use eyre::Result;
use tokio::sync::Mutex;
use workforce_gateway_client::protocol::Attachment;
use workforce_gateway_client::{GatewayClient, GatewayClientConfig, GatewayRestClient};

use app::{
    classify_file, format_size, mime_for_file, Action, App, AttachmentKind, PendingAttachment,
};
use event::EventHandler;

/// Run the TUI chat client connected to the given gateway.
pub async fn run(
    gateway_url: &str,
    token: &str,
    model: &str,
    agent_id: Option<&str>,
) -> Result<()> {
    // Set up the gateway client
    let config = GatewayClientConfig {
        url: gateway_url.to_string(),
        token: token.to_string(),
        ..Default::default()
    };
    let mut client = GatewayClient::new(config);
    if let Some(id) = agent_id {
        client.set_agent_id(id);
    }
    let gateway_rx = client
        .take_events()
        .expect("events receiver should be available");

    // Connect to gateway
    client.connect().await?;

    // Rest client for /sessions etc.
    let rest_client = GatewayRestClient::new(gateway_url, token);

    // Suppress tracing output while TUI is active (it corrupts the alternate screen)
    workforce_common::logging::suppress_logging();

    // Set up terminal
    terminal::install_panic_hook();
    let mut term = terminal::init()?;

    // Build app state
    let mut app = App::new(
        model.to_string(),
        agent_id.map(|s| s.to_string()),
        gateway_url.to_string(),
    );
    app.connection_status = app::ConnectionStatus::Connected;

    // Build event handler
    let mut events = EventHandler::new(gateway_rx, 4.0, 30.0);

    // Wrap client for async operations in the loop
    let client = Arc::new(Mutex::new(client));

    // Initial render
    term.draw(|frame| ui::draw(frame, &mut app))?;

    // Pre-populate caches for tab completion
    populate_agent_cache(&mut app, &rest_client).await;
    populate_session_cache(&mut app, &rest_client).await;

    // Main event loop
    loop {
        let Some(event) = events.next().await else {
            break;
        };

        let action = app.handle_event(event);

        // Auto-send welcome directive if the agent has one and we just connected
        if let Some(directive) = app.pending_welcome.take() {
            let c = client.lock().await;
            if let Err(e) = c
                .send_chat(&format!(
                    "[WELCOME DIRECTIVE — system instruction, not a user message]\n{}",
                    directive
                ))
                .await
            {
                app.push_message(
                    app::MessageRole::System,
                    format!("Failed to send welcome: {}", e),
                );
            } else {
                app.waiting_for_response = true;
            }
            // Update session ID
            if let Some(sid) = c.session_id().await {
                app.session_id = Some(sid);
            }
        }

        match action {
            Action::Render => {
                term.draw(|frame| ui::draw(frame, &mut app))?;
            }
            Action::SendMessage(text) => {
                // If the previous response was cancelled, let the AI know
                let chat_text = if app.stream_cancelled {
                    format!(
                        "[SYSTEM NOTE: Your previous response was interrupted/cancelled by the user. \
                         Do not continue or act on your previous partial output. \
                         Address only the user's new message below.]\n\n{}",
                        text
                    )
                } else {
                    text.clone()
                };

                // Read and encode pending attachments
                let (attachments, attachment_labels) = read_pending_attachments(&mut app);

                // Show user message with attachment labels
                let display_text = if attachment_labels.is_empty() {
                    text.clone()
                } else {
                    format!("{}\n{}", text, attachment_labels.join("\n"))
                };
                app.add_user_message(&display_text);

                let c = client.lock().await;
                let send_result = if attachments.is_empty() {
                    c.send_chat(&chat_text).await
                } else {
                    c.send_chat_with_attachments(&chat_text, attachments).await
                };
                if let Err(e) = send_result {
                    app.push_message(app::MessageRole::System, format!("Failed to send: {}", e));
                    app.waiting_for_response = false;
                }
                // Update session ID
                if let Some(sid) = c.session_id().await {
                    app.session_id = Some(sid);
                }
            }
            Action::CancelStream => {
                app.cancel_stream();
                let c = client.lock().await;
                let _ = c.send_cancel().await;
            }
            Action::ToggleMouseCapture => {
                app.mouse_capture = !app.mouse_capture;
                if app.mouse_capture {
                    execute!(io::stderr(), EnableMouseCapture).ok();
                } else {
                    execute!(io::stderr(), DisableMouseCapture).ok();
                }
            }
            Action::AttachFile(path) => {
                handle_attach_file(&mut app, &path);
            }
            Action::RemoveAttachment => {
                if let Some(removed) = app.pending_attachments.pop() {
                    app.push_message(
                        app::MessageRole::System,
                        format!("Removed: {}", removed.filename),
                    );
                }
            }
            Action::LocalCommand(cmd) => {
                handle_local_command(&mut app, &cmd, &client, &rest_client).await;
            }
            Action::Quit => {
                app.should_quit = true;
            }
            Action::None => {}
        }

        if app.should_quit {
            break;
        }
    }

    // Cleanup
    {
        let mut c = client.lock().await;
        c.disconnect().await;
    }
    terminal::restore()?;
    workforce_common::logging::restore_logging();

    Ok(())
}

/// Handle local slash commands (client-side).
async fn handle_local_command(
    app: &mut App,
    cmd: &str,
    client: &Arc<Mutex<GatewayClient>>,
    rest_client: &GatewayRestClient,
) {
    let parts: Vec<&str> = cmd.splitn(2, char::is_whitespace).collect();
    let command = parts[0].to_lowercase();
    let args = parts.get(1).map(|s| s.trim());

    match command.as_str() {
        "/quit" | "/exit" | "/q" => {
            app.should_quit = true;
        }
        "/help" | "/h" | "/?" => {
            app.show_help = !app.show_help;
            app.push_message(
                app::MessageRole::System,
                "## Commands\n\n\
                 | Command | |\n\
                 |---|---|\n\
                 | `/quit`, `/q` | Exit |\n\
                 | `/new` | New session |\n\
                 | `/sessions` | List sessions |\n\
                 | `/switch` *id* | Switch session (Tab to complete) |\n\
                 | `/agents` | List available agents |\n\
                 | `/agent` *id* | Switch agent (Tab to complete) |\n\
                 | `/attach` *path* | Attach file or image |\n\
                 | `/mode` | Connection info |\n\
                 | `/help` | This help |\n\n\
                 Server commands (forwarded): `/status`, `/compact`, `/context`, `/model`, `/health`, `/wake`, `/heartbeat`, `/reset`, `/select-agent`, `/tools`\n\n\
                 ## Keybindings\n\n\
                 | Key | |\n\
                 |---|---|\n\
                 | Esc (streaming) | Stop AI response |\n\
                 | Esc | Scroll mode |\n\
                 | i / Enter | Input mode |\n\
                 | Up/Down/PgUp/PgDn | Scroll chat |\n\
                 | G | Jump to bottom |\n\
                 | Ctrl+U | Clear input |\n\
                 | Ctrl+X | Remove attachment |\n\
                 | Ctrl+Y | Toggle mouse capture |\n\
                 | Tab / Shift+Tab | Auto-complete |\n\
                 | Shift+click | Select text |\n\n\
                 Drag files into terminal or use `/attach` to send files. \
                 Multiline paste is supported automatically."
                    .to_string(),
            );
        }
        "/new" | "/n" => {
            let c = client.lock().await;
            c.clear_session_id().await;
            app.session_id = None;
            app.messages.clear();
            app.message_count = 0;
            app.context_tokens = 0;
            app.push_message(app::MessageRole::System, "New session started".to_string());
        }
        "/sessions" => match rest_client.sessions().await {
            Ok(resp) => {
                // Update session cache for tab completion
                app.cached_sessions = resp
                    .sessions
                    .iter()
                    .map(|s| (s.id.clone(), s.label.clone()))
                    .collect();

                if resp.sessions.is_empty() {
                    app.push_message(app::MessageRole::System, "No active sessions".to_string());
                } else {
                    let current = {
                        let c = client.lock().await;
                        c.session_id().await
                    };
                    let mut text =
                        String::from("| Session | Label | Msgs | Tokens | Channel | Updated | |\n");
                    text.push_str(
                        "|-------------|-------|------|--------|---------|---------|---|\n",
                    );
                    for s in &resp.sessions {
                        let marker = if current.as_deref() == Some(s.id.as_str()) {
                            "current"
                        } else {
                            ""
                        };
                        let label = s.label.as_deref().unwrap_or("—");
                        let ago = s
                            .updated_at
                            .as_deref()
                            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                            .map(|dt| format_time_ago(dt.with_timezone(&chrono::Utc)))
                            .unwrap_or_else(|| "—".to_string());
                        text.push_str(&format!(
                            "| {} | {} | {} | {} | {} | {} | {} |\n",
                            &s.id[..s.id.len().min(12)],
                            label,
                            s.messages,
                            s.total_tokens,
                            s.channel.as_deref().unwrap_or("?"),
                            ago,
                            marker,
                        ));
                    }
                    app.push_message(app::MessageRole::System, text);
                }
            }
            Err(e) => {
                app.push_message(
                    app::MessageRole::System,
                    format!("Failed to list sessions: {}", e),
                );
            }
        },
        "/switch" => {
            if let Some(target) = args {
                // Resolve short/prefix session IDs to full UUIDs
                let full_id = if target.len() < 32 {
                    match rest_client.sessions().await {
                        Ok(resp) => {
                            let matches: Vec<_> = resp
                                .sessions
                                .iter()
                                .filter(|s| s.id.starts_with(target))
                                .collect();
                            match matches.len() {
                                1 => matches[0].id.clone(),
                                0 => {
                                    app.push_message(
                                        app::MessageRole::System,
                                        format!("No session matching `{}`", target),
                                    );
                                    return;
                                }
                                n => {
                                    app.push_message(
                                        app::MessageRole::System,
                                        format!("Ambiguous: {} sessions match `{}`", n, target,),
                                    );
                                    return;
                                }
                            }
                        }
                        Err(_) => target.to_string(),
                    }
                } else {
                    target.to_string()
                };

                let c = client.lock().await;
                c.set_session_id(&full_id).await;
                app.session_id = Some(full_id.clone());
                app.messages.clear();

                // Load session history from the server
                match rest_client.transcript(&full_id).await {
                    Ok(resp) => {
                        app.load_transcript(&resp.entries);
                        app.push_message(
                            app::MessageRole::System,
                            format!(
                                "Switched to session {} ({} messages loaded)",
                                &full_id[..full_id.len().min(12)],
                                app.message_count,
                            ),
                        );
                    }
                    Err(e) => {
                        app.push_message(
                            app::MessageRole::System,
                            format!(
                                "Switched to session {} (history unavailable: {})",
                                &full_id[..full_id.len().min(12)],
                                e,
                            ),
                        );
                    }
                }
            } else {
                app.push_message(
                    app::MessageRole::System,
                    "Usage: `/switch <session-id>`".to_string(),
                );
            }
        }
        "/mode" => {
            let sid = {
                let c = client.lock().await;
                c.session_id().await
            };
            let agent = app.agent_id.as_deref().unwrap_or("default").to_string();
            app.push_message(
                app::MessageRole::System,
                format!(
                    "**Mode:** gateway  \n**URL:** {}  \n**Agent:** @{}  \n**Model:** {}  \n**Session:** `{}`",
                    app.gateway_url,
                    agent,
                    app.model,
                    sid.as_deref().unwrap_or("(none)")
                ),
            );
        }
        "/attach" => {
            if let Some(path_str) = args {
                let path = PathBuf::from(path_str.trim_matches(|c| c == '\'' || c == '"'));
                handle_attach_file(app, &path);
            } else {
                app.push_message(
                    app::MessageRole::System,
                    "Usage: `/attach <path>`".to_string(),
                );
            }
        }
        "/agents" => match rest_client.agents().await {
            Ok(resp) => {
                // Update agent cache for tab completion
                app.cached_agents = resp
                    .agents
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect();

                if resp.agents.is_empty() {
                    app.push_message(app::MessageRole::System, "No agents configured".to_string());
                } else {
                    let current = app.agent_id.as_deref().unwrap_or("default");
                    let mut text = String::from("| Agent | Name | Model | |\n");
                    text.push_str("|-------|------|-------|---------|\n");
                    for a in &resp.agents {
                        let marker = if a.default || a.id == current {
                            if a.default && a.id == current {
                                "current, default"
                            } else if a.id == current {
                                "current"
                            } else {
                                "default"
                            }
                        } else {
                            ""
                        };
                        text.push_str(&format!(
                            "| {} | {} | {} | {} |\n",
                            a.id,
                            a.name.as_deref().unwrap_or("—"),
                            a.model.as_deref().unwrap_or("—"),
                            marker,
                        ));
                    }
                    app.push_message(app::MessageRole::System, text);
                }
            }
            Err(e) => {
                app.push_message(
                    app::MessageRole::System,
                    format!("Failed to list agents: {}", e),
                );
            }
        },
        "/agent" => {
            if let Some(target) = args {
                let target = target.trim();
                // Validate against cached agents if available
                if !app.cached_agents.is_empty()
                    && !app.cached_agents.iter().any(|(id, _)| id == target)
                {
                    app.push_message(
                        app::MessageRole::System,
                        format!(
                            "Unknown agent `{}`. Use `/agents` to list available agents.",
                            target
                        ),
                    );
                    return;
                }
                let mut c = client.lock().await;
                c.set_agent_id(target);
                app.agent_id = Some(target.to_string());
                app.push_message(
                    app::MessageRole::System,
                    format!("Switched to agent @{}", target),
                );
            } else {
                app.push_message(
                    app::MessageRole::System,
                    "Usage: `/agent <id>` — switch to a different agent. Use `/agents` to list."
                        .to_string(),
                );
            }
        }
        // Everything else → forward to gateway as a regular chat message
        _ => {
            app.add_user_message(cmd);
            let c = client.lock().await;
            if let Err(e) = c.send_chat(cmd).await {
                app.push_message(app::MessageRole::System, format!("Failed to send: {}", e));
                app.waiting_for_response = false;
            }
        }
    }
}

/// Validate and add a file to the pending attachments list.
fn handle_attach_file(app: &mut App, path: &std::path::Path) {
    if !path.exists() {
        app.push_message(
            app::MessageRole::System,
            format!("File not found: {}", path.display()),
        );
        return;
    }
    if !path.is_file() {
        app.push_message(
            app::MessageRole::System,
            format!("Not a file: {}", path.display()),
        );
        return;
    }

    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            app.push_message(app::MessageRole::System, format!("Cannot read file: {}", e));
            return;
        }
    };

    let size = metadata.len();
    let kind = classify_file(path);

    let max_size = match kind {
        AttachmentKind::Image => 5 * 1024 * 1024,   // 5 MB
        AttachmentKind::Text => 1024 * 1024,        // 1 MB
        AttachmentKind::Binary => 20 * 1024 * 1024, // 20 MB
    };
    if size > max_size {
        app.push_message(
            app::MessageRole::System,
            format!(
                "File too large: {} (max {})",
                format_size(size),
                format_size(max_size),
            ),
        );
        return;
    }

    let filename = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    app.pending_attachments.push(PendingAttachment {
        path: path.to_path_buf(),
        filename: filename.clone(),
        kind,
        size,
    });
    app.push_message(
        app::MessageRole::System,
        format!("Attached: {} ({})", filename, format_size(size)),
    );
}

/// Read all pending attachments, encode them, and return protocol Attachments.
/// Clears `app.pending_attachments` on success.
fn read_pending_attachments(app: &mut App) -> (Vec<Attachment>, Vec<String>) {
    if app.pending_attachments.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut attachments = Vec::new();
    let mut labels = Vec::new();
    let pending: Vec<PendingAttachment> = app.pending_attachments.drain(..).collect();

    for att in pending {
        let result = match att.kind {
            AttachmentKind::Text => std::fs::read_to_string(&att.path).map(|content| Attachment {
                filename: att.filename.clone(),
                mime_type: "text/plain".to_string(),
                data: content,
                ..Attachment::default()
            }),
            AttachmentKind::Image | AttachmentKind::Binary => {
                use base64::Engine;
                std::fs::read(&att.path).map(|bytes| {
                    let mime = mime_for_file(&att.path);
                    Attachment {
                        filename: att.filename.clone(),
                        mime_type: mime.to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        ..Attachment::default()
                    }
                })
            }
        };

        match result {
            Ok(attachment) => {
                labels.push(format!(
                    "[attached: {} ({})]",
                    att.filename,
                    format_size(att.size)
                ));
                attachments.push(attachment);
            }
            Err(e) => {
                app.push_message(
                    app::MessageRole::System,
                    format!("Failed to read {}: {}", att.filename, e),
                );
            }
        }
    }

    (attachments, labels)
}

/// Populate the agent cache for tab completion. Silently ignores errors.
async fn populate_agent_cache(app: &mut App, rest_client: &GatewayRestClient) {
    if let Ok(resp) = rest_client.agents().await {
        app.cached_agents = resp
            .agents
            .iter()
            .map(|a| (a.id.clone(), a.name.clone()))
            .collect();
    }
}

/// Populate the session cache for tab completion. Silently ignores errors.
async fn populate_session_cache(app: &mut App, rest_client: &GatewayRestClient) {
    if let Ok(resp) = rest_client.sessions().await {
        app.cached_sessions = resp
            .sessions
            .iter()
            .map(|s| (s.id.clone(), s.label.clone()))
            .collect();
    }
}

/// Format a timestamp as a human-readable relative time (e.g. "2m ago", "3h ago").
fn format_time_ago(dt: chrono::DateTime<chrono::Utc>) -> String {
    let delta = chrono::Utc::now().signed_duration_since(dt);

    if delta.num_seconds() < 60 {
        "just now".to_string()
    } else if delta.num_minutes() < 60 {
        format!("{}m ago", delta.num_minutes())
    } else if delta.num_hours() < 24 {
        format!("{}h ago", delta.num_hours())
    } else if delta.num_days() < 30 {
        format!("{}d ago", delta.num_days())
    } else {
        format!("{}mo ago", delta.num_days() / 30)
    }
}
