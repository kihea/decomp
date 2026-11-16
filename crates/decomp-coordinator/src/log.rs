//! An append-only file of JSON lines. The coordinator's durable records are logs:
//! account operations (replayed at startup) and job receipts.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use serde::Serialize;

pub struct Log {
    file: File,
}

impl Log {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        Ok(Self { file })
    }

    /// The lines already in the log, oldest first.
    pub fn lines(&self) -> impl Iterator<Item = io::Result<String>> + '_ {
        BufReader::new(&self.file).lines()
    }

    /// Appends one entry and returns once it is on disk.
    ///
    /// The caller has already acted on the entry in memory, so a failed write
    /// leaves no safe way to carry on. This stops the process instead; the next
    /// start rebuilds from the log, without the entry.
    pub fn append<T: Serialize>(&mut self, entry: &T) {
        let mut line = serde_json::to_string(entry).expect("log entries serialize");
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .expect("could not write to a log file");
    }
}
