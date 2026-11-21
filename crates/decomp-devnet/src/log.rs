//! A log kept as a file of JSON lines, appended to and synced on every entry.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use decomp_control::LogBackend;

pub struct FileLog {
    file: File,
}

impl FileLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        Ok(Self { file })
    }
}

impl LogBackend for FileLog {
    fn lines(&self) -> io::Result<Vec<String>> {
        (&self.file).seek(SeekFrom::Start(0))?;
        BufReader::new(&self.file).lines().collect()
    }

    fn append(&mut self, line: &str) {
        let mut line = line.to_string();
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.sync_data())
            .expect("could not write to a log file");
    }
}
