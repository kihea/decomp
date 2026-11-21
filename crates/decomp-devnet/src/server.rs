//! The devnet's HTTP server: the control plane's interface over HTTP, the
//! clock that posts batches, the beacon follower, and the clock that lets
//! time pass. Requests are answered by `decomp_control::handle`, which the
//! canisters answer with too.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use decomp_control::ControlPlane;
use decomp_control::fallback::{self, FallBackRun};
use decomp_core::{BEACON_PERIOD_SECS, MAX_MESSAGE_BYTES};
use decomp_node::beacon::Rounds;
use decomp_sandbox::Sandbox;
use tower_http::cors::CorsLayer;

struct App {
    control: Mutex<ControlPlane>,
    /// For running always-on fallbacks.
    sandbox: Sandbox,
}

/// How often time is let pass for jobs nobody is asking about: windows close,
/// leases lapse, and holds are released.
const TICK_SECS: u64 = 1;

/// The control plane's API, the clock that posts a batch to the settlement log
/// every `batch_period`, the one that fetches each round of public randomness,
/// and the one that lets time pass.
pub fn router(control: ControlPlane, batch_period: Duration) -> Router {
    let app = Arc::new(App {
        control: Mutex::new(control),
        sandbox: Sandbox::new(),
    });
    tokio::spawn(post_batches(Arc::clone(&app), batch_period));
    tokio::spawn(follow_beacon(Arc::clone(&app)));
    tokio::spawn(tick(Arc::clone(&app)));
    Router::new()
        .route("/v1/ops", post(answer))
        .route("/v1/request", post(answer))
        .layer(DefaultBodyLimit::max(MAX_MESSAGE_BYTES))
        // Requests carry no cookies or credentials, so any web origin may call.
        .layer(CorsLayer::permissive())
        .with_state(app)
}

/// Fetches the beacon's newest round every time there is a new one. Nodes are
/// drawn for jobs from it (spec section 6: "Assignment draws on public
/// randomness"), so while the beacon cannot be reached, jobs wait.
async fn follow_beacon(app: Arc<App>) {
    let rounds = Arc::new(Rounds::default());
    let mut ticks = tokio::time::interval(Duration::from_secs(BEACON_PERIOD_SECS));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reached = true;
    loop {
        ticks.tick().await;
        let beacon = Arc::clone(&rounds);
        let fetched = tokio::task::spawn_blocking(move || beacon.latest()).await;
        match fetched.expect("fetching a round does not panic") {
            Ok(beacon) => {
                let mut control = app.control.lock().expect("control plane lock");
                control.set_beacon(beacon);
                reached = true;
            }
            // Said once each time the beacon is lost, not every three seconds.
            Err(error) if reached => {
                eprintln!("no public randomness, so jobs wait: {error}");
                reached = false;
            }
            Err(_) => {}
        }
    }
}

async fn post_batches(app: Arc<App>, period: Duration) {
    let mut ticks = tokio::time::interval(period);
    loop {
        ticks.tick().await;
        let mut control = app.control.lock().expect("control plane lock");
        control.post_batch(unix_now());
    }
}

async fn tick(app: Arc<App>) {
    let mut ticks = tokio::time::interval(Duration::from_secs(TICK_SECS));
    loop {
        ticks.tick().await;
        let work = {
            let mut control = app.control.lock().expect("control plane lock");
            control.settle(unix_now());
            control.take_fall_backs()
        };
        run_aside(&app, work);
    }
}

/// A fallback runs the job, which can take as long as the job did, so it
/// happens off to the side and the control plane stays free.
fn run_aside(app: &Arc<App>, fall_backs: Vec<(decomp_core::JobId, FallBackRun)>) {
    for (job_id, run) in fall_backs {
        let app = Arc::clone(app);
        tokio::task::spawn_blocking(move || {
            let (result, produced) = fallback::run(&app.sandbox, &run, job_id);
            let mut control = app.control.lock().expect("control plane lock");
            control.fell_back(job_id, result, produced, unix_now());
        });
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

/// Answers either call: the control plane decides from the path.
async fn answer(State(app): State<Arc<App>>, uri: Uri, body: Bytes) -> Response {
    let now = unix_now();
    let (answer, work) = {
        let mut control = app.control.lock().expect("control plane lock");
        let answer = decomp_control::handle(&mut control, uri.path(), &body, now);
        // Any call can find that a job's window or deadline has passed, so
        // settle after each.
        control.settle(now);
        (answer, control.take_fall_backs())
    };
    run_aside(&app, work);
    let status = StatusCode::from_u16(answer.status.0).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, [("content-type", "application/json")], answer.body).into_response()
}
