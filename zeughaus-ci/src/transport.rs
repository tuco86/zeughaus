//! What every weida service of this process shares: who a peer is, which of
//! the two kinds of peer it is, and the snapshot answer.
//!
//! The listener trusts two sets of keys: editors (`clients/*.pem`) and the
//! runners `ci.toml` names. A runner may enlist and move artifacts and
//! nothing else; an editor may read pipelines and transcripts and nothing
//! else. Each service checks the kind itself, because the handshake only
//! knows "trusted".

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use weida::{IncomingMeta, PeerIdentity, Replier, TransferMeta};
use zeughaus_link::Snapshot;

use crate::config::CiConfig;

/// The name this process shows for a peer: the fingerprint it proved.
pub fn principal_of(meta: &IncomingMeta) -> Option<String> {
    match &meta.peer {
        Some(PeerIdentity::Key(fp)) => Some(fp.to_string()),
        Some(other) => Some(format!("{other:?}")),
        None => None,
    }
}

/// The principal of an exchange that an editor may use: it proved an
/// identity and that identity is not a runner's. `Err` says why not.
pub fn editor_principal(config: &CiConfig, meta: &IncomingMeta) -> Result<String, &'static str> {
    let principal = principal_of(meta).ok_or("proved no identity")?;
    if config.is_runner_fingerprint(&principal) {
        return Err("is a runner, which may only enlist");
    }
    Ok(principal)
}

/// Answers snapshot requests. The CI has no outputs or errors; the editor
/// only needs the sequence number that its `/events` stream continues from.
///
/// The request body carries nothing and is dropped unread.
pub async fn serve_snapshots(replier: Replier, config: Arc<CiConfig>, seq: Arc<AtomicU64>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[ci] stopped serving snapshots: {e}");
                return;
            }
        };
        if let Err(why) = editor_principal(&config, request.meta()) {
            eprintln!("[ci] refused a snapshot request from a peer that {why}");
            request.refuse(weida::ErrorCode::Rejected).await;
            continue;
        }
        drop(request.take_body());
        let encoded = snapshot(seq.load(Ordering::SeqCst)).encode();
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot reply to a snapshot request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&encoded).await {
            eprintln!("[ci] cannot write a snapshot: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[ci] cannot finish a snapshot: {e}");
        }
    }
}

fn snapshot(seq: u64) -> Snapshot {
    Snapshot {
        seq,
        machine: None,
        ..Snapshot::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_snapshot_carries_only_the_sequence() {
        let snapshot = snapshot(7);
        assert_eq!(snapshot.seq, 7);
        assert!(snapshot.outputs.is_empty() && snapshot.errors.is_empty());
        assert!(snapshot.rejections.is_empty() && snapshot.machine.is_none());
    }
}
