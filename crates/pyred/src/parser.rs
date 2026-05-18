//! VTE-driven block parser. Watches PTY output for OSC 133 (FinalTerm/Wezterm/
//! Kitty shell-integration markers) and emits `BlockEvent`s.
//!
//! Markers:
//!   ESC ] 133 ; A BEL          -> Prompt start
//!   ESC ] 133 ; B BEL          -> Command start (some shells; treat as A-end / pre-command)
//!   ESC ] 133 ; C BEL          -> Output start
//!   ESC ] 133 ; D ; <exit> BEL -> Block end

use bytes::Bytes;
use pyre_proto::{BlockEvent, BlockId, SessionId};
use uuid::Uuid;

// alacritty_terminal 0.26 re-exports vte 0.15 directly.
use alacritty_terminal::vte;

const OUTPUT_FLUSH_BYTES: usize = 4096;

pub struct BlockMachine {
    session: SessionId,
    current_block: Option<BlockId>,
    /// Buffer for the command text accumulated between A/B and C.
    cmd_buf: Vec<u8>,
    /// Are we currently capturing the command (between A/B and C)?
    capturing_cmd: bool,
    /// Buffer for the running output chunk between C and D.
    out_buf: Vec<u8>,
    /// Events drained into here on each feed() call.
    pending: Vec<BlockEvent>,
}

impl BlockMachine {
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            current_block: None,
            cmd_buf: Vec::new(),
            capturing_cmd: false,
            out_buf: Vec::with_capacity(OUTPUT_FLUSH_BYTES),
            pending: Vec::new(),
        }
    }

    pub fn flush_output(&mut self) {
        if let Some(block) = self.current_block {
            if !self.out_buf.is_empty() {
                let data = Bytes::copy_from_slice(&self.out_buf);
                self.out_buf.clear();
                self.pending.push(BlockEvent::OutputChunk { block, data });
            }
        }
    }

    fn push_byte(&mut self, b: u8) {
        if self.current_block.is_some() {
            self.out_buf.push(b);
            if self.out_buf.len() >= OUTPUT_FLUSH_BYTES || b == b'\n' {
                self.flush_output();
            }
        } else if self.capturing_cmd && b != 0 {
            self.cmd_buf.push(b);
        }
    }
}

impl vte::Perform for BlockMachine {
    fn print(&mut self, c: char) {
        let mut buf = [0u8; 4];
        for &b in c.encode_utf8(&mut buf).as_bytes() {
            self.push_byte(b);
        }
    }

    fn execute(&mut self, byte: u8) {
        self.push_byte(byte);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() || params[0] != b"133" {
            return;
        }
        let sub = params.get(1).copied().unwrap_or(b"");
        if sub.starts_with(b"A") {
            self.flush_output();
            self.cmd_buf.clear();
            self.capturing_cmd = true;
            self.current_block = None;
            self.pending.push(BlockEvent::PromptStart {
                session: self.session,
            });
        } else if sub.starts_with(b"B") {
            // Some shells emit B between A and C — keep capturing.
        } else if sub.starts_with(b"C") {
            let command = String::from_utf8_lossy(&self.cmd_buf)
                .trim_end()
                .to_string();
            self.cmd_buf.clear();
            self.capturing_cmd = false;
            let block = BlockId(Uuid::new_v4());
            self.current_block = Some(block);
            self.pending.push(BlockEvent::CommandStart {
                session: self.session,
                block,
                command,
                cwd: None,
            });
        } else if sub.starts_with(b"D") {
            self.flush_output();
            let exit_code = params
                .get(2)
                .and_then(|p| std::str::from_utf8(p).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
            if let Some(block) = self.current_block.take() {
                self.pending.push(BlockEvent::BlockEnd { block, exit_code });
            }
        }
    }
}

pub struct BlockParser {
    parser: vte::Parser,
    machine: BlockMachine,
}

impl BlockParser {
    pub fn new(session: SessionId) -> Self {
        Self {
            parser: vte::Parser::new(),
            machine: BlockMachine::new(session),
        }
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<BlockEvent>) {
        self.parser.advance(&mut self.machine, bytes);
        // Flush any partial output buffer so callers see pending chunks.
        self.machine.flush_output();
        out.append(&mut self.machine.pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyre_proto::SessionId;
    use uuid::Uuid;

    #[test]
    fn osc133_pwd_roundtrip() {
        let session = SessionId(Uuid::nil());
        let mut p = BlockParser::new(session);
        let mut out = Vec::new();
        p.feed(
            b"\x1b]133;A\x07pwd\x1b]133;C\x07/tmp\n\x1b]133;D;0\x07",
            &mut out,
        );
        // Expected sequence: PromptStart, CommandStart{command:"pwd"},
        // OutputChunk("/tmp\n"), BlockEnd{exit:0}.
        let mut iter = out.iter();
        assert!(
            matches!(iter.next(), Some(BlockEvent::PromptStart { .. })),
            "expected PromptStart"
        );
        match iter.next() {
            Some(BlockEvent::CommandStart { command, .. }) => {
                assert_eq!(command, "pwd");
            }
            other => panic!("expected CommandStart, got {other:?}"),
        }
        match iter.next() {
            Some(BlockEvent::OutputChunk { data, .. }) => {
                assert_eq!(data.as_ref(), b"/tmp\n");
            }
            other => panic!("expected OutputChunk, got {other:?}"),
        }
        match iter.next() {
            Some(BlockEvent::BlockEnd { exit_code, .. }) => {
                assert_eq!(*exit_code, Some(0));
            }
            other => panic!("expected BlockEnd, got {other:?}"),
        }
    }
}
