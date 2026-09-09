//! Opens a new terminal window running `ssh <host>`, so the user can jump
//! into a device's CLI (e.g. to fix a misconfigured community string or
//! SNMPv3 credentials) without leaving the app to find a terminal themselves.
//!
//! There's no cross-platform API for "open a terminal running this command",
//! so each platform gets its own strategy. Arguments are always passed as
//! separate `Command` args, never interpolated into a shell string, so a
//! host address containing shell metacharacters can't do anything beyond
//! being (harmlessly) rejected by `ssh` itself as a bad hostname.

use std::process::Command;

pub fn open(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("no host address given".to_string());
    }

    #[cfg(target_os = "macos")]
    {
        // Terminal.app understands ssh:// URLs and opens a new window already
        // running `ssh <host>`. Target it explicitly with `-a Terminal`
        // rather than leaving the scheme to LaunchServices' default-handler
        // resolution: any other ssh-capable app ever installed (iTerm2,
        // electerm, ...) can also register for `ssh:`, and when several
        // apps claim it with equal rank, `open ssh://host` alone can pick
        // one arbitrarily and silently do nothing if it's stale/uninstalled.
        return Command::new("open")
            .args(["-a", "Terminal", &format!("ssh://{host}")])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string());
    }

    #[cfg(target_os = "windows")]
    {
        // Windows has no registered ssh:// handler, but 10 (1809+) and 11 ship
        // OpenSSH's `ssh.exe` in the box. Spawn it directly with its own new
        // console window rather than going through `cmd.exe /C`, which would
        // let shell metacharacters in `host` be reinterpreted by cmd.
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        return Command::new("ssh").arg(host).creation_flags(CREATE_NEW_CONSOLE).spawn().map(|_| ()).map_err(|e| e.to_string());
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux/BSD have no standard terminal or ssh-URL handler, so try a
        // handful of common terminal emulators in turn and go with whichever
        // one actually exists.
        const CANDIDATES: &[(&str, &[&str])] = &[
            ("x-terminal-emulator", &["-e", "ssh"]),
            ("gnome-terminal", &["--", "ssh"]),
            ("konsole", &["-e", "ssh"]),
            ("xfce4-terminal", &["-x", "ssh"]),
            ("xterm", &["-e", "ssh"]),
        ];
        for (cmd, args) in CANDIDATES {
            if Command::new(cmd).args(*args).arg(host).spawn().is_ok() {
                return Ok(());
            }
        }
        return Err(
            "no supported terminal emulator found (tried x-terminal-emulator, gnome-terminal, konsole, xfce4-terminal, xterm)"
                .to_string(),
        );
    }
}
