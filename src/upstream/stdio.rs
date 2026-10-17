//! Local upstream processes: their own process group, a cap on message size, redacted stderr.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context as TaskContext, Poll};

use anyhow::Context;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::secrets;

/// Upper bound for one JSON-RPC line from a stdio server. Large enough for base64 screenshots,
/// small enough that one misbehaving server can't exhaust the shared daemon's memory.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Longer stderr lines are cut in the log, so a server dumping a blob can't flood memory or the log.
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;

/// A running stdio server. Dropping it stops the whole process group, so launchers like
/// `npx` don't leave their `node` child behind.
pub struct StdioProcess {
    child: Child,
    process_group: Option<rustix::process::Pid>,
}

pub struct StdioPipes {
    pub stdout: LineLengthLimit<ChildStdout>,
    pub stdin: ChildStdin,
}

pub struct SpawnRequest<'a> {
    pub server_name: String,
    pub command: &'a str,
    pub args: &'a [String],
    pub env: &'a BTreeMap<String, String>,
    /// Values whose env template held a secret ref; scrubbed from stderr before it is logged.
    pub secret_values: Vec<String>,
    pub working_dir: Option<&'a Path>,
}

pub fn spawn(request: SpawnRequest<'_>) -> anyhow::Result<(StdioProcess, StdioPipes)> {
    let mut command = tokio::process::Command::new(request.command);
    command
        .args(request.args)
        .envs(request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(working_dir) = request.working_dir {
        command.current_dir(working_dir);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("starting `{}` for {}", request.command, request.server_name))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        anyhow::bail!("`{}` started without piped stdio", request.command);
    };
    forward_stderr(request.server_name, stderr, request.secret_values);

    let process_group = child
        .id()
        .and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
    let pipes = StdioPipes {
        stdout: LineLengthLimit::new(stdout, MAX_MESSAGE_BYTES),
        stdin,
    };
    Ok((
        StdioProcess {
            child,
            process_group,
        },
        pipes,
    ))
}

impl StdioProcess {
    /// Asks every process in the group to exit; the leader is also killed when `child` drops.
    pub fn terminate(&mut self) {
        if let Some(process_group) = self.process_group.take()
            && let Err(error) =
                rustix::process::kill_process_group(process_group, rustix::process::Signal::TERM)
        {
            tracing::debug!(%error, "process group already gone");
        }
        if let Err(error) = self.child.start_kill() {
            tracing::debug!(%error, "stdio server already exited");
        }
    }
}

impl Drop for StdioProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Errors the read side once a single line grows past `limit` bytes.
pub struct LineLengthLimit<R> {
    inner: R,
    limit: usize,
    bytes_since_newline: usize,
}

impl<R> LineLengthLimit<R> {
    pub fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            bytes_since_newline: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for LineLengthLimit<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let already_filled = buffer.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(context, buffer);
        if let Poll::Ready(Ok(())) = &poll {
            let new_bytes = &buffer.filled()[already_filled..];
            self.bytes_since_newline = match new_bytes.iter().rposition(|byte| *byte == b'\n') {
                Some(last_newline) => new_bytes.len() - last_newline - 1,
                None => self.bytes_since_newline + new_bytes.len(),
            };
            if self.bytes_since_newline > self.limit {
                // An erroring read must not hand out bytes (AsyncRead contract).
                buffer.set_filled(already_filled);
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "stdio server sent a message larger than {} bytes",
                        self.limit
                    ),
                )));
            }
        }
        poll
    }
}

/// Upstream stderr goes to the daemon log, so a crashing stdio server is debuggable.
fn forward_stderr(
    server_name: String,
    stderr: tokio::process::ChildStderr,
    secret_values: Vec<String>,
) {
    // Keep enough past the cap to redact a secret that straddles it before cutting.
    let longest_secret = secret_values.iter().map(String::len).max().unwrap_or(0);
    let kept_bytes = MAX_STDERR_LINE_BYTES + longest_secret;
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            match read_capped_line(&mut reader, &mut line, kept_bytes).await {
                Ok(Some(cut_short)) => {
                    let text = String::from_utf8_lossy(&line);
                    let redacted = redact_known_secrets(&text, &secret_values);
                    let shown = truncate_line(&redacted, MAX_STDERR_LINE_BYTES, cut_short);
                    tracing::debug!(server = %server_name, "stderr: {shown}");
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(server = %server_name, %error, "stopped reading stderr");
                    break;
                }
            }
        }
    });
}

/// Reads one line into `line` (without the newline), keeping at most `kept_bytes` and skipping
/// the rest. Returns whether bytes were skipped, or `None` at the end of the stream.
async fn read_capped_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
    kept_bytes: usize,
) -> io::Result<Option<bool>> {
    line.clear();
    let mut read_anything = false;
    let mut skipped = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(read_anything.then_some(skipped));
        }
        read_anything = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content = &available[..newline.unwrap_or(available.len())];
        let room = kept_bytes.saturating_sub(line.len());
        if content.len() > room {
            skipped = true;
        }
        line.extend_from_slice(&content[..content.len().min(room)]);
        let consumed = content.len() + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(skipped));
        }
    }
}

/// Cuts `text` to at most `max_bytes` on a character boundary and marks lines that were cut.
fn truncate_line(text: &str, max_bytes: usize, cut_short: bool) -> String {
    if text.len() <= max_bytes && !cut_short {
        return text.to_string();
    }
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [line truncated]", &text[..end])
}

fn redact_known_secrets(line: &str, secret_values: &[String]) -> String {
    let mut redacted = line.to_string();
    for secret in secret_values {
        redacted = redacted.replace(secret.as_str(), "[redacted]");
    }
    redacted
}

/// Resolved env values that came from a secret ref or sit under a secret-looking name.
pub fn secret_env_values(
    templates: &BTreeMap<String, String>,
    resolved: &BTreeMap<String, String>,
) -> Vec<String> {
    templates
        .iter()
        .filter(|(name, template)| {
            secrets::contains_secret_ref(template) || secrets::looks_secret(name)
        })
        .filter_map(|(name, _)| resolved.get(name))
        // Very short values would redact unrelated text everywhere.
        .filter(|value| value.len() >= 4)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn lines_within_the_limit_pass_through() {
        let input: &[u8] = b"short\nalso short\n";
        let mut limited = LineLengthLimit::new(input, 16);
        let mut output = String::new();
        limited.read_to_string(&mut output).await.unwrap();
        assert_eq!(output, "short\nalso short\n");
    }

    #[tokio::test]
    async fn a_line_over_the_limit_is_an_error() {
        let input = vec![b'x'; 64];
        let mut limited = LineLengthLimit::new(input.as_slice(), 16);
        let mut output = Vec::new();
        let error = limited.read_to_end(&mut output).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn stderr_lines_are_capped_and_the_rest_skipped() {
        let input = format!("short\n{}\nnext\nlast", "x".repeat(100));
        let mut reader = input.as_bytes();
        let mut line = Vec::new();
        let mut lines = Vec::new();
        while let Some(cut_short) = read_capped_line(&mut reader, &mut line, 10).await.unwrap() {
            lines.push((String::from_utf8(line.clone()).unwrap(), cut_short));
        }
        assert_eq!(
            lines,
            [
                ("short".to_string(), false),
                ("x".repeat(10), true),
                ("next".to_string(), false),
                ("last".to_string(), false),
            ]
        );
    }

    #[test]
    fn truncated_lines_are_marked_and_cut_on_a_character_boundary() {
        assert_eq!(truncate_line("short", 10, false), "short");
        assert_eq!(truncate_line("ab€cd", 3, false), "ab [line truncated]");
        assert_eq!(truncate_line("abc", 10, true), "abc [line truncated]");
    }

    #[test]
    fn stderr_redaction_hides_resolved_secrets() {
        let secrets = vec!["test-token-client-a".to_string()];
        assert_eq!(
            redact_known_secrets("API_TOKEN=test-token-client-a failed", &secrets),
            "API_TOKEN=[redacted] failed"
        );
    }

    #[test]
    fn secret_env_values_come_from_refs_and_secret_names() {
        let templates = BTreeMap::from([
            ("API_TOKEN".to_string(), "literal-token-value".to_string()),
            ("REGION".to_string(), "eu-west".to_string()),
            ("PASSWORDLESS".to_string(), "${env:SOMETHING}".to_string()),
        ]);
        let resolved = BTreeMap::from([
            ("API_TOKEN".to_string(), "literal-token-value".to_string()),
            ("REGION".to_string(), "eu-west".to_string()),
            ("PASSWORDLESS".to_string(), "resolved-secret".to_string()),
        ]);
        let mut values = secret_env_values(&templates, &resolved);
        values.sort();
        assert_eq!(values, ["literal-token-value", "resolved-secret"]);
    }
}
