//! `terminal`: run a shell command in the session's working directory.
//!
//! The command runs as `bash -c 'eval "$MERCURY_CMD"; …'` so `cd` persists between calls: after
//! the command we print a marker with `$PWD` and adopt it as the new working directory.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const CWD_MARKER: &str = "__MERCURY_CWD__";
pub const MAX_OUTPUT: usize = 30_000;

const WRAPPER: &str = r#"eval "$MERCURY_CMD"
__mercury_status=$?
printf '\n__MERCURY_CWD__%s\n' "$PWD"
exit $__mercury_status"#;

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub new_cwd: Option<PathBuf>,
    pub timed_out: bool,
}

impl TerminalResult {
    pub fn render(&self) -> String {
        let mut s = self.output.trim_end().to_string();
        if s.is_empty() {
            s = "(no output)".into();
        }
        if self.timed_out {
            s.push_str("\n[timed out; process killed. For long-running servers use background=true]");
        } else if let Some(c) = self.exit_code {
            if c != 0 {
                s.push_str(&format!("\n[exit code {c}]"));
            }
        } else {
            s.push_str("\n[terminated by signal]");
        }
        s
    }
}

pub fn strip_ansi(s: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap());
    RE.replace_all(s, "").into_owned()
}

/// Keep the head and the (usually more useful) tail of long output.
pub fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head_len = max * 2 / 5;
    let tail_len = max - head_len;
    let mut h = head_len;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail_len;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}\n\n[… {} bytes omitted …]\n\n{}", &s[..h], t - h, &s[t..])
}

fn command(shell_cmd: &str, cwd: &Path) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("bash");
    c.arg("-c").arg(WRAPPER).current_dir(cwd);
    c.env("MERCURY_CMD", shell_cmd)
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("GIT_TERMINAL_PROMPT", "0");
    // AppImage runtimes export these; they'd confuse child programs.
    for k in ["APPIMAGE", "APPDIR", "ARGV0", "OWD"] {
        c.env_remove(k);
    }
    c
}

fn kill_group(pid: u32) {
    let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).status();
}

pub async fn run(cmd: &str, cwd: &Path, timeout: Duration, cancel: &CancellationToken) -> std::io::Result<TerminalResult> {
    let mut c = command(cmd, cwd);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).kill_on_drop(true);
    let mut child = c.spawn()?;
    let pid = child.id().unwrap_or(0);
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let read_out = async {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b).await;
        b
    };
    let read_err = async {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b).await;
        b
    };
    let work = async {
        let (o, e, status) = tokio::join!(read_out, read_err, child.wait());
        (o, e, status)
    };
    let (out, err, status, timed_out) = tokio::select! {
        (o, e, s) = work => (o, e, s.ok(), false),
        _ = tokio::time::sleep(timeout) => { kill_group(pid); (vec![], vec![], None, true) },
        _ = cancel.cancelled() => { kill_group(pid); return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")) },
    };
    let mut text = String::from_utf8_lossy(&out).into_owned();
    let mut new_cwd = None;
    if let Some(i) = text.rfind(&format!("\n{CWD_MARKER}")) {
        let line = text[i + 1..].lines().next().unwrap_or("").to_string();
        new_cwd = line.strip_prefix(CWD_MARKER).map(|p| PathBuf::from(p.trim_end()));
        text.truncate(i);
    }
    let err = String::from_utf8_lossy(&err);
    if !err.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&err);
    }
    let text = truncate_middle(&strip_ansi(&text), MAX_OUTPUT);
    Ok(TerminalResult { output: text, exit_code: status.and_then(|s| s.code()), new_cwd, timed_out })
}

/// Start a long-running command detached, logging to a file. Returns (pid, log path).
pub fn spawn_background(cmd: &str, cwd: &Path, log_dir: &Path) -> std::io::Result<(u32, PathBuf)> {
    std::fs::create_dir_all(log_dir)?;
    let log = log_dir.join(format!("bg-{}.log", uuid::Uuid::new_v4().simple()));
    let f = std::fs::File::create(&log)?;
    let f2 = f.try_clone()?;
    let mut c = std::process::Command::new("bash");
    c.arg("-c").arg(cmd).current_dir(cwd).stdin(Stdio::null()).stdout(f).stderr(f2);
    std::os::unix::process::CommandExt::process_group(&mut c, 0);
    let child = c.spawn()?;
    Ok((child.id(), log))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_and_tracks_cwd() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let tok = CancellationToken::new();
        let r = run("echo hi; cd sub", d.path(), Duration::from_secs(10), &tok).await.unwrap();
        assert_eq!(r.output.trim(), "hi");
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.new_cwd.unwrap().canonicalize().unwrap(), d.path().join("sub").canonicalize().unwrap());
        let r = run("echo oops >&2; exit 3", d.path(), Duration::from_secs(10), &tok).await.unwrap();
        assert!(r.render().contains("oops") && r.render().contains("[exit code 3]"));
        assert!(r.new_cwd.is_none(), "explicit exit skips the marker");
        // Quotes and special characters survive because the command travels in an env var.
        let r = run(r#"printf '%s' "a'b\"c $((1+1))""#, d.path(), Duration::from_secs(10), &tok).await.unwrap();
        assert_eq!(r.output.trim(), "a'b\"c 2");
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let d = tempfile::tempdir().unwrap();
        let tok = CancellationToken::new();
        let start = std::time::Instant::now();
        let r = run("sleep 30 & sleep 30", d.path(), Duration::from_millis(300), &tok).await.unwrap();
        assert!(r.timed_out);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(r.render().contains("timed out"));
    }

    #[tokio::test]
    async fn cancel_interrupts() {
        let d = tempfile::tempdir().unwrap();
        let tok = CancellationToken::new();
        let t2 = tok.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            t2.cancel();
        });
        let e = run("sleep 20", d.path(), Duration::from_secs(30), &tok).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn ansi_and_truncation() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m \x1b]0;title\x07ok"), "red ok");
        let long = "a".repeat(100) + &"b".repeat(100);
        let t = truncate_middle(&long, 50);
        assert!(t.starts_with(&"a".repeat(20)) && t.ends_with(&"b".repeat(30)));
        assert!(t.contains("150 bytes omitted"));
    }

    #[test]
    fn background_spawn() {
        let d = tempfile::tempdir().unwrap();
        let (pid, log) = spawn_background("echo started", d.path(), d.path()).unwrap();
        assert!(pid > 0);
        for _ in 0..50 {
            if std::fs::read_to_string(&log).unwrap_or_default().contains("started") {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("background output never appeared");
    }
}
