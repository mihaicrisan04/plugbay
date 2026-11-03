//! `plugbay logs`: the end of the daemon's log, optionally followed as it grows.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::paths::Paths;

const TAIL_CHUNK_BYTES: usize = 64 * 1024;
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

pub async fn logs(paths: &Paths, follow: bool, lines: usize) -> anyhow::Result<()> {
    let path = paths.daemon_log();
    let shown = tokio::task::spawn_blocking(move || show_log(&path, follow, lines)).await?;
    match shown {
        Err(error) if is_broken_pipe(&error) => Ok(()),
        other => other,
    }
}

fn show_log(path: &Path, follow: bool, lines: usize) -> anyhow::Result<()> {
    let mut stdout = io::stdout();
    let log = match OpenLog::open(path) {
        Ok(mut log) => {
            let start = tail_start(&mut log.file, lines)?;
            log.file.seek(SeekFrom::Start(start))?;
            io::copy(&mut log.file, &mut stdout)?;
            stdout.flush()?;
            Some(log)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let hint =
                "the daemon writes it once an agent, `plugbay tools` or `plugbay daemon` starts it";
            if !follow {
                bail!("no daemon log yet at {}: {hint}", path.display());
            }
            eprintln!("waiting for {} ({hint})", path.display());
            None
        }
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    if follow {
        follow_log(path, log, &mut stdout)?;
    }
    Ok(())
}

/// Prints what is appended to the log until interrupted. A log that appears, is replaced
/// (rotated) or truncated is read again from its start; the old file is finished first.
fn follow_log(path: &Path, mut log: Option<OpenLog>, out: &mut impl Write) -> anyhow::Result<()> {
    loop {
        if let Some(log) = &mut log {
            io::copy(&mut log.file, out)?;
            out.flush()?;
        }
        log = match (log, std::fs::metadata(path)) {
            (Some(current), Ok(metadata)) if current.is_same_file(&metadata) => {
                Some(current.rewound_if_truncated(&metadata)?)
            }
            (_, Ok(_)) => {
                Some(OpenLog::open(path).with_context(|| format!("opening {}", path.display()))?)
            }
            // Moved away and not recreated yet: keep reading the old file meanwhile.
            (current, Err(_)) => current,
        };
        std::thread::sleep(FOLLOW_INTERVAL);
    }
}

struct OpenLog {
    file: File,
    device: u64,
    inode: u64,
}

impl OpenLog {
    fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        Ok(Self {
            file,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn is_same_file(&self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }

    fn rewound_if_truncated(mut self, metadata: &std::fs::Metadata) -> io::Result<Self> {
        if metadata.len() < self.file.stream_position()? {
            self.file.seek(SeekFrom::Start(0))?;
        }
        Ok(self)
    }
}

/// Where the last `lines` lines start, found by reading backwards from the end, so a large log
/// is never read whole. A final newline ends the last line rather than starting another.
fn tail_start(file: &mut (impl Read + Seek), lines: usize) -> io::Result<u64> {
    let end = file.seek(SeekFrom::End(0))?;
    if lines == 0 {
        return Ok(end);
    }
    let mut buffer = vec![0; TAIL_CHUNK_BYTES];
    let mut newlines = 0;
    let mut chunk_end = end;
    while chunk_end > 0 {
        let chunk_start = chunk_end.saturating_sub(TAIL_CHUNK_BYTES as u64);
        let chunk = &mut buffer[..(chunk_end - chunk_start) as usize];
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(chunk)?;
        for (offset, byte) in chunk.iter().enumerate().rev() {
            let line_start = chunk_start + offset as u64 + 1;
            if *byte != b'\n' || line_start == end {
                continue;
            }
            newlines += 1;
            if newlines == lines {
                return Ok(line_start);
            }
        }
        chunk_end = chunk_start;
    }
    Ok(0)
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|io| io.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn tail(text: &str, lines: usize) -> String {
        let mut cursor = Cursor::new(text.as_bytes().to_vec());
        let start = tail_start(&mut cursor, lines).unwrap() as usize;
        text[start..].to_string()
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn tail_of_a_short_log_is_all_of_it() {
        assert_eq!(tail("a\nb\n", 50), "a\nb\n");
        assert_eq!(tail("", 5), "");
    }

    #[test]
    fn tail_of_zero_lines_is_empty() {
        assert_eq!(tail("a\nb\n", 0), "");
    }

    #[test]
    fn tail_reads_across_chunks() {
        let long_line = "x".repeat(TAIL_CHUNK_BYTES + 10);
        let text = format!("first\n{long_line}\nlast\n");
        assert_eq!(tail(&text, 2), format!("{long_line}\nlast\n"));
        assert_eq!(tail(&text, 3), text);
    }

    #[test]
    fn following_picks_up_a_replaced_log() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("daemon.log");
        std::fs::write(&path, "old\n").unwrap();
        let mut log = OpenLog::open(&path).unwrap();
        log.file.seek(SeekFrom::End(0)).unwrap();

        std::fs::rename(&path, temp.path().join("daemon.log.1")).unwrap();
        std::fs::write(&path, "new\n").unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        assert!(!log.is_same_file(&metadata));
    }

    #[test]
    fn following_rewinds_a_truncated_log() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("daemon.log");
        std::fs::write(&path, "a long first line\n").unwrap();
        let mut log = OpenLog::open(&path).unwrap();
        log.file.seek(SeekFrom::End(0)).unwrap();

        std::fs::write(&path, "short\n").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mut log = log.rewound_if_truncated(&metadata).unwrap();
        let mut text = String::new();
        log.file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "short\n");
    }
}
