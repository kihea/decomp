//! A node's work loop, shared by the command-line node and the desktop app:
//! which leased jobs to start, which committed results to reveal, and whether
//! the owner's rules let the node work right now.
//!
//! The job board applies the parts of an owner's work preferences it can see
//! (classes, tiers, rate floor, slots). The rest depend on the device and are
//! applied here: the hours of the day the node works, and whether it works on
//! battery power or on a metered network (spec section 1, "Node agent").

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use decomp_core::{CONFIRMATION_WINDOW_SECS, LEASE_SECS, REVEAL_WINDOW_SECS};
use decomp_core::{JobId, LeasesGranted, WorkPreferences};
use decomp_sandbox::Sandbox;

use crate::client::Client;
use crate::work::{Committed, reveal, work};

/// What the device knows about its own situation, which the job board cannot.
#[derive(Clone, Copy, Debug, Default)]
pub struct Conditions {
    pub on_battery: bool,
    pub on_metered: bool,
}

/// Whether the owner's rules let the node ask for new work now.
pub fn may_work(preferences: &WorkPreferences, conditions: Conditions) -> bool {
    preferences.contributing
        && (preferences.on_battery || !conditions.on_battery)
        && (preferences.on_metered || !conditions.on_metered)
        && in_hours(preferences.hours)
}

/// Whether the local time of day is inside the owner's working hours. Hours
/// that wrap past midnight, such as 22 to 6, work as expected.
fn in_hours(hours: Option<(u8, u8)>) -> bool {
    let Some((start, end)) = hours else {
        return true;
    };
    let hour = jiff::Zoned::now().hour() as u8;
    if start <= end {
        (start..end).contains(&hour)
    } else {
        hour >= start || hour < end
    }
}

/// Something the worker did, for the front end to show or log.
#[derive(Clone, Debug)]
pub enum Event {
    Committed { job_id: JobId, units: u64 },
    Revealed { job_id: JobId },
    Failed { job_id: JobId, error: String },
}

/// How long a committed result is kept for a reveal the job board may still
/// ask for. After that the job has moved on without it.
const KEEP_COMMITTED: Duration =
    Duration::from_secs(LEASE_SECS + CONFIRMATION_WINDOW_SECS + REVEAL_WINDOW_SECS);

/// Runs leased jobs on their own threads and reveals their results when asked.
#[derive(Clone)]
pub struct Worker {
    client: Arc<Client>,
    sandbox: Arc<Sandbox>,
    running: Arc<Mutex<HashSet<JobId>>>,
    committed: Arc<Mutex<HashMap<JobId, (Instant, Committed)>>>,
    events: Arc<Mutex<Vec<Event>>>,
}

impl Worker {
    pub fn new(client: Arc<Client>, sandbox: Arc<Sandbox>) -> Self {
        Self {
            client,
            sandbox,
            running: Arc::default(),
            committed: Arc::default(),
            events: Arc::default(),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Jobs running now.
    pub fn running(&self) -> usize {
        self.running.lock().expect("worker lock").len()
    }

    /// Whether a committed result still waits to be revealed. A node that
    /// stops asking for work before then misses the reveal.
    pub fn has_unrevealed(&self) -> bool {
        !self.committed.lock().expect("worker lock").is_empty()
    }

    /// Acts on what the job board said: starts each leased job that is not
    /// already running or committed, and reveals the results it asks for.
    pub fn handle(&self, granted: &LeasesGranted) {
        {
            let mut committed = self.committed.lock().expect("worker lock");
            committed.retain(|_, (at, _)| at.elapsed() < KEEP_COMMITTED);
        }
        for lease in &granted.leases {
            let job_id = lease.job_id;
            let started = {
                let mut running = self.running.lock().expect("worker lock");
                let known = self
                    .committed
                    .lock()
                    .expect("worker lock")
                    .contains_key(&job_id);
                !known && running.insert(job_id)
            };
            if !started {
                continue;
            }
            let worker = self.clone();
            let lease = lease.clone();
            thread::spawn(move || {
                let event = match work(&worker.client, &worker.sandbox, &lease) {
                    Ok(committed) => {
                        let units = committed.result.units;
                        let entry = (Instant::now(), committed);
                        worker
                            .committed
                            .lock()
                            .expect("worker lock")
                            .insert(job_id, entry);
                        Event::Committed { job_id, units }
                    }
                    Err(error) => Event::Failed {
                        job_id,
                        error: error.to_string(),
                    },
                };
                worker.running.lock().expect("worker lock").remove(&job_id);
                worker.events.lock().expect("worker lock").push(event);
            });
        }
        for job_id in &granted.reveal {
            let committed = self
                .committed
                .lock()
                .expect("worker lock")
                .get(job_id)
                .cloned();
            let Some((_, committed)) = committed else {
                continue;
            };
            let event = match reveal(&self.client, &committed) {
                Ok(()) => {
                    self.committed.lock().expect("worker lock").remove(job_id);
                    Event::Revealed { job_id: *job_id }
                }
                Err(error) => Event::Failed {
                    job_id: *job_id,
                    error: error.to_string(),
                },
            };
            self.events.lock().expect("worker lock").push(event);
        }
    }

    /// What the worker did since this was last called.
    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().expect("worker lock"))
    }
}
