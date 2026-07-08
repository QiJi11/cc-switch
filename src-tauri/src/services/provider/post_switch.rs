use crate::app_config::AppType;

#[cfg(windows)]
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
const RUNNER_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(windows)]
const WARNING_PREFIX: &str = "codex_post_switch_sync_failed";

pub(crate) fn run_after_switch(app_type: &AppType) -> Option<String> {
    if !matches!(app_type, AppType::Codex) {
        return None;
    }

    run_codex_current_sync()
}

#[cfg(not(windows))]
fn run_codex_current_sync() -> Option<String> {
    None
}

#[cfg(windows)]
fn run_codex_current_sync() -> Option<String> {
    let user_profile = user_profile_dir()?;
    let script_path = current_sync_script_path(&user_profile);
    if !script_path.is_file() {
        return None;
    }

    match run_with_shell("pwsh", &script_path) {
        Ok(()) => None,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            run_with_shell("powershell.exe", &script_path)
                .err()
                .map(warning_message)
        }
        Err(err) => Some(warning_message(err)),
    }
}

#[cfg(windows)]
fn run_with_shell(shell: &str, script_path: &Path) -> io::Result<()> {
    let mut child = Command::new(shell)
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script_path)
        .arg("-Quiet")
        .env("PYTHONUTF8", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let status = wait_for_runner(&mut child, RUNNER_TIMEOUT)?;
    if status.success() {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "current sync runner exited with {status}"
    )))
}

#[cfg(windows)]
fn wait_for_runner(child: &mut Child, timeout: Duration) -> io::Result<ExitStatus> {
    let started_at = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if started_at.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "current sync runner timed out",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn warning_message(error: io::Error) -> String {
    match error.kind() {
        io::ErrorKind::TimedOut => format!("{WARNING_PREFIX}:timeout"),
        io::ErrorKind::NotFound => format!("{WARNING_PREFIX}:powershell_not_found"),
        _ => format!("{WARNING_PREFIX}:{error}"),
    }
}

#[cfg(windows)]
fn user_profile_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn current_sync_script_path(user_profile: &Path) -> PathBuf {
    user_profile
        .join("Scripts")
        .join("codex-app-shell")
        .join("repair-fast.ps1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    use serial_test::serial;
    #[cfg(windows)]
    use std::ffi::OsString;
    #[cfg(windows)]
    use std::{env, fs};
    #[cfg(windows)]
    use tempfile::TempDir;

    #[cfg(windows)]
    struct UserProfileScope {
        _temp: TempDir,
        previous_userprofile: Option<OsString>,
    }

    #[cfg(windows)]
    impl UserProfileScope {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temp user profile");
            let previous_userprofile = env::var_os("USERPROFILE");
            env::set_var("USERPROFILE", temp.path());

            Self {
                _temp: temp,
                previous_userprofile,
            }
        }

        fn path(&self) -> &Path {
            self._temp.path()
        }
    }

    #[cfg(windows)]
    impl Drop for UserProfileScope {
        fn drop(&mut self) {
            match &self.previous_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }
        }
    }

    #[cfg(windows)]
    fn write_current_sync_script(home: &Path, body: &str) {
        let script_path = current_sync_script_path(home);
        fs::create_dir_all(script_path.parent().expect("script parent"))
            .expect("create script dir");
        fs::write(script_path, body).expect("write current sync script");
    }

    #[test]
    fn non_codex_switch_skips_runner() {
        assert!(run_after_switch(&AppType::OpenCode).is_none());
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn missing_current_sync_script_is_noop() {
        let _profile = UserProfileScope::new();

        assert!(run_after_switch(&AppType::Codex).is_none());
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn current_sync_script_success_returns_no_warning() {
        let profile = UserProfileScope::new();
        let marker_path = profile.path().join("runner-marker.txt");
        write_current_sync_script(
            profile.path(),
            &format!(
                "param([switch]$Quiet)\nSet-Content -LiteralPath '{}' -Value $Quiet.IsPresent\nexit 0\n",
                marker_path.display()
            ),
        );

        assert!(run_after_switch(&AppType::Codex).is_none());
        let marker = fs::read_to_string(marker_path).expect("read marker");
        assert!(marker.contains("True"));
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn current_sync_script_failure_returns_warning() {
        let profile = UserProfileScope::new();
        write_current_sync_script(profile.path(), "param([switch]$Quiet)\nexit 7\n");

        let warning = run_after_switch(&AppType::Codex).expect("warning");
        assert!(warning.starts_with(WARNING_PREFIX));
    }
}
