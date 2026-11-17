//! An append-only file of JSON lines. The coordinator's durable records are logs:
//! account operations (replayed at startup) and job receipts.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use serde::Serialize;

pub struct Log {
    file: File,
    /// How many entries the log holds.
    len: u64,
}

impl Log {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        let mut log = Self { file, len: 0 };
        let mut len = 0;
        for line in log.lines()? {
            line?;
            len += 1;
        }
        log.len = len;
        Ok(log)
    }

    /// How many entries the log holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// The lines in the log, oldest first. This reads the file from its start.
    pub fn lines(&self) -> io::Result<impl Iterator<Item = io::Result<String>> + '_> {
        // Entries are always written at the end, wherever reading left off.
        (&self.file).seek(SeekFrom::Start(0))?;
        Ok(BufReader::new(&self.file).lines())
    }

    /// Up to `limit` lines of the log, starting at line number `from`.
    pub fn range(&self, from: u64, limit: usize) -> io::Result<Vec<String>> {
        let from = usize::try_from(from).unwrap_or(usize::MAX);
        self.lines()?.skip(from).take(limit).collect()
    }

    /// Appends one entry and returns once it is on disk.
    ///
    /// The caller has already acted on the entry in memory, so a failed write
    /// leaves no safe way to carry on. This stops the process instead; the next
    /// start rebuilds from the log, without the entry.
    pub fn append<T: Serialize>(&mut self, entry: &T) {
        self.append_line(serde_json::to_string(entry).expect("log entries serialize"));
    }

    /// Appends an entry that is already text, exactly as it is given.
    pub fn append_line(&mut self, mut line: String) {
        line.push('\n');
        self.len += 1;
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .expect("could not write to a log file");
    }
}
