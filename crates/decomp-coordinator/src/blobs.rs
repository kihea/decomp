//! Job code, inputs, and outputs, stored as files named by their hash. This
//! stands in for network storage, which arrives with the SDK (spec section 11).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use decomp_core::ContentHash;

pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn put(&self, bytes: &[u8]) -> io::Result<ContentHash> {
        let hash = ContentHash::of(bytes);
        let path = self.path(&hash);
        if !path.exists() {
            fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &ContentHash) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn contains(&self, hash: &ContentHash) -> bool {
        self.path(hash).exists()
    }

    fn path(&self, hash: &ContentHash) -> PathBuf {
        Path::new(&self.dir).join(hash.to_string())
    }
}
