//! `/activity`: what this runner's terminals are doing, live and archived.
//!
//! The live part comes from the mux (running and exited terminals), the
//! rest from the run directories under `<state-dir>/runs/` (every closed
//! shell, session and run that wrote its `activity.json`). An editor asks
//! once and follows the `activity` events from then on.

use std::sync::Arc;

use weida::{Replier, TransferMeta};
use zeughaus_link::activity::MAX_ACTIVITY_REQUEST_BYTES;
use zeughaus_link::{
    ActivityEntry, ActivityReply, ActivityRequest, ActivityState, MAX_ACTIVITY_ENTRIES,
};

use crate::mux::MuxService;
use crate::runs::{RunDirs, read_activity};

/// Answers activity requests until the replier goes away.
pub async fn serve(replier: Replier, mux: MuxService, runs: Arc<RunDirs>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[runner] stopped serving activity: {e}");
                return;
            }
        };
        let asked = match request
            .take_body()
            .collect(MAX_ACTIVITY_REQUEST_BYTES)
            .await
        {
            Ok(asked) => asked,
            Err(e) => {
                eprintln!("[runner] unreadable activity request: {e}");
                continue;
            }
        };
        let entries = match ActivityRequest::decode(&asked) {
            Some(asked) => {
                let mux = mux.clone();
                let runs = Arc::clone(&runs);
                let limit = asked.limit.min(MAX_ACTIVITY_ENTRIES) as usize;
                tokio::task::spawn_blocking(move || listing(&mux, &runs, limit))
                    .await
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        let answer = ActivityReply { entries }.encode();
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[runner] cannot reply to an activity request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&answer).await {
            eprintln!("[runner] cannot write an activity reply: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[runner] cannot finish an activity reply: {e}");
        }
    }
}

/// The live terminals, newest first, then the newest archived runs, at most
/// `limit` in all. A run whose terminal is still live is listed once, live.
fn listing(mux: &MuxService, runs: &RunDirs, limit: usize) -> Vec<ActivityEntry> {
    let mut entries = mux.activity();
    entries.truncate(limit);
    for id in runs.ids() {
        if entries.len() >= limit {
            break;
        }
        if entries.iter().any(|e| e.run == Some(id)) {
            continue;
        }
        let dir = runs.dir(id);
        let Some(activity) = read_activity(&dir) else {
            continue;
        };
        // A run without an exit record is still going, or was cut short
        // with a runner; only the mux knows the first.
        let Some(end) = zeughaus_job::read_run_end(&dir) else {
            continue;
        };
        let (started, ended) = times(&dir);
        entries.push(ActivityEntry {
            terminal: activity.terminal,
            run: Some(id),
            kind: activity.kind,
            label: activity.label,
            state: ActivityState::Closed,
            started,
            ended,
            code: end.code,
            killed: end.killed,
            agent: None,
        });
    }
    entries
}

/// `started=` and `finished=` of a run's exit record.
fn times(dir: &std::path::Path) -> (u64, Option<u64>) {
    let text = std::fs::read_to_string(dir.join("exit")).unwrap_or_default();
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.parse::<u64>().ok())
    };
    (field("started=").unwrap_or(0), field("finished="))
}
