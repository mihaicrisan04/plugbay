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

/// Prints what is appended to the log until interrupted, reading it again from the start once
/// it appears, or when someone deletes, replaces or empties it (plugbay itself only appends).
fn follow_log(path: &Path, mut log: Option<OpenLog>, out: &mut impl Write) -> anyhow::Result<()> {
    loop {
        if let Some(log) = &mut log {
            io::copy(&mut log.file, out)?;
            out.flush()?;
        }
        if let Ok(metadata) = std::fs::metadata(path)
            && log.as_ref().is_none_or(|log| log.is_stale(&metadata))
        {
            log = Some(OpenLog::open(path).with_context(|| format!("opening {}", path.display()))?);
        }
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

    /// The path's file changed or got shorter than what was read of it.
    fn is_stale(&self, metadata: &std::fs::Metadata) -> bool {
        let same_file = self.device == metadata.dev() && self.inode == metadata.ino();
        let read = (&self.file).stream_position().unwrap_or(0);
        !same_file || metadata.len() < read
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

    fn read_to_end(path: &Path) -> OpenLog {
        let mut log = OpenLog::open(path).unwrap();
        io::copy(&mut log.file, &mut io::sink()).unwrap();
        log
    }

    #[test]
    fn a_replaced_log_is_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("daemon.log");
        std::fs::write(&path, "old\n").unwrap();
        let log = read_to_end(&path);

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "a much longer new log\n").unwrap();
        assert!(log.is_stale(&std::fs::metadata(&path).unwrap()));
    }

    #[test]
    fn a_shortened_log_is_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("daemon.log");
        std::fs::write(&path, "a long first line\n").unwrap();
        let log = read_to_end(&path);

        std::fs::write(&path, "short\n").unwrap();
        assert!(log.is_stale(&std::fs::metadata(&path).unwrap()));
    }

    #[test]
    fn a_growing_log_is_not_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("daemon.log");
        std::fs::write(&path, "first\n").unwrap();
        let log = read_to_end(&path);

        let mut appending = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        appending.write_all(b"second\n").unwrap();
        assert!(!log.is_stale(&std::fs::metadata(&path).unwrap()));
    }
}
