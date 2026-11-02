//! Claims: which agent terminal is responsible for a proposal or a
//! pipeline. Pure functions over the registry the sessions book keeps; a
//! claim lives while its holder renews it within [`CLAIM_LEASE_SECS`].

use zeughaus_link::{CLAIM_LEASE_SECS, ClaimHolder, ClaimSubject, ClaimView};

/// Takes `subject` for `holder`. A live claim of another holder is answered
/// unchanged with `false`; the holder's own claim is renewed and `true`; a
/// free subject gets a new claim and `true`.
pub fn take(
    claims: &mut Vec<ClaimView>,
    subject: ClaimSubject,
    holder: ClaimHolder,
    label: String,
    channel: String,
    now: u64,
) -> (ClaimView, bool) {
    if let Some(claim) = claims.iter_mut().find(|claim| claim.subject == subject) {
        if !claim.holder.same(&holder) {
            return (claim.clone(), false);
        }
        claim.renewed = now;
        claim.label = label;
        claim.holder.session = holder.session;
        return (claim.clone(), true);
    }
    let claim = ClaimView {
        subject,
        holder,
        label,
        channel,
        since: now,
        renewed: now,
    };
    claims.push(claim.clone());
    (claim, true)
}

/// Renews every claim of `holder`; how many it has.
pub fn renew(claims: &mut [ClaimView], holder: &ClaimHolder, now: u64) -> usize {
    let mut count = 0;
    for claim in claims.iter_mut().filter(|claim| claim.holder.same(holder)) {
        claim.renewed = now;
        count += 1;
    }
    count
}

/// Drops `subject` of `holder`, or every claim of `holder` without one;
/// `true` when one was dropped.
pub fn release(
    claims: &mut Vec<ClaimView>,
    holder: &ClaimHolder,
    subject: Option<&ClaimSubject>,
) -> bool {
    let before = claims.len();
    claims.retain(|claim| {
        !(claim.holder.same(holder) && subject.is_none_or(|s| *s == claim.subject))
    });
    claims.len() != before
}

/// Drops the claims whose lease ran out; `true` when one was dropped.
pub fn expire(claims: &mut Vec<ClaimView>, now: u64) -> bool {
    let before = claims.len();
    claims.retain(|claim| claim.renewed + CLAIM_LEASE_SECS > now);
    claims.len() != before
}

/// Drops the claims of session `id`, and those of its terminal on `runner`;
/// `true` when one was dropped.
pub fn release_session(
    claims: &mut Vec<ClaimView>,
    id: u64,
    runner: &str,
    terminal: Option<u64>,
) -> bool {
    let before = claims.len();
    claims.retain(|claim| {
        claim.holder.session != Some(id)
            && !(claim.holder.runner == runner && Some(claim.holder.terminal) == terminal)
    });
    claims.len() != before
}

/// Pipeline `number` of `repo` in `channel` ended: a success settles every
/// claim of the channel up to it, any other end only its own claim. `true`
/// when one was dropped.
pub fn pipeline_ended(
    claims: &mut Vec<ClaimView>,
    repo: &str,
    channel: &str,
    number: u64,
    succeeded: bool,
) -> bool {
    let before = claims.len();
    claims.retain(|claim| {
        let ClaimSubject::Pipeline { repo: r, number: n } = &claim.subject else {
            return true;
        };
        if r != repo {
            return true;
        }
        if succeeded {
            !(claim.channel == channel && *n <= number)
        } else {
            *n != number
        }
    });
    claims.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(terminal: u64) -> ClaimHolder {
        ClaimHolder {
            runner: "atik".into(),
            terminal,
            session: None,
        }
    }

    fn pipeline(number: u64) -> ClaimSubject {
        ClaimSubject::Pipeline {
            repo: "demo".into(),
            number,
        }
    }

    fn proposal(number: u64) -> ClaimSubject {
        ClaimSubject::Ticket {
            repo: "demo".into(),
            number,
        }
    }

    fn numbers(claims: &[ClaimView]) -> Vec<u64> {
        claims.iter().map(|c| c.subject.number()).collect()
    }

    #[test]
    fn the_first_holder_keeps_a_subject_and_renews_it_by_taking_it_again() {
        let mut claims = Vec::new();
        let (first, ours) = take(
            &mut claims,
            proposal(3),
            holder(1),
            String::new(),
            String::new(),
            10,
        );
        assert!(ours);
        assert_eq!((first.since, first.renewed), (10, 10));
        let (theirs, ours) = take(
            &mut claims,
            proposal(3),
            holder(2),
            "plan demo #3".into(),
            String::new(),
            11,
        );
        assert!(!ours);
        assert_eq!(theirs, first);
        let again = ClaimHolder {
            session: Some(5),
            ..holder(1)
        };
        let (renewed, ours) = take(
            &mut claims,
            proposal(3),
            again,
            "plan demo #3".into(),
            String::new(),
            12,
        );
        assert!(ours);
        assert_eq!(
            (
                renewed.since,
                renewed.renewed,
                renewed.label.as_str(),
                renewed.holder.session
            ),
            (10, 12, "plan demo #3", Some(5))
        );
        assert_eq!(claims.len(), 1);
    }

    #[test]
    fn a_claim_lives_while_it_is_renewed_within_its_lease() {
        let mut claims = Vec::new();
        take(
            &mut claims,
            pipeline(1),
            holder(1),
            String::new(),
            "dev".into(),
            0,
        );
        take(
            &mut claims,
            pipeline(2),
            holder(2),
            String::new(),
            "dev".into(),
            0,
        );
        assert_eq!(renew(&mut claims, &holder(1), 200), 1);
        assert!(!expire(&mut claims, CLAIM_LEASE_SECS - 1));
        assert!(expire(&mut claims, CLAIM_LEASE_SECS));
        assert_eq!(numbers(&claims), vec![1]);
        assert!(expire(&mut claims, 200 + CLAIM_LEASE_SECS));
        assert!(claims.is_empty());
    }

    #[test]
    fn a_holder_releases_one_subject_or_everything() {
        let mut claims = Vec::new();
        for n in 1..=3 {
            take(
                &mut claims,
                pipeline(n),
                holder(1),
                String::new(),
                "dev".into(),
                0,
            );
        }
        take(
            &mut claims,
            proposal(9),
            holder(2),
            String::new(),
            String::new(),
            0,
        );
        assert!(release(&mut claims, &holder(1), Some(&pipeline(2))));
        assert!(!release(&mut claims, &holder(2), Some(&pipeline(1))));
        assert_eq!(numbers(&claims), vec![1, 3, 9]);
        assert!(release(&mut claims, &holder(1), None));
        assert_eq!(numbers(&claims), vec![9]);
    }

    #[test]
    fn an_ended_session_takes_its_claims_and_its_terminals_along() {
        let mut claims = Vec::new();
        let in_session = ClaimHolder {
            session: Some(7),
            ..holder(1)
        };
        take(
            &mut claims,
            pipeline(1),
            in_session,
            String::new(),
            "dev".into(),
            0,
        );
        take(
            &mut claims,
            pipeline(2),
            holder(4),
            String::new(),
            "dev".into(),
            0,
        );
        take(
            &mut claims,
            pipeline(3),
            holder(5),
            String::new(),
            "dev".into(),
            0,
        );
        assert!(release_session(&mut claims, 7, "atik", Some(4)));
        assert_eq!(numbers(&claims), vec![3]);
        assert!(!release_session(&mut claims, 8, "sadala", Some(5)));
    }

    #[test]
    fn a_success_settles_the_channel_up_to_it_and_a_cancel_only_itself() {
        let mut claims = Vec::new();
        for (n, channel) in [(1, "dev"), (2, "nightly"), (3, "dev"), (5, "dev")] {
            take(
                &mut claims,
                pipeline(n),
                holder(n),
                String::new(),
                channel.into(),
                0,
            );
        }
        assert!(pipeline_ended(&mut claims, "demo", "dev", 5, false));
        assert_eq!(numbers(&claims), vec![1, 2, 3]);
        assert!(!pipeline_ended(&mut claims, "other", "dev", 9, true));
        assert!(pipeline_ended(&mut claims, "demo", "dev", 4, true));
        assert_eq!(numbers(&claims), vec![2]);
    }
}
