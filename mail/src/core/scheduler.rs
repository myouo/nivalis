use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::store::sqlite::{AccountGeneration, AccountId};

const MAX_TARGETS: usize = 64;
const SUCCESS_INTERVAL: Duration = Duration::from_secs(5 * 60);
const INITIAL_FAILURE_DELAY: Duration = Duration::from_secs(30);
const MAX_FAILURE_DELAY: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SyncToken {
    account_id: AccountId,
    generation: AccountGeneration,
    nonce: u64,
}

impl SyncToken {
    pub(super) fn account_id(self) -> AccountId {
        self.account_id
    }

    pub(super) fn generation(self) -> AccountGeneration {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SyncCompletion {
    Complete,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchedulerError {
    TooManyTargets,
    TokenExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetReadiness {
    Waiting(Instant),
    InFlight(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TargetState {
    account_id: AccountId,
    generation: AccountGeneration,
    consecutive_failures: u8,
    readiness: TargetReadiness,
}

impl TargetState {
    fn new(account_id: AccountId, generation: AccountGeneration, now: Instant) -> Self {
        Self {
            account_id,
            generation,
            consecutive_failures: 0,
            readiness: TargetReadiness::Waiting(now),
        }
    }

    fn matches(&self, account_id: AccountId, generation: AccountGeneration) -> bool {
        self.account_id == account_id && self.generation == generation
    }
}

#[derive(Debug)]
pub(super) struct SyncScheduler {
    targets: VecDeque<TargetState>,
    wake_deadline: Option<Instant>,
    next_nonce: u64,
}

impl SyncScheduler {
    pub(super) fn new() -> Self {
        Self {
            targets: VecDeque::with_capacity(MAX_TARGETS),
            wake_deadline: None,
            next_nonce: 1,
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.targets.len()
    }

    pub(super) fn wake_deadline(&self) -> Option<Instant> {
        self.wake_deadline
    }

    pub(super) fn replace_targets<I>(
        &mut self,
        targets: I,
        now: Instant,
    ) -> Result<(), SchedulerError>
    where
        I: IntoIterator<Item = (AccountId, AccountGeneration)>,
    {
        let mut normalized = Vec::with_capacity(MAX_TARGETS);
        for (account_id, generation) in targets {
            if let Some(existing) = normalized
                .iter_mut()
                .find(|(existing_id, _)| *existing_id == account_id)
            {
                *existing = (account_id, generation);
                continue;
            }
            if normalized.len() == MAX_TARGETS {
                return Err(SchedulerError::TooManyTargets);
            }
            normalized.push((account_id, generation));
        }

        let mut retained = VecDeque::with_capacity(normalized.len());
        while let Some(target) = self.targets.pop_front() {
            if normalized
                .iter()
                .any(|(account_id, generation)| target.matches(*account_id, *generation))
            {
                retained.push_back(target);
            }
        }
        for (account_id, generation) in normalized {
            if retained
                .iter()
                .all(|target| !target.matches(account_id, generation))
            {
                retained.push_back(TargetState::new(account_id, generation, now));
            }
        }
        self.targets = retained;
        self.refresh_wake_deadline();
        Ok(())
    }

    pub(super) fn take_next(&mut self, now: Instant) -> Result<Option<SyncToken>, SchedulerError> {
        let Some(index) = self.targets.iter().position(
            |target| matches!(target.readiness, TargetReadiness::Waiting(deadline) if deadline <= now),
        ) else {
            return Ok(None);
        };
        let target = self.targets[index];
        let token = self.issue_token(&target)?;
        let mut target = self.targets.remove(index).expect("target index was found");
        target.readiness = TargetReadiness::InFlight(token.nonce);
        self.targets.push_back(target);
        self.refresh_wake_deadline();
        Ok(Some(token))
    }

    pub(super) fn take_manual(
        &mut self,
        account_id: AccountId,
        generation: AccountGeneration,
        now: Instant,
    ) -> Result<Option<SyncToken>, SchedulerError> {
        if let Some(index) = self
            .targets
            .iter()
            .position(|target| target.matches(account_id, generation))
        {
            if matches!(self.targets[index].readiness, TargetReadiness::InFlight(_)) {
                return Ok(None);
            }
            return self.issue_manual_at(index);
        }

        let replacement_index = self
            .targets
            .iter()
            .position(|target| target.account_id == account_id);
        if replacement_index.is_none() && self.targets.len() == MAX_TARGETS {
            return Err(SchedulerError::TooManyTargets);
        }

        let target = TargetState::new(account_id, generation, now);
        let token = self.issue_token(&target)?;
        if let Some(index) = replacement_index {
            self.targets.remove(index);
        }
        self.push_in_flight(target, token);
        Ok(Some(token))
    }

    fn issue_manual_at(&mut self, index: usize) -> Result<Option<SyncToken>, SchedulerError> {
        let target = self.targets[index];
        let token = self.issue_token(&target)?;
        let target = self.targets.remove(index).expect("target index was found");
        self.push_in_flight(target, token);
        Ok(Some(token))
    }

    fn push_in_flight(&mut self, mut target: TargetState, token: SyncToken) {
        target.readiness = TargetReadiness::InFlight(token.nonce);
        self.targets.push_back(target);
        self.refresh_wake_deadline();
    }

    pub(super) fn complete(
        &mut self,
        token: SyncToken,
        completion: SyncCompletion,
        now: Instant,
    ) -> bool {
        let Some(index) = self.targets.iter().position(|target| {
            target.matches(token.account_id, token.generation)
                && target.readiness == TargetReadiness::InFlight(token.nonce)
        }) else {
            return false;
        };
        let mut target = self.targets.remove(index).expect("target index was found");

        match completion {
            SyncCompletion::Complete => {
                target.consecutive_failures = 0;
                target.readiness = TargetReadiness::Waiting(deadline_after(now, SUCCESS_INTERVAL));
            }
            SyncCompletion::Failed => {
                let delay = failure_delay(target.consecutive_failures);
                target.consecutive_failures = target.consecutive_failures.saturating_add(1);
                target.readiness = TargetReadiness::Waiting(deadline_after(now, delay));
            }
        }
        self.targets.push_back(target);
        self.refresh_wake_deadline();
        true
    }

    pub(super) fn promote(
        &mut self,
        account_id: AccountId,
        generation: AccountGeneration,
        now: Instant,
    ) -> bool {
        let Some(target) = self
            .targets
            .iter_mut()
            .find(|target| target.matches(account_id, generation))
        else {
            return false;
        };
        if matches!(target.readiness, TargetReadiness::InFlight(_)) {
            return false;
        }
        target.readiness = TargetReadiness::Waiting(now);
        self.refresh_wake_deadline();
        true
    }

    fn issue_token(&mut self, target: &TargetState) -> Result<SyncToken, SchedulerError> {
        let nonce = self.next_nonce;
        self.next_nonce = nonce.checked_add(1).ok_or(SchedulerError::TokenExhausted)?;
        Ok(SyncToken {
            account_id: target.account_id,
            generation: target.generation,
            nonce,
        })
    }

    fn refresh_wake_deadline(&mut self) {
        self.wake_deadline = self
            .targets
            .iter()
            .filter_map(|target| match target.readiness {
                TargetReadiness::Waiting(deadline) => Some(deadline),
                TargetReadiness::InFlight(_) => None,
            })
            .min();
    }
}

impl Default for SyncScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn deadline_after(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay)
        .expect("bounded scheduler delay must fit in Instant")
}

fn failure_delay(consecutive_failures: u8) -> Duration {
    let exponent = consecutive_failures.min(7);
    INITIAL_FAILURE_DELAY
        .saturating_mul(1_u32 << exponent)
        .min(MAX_FAILURE_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(value: i64) -> AccountId {
        AccountId::new(value).unwrap()
    }

    fn generation(value: i64) -> AccountGeneration {
        AccountGeneration::new(value).unwrap()
    }

    fn target(account_id: i64, generation_value: i64) -> (AccountId, AccountGeneration) {
        (account(account_id), generation(generation_value))
    }

    fn take_account(scheduler: &mut SyncScheduler, now: Instant) -> (i64, SyncToken) {
        let token = scheduler.take_next(now).unwrap().expect("ready target");
        (token.account_id().get(), token)
    }

    #[test]
    fn completed_tokens_rotate_fairly_across_accounts() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler
            .replace_targets([target(1, 1), target(2, 1), target(3, 1)], now)
            .unwrap();

        let mut order = Vec::new();
        for _ in 0..3 {
            let (account_id, token) = take_account(&mut scheduler, now);
            order.push(account_id);
            assert!(scheduler.complete(token, SyncCompletion::Complete, now));
        }

        assert_eq!(order, [1, 2, 3]);
        assert_eq!(take_account(&mut scheduler, now + SUCCESS_INTERVAL).0, 1);
    }

    #[test]
    fn failed_account_does_not_block_ready_accounts() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler
            .replace_targets([target(1, 1), target(2, 1), target(3, 1)], now)
            .unwrap();

        let (first, token) = take_account(&mut scheduler, now);
        assert_eq!(first, 1);
        assert!(scheduler.complete(token, SyncCompletion::Failed, now));
        assert_eq!(take_account(&mut scheduler, now).0, 2);
        assert_eq!(take_account(&mut scheduler, now).0, 3);
        assert!(scheduler.take_next(now).unwrap().is_none());
        assert_eq!(scheduler.wake_deadline(), Some(now + INITIAL_FAILURE_DELAY));
    }

    #[test]
    fn replacement_deduplicates_accounts() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler
            .replace_targets(
                [target(1, 1), target(1, 1), target(2, 1), target(1, 1)],
                now,
            )
            .unwrap();

        assert_eq!(scheduler.len(), 2);
        assert_eq!(take_account(&mut scheduler, now).0, 1);
        assert_eq!(take_account(&mut scheduler, now).0, 2);
        assert!(scheduler.take_next(now).unwrap().is_none());
    }

    #[test]
    fn same_generation_preserves_state_and_new_generation_resets_it() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler.replace_targets([target(1, 1)], now).unwrap();

        let (_, first) = take_account(&mut scheduler, now);
        assert!(scheduler.complete(first, SyncCompletion::Failed, now));
        scheduler
            .replace_targets([target(1, 1)], now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(scheduler.wake_deadline(), Some(now + INITIAL_FAILURE_DELAY));

        let manual = scheduler
            .take_manual(account(1), generation(1), now + Duration::from_secs(2))
            .unwrap()
            .expect("manual work bypasses backoff");
        scheduler
            .replace_targets([target(1, 2)], now + Duration::from_secs(3))
            .unwrap();
        assert!(!scheduler.complete(manual, SyncCompletion::Failed, now + Duration::from_secs(4)));
        let replacement = scheduler
            .take_next(now + Duration::from_secs(3))
            .unwrap()
            .expect("new generation is immediately ready");
        assert_eq!(replacement.generation(), generation(2));
    }

    #[test]
    fn target_limit_is_atomic_and_counts_unique_accounts() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler.replace_targets([target(1, 1)], now).unwrap();
        let sixty_four = (1..=64).map(|id| target(id, 1)).collect::<Vec<_>>();
        scheduler
            .replace_targets(
                sixty_four
                    .iter()
                    .copied()
                    .chain(std::iter::once(target(64, 1))),
                now,
            )
            .unwrap();
        assert_eq!(scheduler.len(), MAX_TARGETS);

        let sixty_five = (1..=65).map(|id| target(id, 1));
        assert_eq!(
            scheduler.replace_targets(sixty_five, now),
            Err(SchedulerError::TooManyTargets)
        );
        assert_eq!(scheduler.len(), MAX_TARGETS);
        assert_eq!(
            scheduler.take_manual(account(65), generation(1), now),
            Err(SchedulerError::TooManyTargets)
        );
        assert_eq!(scheduler.len(), MAX_TARGETS);
        assert_eq!(take_account(&mut scheduler, now).0, 1);
    }

    #[test]
    fn one_deadline_tracks_success_manual_promotion_and_bounded_backoff() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler.replace_targets([target(1, 1)], now).unwrap();
        assert_eq!(scheduler.wake_deadline(), Some(now));

        let (_, token) = take_account(&mut scheduler, now);
        assert_eq!(scheduler.wake_deadline(), None);
        assert!(scheduler.complete(token, SyncCompletion::Complete, now));
        assert_eq!(scheduler.wake_deadline(), Some(now + SUCCESS_INTERVAL));

        let mut current = now + Duration::from_secs(1);
        for expected_delay in [30, 60, 120, 240, 480, 960, 1_920, 3_600, 3_600] {
            let token = scheduler
                .take_manual(account(1), generation(1), current)
                .unwrap()
                .expect("manual work is promoted immediately");
            assert!(scheduler.complete(token, SyncCompletion::Failed, current));
            assert_eq!(
                scheduler.wake_deadline(),
                Some(current + Duration::from_secs(expected_delay))
            );
            current += Duration::from_secs(1);
        }
    }

    #[test]
    fn idle_notification_promotes_only_the_matching_waiting_generation() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();
        scheduler
            .replace_targets([target(1, 1), target(2, 1)], now)
            .unwrap();
        let (_, first) = take_account(&mut scheduler, now);
        assert!(scheduler.complete(first, SyncCompletion::Complete, now));
        let (_, second) = take_account(&mut scheduler, now);

        assert!(scheduler.promote(account(1), generation(1), now + Duration::from_secs(1)));
        assert_eq!(
            scheduler.wake_deadline(),
            Some(now + Duration::from_secs(1))
        );
        assert!(!scheduler.promote(account(2), generation(1), now));
        assert!(!scheduler.promote(account(1), generation(2), now));
        assert!(scheduler.complete(second, SyncCompletion::Complete, now));
    }

    #[test]
    fn manual_take_inserts_before_scan_and_replaces_generation() {
        let now = Instant::now();
        let mut scheduler = SyncScheduler::new();

        let old = scheduler
            .take_manual(account(7), generation(1), now)
            .unwrap()
            .expect("missing target is inserted and issued");
        assert_eq!(scheduler.len(), 1);
        assert_eq!(scheduler.wake_deadline(), None);
        assert!(
            scheduler
                .take_manual(account(7), generation(1), now)
                .unwrap()
                .is_none()
        );

        let replacement = scheduler
            .take_manual(account(7), generation(2), now + Duration::from_secs(1))
            .unwrap()
            .expect("new generation replaces the in-flight target");
        assert_eq!(scheduler.len(), 1);
        assert_eq!(replacement.generation(), generation(2));
        assert!(!scheduler.complete(old, SyncCompletion::Complete, now + Duration::from_secs(2)));
        assert!(scheduler.complete(
            replacement,
            SyncCompletion::Complete,
            now + Duration::from_secs(2)
        ));
        assert_eq!(
            scheduler.wake_deadline(),
            Some(now + Duration::from_secs(2) + SUCCESS_INTERVAL)
        );
    }
}
