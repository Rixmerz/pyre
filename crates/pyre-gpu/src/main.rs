//! pyre-gpu — S6 GPU-backed terminal viewer (single-pane MVP).
//!
//! Connects to `pyred` over UDS, attaches to one pane stream, and renders the
//! VT grid via winit + softbuffer. Multiplexing parity with `pyre` TUI is
//! planned; this binary is the render-backend proof for ADR-003.

mod atlas;
mod paint;
mod search;
mod term;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use atlas::grid_dims_for_window;
use bytes::Bytes;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use paint::Painter;
use pyre_proto::{
    write_control_client, InputFrame, OutputFrame, PaneId, PyreDaemonClient, SessionId, SpawnReq,
    SpawnResp, MODE_STREAM,
};
use softbuffer::{Context as SbContext, Surface};
use tarpc::client;
use tarpc::tokio_serde::formats::Bincode;
use term::{collect_grid, TermView};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio_serde::formats::SymmetricalBincode;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing_subscriber::EnvFilter;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::ModifiersState;
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

#[derive(Parser, Debug)]
#[command(name = "pyre-gpu", version, about = "GPU terminal viewer for pyred")]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(long, global = true)]
    shell: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    pane: Option<String>,
}

fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("PYRE_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("pyre.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/pyre-{uid}.sock"))
}

async fn control_client(socket: &Path) -> Result<PyreDaemonClient> {
    let mut sock = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    write_control_client(&mut sock).await?;
    let transport = tarpc::serde_transport::new(
        tokio_util::codec::Framed::new(sock, LengthDelimitedCodec::new()),
        Bincode::default(),
    );
    Ok(PyreDaemonClient::new(client::Config::default(), transport).spawn())
}

async fn resolve_session(client: &PyreDaemonClient, prefix: &str) -> Result<SessionId> {
    let sessions = client
        .list_sessions(tarpc::context::current())
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    let matches: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.0.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no session matches '{prefix}'")),
        1 => Ok(matches[0].id),
        n => Err(anyhow!("{n} sessions match '{prefix}'")),
    }
}

async fn resolve_pane(
    client: &PyreDaemonClient,
    session: SessionId,
    prefix: &str,
) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    let matches: Vec<_> = panes
        .iter()
        .filter(|p| p.id.0.to_string().starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(anyhow!("no pane matches '{prefix}'")),
        1 => Ok(matches[0].id),
        n => Err(anyhow!("{n} panes match '{prefix}'")),
    }
}

async fn first_pane(client: &PyreDaemonClient, session: SessionId) -> Result<PaneId> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    panes
        .into_iter()
        .next()
        .map(|p| p.id)
        .ok_or_else(|| anyhow!("session has no panes"))
}

fn term_size() -> (u16, u16) {
    (120, 40)
}

struct App {
    output_rx: mpsc::UnboundedReceiver<Bytes>,
    input_tx: mpsc::UnboundedSender<Bytes>,
    term: Arc<Mutex<TermView>>,
    painter: Painter,
    control: PyreDaemonClient,
    search: search::SearchUi,
    window: Option<Arc<Window>>,
    surface: Option<Surface<Arc<Window>, Arc<Window>>>,
    cols: usize,
    rows: usize,
    needs_redraw: bool,
    session: SessionId,
    pane_ids: Vec<PaneId>,
    pane_index: usize,
    switch_tx: mpsc::UnboundedSender<(SessionId, PaneId)>,
    modifiers: ModifiersState,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let title = window_title(self.session, self.pane_ids.get(self.pane_index).copied());
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(1200u32, 800u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let ctx = SbContext::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&ctx, window.clone()).expect("softbuffer surface");
        self.window = Some(window);
        self.surface = Some(surface);
        self.needs_redraw = true;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::Resized(size) => {
                let (c, r) = grid_dims_for_window(size.width, size.height);
                self.cols = c;
                self.rows = r;
                if let Ok(mut tv) = self.term.lock() {
                    tv.resize(c, r);
                    tv.flush_pending();
                }
                self.needs_redraw = true;
            }
            WindowEvent::RedrawRequested => {
                while let Ok(chunk) = self.output_rx.try_recv() {
                    if let Ok(mut tv) = self.term.lock() {
                        tv.push_bytes(&chunk);
                    }
                }
                if let Ok(mut tv) = self.term.lock() {
                    tv.flush_pending();
                    for reply in tv.drain_pty_replies() {
                        let _ = self.input_tx.send(reply);
                    }
                }
                self.draw_frame();
                self.needs_redraw = false;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                if search_toggle_key(&event, self.modifiers) {
                    if self.search.open {
                        self.search.close();
                    } else {
                        self.search.open_overlay();
                    }
                    self.needs_redraw = true;
                    return;
                }
                if self.search.open {
                    let client = self.control.clone();
                    if self.search.handle_key(&event, client) {
                        let _ = self.search.tick_debounce(self.control.clone());
                        self.needs_redraw = true;
                    }
                    return;
                }
                if cycle_pane_key(&event, self.modifiers) {
                    self.cycle_pane(1);
                    return;
                }
                if cycle_pane_key_back(&event, self.modifiers) {
                    self.cycle_pane(-1);
                    return;
                }
                if let Some(text) = key_to_bytes(&event) {
                    let _ = self.input_tx.send(Bytes::from(text));
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if self.output_rx.try_recv().is_ok() {
            self.needs_redraw = true;
        }
        if self.search.poll_results() {
            self.needs_redraw = true;
        }
        if self.search.tick_debounce(self.control.clone()) {
            self.needs_redraw = true;
        }
        if self.needs_redraw {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }
}

impl App {
    fn cycle_pane(&mut self, delta: isize) {
        if self.pane_ids.len() <= 1 {
            return;
        }
        let n = self.pane_ids.len() as isize;
        let next = (self.pane_index as isize + delta).rem_euclid(n) as usize;
        self.pane_index = next;
        let pane = self.pane_ids[next];
        let _ = self.switch_tx.send((self.session, pane));
        if let Ok(mut tv) = self.term.lock() {
            *tv = TermView::new(self.cols, self.rows);
        }
        if let Some(w) = &self.window {
            w.set_title(&window_title(self.session, Some(pane)));
        }
        self.needs_redraw = true;
    }

    fn draw_frame(&mut self) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let cells = self
            .term
            .lock()
            .map(|tv| collect_grid(&tv, self.cols, self.rows))
            .unwrap_or_default();
        let width = self.cols * atlas::CELL_W;
        let height = self.rows * atlas::CELL_H;
        let mut buffer = vec![0u32; width * height];
        self.painter
            .paint(&cells, self.cols, self.rows, &mut buffer);
        self.search.paint_overlay(
            &mut self.painter.atlas,
            &mut buffer,
            width,
            height,
            self.cols,
            self.rows,
        );
        let Ok(mut sb) = surface.buffer_mut() else {
            return;
        };
        let sw = sb.width().get() as usize;
        let sh = sb.height().get() as usize;
        for y in 0..sh.min(height) {
            for x in 0..sw.min(width) {
                let idx = y * sw + x;
                let src = buffer[y * width + x];
                sb[idx] = src;
            }
        }
        let _ = sb.present();
    }
}

fn window_title(session: SessionId, pane: Option<PaneId>) -> String {
    let sess: String = session.0.to_string().chars().take(8).collect();
    match pane {
        Some(p) => {
            let pane: String = p.0.to_string().chars().take(8).collect();
            format!("pyre-gpu [{sess}/{pane}] — Ctrl+Tab panes, Ctrl+/ search")
        }
        None => format!("pyre-gpu [{sess}]"),
    }
}

fn cycle_pane_key(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
        && mods.contains(ModifiersState::CONTROL)
        && !mods.contains(ModifiersState::SHIFT)
}

fn search_toggle_key(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    if event.state != ElementState::Pressed || !mods.contains(ModifiersState::CONTROL) {
        return false;
    }
    matches!(event.physical_key, PhysicalKey::Code(KeyCode::Slash))
        || matches!(&event.logical_key, Key::Character(s) if s == "/")
}

fn cycle_pane_key_back(event: &KeyEvent, mods: ModifiersState) -> bool {
    use winit::keyboard::{KeyCode, PhysicalKey};
    event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
        && mods.contains(ModifiersState::CONTROL | ModifiersState::SHIFT)
}

fn key_to_bytes(event: &KeyEvent) -> Option<Vec<u8>> {
    use winit::keyboard::{KeyCode, PhysicalKey};
    match event.physical_key {
        PhysicalKey::Code(KeyCode::Enter) => return Some(b"\r".to_vec()),
        PhysicalKey::Code(KeyCode::Backspace) => return Some(vec![0x7f]),
        PhysicalKey::Code(KeyCode::Tab) => return Some(b"\t".to_vec()),
        PhysicalKey::Code(KeyCode::Escape) => return Some(b"\x1b".to_vec()),
        _ => {}
    }
    match &event.logical_key {
        Key::Character(s) => Some(s.as_bytes().to_vec()),
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
        _ => None,
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(default_socket);
    let painter = Painter::from_system().context("init painter")?;

    let client = control_client(&socket).await?;
    let (session, pane) = if let Some(ref sess_prefix) = cli.session {
        let session = resolve_session(&client, sess_prefix).await?;
        let pane = match &cli.pane {
            Some(p) => resolve_pane(&client, session, p).await?,
            None => first_pane(&client, session).await?,
        };
        (session, pane)
    } else {
        let existing = client
            .list_sessions(tarpc::context::current())
            .await
            .context("rpc")?
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(sess) = existing.into_iter().next() {
            if let Ok(pane) = first_pane(&client, sess.id).await {
                (sess.id, pane)
            } else {
                spawn_default(&client, cli.shell).await?
            }
        } else {
            spawn_default(&client, cli.shell).await?
        }
    };

    let pane_ids = list_session_panes(&client, session).await?;
    let pane_index = pane_ids.iter().position(|p| *p == pane).unwrap_or(0);

    let (output_tx, output_rx) = mpsc::unbounded_channel();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Bytes>();
    let input_tx_bridge = input_tx.clone();
    let (switch_tx, mut switch_rx) = mpsc::unbounded_channel::<(SessionId, PaneId)>();

    let stream_socket = socket.clone();
    tokio::spawn(async move {
        let mut cur_session = session;
        let mut cur_pane = pane;
        loop {
            let (cancel_tx, cancel_rx) = watch::channel(());
            let bridge = stream_bridge(
                stream_socket.clone(),
                cur_session,
                cur_pane,
                output_tx.clone(),
                &mut input_rx,
                cancel_rx,
            );
            tokio::select! {
                res = bridge => {
                    if let Err(e) = res {
                        tracing::error!("stream: {e:#}");
                    }
                }
                cmd = switch_rx.recv() => {
                    let Some((s, p)) = cmd else { break };
                    let _ = cancel_tx.send(());
                    cur_session = s;
                    cur_pane = p;
                }
            }
        }
    });

    let (cols, rows) = (80usize, 24usize);
    let term = Arc::new(Mutex::new(TermView::new(cols, rows)));

    let mut app = App {
        output_rx,
        input_tx: input_tx_bridge,
        term,
        painter,
        control: client,
        search: search::SearchUi::default(),
        window: None,
        surface: None,
        cols,
        rows,
        needs_redraw: true,
        session,
        pane_ids,
        pane_index,
        switch_tx,
        modifiers: ModifiersState::default(),
    };

    let event_loop = EventLoop::new().context("event loop")?;
    event_loop.run_app(&mut app).context("run_app")?;
    Ok(())
}

async fn spawn_default(
    client: &PyreDaemonClient,
    shell: Option<String>,
) -> Result<(SessionId, PaneId)> {
    let (cols, rows) = term_size();
    let req = SpawnReq {
        shell: shell.or_else(|| std::env::var("SHELL").ok()),
        cwd: std::env::current_dir().ok(),
        cols,
        rows,
        env: std::env::vars().collect(),
        name: None,
    };
    let SpawnResp { session, pane } = client
        .spawn(tarpc::context::current(), req)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    Ok((session, pane))
}

async fn list_session_panes(client: &PyreDaemonClient, session: SessionId) -> Result<Vec<PaneId>> {
    let panes = client
        .list_panes(tarpc::context::current(), session)
        .await
        .context("rpc")?
        .map_err(|e| anyhow!("{e}"))?;
    Ok(panes.into_iter().map(|p| p.id).collect())
}

async fn stream_bridge(
    socket: PathBuf,
    session: SessionId,
    pane: PaneId,
    output_tx: mpsc::UnboundedSender<Bytes>,
    input_rx: &mut mpsc::UnboundedReceiver<Bytes>,
    mut cancel: watch::Receiver<()>,
) -> Result<()> {
    let mut stream_sock = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect {}", socket.display()))?;
    stream_sock.write_all(&[MODE_STREAM]).await?;
    stream_sock.write_all(session.0.as_bytes()).await?;
    stream_sock.write_all(pane.0.as_bytes()).await?;

    let (rd, wr) = stream_sock.into_split();
    let mut output_frames = tokio_serde::SymmetricallyFramed::new(
        FramedRead::new(rd, LengthDelimitedCodec::new()),
        SymmetricalBincode::<OutputFrame>::default(),
    );
    let mut input_frames = tokio_serde::SymmetricallyFramed::new(
        FramedWrite::new(wr, LengthDelimitedCodec::new()),
        SymmetricalBincode::<InputFrame>::default(),
    );

    loop {
        tokio::select! {
            _ = cancel.changed() => return Ok(()),
            frame = output_frames.next() => {
                match frame {
                    Some(Ok(f)) => {
                        if !f.data.is_empty() {
                            let _ = output_tx.send(f.data);
                        }
                    }
                    Some(Err(e)) => return Err(anyhow!("output transport: {e}")),
                    None => break,
                }
            }
            inp = input_rx.recv() => {
                match inp {
                    Some(data) => {
                        input_frames
                            .send(InputFrame {
                                session,
                                data,
                            })
                            .await?;
                    }
                    None => break,
                }
            }
        }
    }
    Ok(())
}
