use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt;

const MAX_OUT: usize = 80_000;
const DEFAULT_SECS: u64 = 120;
const MAX_SECS: u64 = 1800;
#[cfg(target_os = "macos")]
const SEATBELT: &str = "/usr/bin/sandbox-exec";

/// Allow-default: GUI (`open`, Apple Events) works. Only secret paths stay closed.
#[cfg(target_os = "macos")]
const POLICY: &str = r#"(version 1)
(allow default)
(deny file-read* (subpath (param "SSH")))
(deny file-write* (subpath (param "SSH")))
(deny file-read* (subpath (param "GNUPG")))
(deny file-write* (subpath (param "GNUPG")))
(deny file-read* (subpath (param "AWS")))
(deny file-write* (subpath (param "AWS")))
(deny file-read* (subpath (param "NETRC")))
(deny file-write* (subpath (param "NETRC")))
(deny file-read* (subpath (param "DOTENV_APP")))
(deny file-write* (subpath (param "DOTENV_APP")))
(deny file-read* (subpath (param "DOTENV_WS")))
(deny file-write* (subpath (param "DOTENV_WS")))
"#;

#[derive(Default)]
pub struct LiveCmds {
    kids: Vec<std::process::Child>,
}

impl Drop for LiveCmds {
    fn drop(&mut self) {
        for child in &mut self.kids {
            kill_group(child);
            let _ = child.wait();
        }
    }
}

#[derive(Debug)]
pub struct RunResult {
    pub output: String,
}

pub fn timeout_secs(raw: Option<u64>) -> u64 {
    let Some(n) = raw else {
        return DEFAULT_SECS;
    };
    if n == 0 {
        return DEFAULT_SECS;
    }
    if n >= 1_000 {
        return (n / 1_000).clamp(1, MAX_SECS);
    }
    n.clamp(1, MAX_SECS)
}

pub fn clip_cmd(cmd: &str) -> String {
    let one = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 72 {
        return one;
    }
    let mut out = String::new();
    for c in one.chars() {
        if out.chars().count() >= 72 {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

pub fn run_in_workspace(
    root: &Path,
    command: &str,
    cwd_rel: &str,
    timeout: u64,
) -> Result<RunResult, String> {
    let mut live = LiveCmds::default();
    run_cmd(root, command, cwd_rel, timeout, false, &mut live)
}

pub fn run_cmd(
    root: &Path,
    command: &str,
    cwd_rel: &str,
    timeout: u64,
    background: bool,
    live: &mut LiveCmds,
) -> Result<RunResult, String> {
    let command = command.trim();
    if command.is_empty() {
        return Err("Empty command.".into());
    }
    if command.contains('\0') {
        return Err("Bad command.".into());
    }
    let cwd = crate::coder::jail(root, if cwd_rel.trim().is_empty() {
        "."
    } else {
        cwd_rel
    })?;
    if !cwd.is_dir() {
        return Err(format!(
            "{} is not a folder.",
            crate::coder::rel_of(root, &cwd)
        ));
    }
    let secs = timeout_secs(Some(timeout));
    let mut cmd = sandboxed(root, &cwd, command)?;
    cmd.current_dir(&cwd)
        .env_clear()
        .envs(safe_env())
        .stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if background {
        let log = std::env::temp_dir().join(format!(
            "puck-bg-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = std::fs::File::create(&log).map_err(|e| format!("Log: {e}"))?;
        let err = file.try_clone().map_err(|e| format!("Log: {e}"))?;
        cmd.stdout(Stdio::from(file)).stderr(Stdio::from(err));
        let child = cmd.spawn().map_err(|e| format!("Could not start: {e}"))?;
        let pid = child.id();
        live.kids.push(child);
        return Ok(RunResult {
            output: format!(
                "started pid {pid} (background, until this job ends)\nlog: {}\nRead the log with run if you need the output.",
                log.display()
            ),
        });
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("Could not start: {e}"))?;
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "No stdout.".to_string())?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "No stderr.".to_string())?;
    let out_h = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_h = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let (timed_out, exit) = match child
        .wait_timeout(Duration::from_secs(secs))
        .map_err(|e| e.to_string())?
    {
        Some(status) => (false, status.code()),
        None => {
            kill_group(&mut child);
            let _ = child.wait();
            (true, None)
        }
    };
    let stdout = String::from_utf8_lossy(&out_h.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_h.join().unwrap_or_default()).into_owned();
    Ok(RunResult {
        output: format_run(secs, exit, timed_out, &stdout, &stderr),
    })
}

fn format_run(
    secs: u64,
    exit: Option<i32>,
    timed_out: bool,
    stdout: &str,
    stderr: &str,
) -> String {
    let mut body = if timed_out {
        format!("Error: timed out after {secs}s (killed).\n")
    } else {
        match exit {
            Some(code) => format!("exit: {code}\n"),
            None => "exit: (no code)\n".into(),
        }
    };
    body.push_str("stdout:\n");
    body.push_str(if stdout.is_empty() { "(empty)" } else { stdout });
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("stderr:\n");
    body.push_str(if stderr.is_empty() { "(empty)" } else { stderr });
    if body.len() > MAX_OUT {
        body.truncate(MAX_OUT);
        body.push_str("\n… truncated.");
    }
    body
}

fn sandboxed(root: &Path, cwd: &Path, command: &str) -> Result<Command, String> {
    let shell = login_shell();
    #[cfg(target_os = "macos")]
    {
        if Path::new(SEATBELT).is_file() {
            let (policy, params) = seatbelt_spec(root)?;
            let mut cmd = Command::new(SEATBELT);
            cmd.arg("-p")
                .arg(policy)
                .args(params)
                .arg("--")
                .arg(&shell)
                .arg("-lc")
                .arg(command);
            let _ = cwd;
            return Ok(cmd);
        }
    }
    let mut cmd = Command::new(&shell);
    cmd.arg("-lc").arg(command);
    let _ = (root, cwd);
    Ok(cmd)
}

#[cfg(target_os = "macos")]
fn seatbelt_spec(root: &Path) -> Result<(String, Vec<String>), String> {
    let workspace = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let puck_env = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.env");
    let ws_env = workspace.join(".env");
    let closed = [
        ("SSH", home.join(".ssh")),
        ("GNUPG", home.join(".gnupg")),
        ("AWS", home.join(".aws")),
        ("NETRC", home.join(".netrc")),
        ("DOTENV_APP", puck_env),
        ("DOTENV_WS", ws_env),
    ];
    let mut params = Vec::new();
    for (key, path) in &closed {
        let s = path.to_string_lossy();
        if s.as_bytes().contains(&0) || s.contains('\n') {
            return Err("Bad path for sandbox.".into());
        }
        params.push(format!("-D{key}={}", path.display()));
    }
    Ok((POLICY.to_string(), params))
}

fn login_shell() -> PathBuf {
    let raw = std::env::var("SHELL").unwrap_or_default();
    let p = PathBuf::from(raw.trim());
    const OK: &[&str] = &[
        "/bin/zsh",
        "/bin/bash",
        "/usr/bin/zsh",
        "/usr/bin/bash",
        "/opt/homebrew/bin/zsh",
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/zsh",
        "/usr/local/bin/bash",
    ];
    if OK.iter().any(|s| p.as_os_str() == *s) && p.is_file() {
        return p;
    }
    PathBuf::from("/bin/zsh")
}

fn safe_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for key in [
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "PATH",
        "SHELL",
        "SSH_AUTH_SOCK",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "NVM_DIR",
        "PYENV_ROOT",
        "VIRTUAL_ENV",
        "GOPATH",
        "GOROOT",
        "JAVA_HOME",
        "HOMEBREW_PREFIX",
        "HOMEBREW_CELLAR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "SDKROOT",
    ] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                out.push((key.to_string(), v));
            }
        }
    }
    out.push(("TERM".into(), "dumb".into()));
    out.push(("NO_COLOR".into(), "1".into()));
    out
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_ws() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "puck-shell-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn timeout_treats_ms_as_ms() {
        assert_eq!(timeout_secs(None), 120);
        assert_eq!(timeout_secs(Some(30)), 30);
        assert_eq!(timeout_secs(Some(120_000)), 120);
        assert_eq!(timeout_secs(Some(900)), 900);
        assert_eq!(timeout_secs(Some(0)), 120);
        assert_eq!(timeout_secs(Some(4_000_000)), 1800);
    }

    #[test]
    fn clip_cmd_is_one_line() {
        assert_eq!(clip_cmd("echo   hi"), "echo hi");
        let long = "x".repeat(90);
        assert!(clip_cmd(&long).ends_with('…'));
        assert!(clip_cmd(&long).chars().count() <= 73);
    }

    #[test]
    fn empty_command_fails() {
        let root = tmp_ws();
        let err = run_in_workspace(&root, "  ", ".", 5).unwrap_err();
        assert!(err.contains("Empty"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn echo_runs_in_workspace() {
        let root = tmp_ws();
        let out = run_in_workspace(&root, "pwd && echo puck-ok", ".", 15).unwrap();
        assert!(out.output.contains("puck-ok"), "{}", out.output);
        assert!(
            out.output.contains(&root.canonicalize().unwrap().display().to_string())
                || out.output.contains("puck-shell-"),
            "{}",
            out.output
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn can_write_inside_workspace() {
        let root = tmp_ws();
        let out = run_in_workspace(&root, "echo inside > marker.txt", ".", 15).unwrap();
        assert!(
            out.output.contains("exit: 0") || Path::new(&root.join("marker.txt")).is_file(),
            "{}",
            out.output
        );
        let body = fs::read_to_string(root.join("marker.txt")).unwrap_or_default();
        assert!(body.contains("inside"), "{body} / {}", out.output);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn can_write_outside_workspace() {
        let root = tmp_ws();
        let outside = root.parent().unwrap().join(format!(
            "puck-shell-outside-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&outside);
        let quoted = outside.display().to_string().replace('\'', "'\\''");
        let cmd = format!("echo pwned > '{quoted}'");
        let out = run_in_workspace(&root, &cmd, ".", 15).unwrap();
        let leaked = outside.is_file();
        let _ = fs::remove_file(&outside);
        assert!(
            leaked || out.output.contains("exit: 0"),
            "should write outside workspace: {} / {}",
            outside.display(),
            out.output
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cannot_write_ssh() {
        let root = tmp_ws();
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let probe = PathBuf::from(&home).join(".ssh").join(format!(
            "puck-deny-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&probe);
        let quoted = probe.display().to_string().replace('\'', "'\\''");
        let cmd = format!("echo pwned > '{quoted}'");
        let out = run_in_workspace(&root, &cmd, ".", 15).unwrap();
        let leaked = probe.is_file();
        let _ = fs::remove_file(&probe);
        assert!(!leaked, "wrote into ~/.ssh: {} / {}", probe.display(), out.output);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn network_is_allowed() {
        let root = tmp_ws();
        let out = run_in_workspace(
            &root,
            "python3 -c \"import socket; s=socket.socket(); s.settimeout(5); s.connect(('1.1.1.1', 443)); print('opened')\"",
            ".",
            15,
        )
        .unwrap();
        assert!(
            out.output.contains("stdout:\nopened"),
            "{}",
            out.output
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn git_works_in_workspace() {
        let root = tmp_ws();
        fs::write(root.join("a.txt"), "x\n").unwrap();
        let out = run_in_workspace(
            &root,
            "git init && git add a.txt && git -c user.email=puck@test -c user.name=puck -c commit.gpgsign=false commit -m puck",
            ".",
            20,
        )
        .unwrap();
        assert!(
            out.output.contains("exit: 0"),
            "{}",
            out.output
        );
        assert!(root.join(".git").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn timeout_kills() {
        let root = tmp_ws();
        let started = std::time::Instant::now();
        let out = run_in_workspace(&root, "sleep 20", ".", 1).unwrap();
        assert!(started.elapsed().as_secs() < 8, "took {:?}", started.elapsed());
        assert!(
            out.output.contains("timed out"),
            "{}",
            out.output
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cwd_can_leave_workspace() {
        let root = tmp_ws();
        let out = run_in_workspace(&root, "pwd", "..", 5).unwrap();
        assert!(out.output.contains("exit: 0"), "{}", out.output);
        let _ = fs::remove_dir_all(&root);
    }
}
