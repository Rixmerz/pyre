//! pyre-tui — ratatui terminal frontend for pyre.
//! Key bindings documented in `input/prefix.rs`; architecture in `ARCHITECTURE.md`.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Parser;

mod app;
mod clipboard;
mod fire_motion;
mod input;
mod model;
mod render;
mod rpc;
mod splash;
mod theme;

pub use app::restore_active_session;
use app::run::run_tui;
use rpc::{control_client, first_pane, resolve_pane, resolve_session};

// Re-exports consumed by submodules via `crate::`.
pub(crate) use app::pane_ops::{
    close_focused_pane, close_pane_by_slot_idx, focus_slot, open_new_session, open_new_tab,
    split_active,
};
pub(crate) use app::state::{AppState, PendingMenuAction};
pub(crate) use model::context_menu::{ContextMenu, MenuItem, MENU_ITEMS};
pub(crate) use model::layout::{
    build_pane_slot_map, children_at_mut, collect_leaf_rects, focus_next, focused_slot_idx,
    pane_leaves_in_order, pane_to_slot_idx, rect_contains,
};
pub(crate) use model::prompt::{NamePrompt, PromptKind};

/// How the TUI should initialise its first pane on startup.
pub(crate) enum PaneInit {
    Spawn,
    Existing {
        session: pyre_proto::SessionId,
        session_name: String,
        pane: pyre_proto::PaneId,
    },
}

#[derive(Parser, Debug)]
#[command(name = "pyre", version, about = "Pyre TUI — ratatui terminal frontend")]
struct Cli {
    /// Override socket path
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Shell to use when spawning (default: $SHELL)
    #[arg(long, global = true)]
    shell: Option<String>,
    /// Skip the startup flame animation
    #[arg(long, global = true)]
    no_splash: bool,
    #[command(subcommand)]
    command: Option<Sub>,
}

#[derive(clap::Subcommand, Debug)]
enum Sub {
    /// Attach to an existing session (and optionally a specific pane)
    Attach {
        /// Session id or ≥8-char prefix
        session: String,
        /// Pane id or ≥8-char prefix (default: first pane)
        #[arg(long)]
        pane: Option<String>,
    },
}

fn resolve_shell(shell_arg: Option<String>) -> Option<String> {
    shell_arg
        .or_else(|| std::env::var("SHELL").ok())
        .or_else(|| {
            for candidate in ["/bin/bash", "/bin/sh"] {
                if std::path::Path::new(candidate).exists() {
                    return Some(candidate.to_owned());
                }
            }
            None
        })
}

fn default_socket() -> PathBuf {
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    // SAFETY: getuid() is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/pyre-tui.log");
    match log_file {
        Ok(f) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::sync::Mutex::new(f))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .with_writer(std::io::stderr)
                .init();
        }
    }

    let cli = Cli::parse();
    let splash_colors = {
        let reg = pyre_themes::Registry::builtin();
        let name = pyre_themes::config::load_theme_name()
            .unwrap_or(None)
            .unwrap_or_else(|| pyre_themes::Registry::default_theme().to_owned());
        let theme = reg
            .get(&name)
            .or_else(|| reg.get(pyre_themes::Registry::default_theme()))
            .expect("ember always present");
        splash::SplashColors::from_palette(&theme.palette)
    };
    splash::play_splash(cli.no_splash, Some(splash_colors));
    let socket = cli.socket.unwrap_or_else(default_socket);
    let shell = resolve_shell(cli.shell);

    match cli.command {
        None => {
            let client = control_client(&socket).await?;
            let existing = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            let mut init = PaneInit::Spawn;
            for sess in existing {
                if sess.pane_count == 0 {
                    continue;
                }
                match first_pane(&client, sess.id).await {
                    Ok(pane) => {
                        init = PaneInit::Existing {
                            session: sess.id,
                            session_name: sess.name,
                            pane,
                        };
                        break;
                    }
                    Err(_) => continue,
                }
            }
            run_tui(socket, init, client, shell).await
        }
        Some(Sub::Attach {
            session: session_prefix,
            pane: pane_prefix,
        }) => {
            let client = control_client(&socket).await?;
            let sessions = client
                .list_sessions(tarpc::context::current())
                .await
                .context("rpc transport")?
                .map_err(|e| anyhow!("daemon list_sessions: {e}"))?;

            let session_id = resolve_session(&client, &session_prefix).await?;
            let session_name = sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| {
                    let short8: String = session_id.0.to_string().chars().take(8).collect();
                    format!("session-{short8}")
                });
            let pane = match pane_prefix {
                Some(ref prefix) => resolve_pane(&client, session_id, prefix).await?,
                None => first_pane(&client, session_id).await?,
            };
            run_tui(
                socket,
                PaneInit::Existing {
                    session: session_id,
                    session_name,
                    pane,
                },
                client,
                shell,
            )
            .await
        }
    }
}
