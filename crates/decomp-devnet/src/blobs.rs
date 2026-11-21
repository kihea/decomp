//! Blobs kept as files in a directory, one per hash; and the data directory
//! that holds them, the logs, and the key.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use decomp_control::{BlobBackend, Blobs, Log, StateStore};
use decomp_core::{ContentHash, SigningKey};

use crate::key;
use crate::log::FileLog;

struct FileBlobs {
    dir: PathBuf,
}

impl FileBlobs {
    fn path(&self, hash: &ContentHash) -> PathBuf {
        self.dir.join(hash.to_string())
    }
}

impl BlobBackend for FileBlobs {
    fn put(&self, hash: &ContentHash, bytes: &[u8]) -> io::Result<()> {
        fs::write(self.path(hash), bytes)
    }

    fn get(&self, hash: &ContentHash) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn contains(&self, hash: &ContentHash) -> bool {
        self.path(hash).exists()
    }
}

/// The devnet's data directory: a log file per kind of entry, a directory of
/// blobs, and the key.
pub struct DataDir {
    dir: PathBuf,
}

impl DataDir {
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir.join("blobs"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }
}

impl StateStore for DataDir {
    fn log(&mut self, name: &str) -> io::Result<Log> {
        Log::new(Box::new(FileLog::open(
            &self.dir.join(format!("{name}.jsonl")),
        )?))
    }

    fn blobs(&mut self) -> io::Result<Blobs> {
        Ok(Blobs::new(Box::new(FileBlobs {
            dir: self.dir.join("blobs"),
        })))
    }

    fn control_key(&mut self) -> io::Result<SigningKey> {
        key::control_key(&self.dir)
    }
}
