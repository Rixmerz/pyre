//! OSC 133 shell integration scripts shared between pyrec (CLI emission) and
//! pyred (auto-injection at pane spawn).
//!
//! Each constant is a complete, self-contained shell script that installs
//! OSC 133 precmd/preexec hooks so pyre can segment PTY output into blocks
//! with exit codes and command text.
//!
//! # Usage in a shell
//!
//! ```sh
//! # bash / zsh — add to ~/.bashrc / ~/.zshrc:
//! eval "$(pyrec shell-init bash)"
//!
//! # fish — add to ~/.config/fish/config.fish:
//! pyrec shell-init fish | source
//! ```
//!
//! # Auto-injection at spawn
//!
//! `pyred` writes `BASH_SCRIPT` to a temporary rcfile and spawns bash as
//! `bash --rcfile <path>`.  The rcfile first sources `~/.bashrc` to preserve
//! user configuration, then appends the integration script.  The idempotency
//! guard (`PYRE_SHELL_INTEGRATION=1`) prevents double-registration when the
//! user already sources the script from their own rc files.
//!
//! Opt-out: set `PYRE_NO_AUTO_INTEGRATION=1` in the daemon environment.

/// OSC 133 bash shell integration script.
///
/// Installs `__pyre_precmd` / `__pyre_preexec` hooks via `PROMPT_COMMAND` and
/// the `DEBUG` trap.  Includes an idempotency guard (`PYRE_SHELL_INTEGRATION`)
/// and a first-command guard (`PYRE_CMD_STARTED`) that prevents a stray `D`
/// marker on the very first prompt before any command has run.
///
/// # Sentinel design (bash-preexec / kitty pattern)
///
/// The core problem with a bare DEBUG trap is that bash fires it for *every*
/// simple command — including the `__pyre_prompt_cmd` function call itself and
/// every `printf` inside PROMPT_COMMAND hooks.  The `PYRE_AT_PROMPT` sentinel
/// gates the DEBUG trap so that only the *first* firing after the prompt is
/// drawn (i.e. the genuine user command) emits B+C markers:
///
/// 1. `__pyre_prompt_cmd` is appended LAST to PROMPT_COMMAND so all existing
///    hooks run first (with `PYRE_AT_PROMPT=0` still in effect, suppressing
///    any spurious DEBUG firings from those hooks).
/// 2. `__pyre_prompt_cmd` emits `D;$?` (if a command ran), emits `A`, then
///    sets `PYRE_AT_PROMPT=1` as its final step.
/// 3. The DEBUG trap handler returns immediately unless `PYRE_AT_PROMPT == 1`.
///    Extra guards filter out tab-completion (`COMP_LINE`) and subshells
///    (`BASH_SUBSHELL`).
/// 4. On the genuine user command: emit `C`, clear `PYRE_AT_PROMPT`, set
///    `PYRE_CMD_STARTED=1`.  Subsequent DEBUG firings for the same command
///    pipeline see `PYRE_AT_PROMPT=0` and are suppressed.
pub const BASH_SCRIPT: &str = r#"# pyre bash shell integration (OSC 133)
# Install: eval "$(pyrec shell-init bash)"
#
# Guards against double-installation.
if [ "${PYRE_SHELL_INTEGRATION:-0}" = "1" ]; then
  return 0 2>/dev/null || true
fi
export PYRE_SHELL_INTEGRATION=1

# PYRE_AT_PROMPT: set to 1 by __pyre_prompt_cmd after the prompt is drawn.
# The DEBUG trap only fires OSC 133 markers when this is 1 — i.e. only for
# the genuine user command, not for any commands executed during
# PROMPT_COMMAND (which run with the sentinel still 0).
PYRE_AT_PROMPT=0

# PYRE_CMD_STARTED: set to 1 after a C marker is emitted.
# Prevents emitting D before any command has run (first prompt guard).
PYRE_CMD_STARTED=0

__pyre_prompt_cmd() {
  local __pyre_exit=$?
  # Emit D;<exit> only if a prior command actually ran.
  if [ "$PYRE_CMD_STARTED" = "1" ]; then
    printf '\033]133;D;%s\007' "$__pyre_exit"
    PYRE_CMD_STARTED=0
  fi
  # Emit A — PromptStart.  From this point, bytes typed to the terminal
  # accumulate in BlockMachine.cmd_buf until C is received.
  printf '\033]133;A\007'
  # Set the sentinel LAST so that:
  #   a) The DEBUG trap firing for the __pyre_prompt_cmd call itself sees 0
  #      and returns early (the call happens before the function body runs).
  #   b) Any earlier PROMPT_COMMAND entries that ran simple commands also
  #      saw 0 and were suppressed.
  PYRE_AT_PROMPT=1
}

__pyre_preexec() {
  # Tab-completion and subshells must not emit markers.
  [[ -n $COMP_LINE ]] && return 0
  [[ $BASH_SUBSHELL != 0 ]] && return 0
  # Only fire for the genuine user command (at-prompt sentinel).
  [[ $PYRE_AT_PROMPT != 1 ]] && return 0
  # Clear the sentinel immediately so subsequent simple commands in the
  # same pipeline (e.g. `cmd1; cmd2`) do not emit duplicate C markers.
  PYRE_AT_PROMPT=0
  # Emit C — CommandStart.  BlockMachine drains cmd_buf as the command text.
  printf '\033]133;C\007'
  PYRE_CMD_STARTED=1
}

# Append __pyre_prompt_cmd LAST so all existing PROMPT_COMMAND entries run
# first (with PYRE_AT_PROMPT=0 still in effect, suppressing any spurious
# DEBUG firings from those hooks).
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" =~ "declare -a" ]]; then
  PROMPT_COMMAND+=(__pyre_prompt_cmd)
else
  PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND; }__pyre_prompt_cmd"
fi

# Register preexec via the DEBUG trap.
trap '__pyre_preexec' DEBUG
"#;

/// OSC 133 zsh shell integration script.
///
/// Uses `add-zsh-hook` to register `__pyre_precmd` (precmd) and
/// `__pyre_preexec` (preexec).  Includes the same idempotency and
/// first-command guards as the bash variant.
pub const ZSH_SCRIPT: &str = r#"# pyre zsh shell integration (OSC 133)
# Install: eval "$(pyrec shell-init zsh)"
#
# Guards against double-installation.
if [[ "${PYRE_SHELL_INTEGRATION:-0}" == "1" ]]; then
  return 0
fi
export PYRE_SHELL_INTEGRATION=1

# Tracks whether preexec fired since the last precmd.
typeset -g PYRE_CMD_STARTED=0

__pyre_precmd() {
  local __pyre_exit=$?
  # Emit D;<exit> only if a command was started.
  if [[ "$PYRE_CMD_STARTED" == "1" ]]; then
    printf '\033]133;D;%s\007' "$__pyre_exit"
    PYRE_CMD_STARTED=0
  fi
  # Emit A — PromptStart. Flushes output, starts command-text capture.
  printf '\033]133;A\007'
}

__pyre_preexec() {
  # Emit C — CommandStart. BlockMachine takes cmd_buf as the command text.
  printf '\033]133;C\007'
  PYRE_CMD_STARTED=1
}

# add-zsh-hook is the idiomatic zsh hook mechanism.
# It appends our function so existing hooks keep running.
autoload -Uz add-zsh-hook
add-zsh-hook precmd  __pyre_precmd
add-zsh-hook preexec __pyre_preexec
"#;

/// OSC 133 fish shell integration script.
///
/// Uses fish event hooks (`--on-event fish_prompt` and `--on-event fish_preexec`)
/// to register the pyre block-segmentation functions.
pub const FISH_SCRIPT: &str = r#"# pyre fish shell integration (OSC 133)
# Install: pyrec shell-init fish | source
#
# Guards against double-installation.
if set -q PYRE_SHELL_INTEGRATION
  exit 0
end
set -gx PYRE_SHELL_INTEGRATION 1

# Tracks whether a command started since the last fish_prompt event.
set -g __pyre_cmd_started 0

# fish_prompt fires just before the prompt is drawn (equivalent to precmd).
function __pyre_precmd --on-event fish_prompt
  set __pyre_exit $status
  if test "$__pyre_cmd_started" = "1"
    printf '\033]133;D;%s\007' "$__pyre_exit"
    set __pyre_cmd_started 0
  end
  # Emit A — PromptStart.
  printf '\033]133;A\007'
end

# fish_preexec fires after Enter, before the command runs.
function __pyre_preexec --on-event fish_preexec
  # Emit C — CommandStart.
  printf '\033]133;C\007'
  set __pyre_cmd_started 1
end
"#;

#[cfg(test)]
mod tests {
    use super::BASH_SCRIPT;

    /// Verify the bash sentinel produces the correct OSC 133 marker sequence.
    ///
    /// Expected sequence for `echo hi` with a system-style PROMPT_COMMAND:
    ///   A  (PromptStart, first prompt — no D because no prior command)
    ///   C  (CommandStart for the genuine user command)
    ///   D;0  (BlockEnd with exit code 0)
    ///   A  (PromptStart for next prompt — no spurious C from titleX or __pyre_prompt_cmd)
    ///
    /// Notably absent: any C emitted for the system `printf titleX` calls or
    /// for the `__pyre_prompt_cmd` function call itself.
    ///
    /// Skips gracefully when `/bin/bash` is not available.
    #[test]
    fn bash_sentinel_no_spurious_c_markers() {
        if !std::path::Path::new("/bin/bash").exists() {
            eprintln!("skip: /bin/bash not available");
            return;
        }

        // Write temporary files under /tmp — clean up manually at the end.
        let test_dir =
            std::path::PathBuf::from(format!("/tmp/pyre-shell-int-test-{}", std::process::id()));
        std::fs::create_dir_all(&test_dir).expect("mkdir test_dir");

        // Write BASH_SCRIPT to a helper file.
        let helper = test_dir.join("integration.sh");
        std::fs::write(&helper, BASH_SCRIPT).expect("write integration.sh");

        // Write an rcfile that:
        //   1. Installs a system-style PROMPT_COMMAND entry (simulates
        //      /etc/bash.bashrc title-setting printf) before the integration.
        //   2. Sources BASH_SCRIPT so the sentinel hooks are appended last.
        //   3. Sets PS1 to a plain string so no external commands are invoked.
        let rc_path = test_dir.join("test.rc");
        let rc_content = format!(
            "PROMPT_COMMAND=\"printf 'titleX'\"\n. {}\nPS1='$ '\n",
            helper.display()
        );
        std::fs::write(&rc_path, &rc_content).expect("write rc");

        // Run bash interactively, send one command, then exit.
        let output = std::process::Command::new("/bin/bash")
            .arg("--rcfile")
            .arg(&rc_path)
            .arg("-i")
            .env("HOME", &test_dir)
            .env("TERM", "dumb")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(b"echo hi\nexit\n")
                    .unwrap();
                child.wait_with_output()
            })
            .expect("run bash");

        let raw = output.stdout;

        // Collect OSC 133 sequences: \x1b]133;X\x07
        let mut markers: Vec<String> = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            if raw[i..].starts_with(b"\x1b]133;") {
                let start = i + 6; // skip \x1b]133;
                if let Some(end) = raw[start..].iter().position(|&b| b == 0x07) {
                    let seq = String::from_utf8_lossy(&raw[start..start + end]).into_owned();
                    markers.push(seq);
                    i = start + end + 1;
                    continue;
                }
            }
            i += 1;
        }

        // Cleanup before asserting so /tmp doesn't accumulate on failures.
        let _ = std::fs::remove_dir_all(&test_dir);

        // We expect at minimum: A, C, D;0, A
        // (The exit command also gets a C+D but we only assert the first four.)
        assert!(
            markers.len() >= 4,
            "expected at least 4 OSC 133 markers, got {}: {:?}",
            markers.len(),
            markers
        );
        assert_eq!(markers[0], "A", "first marker must be A (PromptStart)");
        assert_eq!(
            markers[1], "C",
            "second marker must be C (CommandStart for genuine user command)"
        );
        assert_eq!(
            markers[2], "D;0",
            "third marker must be D;0 (BlockEnd with exit code 0)"
        );
        assert_eq!(
            markers[3], "A",
            "fourth marker must be A (PromptStart for next prompt)"
        );

        // No C marker may appear before the first A — the sentinel must
        // suppress all DEBUG firings during the initial rcfile sourcing.
        let first_a = markers.iter().position(|m| m == "A").unwrap_or(0);
        let spurious_c_before_a = markers[..first_a].iter().any(|m| m == "C");
        assert!(
            !spurious_c_before_a,
            "spurious C marker appeared before the first A: {:?}",
            markers
        );
    }
}
