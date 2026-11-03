//! Asks: an agent's open question, kept in memory while its waiting process
//! polls it within [`ASK_LEASE_SECS`].

use zeughaus_link::{
    ASK_LEASE_SECS, AskQuestion, AskReply, AskView, ClaimHolder, check_questions, check_replies,
};

/// Posts an ask for `holder`, replacing its earlier one; the new ask gets
/// the next id.
#[allow(clippy::too_many_arguments)]
pub fn post(
    asks: &mut Vec<AskView>,
    next: &mut u64,
    holder: ClaimHolder,
    repo: String,
    doc: Option<u64>,
    label: String,
    questions: Vec<AskQuestion>,
    now: u64,
) -> Result<AskView, String> {
    check_questions(&questions)?;
    asks.retain(|a| !a.holder.same(&holder));
    *next += 1;
    let ask = AskView {
        id: *next,
        holder,
        repo,
        doc,
        label,
        questions,
        asked: now,
        polled: now,
        answer: None,
    };
    asks.push(ask.clone());
    Ok(ask)
}

/// Renews ask `id` of `holder` and returns it.
pub fn poll(
    asks: &mut [AskView],
    id: u64,
    holder: &ClaimHolder,
    now: u64,
) -> Result<AskView, String> {
    let ask = asks
        .iter_mut()
        .find(|a| a.id == id && a.holder.same(holder))
        .ok_or_else(|| format!("no ask {id}"))?;
    ask.polled = now;
    Ok(ask.clone())
}

/// Stores the answer to ask `id`, once.
pub fn answer(asks: &mut [AskView], id: u64, replies: Vec<AskReply>) -> Result<(), String> {
    let ask = asks
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("no ask {id}"))?;
    if ask.answer.is_some() {
        return Err(format!("ask {id} is answered already"));
    }
    check_replies(&ask.questions, &replies)?;
    ask.answer = Some(replies);
    Ok(())
}

/// Drops the ask of `holder`; `true` when there was one.
pub fn close(asks: &mut Vec<AskView>, holder: &ClaimHolder) -> bool {
    let before = asks.len();
    asks.retain(|a| !a.holder.same(holder));
    asks.len() != before
}

/// Drops the asks nobody polled within the lease; `true` when one was
/// dropped.
pub fn expire(asks: &mut Vec<AskView>, now: u64) -> bool {
    let before = asks.len();
    asks.retain(|a| now.saturating_sub(a.polled) < ASK_LEASE_SECS);
    asks.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeughaus_link::AskOption;

    fn holder(terminal: u64) -> ClaimHolder {
        ClaimHolder {
            runner: "atik".to_owned(),
            terminal,
            session: None,
        }
    }

    fn questions() -> Vec<AskQuestion> {
        vec![AskQuestion {
            id: "db".to_owned(),
            question: "Database?".to_owned(),
            options: ["SQLite", "Postgres"]
                .into_iter()
                .map(|label| AskOption {
                    label: label.to_owned(),
                    description: None,
                    preview: None,
                })
                .collect(),
            multi: false,
            recommended: None,
            header: None,
        }]
    }

    fn reply(label: &str) -> Vec<AskReply> {
        vec![AskReply {
            id: "db".to_owned(),
            selected: vec![label.to_owned()],
            custom: None,
        }]
    }

    fn posted(asks: &mut Vec<AskView>, next: &mut u64, terminal: u64, now: u64) -> AskView {
        post(
            asks,
            next,
            holder(terminal),
            "zeughaus".to_owned(),
            None,
            String::new(),
            questions(),
            now,
        )
        .unwrap()
    }

    #[test]
    fn a_second_post_of_the_terminal_replaces_the_first() {
        let (mut asks, mut next) = (Vec::new(), 0);
        let first = posted(&mut asks, &mut next, 1, 10);
        let second = posted(&mut asks, &mut next, 1, 11);
        assert_ne!(first.id, second.id);
        assert_eq!(asks.iter().map(|a| a.id).collect::<Vec<_>>(), [second.id]);
    }

    #[test]
    fn invalid_questions_are_not_posted() {
        let (mut asks, mut next) = (Vec::new(), 0);
        let refused = post(
            &mut asks,
            &mut next,
            holder(1),
            String::new(),
            None,
            String::new(),
            Vec::new(),
            0,
        );
        assert_eq!(refused, Err("an ask has at least one question".to_owned()));
        assert!(asks.is_empty());
    }

    #[test]
    fn another_terminal_cannot_poll_an_ask() {
        let (mut asks, mut next) = (Vec::new(), 0);
        let ask = posted(&mut asks, &mut next, 1, 10);
        assert_eq!(
            poll(&mut asks, ask.id, &holder(2), 12),
            Err(format!("no ask {}", ask.id))
        );
        assert_eq!(poll(&mut asks, ask.id, &holder(1), 12).unwrap().polled, 12);
    }

    #[test]
    fn an_answer_is_stored_once() {
        let (mut asks, mut next) = (Vec::new(), 0);
        let ask = posted(&mut asks, &mut next, 1, 10);
        assert_eq!(answer(&mut asks, ask.id, reply("Postgres")), Ok(()));
        assert_eq!(
            answer(&mut asks, ask.id, reply("SQLite")),
            Err(format!("ask {} is answered already", ask.id))
        );
        let polled = poll(&mut asks, ask.id, &holder(1), 11).unwrap();
        assert_eq!(polled.answer, Some(reply("Postgres")));
    }

    #[test]
    fn an_invalid_answer_leaves_the_ask_open() {
        let (mut asks, mut next) = (Vec::new(), 0);
        let ask = posted(&mut asks, &mut next, 1, 10);
        assert_eq!(
            answer(&mut asks, ask.id, reply("MySQL")),
            Err("\"MySQL\" is no option of db".to_owned())
        );
        assert_eq!(asks[0].answer, None);
    }

    #[test]
    fn an_ask_expires_at_the_end_of_its_lease() {
        let (mut asks, mut next) = (Vec::new(), 0);
        posted(&mut asks, &mut next, 1, 100);
        assert!(!expire(&mut asks, 100 + ASK_LEASE_SECS - 1));
        assert_eq!(asks.len(), 1);
        assert!(expire(&mut asks, 100 + ASK_LEASE_SECS));
        assert!(asks.is_empty());
    }

    #[test]
    fn close_drops_only_the_holders_ask() {
        let (mut asks, mut next) = (Vec::new(), 0);
        posted(&mut asks, &mut next, 1, 10);
        let other = posted(&mut asks, &mut next, 2, 10);
        assert!(close(&mut asks, &holder(1)));
        assert!(!close(&mut asks, &holder(1)));
        assert_eq!(asks.iter().map(|a| a.id).collect::<Vec<_>>(), [other.id]);
    }
}
