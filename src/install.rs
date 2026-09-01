//! Installing and removing the service.
//!
//! The binary is its own installer, so getting rcmd running is downloading
//! one file and running it once. There is no package to build, no playbook
//! to keep in sync, and nothing that can drift from what the binary
//! actually expects, because the unit file is compiled into the binary that
//! the unit file starts.
//!
//! The one thing this gets right that a hand written unit usually does not
//! is `KillMode=process`. Servers run in tmux sessions that end up inside
//! the service's cgroup, and systemd's default kills the whole cgroup, so
//! stopping the supervisor takes every server down with it. That is not
//! theoretical: it is exactly what the daemon rcmd replaces did, and
//! nobody noticed for years because nobody ever stopped it.

use std::path::Path;

/// The unit, with the bits that vary filled in.
///
/// Kept here rather than read from disk so that `rcmd install` works from a
/// single downloaded binary with nothing next to it.
pub fn unit_file(user: &str, binary: &Path, config: &Path) -> String {
    format!(
        r#"[Unit]
Description=RedCraft Manager Daemon
Documentation=https://github.com/redcraft-org/RedCraftManagerDaemon
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User={user}
Group={user}
ExecStart={binary} --config {config} serve
Restart=on-failure
RestartSec=5

# Secrets go in here, not in the config file. Systemd reads this as root
# before dropping to {user}, so it can stay root only.
EnvironmentFile=-/etc/rcmd/env

StateDirectory=rcmd
ConfigurationDirectory=rcmd

# The important line. Servers live in tmux sessions that end up inside this
# unit's cgroup, and the systemd default of control-group would send them
# all a SIGTERM when rcmd stops. With process, stopping rcmd stops rcmd, and
# every server keeps running and gets adopted again when it comes back.
KillMode=process
TimeoutStopSec=30

NoNewPrivileges=true
ProtectSystem=full
ProtectHome=read-only
# The tmux socket lives in /tmp. A private one means rcmd sees no sessions,
# concludes every server has crashed, and starts overwriting them.
PrivateTmp=false

[Install]
WantedBy=multi-user.target
"#,
        binary = binary.display(),
        config = config.display(),
    )
}

pub const UNIT_PATH: &str = "/etc/systemd/system/rcmd.service";

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("this needs to run as root, try sudo")]
    NotRoot,
    #[error("{0}")]
    Failed(String),
}

/// Who the service should run as.
///
/// Defaults to whoever owns the servers directory, because that is the user
/// whose tmux sessions the servers live in. Running as anybody else means
/// rcmd cannot see them, decides they have all crashed, and applies a fresh
/// template over each one. Guessing "root" here would be a data loss bug
/// with a friendly name.
pub fn default_user(servers_directory: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let uid = std::fs::metadata(servers_directory).ok()?.uid();
    user_name_for(uid)
}

/// Looks a uid up in /etc/passwd, so this works without a libc crate.
fn user_name_for(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd_lookup(&passwd, uid)
}

fn passwd_lookup(passwd: &str, uid: u32) -> Option<String> {
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next();
        let found: u32 = fields.next()?.parse().ok()?;
        if found == uid {
            return Some(name.to_string());
        }
    }
    None
}

fn require_root() -> Result<(), InstallError> {
    // No libc dependency for one number: /proc/self/status has it.
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let euid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|uid| uid.parse::<u32>().ok());
    match euid {
        Some(0) | None => Ok(()),
        Some(_) => Err(InstallError::NotRoot),
    }
}

fn systemctl(args: &[&str]) -> Result<(), InstallError> {
    let output = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|e| InstallError::Failed(format!("could not run systemctl: {e}")))?;
    if !output.status.success() {
        return Err(InstallError::Failed(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Writes the unit, puts the binary where the unit expects it, and reloads.
pub fn install(
    config_path: &Path,
    servers_directory: &Path,
    user: Option<String>,
    binary_target: &Path,
    start: bool,
) -> Result<(), InstallError> {
    require_root()?;

    let user = match user {
        Some(user) => user,
        None => default_user(servers_directory).ok_or_else(|| {
            InstallError::Failed(format!(
                "could not work out who owns {}, so pass --user",
                servers_directory.display()
            ))
        })?,
    };
    println!("Service user: {user}");

    // Copy ourselves into place, unless we are already there.
    let running = std::env::current_exe()
        .map_err(|e| InstallError::Failed(format!("could not find my own path: {e}")))?;
    if running != binary_target {
        if let Some(parent) = binary_target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| InstallError::Failed(e.to_string()))?;
        }
        // Write beside and rename: replacing a running binary in place is
        // how you get "text file busy" or a half written file.
        let temporary = binary_target.with_extension("installing");
        std::fs::copy(&running, &temporary)
            .map_err(|e| InstallError::Failed(format!("could not copy the binary: {e}")))?;
        set_mode(&temporary, 0o755)?;
        std::fs::rename(&temporary, binary_target)
            .map_err(|e| InstallError::Failed(format!("could not put the binary in place: {e}")))?;
        println!("Installed the binary at {}", binary_target.display());
    } else {
        println!("Already running from {}", binary_target.display());
    }

    std::fs::create_dir_all("/etc/rcmd").map_err(|e| InstallError::Failed(e.to_string()))?;

    // Never clobber a config somebody has edited.
    if config_path.exists() {
        println!("Keeping the config already at {}", config_path.display());
    } else {
        println!(
            "No config at {}, run `rcmd init` to write one",
            config_path.display()
        );
    }

    std::fs::write(UNIT_PATH, unit_file(&user, binary_target, config_path))
        .map_err(|e| InstallError::Failed(format!("could not write {UNIT_PATH}: {e}")))?;
    set_mode(Path::new(UNIT_PATH), 0o644)?;
    println!("Wrote {UNIT_PATH}");

    systemctl(&["daemon-reload"])?;
    println!("Reloaded systemd");

    if start {
        systemctl(&["enable", "--now", "rcmd"])?;
        println!("Enabled and started rcmd");
    } else {
        println!("Not started. `systemctl enable --now rcmd` when you are ready.");
    }
    Ok(())
}

/// Stops the service and takes the unit away.
///
/// Leaves the config, the database and the binary alone unless asked,
/// because "uninstall" should not be a synonym for "delete my audit log".
pub fn uninstall(purge: bool, binary_target: &Path) -> Result<(), InstallError> {
    require_root()?;

    if Path::new(UNIT_PATH).exists() {
        // Failing here is fine: it may already be stopped or disabled.
        let _ = systemctl(&["disable", "--now", "rcmd"]);
        println!("Stopped and disabled rcmd");
        println!("Your servers are still running: they are in tmux, not in the service.");
        std::fs::remove_file(UNIT_PATH)
            .map_err(|e| InstallError::Failed(format!("could not remove {UNIT_PATH}: {e}")))?;
        println!("Removed {UNIT_PATH}");
        systemctl(&["daemon-reload"])?;
        println!("Reloaded systemd");
    } else {
        println!("No unit at {UNIT_PATH}, nothing to stop");
    }

    if purge {
        for path in ["/etc/rcmd", "/var/lib/rcmd"] {
            match std::fs::remove_dir_all(path) {
                Ok(()) => println!("Deleted {path}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => println!("Could not delete {path}: {e}"),
            }
        }
        if binary_target.exists() {
            match std::fs::remove_file(binary_target) {
                Ok(()) => println!("Deleted {}", binary_target.display()),
                Err(e) => println!("Could not delete {}: {e}", binary_target.display()),
            }
        }
    } else {
        println!("Kept /etc/rcmd and /var/lib/rcmd. Pass --purge to delete them too.");
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| InstallError::Failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_never_ships_without_the_two_lines_that_matter() {
        // KillMode=process keeps the fleet alive when rcmd stops, and
        // PrivateTmp=false is how rcmd sees the tmux socket at all. Losing
        // either is a fleet outage or a mass template overwrite, so they get
        // a test rather than a comment.
        let unit = unit_file(
            "minecraft",
            Path::new("/usr/local/bin/rcmd"),
            Path::new("/etc/rcmd/config.toml"),
        );
        assert!(unit.contains("\nKillMode=process\n"));
        assert!(unit.contains("\nPrivateTmp=false\n"));
        assert!(!unit.contains("KillMode=control-group"));
        assert!(!unit.contains("PrivateTmp=true"));
    }

    #[test]
    fn the_unit_runs_the_binary_and_config_it_was_told_about() {
        let unit = unit_file(
            "mc",
            Path::new("/opt/rcmd/rcmd"),
            Path::new("/opt/rcmd/config.toml"),
        );
        assert!(unit.contains("ExecStart=/opt/rcmd/rcmd --config /opt/rcmd/config.toml serve"));
        assert!(unit.contains("User=mc"));
        assert!(unit.contains("Group=mc"));
    }

    #[test]
    fn a_uid_resolves_to_the_name_in_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      minecraft:x:1000:1000::/home/minecraft:/bin/sh\n";
        assert_eq!(passwd_lookup(passwd, 1000).as_deref(), Some("minecraft"));
        assert_eq!(passwd_lookup(passwd, 0).as_deref(), Some("root"));
        assert_eq!(passwd_lookup(passwd, 4242), None);
    }

    #[test]
    fn a_mangled_passwd_line_does_not_hand_back_the_wrong_user() {
        // Getting this wrong picks the user rcmd runs as, and the wrong one
        // cannot see the tmux sessions.
        let passwd = "broken\nminecraft:x:notanumber:1000::/home/mc:/bin/sh\n";
        assert_eq!(passwd_lookup(passwd, 1000), None);
    }
}
