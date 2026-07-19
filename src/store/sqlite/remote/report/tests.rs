use rusqlite::{Connection, params};

use super::super::{RemoteClaimOutcome, claim_remote};
use super::*;
use crate::store::sqlite::{domain::FailureKind, migrations::migrate};

const CLAIM_AT_MS: i64 = 1_000;
const REPORT_AT_MS: i64 = 2_000;

type StoredFlagRebase = (
    String,
    Option<i64>,
    Option<bool>,
    Option<bool>,
    i64,
    i64,
    i64,
    Option<String>,
);

fn database() -> Connection {
    let mut connection = Connection::open_in_memory().expect("open in-memory database");
    migrate(&mut connection).expect("apply migrations");
    connection
}

fn insert_account(connection: &Connection, provider: &str) {
    connection
        .execute(
            "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
             VALUES (1, ?1, 'account-1', 'Test', 'test@example.test', 'active', 0)",
            [provider],
        )
        .expect("insert account");
}

fn insert_message(connection: &Connection, unread: bool) {
    connection
        .execute(
            "INSERT INTO messages
                 (id, account_id, remote_key, received_at_ms, unread, starred, revision)
             VALUES (1, 1, 'message-1', 0, ?1, 0, 7)",
            [unread],
        )
        .expect("insert message");
}

fn insert_flag_intent(connection: &Connection) -> i64 {
    connection
        .execute(
            "INSERT INTO remote_change_intents
                 (account_id, message_id, target_key, local_revision,
                  unread_base, unread_desired, not_before_ms,
                  created_at_ms, updated_at_ms)
             VALUES (1, 1, 'message-1', 7, 1, 0, 0, 0, 0)",
            [],
        )
        .expect("insert flag intent");
    connection.last_insert_rowid()
}

fn insert_imap_source(connection: &Connection, intent_id: i64) {
    connection
        .execute(
            "INSERT INTO remote_change_intent_imap_sources
                 (intent_id, folder_key, mailbox_object_id, uid_validity, uid,
                  modseq, email_id, remote_seen, remote_flagged)
             VALUES (?1, 'inbox', 'mailbox-1', 11, 42, 23, 'email-1', 0, 1)",
            [intent_id],
        )
        .expect("insert IMAP source");
}

fn seed_jmap_state(connection: &Connection, state: &str) {
    connection
        .execute(
            "INSERT INTO account_object_states
                 (account_id, object_kind, state_token, updated_at_ms)
             VALUES (1, 'email', ?1, 0)",
            [state],
        )
        .expect("insert JMAP state");
}

fn claim(connection: &mut Connection) -> Box<RemoteClaim> {
    let RemoteClaimOutcome::Claimed(claim) =
        claim_remote(connection, 1, CLAIM_AT_MS).expect("claim remote intent")
    else {
        panic!("expected claimed remote intent");
    };
    claim
}

#[test]
fn confirmed_imap_apply_completes_and_releases_children() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    insert_imap_source(&connection, intent_id);
    let first_claim = claim(&mut connection);

    let transition = report_remote(
        &mut connection,
        &first_claim,
        &RemoteReport::confirmed(None),
        REPORT_AT_MS,
    )
    .expect("confirm remote write");

    assert_eq!(transition, ReportTransition::Completed);
    let intent_count: i64 = connection
        .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
            row.get(0)
        })
        .unwrap();
    let usage: (i64, i64) = connection
        .query_row(
            "SELECT child_count, reserved_count FROM remote_journal_usage",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(intent_count, 0);
    assert_eq!(usage, (0, 0));
}

#[test]
fn stale_report_cannot_advance_jmap_checkpoint() {
    let mut connection = database();
    insert_account(&connection, "jmap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    seed_jmap_state(&connection, "state-0");
    let claim = claim(&mut connection);
    connection
        .execute(
            "UPDATE remote_change_intents SET claim_epoch = claim_epoch + 1 WHERE id = ?1",
            [intent_id],
        )
        .unwrap();

    let checkpoint = RemoteCheckpoint::jmap_email_state("state-1").unwrap();
    let transition = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::confirmed(Some(checkpoint)),
        REPORT_AT_MS,
    )
    .unwrap();

    assert_eq!(transition, ReportTransition::Stale);
    let state: String = connection
        .query_row(
            "SELECT state_token FROM account_object_states
             WHERE account_id = 1 AND object_kind = 'email'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let stored: (String, i64, i64) = connection
        .query_row(
            "SELECT state, claim_epoch, updated_at_ms
             FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "state-0");
    assert_eq!(stored, ("in_flight".into(), 2, CLAIM_AT_MS));
}

#[test]
fn jmap_progress_renews_fence_and_old_epoch_becomes_stale() {
    let mut connection = database();
    insert_account(&connection, "jmap");
    insert_message(&connection, false);
    insert_flag_intent(&connection);
    seed_jmap_state(&connection, "state-0");
    let mut claim = claim(&mut connection);

    let progress = RemoteReport::progress(RemoteCheckpoint::jmap_email_state("state-1").unwrap());
    let lease = match report_remote(&mut connection, &claim, &progress, REPORT_AT_MS).unwrap() {
        ReportTransition::Continued(lease) => lease,
        transition => panic!("unexpected progress transition: {transition:?}"),
    };
    assert_eq!(lease.claim_epoch, 2);
    assert_eq!(lease.expires_at_ms, 32_000);

    let stale = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::confirmed(Some(RemoteCheckpoint::jmap_email_state("state-2").unwrap())),
        REPORT_AT_MS + 1,
    )
    .unwrap();
    assert_eq!(stale, ReportTransition::Stale);
    claim.lease = lease;

    let completed = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::confirmed(Some(RemoteCheckpoint::jmap_email_state("state-2").unwrap())),
        REPORT_AT_MS + 2,
    )
    .unwrap();
    assert_eq!(completed, ReportTransition::Completed);
    let state: String = connection
        .query_row(
            "SELECT state_token FROM account_object_states
             WHERE account_id = 1 AND object_kind = 'email'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "state-2");
}

#[test]
fn older_imap_progress_does_not_renew_the_lease_or_regress_flags() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    insert_imap_source(&connection, intent_id);
    let claim = claim(&mut connection);
    let source = RemoteImapSource::new("inbox", None, 11, 42, Some(22), None, true, false).unwrap();
    let checkpoint = RemoteCheckpoint::imap_sources(vec![source].into_boxed_slice()).unwrap();

    let failure = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::progress(checkpoint),
        REPORT_AT_MS,
    )
    .unwrap_err();

    assert_eq!(failure.kind, FailureKind::Conflict);
    let lease: (i64, i64) = connection
        .query_row(
            "SELECT claim_epoch, lease_expires_at_ms
             FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let source: (i64, bool, bool) = connection
        .query_row(
            "SELECT modseq, remote_seen, remote_flagged
             FROM remote_change_intent_imap_sources WHERE intent_id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(lease, (1, 31_000));
    assert_eq!(source, (23, false, true));
}

#[test]
fn v2_flag_reversal_is_rebased_after_v1_confirmation() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    insert_imap_source(&connection, intent_id);
    let claim = claim(&mut connection);
    connection
        .execute(
            "UPDATE messages SET unread = 1, revision = 8 WHERE id = 1",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE remote_change_intents
             SET intent_version = 2, local_revision = 8,
                 unread_base = 1, unread_desired = 1,
                 not_before_ms = 2500,
                 error_class = 'conflict', error_code = 'v2_error',
                 error_detail = 'The newer local intent owns this error.'
             WHERE id = ?1",
            [intent_id],
        )
        .unwrap();

    let transition = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::confirmed(None),
        REPORT_AT_MS,
    )
    .unwrap();

    assert_eq!(
        transition,
        ReportTransition::Pending {
            state: RemotePendingState::Ready,
            wake_at_ms: Some(2_500),
        }
    );
    let stored: StoredFlagRebase = connection
        .query_row(
            "SELECT state, leased_version, unread_base, unread_desired,
                    attempt_count, dispatched_mask, not_before_ms, error_code
             FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            "ready".into(),
            None,
            Some(false),
            Some(true),
            0,
            0,
            2_500,
            Some("v2_error".into()),
        )
    );
}

#[test]
fn placement_v2_rebase_consumes_and_releases_reserved_capacity() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    for (id, key) in [(1, "inbox"), (2, "archive"), (3, "label")] {
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (?1, 1, ?2, ?2, 'custom')",
                params![id, key],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO message_folders (message_id, folder_id, account_id)
             VALUES (1, 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO remote_change_intents
                 (account_id, message_id, target_key, local_revision,
                  placement_active, not_before_ms, created_at_ms, updated_at_ms)
             VALUES (1, 1, 'message-1', 7, 1, 0, 0, 0)",
            [],
        )
        .unwrap();
    let intent_id = connection.last_insert_rowid();
    connection
        .execute(
            "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
             VALUES (?1, 'desired', 'archive'), (?1, 'desired', 'label')",
            [intent_id],
        )
        .unwrap();
    insert_imap_source(&connection, intent_id);
    let claim = claim(&mut connection);
    let reserved: i64 = connection
        .query_row(
            "SELECT reserved_count FROM remote_journal_usage",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reserved, 2);
    connection
        .execute(
            "DELETE FROM remote_change_intent_folders
             WHERE intent_id = ?1 AND side = 'desired'",
            [intent_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
             VALUES (?1, 'desired', 'inbox')",
            [intent_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE remote_change_intents
             SET intent_version = 2, local_revision = 8 WHERE id = ?1",
            [intent_id],
        )
        .unwrap();

    let transition = report_remote(
        &mut connection,
        &claim,
        &RemoteReport::confirmed(None),
        REPORT_AT_MS,
    )
    .unwrap();

    assert_eq!(
        transition,
        ReportTransition::Pending {
            state: RemotePendingState::Ready,
            wake_at_ms: Some(REPORT_AT_MS),
        }
    );
    let mut statement = connection
        .prepare(
            "SELECT side, folder_key FROM remote_change_intent_folders
             WHERE intent_id = ?1 ORDER BY side, folder_key",
        )
        .unwrap();
    let folders: Vec<(String, String)> = statement
        .query_map([intent_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        folders,
        vec![
            ("base".into(), "archive".into()),
            ("base".into(), "label".into()),
            ("desired".into(), "inbox".into()),
        ]
    );
    drop(statement);
    let usage: (i64, i64) = connection
        .query_row(
            "SELECT child_count, reserved_count FROM remote_journal_usage",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(usage, (4, 0));
}

#[test]
fn retry_never_shortens_a_newer_local_deadline() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    insert_imap_source(&connection, intent_id);
    let claim = claim(&mut connection);
    connection
        .execute(
            "UPDATE remote_change_intents
             SET intent_version = 2, local_revision = 8, not_before_ms = 5000
             WHERE id = ?1",
            [intent_id],
        )
        .unwrap();
    let problem = RemoteProblem::new(
        RemoteErrorClass::Network,
        "offline",
        "The provider connection was interrupted.",
    )
    .unwrap();
    let report = RemoteReport::retry(Some(0), problem).unwrap();

    let transition = report_remote(&mut connection, &claim, &report, REPORT_AT_MS).unwrap();

    assert_eq!(
        transition,
        ReportTransition::Pending {
            state: RemotePendingState::RetryWait,
            wake_at_ms: Some(5_000),
        }
    );
    let stored: (String, i64) = connection
        .query_row(
            "SELECT state, not_before_ms FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("retry_wait".into(), 5_000));
}

#[test]
fn v2_terminal_supersession_survives_v1_and_final_ack_keeps_tombstone() {
    let mut connection = database();
    insert_account(&connection, "imap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    insert_imap_source(&connection, intent_id);
    let first_claim = claim(&mut connection);
    connection
        .execute(
            "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
             VALUES (1, 'message-1', 1500)",
            [],
        )
        .unwrap();
    connection
        .execute("DELETE FROM messages WHERE id = 1", [])
        .unwrap();
    connection
        .execute(
            "UPDATE remote_change_intents
             SET intent_version = 2, local_revision = 8,
                 unread_base = NULL, unread_desired = NULL,
                 delete_requested = 1,
                 error_class = 'conflict', error_code = 'v2_delete',
                 error_detail = 'The newer terminal intent owns this error.'
             WHERE id = ?1",
            [intent_id],
        )
        .unwrap();

    let first = report_remote(
        &mut connection,
        &first_claim,
        &RemoteReport::confirmed(None),
        REPORT_AT_MS,
    )
    .unwrap();
    assert_eq!(
        first,
        ReportTransition::Pending {
            state: RemotePendingState::Ready,
            wake_at_ms: Some(REPORT_AT_MS),
        }
    );
    let pending: (bool, Option<String>) = connection
        .query_row(
            "SELECT delete_requested, error_code
             FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(pending, (true, Some("v2_delete".into())));

    let delete_claim = claim(&mut connection);
    assert!(delete_claim.delete_requested);
    let final_transition = report_remote(
        &mut connection,
        &delete_claim,
        &RemoteReport::confirmed(None),
        REPORT_AT_MS + 1,
    )
    .unwrap();
    assert_eq!(final_transition, ReportTransition::Completed);
    let tombstones: i64 = connection
        .query_row("SELECT count(*) FROM message_tombstones", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(tombstones, 1);
}

#[test]
fn acknowledgement_failure_rolls_back_checkpoint_and_keeps_lease() {
    let mut connection = database();
    insert_account(&connection, "jmap");
    insert_message(&connection, false);
    let intent_id = insert_flag_intent(&connection);
    seed_jmap_state(&connection, "state-0");
    let claim = claim(&mut connection);
    connection
        .execute(
            "UPDATE remote_change_intents
             SET unread_base = 0, unread_desired = 0 WHERE id = ?1",
            [intent_id],
        )
        .unwrap();
    let report =
        RemoteReport::confirmed(Some(RemoteCheckpoint::jmap_email_state("state-1").unwrap()));

    let failure = report_remote(&mut connection, &claim, &report, REPORT_AT_MS).unwrap_err();

    assert_eq!(failure.kind, FailureKind::Conflict);
    let state: String = connection
        .query_row(
            "SELECT state_token FROM account_object_states
             WHERE account_id = 1 AND object_kind = 'email'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let lease: (String, i64, i64) = connection
        .query_row(
            "SELECT state, claim_epoch, lease_expires_at_ms
             FROM remote_change_intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, "state-0");
    assert_eq!(lease, ("in_flight".into(), 1, 31_000));
}
