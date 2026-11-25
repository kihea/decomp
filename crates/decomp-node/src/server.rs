//! The web server other devices reach a node through: for the blobs it holds and
//! the channels it relays. Only a node that other devices can connect to runs one.

use decomp_core::{ContentHash, PROBE_BYTES, PROBE_PATH, STAGE_PATH, probe};
use std::io::{self, Read};
use std::sync::Arc;
use std::thread;

use tiny_http::{Header, Method, Request, Response, Server};

use crate::relay::{self, Relay};
use crate::stages::{self, StageService};
use crate::store::{self, Store};

/// How many requests the server works on at once. One slow upload or download
/// must not hold up every channel the node relays.
const SERVER_THREADS: usize = 4;

/// Starts the server at the local address `listen`. It runs on its own threads
/// for as long as the program does.
pub fn serve(
    listen: &str,
    store: Arc<Store>,
    relay: Arc<Relay>,
    stages: Arc<StageService>,
) -> io::Result<()> {
    let server = Arc::new(Server::http(listen).map_err(io::Error::other)?);
    for _ in 0..SERVER_THREADS {
        let (server, store, relay, stages) =
            (server.clone(), store.clone(), relay.clone(), stages.clone());
        thread::spawn(move || {
            for request in server.incoming_requests() {
                // A device that hangs up mid-answer is its own problem.
                let _ = route(&store, &relay, &stages, request);
            }
        });
    }
    Ok(())
}

/// Headers that let a web page call this server. Requests carry no cookies or
/// credentials, and what a caller may do is decided by what it has signed, so any
/// web origin may call.
const WEB_ACCESS: [(&str, &str); 3] = [
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Methods", "GET, PUT, POST"),
    ("Access-Control-Allow-Headers", "content-type"),
];

/// Answers a request. Every answer goes through here so that a web page can
/// read it.
pub(crate) fn respond<R: Read>(request: Request, response: Response<R>) -> io::Result<()> {
    let response = WEB_ACCESS.iter().fold(response, |response, (name, value)| {
        let header = Header::from_bytes(*name, *value).expect("the header is plain text");
        response.with_header(header)
    });
    request.respond(response)
}

/// Serves test data for a bandwidth measurement: `<seed>/<bytes>`, at most
/// `PROBE_BYTES` of it.
fn serve_probe(path: &str, request: Request) -> io::Result<()> {
    let asked = path.split_once('/').and_then(|(seed, bytes)| {
        let seed = seed.parse::<ContentHash>().ok()?;
        let bytes = bytes
            .parse::<u64>()
            .ok()
            .filter(|bytes| *bytes <= PROBE_BYTES)?;
        Some((seed, bytes))
    });
    match asked {
        Some((seed, bytes)) => respond(request, Response::from_data(probe::data(&seed, bytes))),
        None => respond(
            request,
            Response::from_string("not a measurement").with_status_code(400),
        ),
    }
}

fn route(store: &Store, relay: &Relay, stages: &StageService, request: Request) -> io::Result<()> {
    // A browser asks first whether a page may make the request it is about to.
    if *request.method() == Method::Options {
        return respond(request, Response::empty(204));
    }
    let url = request.url().to_owned();
    if let Some(path) = url.strip_prefix(store::BLOB_PATH) {
        store::answer(store, path, request)
    } else if let Some(path) = url.strip_prefix(relay::CHANNEL_PATH) {
        relay::answer(relay, path, request)
    } else if let Some(path) = url.strip_prefix(PROBE_PATH) {
        serve_probe(path, request)
    } else if url == STAGE_PATH {
        stages::answer(stages, request)
    } else {
        respond(
            request,
            Response::from_string("no such address").with_status_code(404),
        )
    }
}
