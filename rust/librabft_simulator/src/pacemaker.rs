// Copyright (c) Calibra Research
// SPDX-License-Identifier: Apache-2.0

use std::{
    cmp::{max, min},
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use super::*;
use record_store::*;

#[cfg(test)]
#[path = "unit_tests/pacemaker_tests.rs"]
mod pacemaker_tests;

// -- BEGIN FILE pacemaker_update_actions --
#[derive(Debug)]
pub struct PacemakerUpdateActions {
    /// Whether to propose a block and on top of which QC hash.
    pub should_propose_block: Option<QuorumCertificateHash>,
    /// Whether we should create a timeout object for the given round.
    pub should_create_timeout: Option<Round>,
    /// Whether we need to send our records to a subset of nodes.
    pub should_send: Vec<Author>,
    /// Whether we need to broadcast data to all other nodes.
    pub should_broadcast: bool,
    /// Whether we need to request data from all other nodes.
    pub should_query_all: bool,
    /// Time at which to call `update_pacemaker` again, at the latest.
    pub next_scheduled_update: NodeTime,
}
// -- END FILE --

// -- BEGIN FILE pacemaker --
pub trait Pacemaker: Debug {
    /// Update our state from the given data and return some action items.
    fn update_pacemaker(
        &mut self,
        // Identity of this node.
        local_author: Author,
        // Current epoch.
        epoch_id: EpochId,
        // Known records.
        record_store: &RecordStore,
        // Local time of the latest query-all by us.
        latest_query_all: NodeTime,
        // Current local time.
        clock: NodeTime,
    ) -> PacemakerUpdateActions;

    /// Current active epoch, round, and leader.
    fn active_epoch(&self) -> EpochId;
    fn active_round(&self) -> Round;
    fn active_leader(&self) -> Option<Author>;
}
// -- END FILE --

// -- BEGIN FILE pacemaker_state --
#[derive(Debug)]
pub struct PacemakerState {
    /// Active epoch.
    active_epoch: EpochId,
    /// Active round.
    active_round: Round,
    /// Leader of the active round.
    active_leader: Option<Author>,
    /// Time at which we entered the round.
    active_round_start_time: NodeTime,
    /// Maximal duration of the current round.
    active_round_duration: Duration,
    /// Maximal duration of the first round after a commit rule.
    delta: Duration,
    /// Exponent to increase round durations.
    gamma: f64,
    /// Coefficient to control the frequency of query-all actions.
    lambda: f64,
}
// -- END FILE --

impl PacemakerState {
    pub fn new(
        epoch_id: EpochId,
        node_time: NodeTime,
        delta: Duration,
        gamma: f64,
        lambda: f64,
    ) -> PacemakerState {
        PacemakerState {
            active_epoch: epoch_id,
            active_round: Round(0),
            active_leader: None,
            active_round_start_time: node_time,
            active_round_duration: 0,
            delta,
            gamma,
            lambda,
        }
    }

    pub fn leader(record_store: &RecordStore, round: Round) -> Author {
        let mut hasher = DefaultHasher::new();
        round.hash(&mut hasher);
        let author = record_store.pick_author(hasher.finish());
        author
    }

    fn duration(&self, record_store: &RecordStore, round: Round) -> Duration {
        let highest_commit_certificate_round = if record_store.highest_committed_round() > Round(0)
        {
            record_store.highest_committed_round() + 2
        } else {
            Round(0)
        };
        assert!(
            round > highest_commit_certificate_round,
            "Active round is higher than any QC round."
        );
        let n = round.0 - highest_commit_certificate_round.0;
        ((self.delta as f64) * (n as f64).powf(self.gamma)) as Duration
    }
}

impl PacemakerUpdateActions {
    pub fn new() -> Self {
        PacemakerUpdateActions {
            next_scheduled_update: NodeTime::never(),
            should_create_timeout: None,
            should_send: Vec::new(),
            should_broadcast: false,
            should_query_all: false,
            should_propose_block: None,
        }
    }
}

impl Pacemaker for PacemakerState {
    // -- BEGIN FILE pacemaker_impl --
    fn update_pacemaker(
        &mut self,
        local_author: Author,
        epoch_id: EpochId,
        record_store: &RecordStore,
        latest_query_all_time: NodeTime,
        clock: NodeTime,
    ) -> PacemakerUpdateActions {
        // Initialize actions with default values.
        let mut actions = PacemakerUpdateActions::new();
        // Compute the active round from the current record store.
        let active_round = max(
            record_store.highest_quorum_certificate_round(),
            record_store.highest_timeout_certificate_round(),
        ) + 1;
        // If the epoch changed or the active round was just updated..
        if epoch_id > self.active_epoch
            || (epoch_id == self.active_epoch && active_round > self.active_round)
        {
            // .. store the new value
            self.active_epoch = epoch_id;
            self.active_round = active_round;
            // .. start a timer
            self.active_round_start_time = clock;
            // .. compute the leader
            self.active_leader = Some(Self::leader(record_store, active_round));
            // .. compute the duration
            self.active_round_duration = self.duration(record_store, active_round);
            // .. synchronize with the leader.
            if self.active_leader != Some(local_author) {
                actions.should_send = self.active_leader.into_iter().collect();
            }
        }
        // If we are the leader and have not proposed yet..
        if self.active_leader == Some(local_author) && record_store.proposed_block(&*self) == None {
            // .. propose a block on top of the highest QC that we know.
            actions.should_propose_block = Some(record_store.highest_quorum_certificate_hash());
            actions.should_broadcast = true;
            // .. force an immediate update to vote on our own proposal.
            actions.next_scheduled_update = clock;
        }
        if !record_store.has_timeout(local_author, active_round) {
            let timeout_deadline = self.active_round_start_time + self.active_round_duration;
            // If we have not created a timeout yet, check if the round has passed its maximal
            // duration. Then, either broadcast a new timeout now, or schedule an update
            // in the future.
            if clock >= timeout_deadline {
                actions.should_create_timeout = Some(active_round);
                actions.should_broadcast = true;
            } else {
                actions.next_scheduled_update =
                    min(actions.next_scheduled_update, timeout_deadline);
            }
        } else {
            // Otherwise, enforce frequent query-all actions if we stay too long on the same round.
            let period = (self.lambda * self.active_round_duration as f64) as i64;
            let mut query_all_deadline = latest_query_all_time + period;
            if clock >= query_all_deadline {
                actions.should_query_all = true;
                query_all_deadline = clock + period;
            }
            actions.next_scheduled_update = min(actions.next_scheduled_update, query_all_deadline);
        }
        // Return all computed actions.
        actions
    }
    // -- END FILE --

    fn active_epoch(&self) -> EpochId {
        self.active_epoch
    }

    fn active_round(&self) -> Round {
        self.active_round
    }

    fn active_leader(&self) -> Option<Author> {
        self.active_leader
    }
}
