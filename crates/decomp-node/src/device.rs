//! This device's key, kept as a file in the agent's directory.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use decomp_core::SigningKey;

const KEY_FILE: &str = "device.key";

pub fn load(dir: &Path) -> io::Result<Option<SigningKey>> {
    let path = dir.join(KEY_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let seed = bytes.try_into().map_err(|_| {
        let message = format!("{} is not a device key", path.display());
        io::Error::new(io::ErrorKind::InvalidData, message)
    })?;
    Ok(Some(SigningKey::from_seed(seed)))
}

/// Makes a new key and saves it. Never replaces an existing key file.
pub fn create(dir: &Path) -> io::Result<SigningKey> {
    fs::create_dir_all(dir)?;
    let key = SigningKey::generate();

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600); // readable by the owner only
    }
    options.open(dir.join(KEY_FILE))?.write_all(&key.seed())?;
    Ok(key)
}
