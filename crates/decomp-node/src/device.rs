//! This device's keys, kept as files in the agent's directory: the key it signs
//! with, and the key it opens what is sealed to it with.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use decomp_core::{OpeningKey, SigningKey};

const KEY_FILE: &str = "device.key";
const OPENING_KEY_FILE: &str = "opening.key";

pub fn load(dir: &Path) -> io::Result<Option<SigningKey>> {
    Ok(read_seed(&dir.join(KEY_FILE))?.map(SigningKey::from_seed))
}

/// The key this device opens sealed data with, if it has made one.
pub fn opening_key_if_made(dir: &Path) -> io::Result<Option<OpeningKey>> {
    Ok(read_seed(&dir.join(OPENING_KEY_FILE))?.map(OpeningKey::from_seed))
}

/// The key this device opens sealed data with. One is made the first time it is
/// asked for.
pub fn opening_key(dir: &Path) -> io::Result<OpeningKey> {
    if let Some(key) = opening_key_if_made(dir)? {
        return Ok(key);
    }
    let key = OpeningKey::generate();
    save_seed(dir, OPENING_KEY_FILE, &key.seed())?;
    Ok(key)
}

/// The 32 secret bytes in a key file, or `None` if there is no such file.
fn read_seed(path: &Path) -> io::Result<Option<[u8; 32]>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let seed = bytes.try_into().map_err(|_| {
        let message = format!("{} is not a key", path.display());
        io::Error::new(io::ErrorKind::InvalidData, message)
    })?;
    Ok(Some(seed))
}

/// Makes a new key and saves it. Never replaces an existing key file.
pub fn create(dir: &Path) -> io::Result<SigningKey> {
    let key = SigningKey::generate();
    save_seed(dir, KEY_FILE, &key.seed())?;
    Ok(key)
}

/// Saves a key's secret bytes as a new file. Never replaces an existing file.
fn save_seed(dir: &Path, file: &str, seed: &[u8; 32]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600); // readable by the owner only
    }
    options.open(dir.join(file))?.write_all(seed)
}
