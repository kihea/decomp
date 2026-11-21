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
use std::thread::sleep;
use std::time::Duration;

use decomp_core::{ContentHash, Placement, StoreOrder, merkle};
use tiny_http::{Method, Request, Response};

use crate::client::Client;
use crate::peer;
use crate::server::respond;

/// Where a node's server keeps blobs, in its web addresses.
pub(crate) const BLOB_PATH: &str = "/v1/stored/";

/// How long a device keeps a job's module, input, and blobs to read on
/// storage nodes: long enough for the job, the checks that may follow it, and
/// its reissue as a known-answer job. A starting value.
pub const JOB_DATA_DAYS: u32 = 7;

/// How long `put` waits for a node to say it holds a blob it took in, and how
/// often it asks. Nodes say so when they next ask for work, seconds apart.
const HELD_WAIT: Duration = Duration::from_secs(30);
const HELD_POLL: Duration = Duration::from_millis(500);

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

    /// Deletes a blob whose deal ended early. A blob that cannot be deleted
    /// now goes when its paid time runs out.
    pub fn drop_blob(&self, root: &ContentHash) {
        let _ = fs::remove_file(self.path(root));
        self.orders.lock().expect("store lock").remove(root);
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

    /// Keeps a blob this node fetched itself, to replace a lost replica. It
    /// must be one the node was asked to hold, and must match its root.
    pub fn keep(&self, root: &ContentHash, bytes: &[u8]) {
        let _ = self.accept(root, &mut &bytes[..]);
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
        return respond(
            request,
            Response::from_string("no such address").with_status_code(404),
        );
    };
    match request.method() {
        Method::Get => match store.read(&root) {
            Ok(bytes) => respond(request, Response::from_data(bytes)),
            Err(_) => respond(
                request,
                Response::from_string("no such blob").with_status_code(404),
            ),
        },
        Method::Put => match store.accept(&root, request.as_reader()) {
            Ok(()) => respond(request, Response::empty(204)),
            Err(reason) => respond(request, Response::from_string(reason).with_status_code(409)),
        },
        _ => respond(
            request,
            Response::from_string("not supported").with_status_code(405),
        ),
    }
}

/// How many times, and how far apart, a blob is offered to a node that is not
/// yet ready to take it. A node refuses a blob until it has heard from the
/// coordinator that it should hold it, which it does at its next check-in.
const UPLOAD_ATTEMPTS: u32 = 15;
const UPLOAD_RETRY: Duration = Duration::from_secs(1);

/// Sends a blob to each node chosen to hold it. Returns how many took it.
pub fn upload_to(placement: &Placement, root: &ContentHash, bytes: &[u8]) -> usize {
    let took = |address: &String| {
        (0..UPLOAD_ATTEMPTS).any(|attempt| {
            if attempt > 0 {
                sleep(UPLOAD_RETRY);
            }
            upload(address, root, bytes).is_ok()
        })
    };
    placement
        .addresses
        .iter()
        .filter(|address| took(address))
        .count()
}

/// Stores `bytes` on storage nodes for `days` days, paid by this device's
/// account, and returns their root once a node has said it holds them, so
/// that from then on anyone can fetch them. A blob is stored once whoever
/// stores it: if it already is, it is not stored again.
pub fn put(client: &Client, bytes: &[u8], days: u32) -> Result<ContentHash, String> {
    let root = merkle::root(bytes);
    match client.store_blob(root, bytes.len() as u64, days, None) {
        Ok(placement) => {
            if upload_to(&placement, &root, bytes) == 0 {
                return Err("no node chosen to hold the blob took it".into());
            }
        }
        Err(error) if error.is("already_stored") => {}
        Err(error) => return Err(error.to_string()),
    }
    let started = std::time::Instant::now();
    loop {
        if client
            .locate(root)
            .is_ok_and(|located| !located.addresses.is_empty())
        {
            return Ok(root);
        }
        if started.elapsed() >= HELD_WAIT {
            return Err(format!("no node said it holds {root} in time"));
        }
        sleep(HELD_POLL);
    }
}

/// Fetches a blob from the first storage node that gives a true copy of it.
/// A blob no storage node holds may be one the control plane keeps for
/// always-on work, such as what a job that fell back produced; it is fetched
/// from there.
pub fn fetch(client: &Client, root: ContentHash) -> Result<Vec<u8>, String> {
    let located = match client.locate(root) {
        Ok(located) if !located.addresses.is_empty() => located,
        _ => return client.get_blob(root).map_err(|error| error.to_string()),
    };
    let mut reasons = Vec::new();
    for address in &located.addresses {
        match download(address, &root, located.size) {
            Ok(bytes) => return Ok(bytes),
            Err(reason) => reasons.push(format!("{address}: {reason}")),
        }
    }
    reasons.push("no node that holds the blob gave it".into());
    Err(reasons.join("; "))
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
