//! The key the local network signs with, kept as a file in the data directory.
//! It signs the settlement log, as the subnet's threshold key does for the
//! control canister on the Internet Computer.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use decomp_core::SigningKey;

const KEY_FILE: &str = "control.key";

/// The network's key. One is made the first time the network starts.
pub fn control_key(data_dir: &Path) -> io::Result<SigningKey> {
    let path = data_dir.join(KEY_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            let seed = bytes.try_into().map_err(|_| {
                let message = format!("{} is not a key", path.display());
                io::Error::new(io::ErrorKind::InvalidData, message)
            })?;
            Ok(SigningKey::from_seed(seed))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let key = SigningKey::generate();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600); // readable by the owner only
            }
            options.open(&path)?.write_all(&key.seed())?;
            Ok(key)
        }
        Err(error) => Err(error),
    }
}
