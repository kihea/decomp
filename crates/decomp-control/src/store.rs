//! Where the control plane keeps what lasts: append-only logs of entries, the
//! blobs it holds, and its signing key. Each host supplies its own backend:
//! files on the devnet, and heap memory saved to stable memory across
//! upgrades in a canister.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use decomp_core::{ContentHash, SigningKey};
use serde::Serialize;

/// How a log's lines are kept.
pub trait LogBackend: Send {
    /// Every line, oldest first.
    fn lines(&self) -> io::Result<Vec<String>>;
    /// Appends one line. A failure here is unrecoverable.
    fn append(&mut self, line: &str);
}

/// How blobs are kept, by the hash of their bytes.
pub trait BlobBackend: Send {
    fn put(&self, hash: &ContentHash, bytes: &[u8]) -> io::Result<()>;
    fn get(&self, hash: &ContentHash) -> io::Result<Option<Vec<u8>>>;
    fn contains(&self, hash: &ContentHash) -> bool;
}

/// What a host gives the control plane to keep its state in.
pub trait StateStore {
    /// The log of one kind of entry, by name, such as `ops` or `ledger`.
    fn log(&mut self, name: &str) -> io::Result<Log>;
    fn blobs(&mut self) -> io::Result<Blobs>;
    /// The key the settlement log and draws are signed with.
    fn control_key(&mut self) -> io::Result<SigningKey>;
}

/// An append-only log of JSON lines.
pub struct Log {
    backend: Box<dyn LogBackend>,
    len: u64,
}

impl Log {
    pub fn new(backend: Box<dyn LogBackend>) -> io::Result<Self> {
        let len = backend.lines()?.len() as u64;
        Ok(Self { backend, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn lines(&self) -> io::Result<impl Iterator<Item = io::Result<String>>> {
        Ok(self.backend.lines()?.into_iter().map(Ok))
    }

    pub fn range(&self, from: u64, limit: usize) -> io::Result<Vec<String>> {
        let from = usize::try_from(from).unwrap_or(usize::MAX);
        Ok(self
            .backend
            .lines()?
            .into_iter()
            .skip(from)
            .take(limit)
            .collect())
    }

    pub fn append<T: Serialize>(&mut self, entry: &T) {
        self.append_line(serde_json::to_string(entry).expect("log entries serialize"));
    }

    pub fn append_line(&mut self, line: String) {
        self.len += 1;
        self.backend.append(&line);
    }
}

/// Blobs, named by the SHA-256 hash of their bytes.
pub struct Blobs {
    backend: Box<dyn BlobBackend>,
}

impl Blobs {
    pub fn new(backend: Box<dyn BlobBackend>) -> Self {
        Self { backend }
    }

    /// Keeps `bytes`, named by their root as stored blobs are.
    pub fn put(&self, bytes: &[u8]) -> io::Result<ContentHash> {
        let hash = decomp_core::merkle::root(bytes);
        if !self.backend.contains(&hash) {
            self.backend.put(&hash, bytes)?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &ContentHash) -> io::Result<Option<Vec<u8>>> {
        self.backend.get(hash)
    }

    pub fn contains(&self, hash: &ContentHash) -> bool {
        self.backend.contains(hash)
    }
}

/// Logs and blobs kept in memory, which a canister saves to stable memory
/// before an upgrade and hands back after it.
/// One log's lines, shared between the log and the storage that saves them.
type SharedLines = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Default)]
pub struct MemoryStorage {
    logs: Arc<Mutex<HashMap<String, SharedLines>>>,
    blobs: Arc<Mutex<HashMap<ContentHash, Vec<u8>>>>,
    key: [u8; 32],
}

/// What a `MemoryStorage` holds, to be saved and restored.
pub type MemorySnapshot = (
    Vec<(String, Vec<String>)>,
    Vec<([u8; 32], Vec<u8>)>,
    [u8; 32],
);

impl MemoryStorage {
    /// Empty storage whose signing key is made from `key_seed`.
    pub fn new(key_seed: [u8; 32]) -> Self {
        Self {
            key: key_seed,
            ..Self::default()
        }
    }

    pub fn snapshot(&self) -> MemorySnapshot {
        let logs = self.logs.lock().expect("storage lock");
        let logs = logs
            .iter()
            .map(|(name, lines)| (name.clone(), lines.lock().expect("log lock").clone()))
            .collect();
        let blobs = self.blobs.lock().expect("storage lock");
        let blobs = blobs
            .iter()
            .map(|(hash, bytes)| (*hash.as_bytes(), bytes.clone()))
            .collect();
        (logs, blobs, self.key)
    }

    pub fn restore((logs, blobs, key): MemorySnapshot) -> Self {
        let logs = logs
            .into_iter()
            .map(|(name, lines)| (name, Arc::new(Mutex::new(lines))))
            .collect();
        let blobs = blobs
            .into_iter()
            .map(|(hash, bytes)| (ContentHash::from_bytes(hash), bytes))
            .collect();
        Self {
            logs: Arc::new(Mutex::new(logs)),
            blobs: Arc::new(Mutex::new(blobs)),
            key,
        }
    }
}

struct MemoryLog(Arc<Mutex<Vec<String>>>);

impl LogBackend for MemoryLog {
    fn lines(&self) -> io::Result<Vec<String>> {
        Ok(self.0.lock().expect("log lock").clone())
    }

    fn append(&mut self, line: &str) {
        self.0.lock().expect("log lock").push(line.to_string());
    }
}

struct MemoryBlobs(Arc<Mutex<HashMap<ContentHash, Vec<u8>>>>);

impl BlobBackend for MemoryBlobs {
    fn put(&self, hash: &ContentHash, bytes: &[u8]) -> io::Result<()> {
        self.0
            .lock()
            .expect("blob lock")
            .insert(*hash, bytes.to_vec());
        Ok(())
    }

    fn get(&self, hash: &ContentHash) -> io::Result<Option<Vec<u8>>> {
        Ok(self.0.lock().expect("blob lock").get(hash).cloned())
    }

    fn contains(&self, hash: &ContentHash) -> bool {
        self.0.lock().expect("blob lock").contains_key(hash)
    }
}

impl StateStore for MemoryStorage {
    fn log(&mut self, name: &str) -> io::Result<Log> {
        let lines = Arc::clone(
            self.logs
                .lock()
                .expect("storage lock")
                .entry(name.to_string())
                .or_default(),
        );
        Log::new(Box::new(MemoryLog(lines)))
    }

    fn blobs(&mut self) -> io::Result<Blobs> {
        Ok(Blobs::new(Box::new(MemoryBlobs(Arc::clone(&self.blobs)))))
    }

    fn control_key(&mut self) -> io::Result<SigningKey> {
        Ok(SigningKey::from_seed(self.key))
    }
}
