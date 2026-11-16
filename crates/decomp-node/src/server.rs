//! The web server other devices reach a node through: for the blobs it holds and
//! the channels it relays. Only a node that other devices can connect to runs one.

use std::io;
use std::sync::Arc;
use std::thread;

use tiny_http::{Request, Response, Server};

use crate::relay::{self, Relay};
use crate::store::{self, Store};

/// How many requests the server works on at once. One slow upload or download
/// must not hold up every channel the node relays.
const SERVER_THREADS: usize = 4;

/// Starts the server at the local address `listen`. It runs on its own threads
/// for as long as the program does.
pub fn serve(listen: &str, store: Arc<Store>, relay: Arc<Relay>) -> io::Result<()> {
    let server = Arc::new(Server::http(listen).map_err(io::Error::other)?);
    for _ in 0..SERVER_THREADS {
        let (server, store, relay) = (server.clone(), store.clone(), relay.clone());
        thread::spawn(move || {
            for request in server.incoming_requests() {
                // A device that hangs up mid-answer is its own problem.
                let _ = route(&store, &relay, request);
            }
        });
    }
    Ok(())
}

fn route(store: &Store, relay: &Relay, request: Request) -> io::Result<()> {
    let url = request.url().to_owned();
    if let Some(path) = url.strip_prefix(store::BLOB_PATH) {
        store::answer(store, path, request)
    } else if let Some(path) = url.strip_prefix(relay::CHANNEL_PATH) {
        relay::answer(relay, path, request)
    } else {
        request.respond(Response::from_string("no such address").with_status_code(404))
    }
}
