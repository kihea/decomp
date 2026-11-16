//! Storage, from both ends: the blobs a node holds for the network, and the two
//! calls a device makes to send a blob to such a node and fetch it back.
//!
//! A node takes in only blobs the coordinator has asked this node to hold, and
//! only if they hash to the root they are stored under, so nobody can fill a
//! node's disk with data it was not asked to take. Anyone who knows a blob's root
//! can fetch it.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use decomp_core::{ContentHash, StoreOrder, merkle};
use tiny_http::{Method, Request, Response};

use crate::peer;

/// Where a node's server keeps blobs, in its web addresses.
pub(crate) const BLOB_PATH: &str = "/v1/stored/";

/// The blobs this node holds, and those it has been asked to take in.
pub struct Store {
    dir: PathBuf,
    /// Orders from the coordinator, by root. An order stays here after the blob
    /// arrives, because it says when the blob may be deleted.
    orders: Mutex<HashMap<ContentHash, StoreOrder>>,
}

impl Store {
    pub fn open(dir: PathBuf) -> io::Result<Arc<Self>> {
        fs::create_dir_all(&dir)?;
        Ok(Arc::new(Self {
            dir,
            orders: Mutex::new(HashMap::new()),
        }))
    }

    /// Notes blobs the coordinator has asked this node to take in.
    pub fn expect(&self, orders: &[StoreOrder]) {
        let mut known = self.orders.lock().expect("store lock");
        for order in orders {
            known.insert(order.root, order.clone());
        }
    }

    pub fn holds(&self, root: &ContentHash) -> bool {
        self.path(root).exists()
    }

    pub fn read(&self, root: &ContentHash) -> io::Result<Vec<u8>> {
        fs::read(self.path(root))
    }

    /// Deletes blobs whose paid time has run out.
    pub fn delete_expired(&self, now: u64) {
        let mut known = self.orders.lock().expect("store lock");
        known.retain(|root, order| {
            let expired = order.until <= now;
            if expired {
                // A blob that cannot be deleted now is tried again at the next call
                // only if the order is kept, so keep it on failure.
                return fs::remove_file(self.path(root)).is_err() && self.path(root).exists();
            }
            true
        });
    }

    /// Takes in a blob sent to this node. Returns why not if it is refused.
    fn accept(&self, root: &ContentHash, body: &mut dyn Read) -> Result<(), &'static str> {
        let order = self.orders.lock().expect("store lock").get(root).cloned();
        let order = order.ok_or("this node has not been asked to hold that blob")?;

        // Read one byte more than expected, so that a longer body is noticed.
        let mut bytes = Vec::new();
        let read = body
            .take(order.size.saturating_add(1))
            .read_to_end(&mut bytes);
        if read.is_err() || bytes.len() as u64 != order.size || merkle::root(&bytes) != *root {
            return Err("the data sent is not the blob with that root");
        }
        fs::write(self.path(root), bytes).map_err(|_| "the blob could not be saved")
    }

    fn path(&self, root: &ContentHash) -> PathBuf {
        self.dir.join(root.to_string())
    }
}

/// Answers a request to this node's server about the blob named in `path`, which
/// is the rest of the address after `BLOB_PATH`.
pub(crate) fn answer(store: &Store, path: &str, mut request: Request) -> io::Result<()> {
    let Ok(root) = path.parse::<ContentHash>() else {
        return request.respond(Response::from_string("no such address").with_status_code(404));
    };
    match request.method() {
        Method::Get => match store.read(&root) {
            Ok(bytes) => request.respond(Response::from_data(bytes)),
            Err(_) => request.respond(Response::from_string("no such blob").with_status_code(404)),
        },
        Method::Put => match store.accept(&root, request.as_reader()) {
            Ok(()) => request.respond(Response::empty(204)),
            Err(reason) => request.respond(Response::from_string(reason).with_status_code(409)),
        },
        _ => request.respond(Response::from_string("not supported").with_status_code(405)),
    }
}

/// Sends a blob to the node at `address`. Fails if the node refuses it, which it
/// does until it has heard from the coordinator that it should hold the blob.
pub fn upload(address: &str, root: &ContentHash, bytes: &[u8]) -> Result<(), String> {
    let response = peer::agent()
        .put(&format!("{address}{BLOB_PATH}{root}"))
        .send(bytes)
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        return Ok(());
    }
    let reason = response.into_body().read_to_string().unwrap_or_default();
    Err(format!("the node refused: {reason}"))
}

/// Fetches the blob named `root`, of `size` bytes, from the node at `address`,
/// and checks that what arrived is that blob.
pub fn download(address: &str, root: &ContentHash, size: u64) -> Result<Vec<u8>, String> {
    let response = peer::agent()
        .get(&format!("{address}{BLOB_PATH}{root}"))
        .call()
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "the node answered with status {}",
            response.status()
        ));
    }
    let mut body = response.into_body();
    let bytes = body
        .with_config()
        .limit(size.saturating_add(1))
        .read_to_vec()
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 != size || merkle::root(&bytes) != *root {
        return Err("the node sent something other than the blob".into());
    }
    Ok(bytes)
}
