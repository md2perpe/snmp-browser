//! Opens a new terminal window running `ssh <host>`, so the user can jump
//! into a device's CLI (e.g. to fix a misconfigured community string or
//! SNMPv3 credentials) without leaving the app to find a terminal themselves.
//!
//! Two things matter for the target audience (lab/test network equipment
//! that gets reflashed and rotates its host key constantly):
//!   1. The window must survive `ssh` exiting, whatever the reason - a
//!      changed host key, a refused connection, a typo in the address - so
//!      there's time to actually read the error instead of it flashing by.
//!      That means running `ssh` inside an ordinary shell in the new window
//!      rather than as the window's direct child process.
//!   2. Host-key verification is turned off (`StrictHostKeyChecking=no`,
//!      throwaway `UserKnownHostsFile`). This is the actual "changed key"
//!      fix: without it, `ssh` refuses to connect at all once a device's key
//!      no longer matches what's on file. That's the right tradeoff for
//!      trusted internal equipment whose keys are *expected* to change, but
//!      it does mean no protection against a real man-in-the-middle - don't
//!      reuse this path for anything reached over an untrusted network.
//!
//! Where a shell has to parse the address (macOS's `do script`, the
//! Linux/BSD fallback), it's embedded via POSIX single-quoting, which is
//! immune to shell metacharacters regardless of what's typed into the
//! Address field. Windows instead validates the address against a safe
//! character set up front, since `cmd.exe`'s own command-line re-parsing
//! can't be neutralized by quoting alone.

use std::process::Command;

/// Single-quotes `s` for safe embedding in a POSIX shell command line -
/// immune to `$()`, backticks, `;`, `&&`, etc. regardless of `s`'s contents.
#[cfg(not(target_os = "windows"))]
fn posix_shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

pub fn open(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("no host address given".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        // `do script` runs the command inside a normal login shell in a new
        // Terminal window; unlike exec'ing `ssh` as the window's process
        // directly (e.g. via an `ssh://` URL), the shell - and the window -
        // stays open once `ssh` exits, letting the user actually read
        // whatever it printed. Escaped once for the shell (single-quoting)
        // and once more for AppleScript's string syntax.
        let ssh_cmd = format!("ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null {}", posix_shell_quote(host));
        let escaped_for_applescript = ssh_cmd.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!("tell application \"Terminal\" to do script \"{escaped_for_applescript}\"");
        return Command::new("osascript").arg("-e").arg(script).spawn().map(|_| ()).map_err(|e| e.to_string());
    }

    #[cfg(target_os = "windows")]
    {
        // Same reasoning as macOS: run through `cmd /K` (keeps the window
        // open after the command finishes) rather than spawning `ssh.exe`
        // as the console's direct process. That means `host` now flows
        // through cmd.exe's own command-line parsing, which quoting can't
        // fully neutralize - so restrict it to a safe character set instead
        // of trying to escape it.
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        if !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_' | '@')) {
            return Err(format!("'{host}' has characters that aren't safe to pass through cmd.exe - stick to a plain hostname, IP, or user@host"));
        }
        let cmd_line = format!("ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL {host}");
        return Command::new("cmd").args(["/K", &cmd_line]).creation_flags(CREATE_NEW_CONSOLE).spawn().map(|_| ()).map_err(|e| e.to_string());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux/BSD have no standard terminal or ssh-URL handler, so try a
        // handful of common terminal emulators in turn and go with whichever
        // one actually exists. Each is told to exec `sh -c '<ssh cmd>; ...'`
        // rather than `ssh` directly, again so the window survives `ssh`
        // exiting instead of closing the instant it does.
        let shell_cmd = format!(
            "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null {}; echo; echo '[ssh exited - press Enter to close]'; read _",
            posix_shell_quote(host)
        );
        const CANDIDATES: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-x"]),
            ("xterm", &["-e"]),
        ];
        for (cmd, args) in CANDIDATES {
            if Command::new(cmd).args(*args).args(["sh", "-c", &shell_cmd]).spawn().is_ok() {
                return Ok(());
            }
        }
        return Err(
            "no supported terminal emulator found (tried x-terminal-emulator, gnome-terminal, konsole, xfce4-terminal, xterm)"
                .to_string(),
        );
    }
}
