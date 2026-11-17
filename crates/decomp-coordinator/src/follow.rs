//! A second operator's coordinator (spec section 6: "Several staked operators
//! run coordinators and audit each other's batches during a dispute window").
//!
//! Started with `DECOMP_FOLLOW`, the coordinator takes no jobs. It follows
//! another operator's coordinator: its settlement log, the records behind each
//! batch, and the draws it publishes. It works every batch out again from the
//! records, disputes one that does not hold, and works every draw out again.
//! It keeps nothing between runs, and starts again from the log's first entry.

use std::error::Error;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use decomp_core::{Auditor, BatchRecords, Envelope, Finding, MAX_LOG_PAGE, Through};
use decomp_node::beacon::Rounds;
use decomp_node::client::{Client, ClientError};

use crate::operator;

/// How often a following coordinator asks for new log entries and draws. It
/// must be well under the dispute window, or a bad batch could go undisputed.
const FOLLOW_POLL: Duration = Duration::from_secs(10);

/// Where the audit has got to.
#[derive(Default)]
struct Followed {
    auditor: Auditor,
    /// How many log entries and published draws have been followed.
    entries: u64,
    draws: u64,
    rounds: Rounds,
    /// Batches found not to hold, which are still to be disputed.
    findings: Vec<Finding>,
}

/// Audits the coordinator at `url` until stopped. Returns only if the log or
/// the records behind it cannot be followed at all.
pub fn run(url: &str, data_dir: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::create_dir_all(data_dir)?;
    let client = Client::new(url, operator::key(data_dir)?);
    println!("auditing the coordinator at {url}; this coordinator takes no jobs");
    println!("operator key: {}", client.device_key());

    let mut followed = Followed::default();
    loop {
        match follow(&client, &mut followed) {
            Ok(()) => {}
            Err(Stop::Retry(error)) => eprintln!("{error}; retrying"),
            Err(Stop::Unfollowable(error)) => return Err(error),
        }
        sleep(FOLLOW_POLL);
    }
}

enum Stop {
    /// The other coordinator could not be reached, or refused this operator.
    Retry(ClientError),
    /// The log or its records do not fit together, so nothing after this
    /// point can be audited.
    Unfollowable(Box<dyn Error>),
}

impl From<ClientError> for Stop {
    fn from(error: ClientError) -> Self {
        Self::Retry(error)
    }
}

/// Follows whatever is new: log entries, then disputes to raise, then draws.
fn follow(client: &Client, followed: &mut Followed) -> Result<(), Stop> {
    loop {
        let page = client.log_page(followed.entries)?.entries;
        for entry in &page {
            audit(client, followed, entry)?;
            followed.entries += 1;
        }
        if page.len() < MAX_LOG_PAGE {
            break;
        }
    }

    while let Some(finding) = followed.findings.first() {
        let batch = finding.batch;
        // Disputed already, by this operator in an earlier run or by another.
        if !followed.auditor.log().is_disputed(batch) {
            let (fingerprint, changes) = (finding.fingerprint, finding.changes.clone());
            match client.dispute(batch, fingerprint, changes) {
                Ok(()) => println!("batch {batch}: disputed, and the dispute was upheld"),
                Err(error @ ClientError::Refused { .. }) => {
                    println!("batch {batch}: the dispute was refused: {error}");
                }
                Err(error) => return Err(error.into()),
            }
        }
        followed.findings.remove(0);
    }

    let Some(operator) = followed.auditor.log().operator() else {
        return Ok(());
    };
    loop {
        let page = client.draws(followed.draws)?.entries;
        for signed in &page {
            if let Err(fault) = followed.rounds.check(signed, &operator) {
                println!("draw {} does not hold: {fault}", followed.draws);
            }
            followed.draws += 1;
        }
        if !page.is_empty() {
            println!("{} draws followed", followed.draws);
        }
        if page.len() < MAX_LOG_PAGE {
            return Ok(());
        }
    }
}

/// Audits one entry of the log, fetching the records behind it first.
fn audit(client: &Client, followed: &mut Followed, entry: &Envelope) -> Result<(), Stop> {
    let unfollowable = |error: Box<dyn Error>| Stop::Unfollowable(error);
    let reach = Auditor::reach(entry).map_err(|error| unfollowable(error.into()))?;
    let records = match reach {
        Some(through) => records(client, followed.auditor.log().through(), through)?,
        None => BatchRecords::default(),
    };
    let audited = followed.auditor.audit(entry, &records);
    match audited.map_err(|error| unfollowable(error.into()))? {
        Some(finding) => {
            println!("batch {} does not hold: {}", finding.batch, finding.reason);
            followed.findings.push(finding);
        }
        None => match reach {
            Some(_) => println!(
                "batch {} holds: {} receipts",
                followed.auditor.log().batches().len() - 1,
                records.receipts.len()
            ),
            None => println!("entry {} is an upheld dispute", followed.entries),
        },
    }
    Ok(())
}

/// The operator's records from `from` up to `to`, fetched a page at a time.
/// If the operator has fewer than the batch reaches to, what there is.
fn records(client: &Client, mut from: Through, to: Through) -> Result<BatchRecords, ClientError> {
    let mut all = BatchRecords::default();
    loop {
        let mut page = client.records(from)?;
        page.ops.truncate((to.ops - from.ops) as usize);
        page.entries.truncate((to.entries - from.entries) as usize);
        page.receipts
            .truncate((to.receipts - from.receipts) as usize);
        let got = page.ops.len() + page.entries.len() + page.receipts.len();
        from.ops += page.ops.len() as u64;
        from.entries += page.entries.len() as u64;
        from.receipts += page.receipts.len() as u64;
        all.ops.append(&mut page.ops);
        all.entries.append(&mut page.entries);
        all.receipts.append(&mut page.receipts);
        if got == 0 || from == to {
            return Ok(all);
        }
    }
}
