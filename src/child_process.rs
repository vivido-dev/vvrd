//! Helpers never inherit the terminal session's capability material.
use std::process::{Child, Command};

pub fn scrub(command: &mut Command) -> &mut Command {
    let explicit: Vec<_> = command
        .get_envs()
        .map(|(name, _)| name.to_owned())
        .collect();
    for name in explicit
        .into_iter()
        .chain(std::env::vars_os().map(|(name, _)| name))
    {
        if name.to_string_lossy().starts_with("VIVID_") {
            command.env_remove(name);
        }
    }
    command
}

/// Give supervised workers their own process group, including conversion descendants.
pub fn isolate(command: &mut Command) -> &mut Command {
    scrub(command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

pub fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill accepts a process group ID; the child was spawned with process_group(0).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = scrub(Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_capability_names_are_removed() {
        let mut command = Command::new("unused");
        command
            .env("VIVID_ROOT_SECRET", "test-sentinel")
            .env("VIVID_FUTURE_CAPABILITY", "test-sentinel");
        scrub(&mut command);
        assert!(command.get_envs().all(|(key, value)| !key.to_string_lossy().starts_with("VIVID_") || value.is_none()));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("VIVID_") {
                assert!(
                    command
                        .get_envs()
                        .any(|(key, value)| key == name && value.is_none())
                );
            }
        }
    }
}
