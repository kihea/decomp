//! This device's keys, kept as files in the agent's directory: the key it signs
//! with, and the key it opens what is sealed to it with. A rented machine is
//! given both in `DECOMP_DEVICE_SECRET` instead, as `add-device` made them
//! on a device of the account, so nothing has to be copied onto its disk.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use decomp_core::{OpeningKey, SigningKey};

const KEY_FILE: &str = "device.key";
const OPENING_KEY_FILE: &str = "opening.key";

/// The variable a rented machine's keys are given in: the signing key's
/// secret bytes, then the opening key's, as 128 hex digits.
pub const SECRET_VAR: &str = "DECOMP_DEVICE_SECRET";

/// Both keys as one secret, for `SECRET_VAR`.
pub fn secret(key: &SigningKey, opening: &OpeningKey) -> String {
    let mut bytes = key.seed().to_vec();
    bytes.extend_from_slice(&opening.seed());
    hex::encode(bytes)
}

/// The keys given in `SECRET_VAR`, if it is set.
fn from_secret() -> io::Result<Option<([u8; 32], [u8; 32])>> {
    let Ok(secret) = std::env::var(SECRET_VAR) else {
        return Ok(None);
    };
    let bad = || {
        let message = format!("{SECRET_VAR} is not a device secret from add-device");
        io::Error::new(io::ErrorKind::InvalidData, message)
    };
    let mut bytes = [0u8; 64];
    hex::decode_to_slice(secret.trim(), &mut bytes).map_err(|_| bad())?;
    let (key, opening) = bytes.split_at(32);
    Ok(Some((
        key.try_into().expect("32 bytes"),
        opening.try_into().expect("32 bytes"),
    )))
}

pub fn load(dir: &Path) -> io::Result<Option<SigningKey>> {
    if let Some((key, _)) = from_secret()? {
        return Ok(Some(SigningKey::from_seed(key)));
    }
    Ok(read_seed(&dir.join(KEY_FILE))?.map(SigningKey::from_seed))
}

/// The key this device opens sealed data with, if it has made one.
pub fn opening_key_if_made(dir: &Path) -> io::Result<Option<OpeningKey>> {
    if let Some((_, opening)) = from_secret()? {
        return Ok(Some(OpeningKey::from_seed(opening)));
    }
    Ok(read_seed(&dir.join(OPENING_KEY_FILE))?.map(OpeningKey::from_seed))
}

/// The key this device opens sealed data with. One is made the first time it is
/// asked for.
pub fn opening_key(dir: &Path) -> io::Result<OpeningKey> {
    if let Some(key) = opening_key_if_made(dir)? {
        return Ok(key);
    }
    let key = OpeningKey::generate();
    save_seed(&dir.join(OPENING_KEY_FILE), &key.seed())?;
    Ok(key)
}

/// A key kept in a file of its own, outside this device's directory: a
/// recovery key, or the key of a coordinator's operator. `None` if there is no
/// such file.
pub fn key_file(path: &Path) -> io::Result<Option<SigningKey>> {
    Ok(read_seed(path)?.map(SigningKey::from_seed))
}

/// Makes a key and saves it as a new file at `path`.
pub fn create_key_file(path: &Path) -> io::Result<SigningKey> {
    let key = SigningKey::generate();
    save_seed(path, &key.seed())?;
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
    save_seed(&dir.join(KEY_FILE), &key.seed())?;
    Ok(key)
}

/// Saves a key's secret bytes as a new file. Never replaces an existing file.
fn save_seed(path: &Path, seed: &[u8; 32]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600); // readable by the owner only
    }
    options.open(path)?.write_all(seed)
}
