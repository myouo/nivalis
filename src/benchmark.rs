use crate::{
    AppWindow,
    content::{ContentLimits, ContentStaging, FileKey, prepare_content},
    store::sqlite::{
        AccountGeneration, AccountId, ContentImportSubmission, DatabaseClient, DatabaseSubmitError,
        FileGcOutcome, MailboxQueryCounts, MessageId, mailbox_query_counts,
    },
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use slint::{ComponentHandle, Model, SharedString, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub(crate) fn automatic_sync_enabled() -> bool {
    automatic_sync_enabled_for_scenario(std::env::var("NIVALIS_STRESS_SCENARIO").ok().as_deref())
}

fn automatic_sync_enabled_for_scenario(scenario: Option<&str>) -> bool {
    !matches!(
        scenario,
        Some("account-diagnostic" | "account-receive" | "account-send")
    )
}

pub(crate) fn install_memory_stress(
    ui: &AppWindow,
    database: DatabaseClient,
    database_path: PathBuf,
    content_path: PathBuf,
) -> Option<Rc<Timer>> {
    let steps = std::env::var("NIVALIS_STRESS_STEPS")
        .ok()?
        .parse::<usize>()
        .ok()?
        .max(1);
    let delay = std::env::var("NIVALIS_STRESS_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let interval = std::env::var("NIVALIS_STRESS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2)
        .max(1);

    match std::env::var("NIVALIS_STRESS_SCENARIO").as_deref() {
        Ok("pagination") => install_pagination_stress(ui, steps, delay, interval),
        Ok("write-search") => install_write_search_stress(ui, steps, delay, interval),
        Ok("content") => install_content_stress(ui, database, content_path, steps, delay),
        Ok("account-diagnostic") => install_account_diagnostic_stress(ui, steps, delay, interval),
        Ok("account-receive") => {
            install_account_receive_stress(ui, database_path, content_path, steps, delay, interval)
        }
        Ok("account-send") => install_account_send_stress(ui, steps, delay, interval),
        Ok("mixed") | Err(_) => install_mixed_stress(ui, steps, delay, interval),
        Ok(scenario) => {
            eprintln!(
                "NIVALIS_STRESS_ERROR scenario={scenario} reason=unsupported_stress_scenario"
            );
            None
        }
    }
}

const ACCOUNT_DIAGNOSTIC_SECRET_LIMIT: u64 = 16 * 1024;
const ACCOUNT_DIAGNOSTIC_DEFAULT_TIMEOUT_MS: u64 = 45_000;
const ACCOUNT_RECEIVE_EXPECTED_SUBJECT: &str = "Received memory fixture";
const ACCOUNT_RECEIVE_EXPECTED_BODY: &str = "Bounded receive body.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountDiagnosticExpectation {
    Ready,
}

impl AccountDiagnosticExpectation {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            _ => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
        }
    }
}

struct AccountDiagnosticConfig {
    name: String,
    address: String,
    login: String,
    imap_host: String,
    imap_port: String,
    smtp_host: String,
    smtp_port: String,
    secret: Zeroizing<Vec<u8>>,
    expected: AccountDiagnosticExpectation,
}

impl AccountDiagnosticConfig {
    fn load() -> Result<Self, &'static str> {
        let secret_path = required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_SECRET_FILE")?;
        let imap_host = required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_IMAP_HOST")?;
        let smtp_host =
            std::env::var("NIVALIS_STRESS_ACCOUNT_SMTP_HOST").unwrap_or_else(|_| imap_host.clone());
        // The harness copies through Slint to exercise production UI, so it only accepts fake
        // credentials for a loopback fixture rather than a real provider secret.
        if !is_loopback_host(&imap_host) {
            return Err("nonlocal_host_rejected");
        }
        if !is_loopback_host(&smtp_host) {
            return Err("nonlocal_smtp_host_rejected");
        }
        Ok(Self {
            name: std::env::var("NIVALIS_STRESS_ACCOUNT_NAME")
                .unwrap_or_else(|_| "Memory diagnostic".into()),
            address: required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_ADDRESS")?,
            login: required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_LOGIN")?,
            smtp_host,
            imap_host,
            imap_port: std::env::var("NIVALIS_STRESS_ACCOUNT_IMAP_PORT")
                .unwrap_or_else(|_| "993".into()),
            smtp_port: std::env::var("NIVALIS_STRESS_ACCOUNT_SMTP_PORT")
                .unwrap_or_else(|_| "465".into()),
            secret: read_account_diagnostic_secret(&PathBuf::from(secret_path))?,
            expected: std::env::var("NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT")
                .ok()
                .as_deref()
                .and_then(AccountDiagnosticExpectation::parse)
                .ok_or("invalid_expected_result")?,
        })
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn required_account_diagnostic_env(name: &str) -> Result<String, &'static str> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or("configuration_unavailable")
}

fn read_account_diagnostic_secret(
    path: &std::path::Path,
) -> Result<Zeroizing<Vec<u8>>, &'static str> {
    if !path.is_absolute() {
        return Err("secret_path_not_absolute");
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "secret_file_unavailable")?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("secret_file_invalid");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("secret_file_permissions");
        }
    }
    if metadata.len() == 0 || metadata.len() > ACCOUNT_DIAGNOSTIC_SECRET_LIMIT {
        return Err("secret_file_size");
    }

    let mut secret = Zeroizing::new(Vec::with_capacity(
        usize::try_from(metadata.len()).map_err(|_| "secret_file_size")?,
    ));
    File::open(path)
        .map_err(|_| "secret_file_unavailable")?
        .take(ACCOUNT_DIAGNOSTIC_SECRET_LIMIT + 1)
        .read_to_end(&mut secret)
        .map_err(|_| "secret_file_unavailable")?;
    if secret.len() as u64 != metadata.len()
        || secret.is_empty()
        || secret.len() as u64 > ACCOUNT_DIAGNOSTIC_SECRET_LIMIT
        || std::str::from_utf8(&secret).is_err()
    {
        return Err("secret_file_invalid");
    }
    Ok(secret)
}

#[derive(Debug)]
enum AccountDiagnosticPhase {
    WaitingForInitialState,
    Diagnosing {
        expected: AccountDiagnosticExpectation,
    },
    WaitingForCatalog {
        account_id: SharedString,
        outcome: Result<AccountDiagnosticExpectation, &'static str>,
    },
    Removing {
        account_id: SharedString,
        outcome: Result<AccountDiagnosticExpectation, &'static str>,
    },
    WaitingForRemoval {
        account_id: SharedString,
        outcome: Result<AccountDiagnosticExpectation, &'static str>,
    },
    Complete,
}

struct AccountDiagnosticStress {
    phase: AccountDiagnosticPhase,
    started: Instant,
    deadline: Instant,
    cleanup_required: bool,
}

fn install_account_diagnostic_stress(
    ui: &AppWindow,
    cycles: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    if cycles != 1 {
        eprintln!(
            "NIVALIS_STRESS_ERROR scenario=account-diagnostic reason=cycles_must_equal_one cycles={cycles}"
        );
        return None;
    }
    let timeout = std::env::var("NIVALIS_STRESS_TRANSITION_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(ACCOUNT_DIAGNOSTIC_DEFAULT_TIMEOUT_MS)
        .max(1);
    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(timer) = timer_weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        let state = Rc::new(RefCell::new(AccountDiagnosticStress {
            phase: AccountDiagnosticPhase::WaitingForInitialState,
            started,
            deadline: started + Duration::from_millis(timeout),
            cleanup_required: false,
        }));
        let state_for_timer = state.clone();
        let timer_for_callback = Rc::downgrade(&timer);
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval.max(25)),
            move || {
                let Some(timer) = timer_for_callback.upgrade() else {
                    return;
                };
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                let mut state = state_for_timer.borrow_mut();
                if Instant::now() >= state.deadline {
                    fail_account_diagnostic_stress(
                        &ui,
                        &timer,
                        "operation_timeout",
                        state.cleanup_required,
                    );
                    state.phase = AccountDiagnosticPhase::Complete;
                    return;
                }

                match &mut state.phase {
                    AccountDiagnosticPhase::WaitingForInitialState => {
                        if ui.get_initial_loading() || ui.get_mailbox_loading() {
                            return;
                        }
                        if ui.get_has_accounts() {
                            fail_account_diagnostic_stress(
                                &ui,
                                &timer,
                                "fixture_not_empty",
                                false,
                            );
                            state.phase = AccountDiagnosticPhase::Complete;
                            return;
                        }
                        let config = match AccountDiagnosticConfig::load() {
                            Ok(config) => config,
                            Err(reason) => {
                                fail_account_diagnostic_stress(&ui, &timer, reason, false);
                                state.phase = AccountDiagnosticPhase::Complete;
                                return;
                            }
                        };
                        let secret = match std::str::from_utf8(&config.secret) {
                            Ok(secret) => SharedString::from(secret),
                            Err(_) => {
                                fail_account_diagnostic_stress(
                                    &ui,
                                    &timer,
                                    "secret_file_invalid",
                                    false,
                                );
                                state.phase = AccountDiagnosticPhase::Complete;
                                return;
                            }
                        };
                        let expected = config.expected;
                        ui.invoke_add_account(
                            config.name.into(),
                            config.address.into(),
                            config.login.into(),
                            config.imap_host.into(),
                            config.imap_port.into(),
                            config.smtp_host.into(),
                            config.smtp_port.into(),
                            secret,
                        );
                        state.cleanup_required = true;
                        state.phase = AccountDiagnosticPhase::Diagnosing { expected };
                    }
                    AccountDiagnosticPhase::Diagnosing { expected } => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        let account_id = ui.get_managed_account_id();
                        if account_id.is_empty() {
                            fail_account_diagnostic_stress(
                                &ui,
                                &timer,
                                "account_identity_missing",
                                true,
                            );
                            state.phase = AccountDiagnosticPhase::Complete;
                            return;
                        }
                        let observed = classify_account_diagnostic(
                            ui.get_managed_account_status().as_str(),
                            ui.get_managed_account_has_error(),
                        );
                        let outcome = if observed == Some(*expected) {
                            Ok(*expected)
                        } else {
                            Err("diagnostic_mismatch")
                        };
                        state.phase = AccountDiagnosticPhase::WaitingForCatalog {
                            account_id,
                            outcome,
                        };
                    }
                    AccountDiagnosticPhase::WaitingForCatalog {
                        account_id,
                        outcome,
                    } => {
                        if !account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        let account_id = account_id.clone();
                        let outcome = *outcome;
                        ui.invoke_remove_account(account_id.clone());
                        state.phase = AccountDiagnosticPhase::Removing {
                            account_id,
                            outcome,
                        };
                    }
                    AccountDiagnosticPhase::Removing {
                        account_id,
                        outcome,
                    } => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_diagnostic_stress(
                                &ui,
                                &timer,
                                "removal_failed",
                                true,
                            );
                            state.phase = AccountDiagnosticPhase::Complete;
                            return;
                        }
                        state.phase = AccountDiagnosticPhase::WaitingForRemoval {
                            account_id: account_id.clone(),
                            outcome: *outcome,
                        };
                    }
                    AccountDiagnosticPhase::WaitingForRemoval {
                        account_id,
                        outcome,
                    } => {
                        if account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        let outcome = *outcome;
                        state.cleanup_required = false;
                        match outcome {
                            Ok(outcome) => {
                                ui.set_status_text("Account diagnostic memory stress complete".into());
                                eprintln!(
                                    "NIVALIS_STRESS_RESULT scenario=account-diagnostic cycles=1 outcome={} removed=1 elapsed_ms={}",
                                    outcome.label(),
                                    state.started.elapsed().as_millis()
                                );
                                stop_stress(&ui, &timer);
                            }
                            Err(reason) => {
                                fail_account_diagnostic_stress(&ui, &timer, reason, false)
                            }
                        }
                        state.phase = AccountDiagnosticPhase::Complete;
                    }
                    AccountDiagnosticPhase::Complete => {}
                }
            },
        );
    });

    Some(timer)
}

fn classify_account_diagnostic(
    status: &str,
    has_error: bool,
) -> Option<AccountDiagnosticExpectation> {
    match (status, has_error) {
        ("Connected" | "Ready", false) => Some(AccountDiagnosticExpectation::Ready),
        _ => None,
    }
}

fn account_model_contains(ui: &AppWindow, account_id: &str) -> bool {
    let accounts = ui.get_accounts();
    (0..accounts.row_count()).any(|index| {
        accounts
            .row_data(index)
            .is_some_and(|account| account.id.as_str() == account_id)
    })
}

fn fail_account_diagnostic_stress(
    ui: &AppWindow,
    timer: &Timer,
    reason: &str,
    cleanup_required: bool,
) {
    ui.set_status_text("Account diagnostic memory stress failed".into());
    eprintln!(
        "NIVALIS_STRESS_ERROR scenario=account-diagnostic reason={reason} cleanup_required={}",
        u8::from(cleanup_required)
    );
    stop_stress(ui, timer);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountReceiveMailboxExpectation {
    Empty,
    Single,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountReceiveGate {
    Waiting,
    Ready,
    Failed(&'static str),
}

#[derive(Clone, Copy)]
struct AccountReceiveMailboxObservation {
    account_selected: bool,
    loading: bool,
    error: bool,
    total_known: bool,
    message_total: i32,
    rows: usize,
    has_previous: bool,
    has_next: bool,
    first_account_matches: bool,
    first_id_present: bool,
    first_subject_matches: bool,
}

fn account_receive_mailbox_gate(
    observation: AccountReceiveMailboxObservation,
    expectation: AccountReceiveMailboxExpectation,
) -> AccountReceiveGate {
    if !observation.account_selected || observation.loading {
        return AccountReceiveGate::Waiting;
    }
    if observation.error {
        return AccountReceiveGate::Failed("mailbox_error");
    }
    if observation.message_total < 0 {
        return AccountReceiveGate::Failed("mailbox_count_invalid");
    }

    match expectation {
        AccountReceiveMailboxExpectation::Empty => {
            if observation.total_known
                && observation.message_total == 0
                && observation.rows == 0
                && !observation.has_previous
                && !observation.has_next
            {
                AccountReceiveGate::Ready
            } else if observation.message_total > 0 || observation.rows > 0 {
                AccountReceiveGate::Failed("fixture_not_empty")
            } else {
                AccountReceiveGate::Waiting
            }
        }
        AccountReceiveMailboxExpectation::Single => {
            if observation.message_total > 1 || observation.rows > 1 {
                return AccountReceiveGate::Failed("import_count_mismatch");
            }
            if observation.total_known
                && observation.message_total == 1
                && observation.rows == 1
                && !observation.has_previous
                && !observation.has_next
            {
                if !observation.first_account_matches
                    || !observation.first_id_present
                    || !observation.first_subject_matches
                {
                    AccountReceiveGate::Failed("imported_message_invalid")
                } else {
                    AccountReceiveGate::Ready
                }
            } else {
                AccountReceiveGate::Waiting
            }
        }
    }
}

#[derive(Clone, Copy)]
struct AccountReceiveDatabaseObservation {
    account_matches: bool,
    message_total: i64,
    body_key_valid: bool,
    body_byte_count: i64,
    body_file_regular: bool,
    body_file_bytes: Option<u64>,
    reader_excerpt_matches_file: bool,
    subject_matches_fixture: bool,
    body_matches_fixture: bool,
    private_permissions: bool,
}

#[cfg(unix)]
fn account_receive_private_permissions(
    content_metadata: &std::fs::Metadata,
    body_directory_metadata: &std::fs::Metadata,
    body_metadata: &std::fs::Metadata,
) -> bool {
    use std::os::unix::fs::PermissionsExt;

    content_metadata.file_type().is_dir()
        && body_directory_metadata.file_type().is_dir()
        && body_metadata.file_type().is_file()
        && content_metadata.permissions().mode() & 0o777 == 0o700
        && body_directory_metadata.permissions().mode() & 0o777 == 0o700
        && body_metadata.permissions().mode() & 0o777 == 0o600
}

#[cfg(not(unix))]
fn account_receive_private_permissions(
    content_metadata: &std::fs::Metadata,
    body_directory_metadata: &std::fs::Metadata,
    body_metadata: &std::fs::Metadata,
) -> bool {
    content_metadata.file_type().is_dir()
        && body_directory_metadata.file_type().is_dir()
        && body_metadata.file_type().is_file()
}

fn account_receive_database_gate(
    database_path: &std::path::Path,
    content_path: &std::path::Path,
    expected_account_id: &str,
    expected_message_id: &str,
) -> AccountReceiveGate {
    let Ok(expected_account_id) = expected_account_id.parse::<i64>() else {
        return AccountReceiveGate::Failed("database_account_identity_invalid");
    };
    let Ok(expected_message_id) = expected_message_id.parse::<i64>() else {
        return AccountReceiveGate::Failed("database_message_identity_invalid");
    };
    let connection = match Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(connection) => connection,
        Err(_) => return AccountReceiveGate::Failed("database_evidence_unavailable"),
    };
    if connection.pragma_update(None, "query_only", true).is_err() {
        return AccountReceiveGate::Failed("database_evidence_unavailable");
    }
    let row = match connection
        .query_row(
            "SELECT message.account_id,
                    (SELECT count(*) FROM messages WHERE account_id = message.account_id),
                    message.subject,
                    content.body_file_key,
                    content.body_byte_count,
                    content.reader_excerpt
             FROM messages AS message
             JOIN message_content AS content ON content.message_id = message.id
             WHERE message.id = ?1
               AND EXISTS (
                   SELECT 1
                   FROM message_folders AS membership
                   JOIN folders AS folder ON folder.id = membership.folder_id
                   WHERE membership.message_id = message.id
                     AND folder.role = 'inbox'
               )",
            [expected_message_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()
    {
        Ok(Some(row)) => row,
        Ok(None) => return AccountReceiveGate::Failed("database_message_missing"),
        Err(_) => return AccountReceiveGate::Failed("database_evidence_unavailable"),
    };
    let (account_id, message_total, subject, body_file_key, body_byte_count, reader_excerpt) = row;
    let body_key = body_file_key
        .as_deref()
        .and_then(|value| FileKey::parse(value).ok());
    let body_path = body_key.as_ref().map(|key| content_path.join(key.as_str()));
    let body_metadata = body_path
        .as_ref()
        .and_then(|path| std::fs::symlink_metadata(path).ok());
    let body_directory_metadata = body_path
        .as_ref()
        .and_then(|path| path.parent())
        .and_then(|path| std::fs::symlink_metadata(path).ok());
    let content_metadata = std::fs::symlink_metadata(content_path).ok();
    let body_bytes = body_path.as_ref().and_then(|path| {
        if !body_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            return None;
        }
        let Ok(file) = File::open(path) else {
            return None;
        };
        let mut bytes = Vec::with_capacity(reader_excerpt.len().saturating_add(2));
        file.take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .ok()
            .filter(|_| bytes.len() <= 1024 * 1024)
            .map(|_| bytes)
    });
    let reader_excerpt_matches_file = body_bytes.as_ref().is_some_and(|bytes| {
        bytes
            .get(..reader_excerpt.len())
            .is_some_and(|prefix| prefix == reader_excerpt.as_bytes())
    });
    let body_matches_fixture = body_bytes
        .as_ref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|body| body.trim_end_matches(['\r', '\n']) == ACCOUNT_RECEIVE_EXPECTED_BODY);
    let private_permissions = content_metadata.as_ref().is_some_and(|content| {
        body_directory_metadata.as_ref().is_some_and(|directory| {
            body_metadata
                .as_ref()
                .is_some_and(|body| account_receive_private_permissions(content, directory, body))
        })
    });
    account_receive_database_observation_gate(AccountReceiveDatabaseObservation {
        account_matches: account_id == expected_account_id,
        message_total,
        body_key_valid: body_key.is_some(),
        body_byte_count,
        body_file_regular: body_metadata
            .as_ref()
            .is_some_and(std::fs::Metadata::is_file),
        body_file_bytes: body_metadata.map(|metadata| metadata.len()),
        reader_excerpt_matches_file,
        subject_matches_fixture: subject == ACCOUNT_RECEIVE_EXPECTED_SUBJECT,
        body_matches_fixture,
        private_permissions,
    })
}

fn account_receive_database_observation_gate(
    observation: AccountReceiveDatabaseObservation,
) -> AccountReceiveGate {
    if !observation.account_matches {
        return AccountReceiveGate::Failed("database_account_mismatch");
    }
    if observation.message_total != 1 {
        return AccountReceiveGate::Failed("database_message_count_mismatch");
    }
    if !observation.body_key_valid || observation.body_byte_count <= 0 {
        return AccountReceiveGate::Failed("database_content_missing");
    }
    if !observation.body_file_regular || observation.body_file_bytes == Some(0) {
        return AccountReceiveGate::Failed("database_body_file_mismatch");
    }
    if !observation.reader_excerpt_matches_file {
        return AccountReceiveGate::Failed("database_body_excerpt_mismatch");
    }
    if !observation.subject_matches_fixture || !observation.body_matches_fixture {
        return AccountReceiveGate::Failed("database_fixture_mismatch");
    }
    if !observation.private_permissions {
        return AccountReceiveGate::Failed("database_body_not_private");
    }
    AccountReceiveGate::Ready
}

#[derive(Clone, Copy)]
struct AccountReceiveReaderObservation {
    detail_open: bool,
    loading: bool,
    error: bool,
    selected_id_matches: bool,
    detail_id_matches: bool,
    subject_matches_fixture: bool,
    body_matches_fixture: bool,
}

fn account_receive_reader_gate(observation: AccountReceiveReaderObservation) -> AccountReceiveGate {
    if observation.loading {
        return AccountReceiveGate::Waiting;
    }
    if observation.error {
        return AccountReceiveGate::Failed("detail_error");
    }
    if !observation.detail_open {
        return AccountReceiveGate::Failed("reader_closed_early");
    }
    if !observation.selected_id_matches || !observation.detail_id_matches {
        return AccountReceiveGate::Failed("opened_message_mismatch");
    }
    if !observation.subject_matches_fixture || !observation.body_matches_fixture {
        return AccountReceiveGate::Failed("reader_fixture_mismatch");
    }
    AccountReceiveGate::Ready
}

#[derive(Debug)]
enum AccountReceivePhase {
    WaitingForInitialState,
    Diagnosing,
    WaitingForCatalog {
        account_id: SharedString,
    },
    WaitingForAccountMailbox {
        account_id: SharedString,
    },
    Syncing {
        account_id: SharedString,
    },
    WaitingForImport {
        account_id: SharedString,
    },
    Opening {
        account_id: SharedString,
        message_id: SharedString,
    },
    ClosingReader {
        account_id: SharedString,
    },
    Removing {
        account_id: SharedString,
    },
    WaitingForRemoval {
        account_id: SharedString,
    },
    Complete,
}

struct AccountReceiveStress {
    phase: AccountReceivePhase,
    started: Instant,
    deadline: Instant,
    transition_timeout: Duration,
    cleanup_required: bool,
}

impl AccountReceiveStress {
    fn advance(&mut self, phase: AccountReceivePhase) {
        self.phase = phase;
        self.deadline = Instant::now() + self.transition_timeout;
    }
}

fn install_account_receive_stress(
    ui: &AppWindow,
    database_path: PathBuf,
    content_path: PathBuf,
    steps: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    if steps != 1 {
        eprintln!(
            "NIVALIS_STRESS_ERROR scenario=account-receive reason=steps_must_equal_one steps={steps} cleanup_required=0"
        );
        return None;
    }
    let timeout = std::env::var("NIVALIS_STRESS_TRANSITION_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(ACCOUNT_DIAGNOSTIC_DEFAULT_TIMEOUT_MS)
        .max(1);
    let transition_timeout = Duration::from_millis(timeout);
    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(timer) = timer_weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        let state = Rc::new(RefCell::new(AccountReceiveStress {
            phase: AccountReceivePhase::WaitingForInitialState,
            started,
            deadline: started + transition_timeout,
            transition_timeout,
            cleanup_required: false,
        }));
        let state_for_timer = state.clone();
        let timer_for_callback = Rc::downgrade(&timer);
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval.max(25)),
            move || {
                let Some(timer) = timer_for_callback.upgrade() else {
                    return;
                };
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                let mut state = state_for_timer.borrow_mut();
                if Instant::now() >= state.deadline {
                    fail_account_receive_stress(
                        &ui,
                        &timer,
                        "transition_timeout",
                        state.cleanup_required,
                    );
                    state.phase = AccountReceivePhase::Complete;
                    return;
                }

                match &state.phase {
                    AccountReceivePhase::WaitingForInitialState => {
                        if ui.get_initial_loading() || ui.get_mailbox_loading() {
                            return;
                        }
                        if ui.get_mailbox_error() {
                            fail_account_receive_stress(
                                &ui,
                                &timer,
                                "initial_mailbox_error",
                                false,
                            );
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        if ui.get_has_accounts() {
                            fail_account_receive_stress(
                                &ui,
                                &timer,
                                "fixture_not_empty",
                                false,
                            );
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        let config = match AccountDiagnosticConfig::load() {
                            Ok(config) => config,
                            Err(reason) => {
                                fail_account_receive_stress(&ui, &timer, reason, false);
                                state.phase = AccountReceivePhase::Complete;
                                return;
                            }
                        };
                        let secret = match std::str::from_utf8(&config.secret) {
                            Ok(secret) => SharedString::from(secret),
                            Err(_) => {
                                fail_account_receive_stress(
                                    &ui,
                                    &timer,
                                    "secret_file_invalid",
                                    false,
                                );
                                state.phase = AccountReceivePhase::Complete;
                                return;
                            }
                        };
                        ui.invoke_add_account(
                            config.name.into(),
                            config.address.into(),
                            config.login.into(),
                            config.imap_host.into(),
                            config.imap_port.into(),
                            config.smtp_host.into(),
                            config.smtp_port.into(),
                            secret,
                        );
                        state.cleanup_required = true;
                        state.advance(AccountReceivePhase::Diagnosing);
                    }
                    AccountReceivePhase::Diagnosing => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_receive_stress(&ui, &timer, "diagnostic_failed", true);
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        let account_id = ui.get_managed_account_id();
                        if account_id.is_empty() {
                            fail_account_receive_stress(
                                &ui,
                                &timer,
                                "account_identity_missing",
                                true,
                            );
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        if classify_account_diagnostic(
                            ui.get_managed_account_status().as_str(),
                            ui.get_managed_account_has_error(),
                        ) != Some(AccountDiagnosticExpectation::Ready)
                        {
                            fail_account_receive_stress(
                                &ui,
                                &timer,
                                "diagnostic_mismatch",
                                true,
                            );
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        state.advance(AccountReceivePhase::WaitingForCatalog { account_id });
                    }
                    AccountReceivePhase::WaitingForCatalog { account_id } => {
                        if !account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        let account_id = account_id.clone();
                        if ui.get_active_account_id().as_str() != account_id.as_str() {
                            ui.invoke_switch_account(account_id.clone());
                        }
                        state.advance(AccountReceivePhase::WaitingForAccountMailbox {
                            account_id,
                        });
                    }
                    AccountReceivePhase::WaitingForAccountMailbox { account_id } => {
                        match account_receive_mailbox_gate(
                            account_receive_mailbox_observation(&ui, account_id.as_str()),
                            AccountReceiveMailboxExpectation::Empty,
                        ) {
                            AccountReceiveGate::Waiting => {}
                            AccountReceiveGate::Failed(reason) => {
                                fail_account_receive_stress(&ui, &timer, reason, true);
                                state.phase = AccountReceivePhase::Complete;
                            }
                            AccountReceiveGate::Ready => {
                                let account_id = account_id.clone();
                                ui.invoke_sync_account(account_id.clone());
                                state.advance(AccountReceivePhase::Syncing { account_id });
                            }
                        }
                    }
                    AccountReceivePhase::Syncing { account_id } => {
                        if ui.get_sync_loading() || ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_receive_stress(&ui, &timer, "sync_failed", true);
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        let account_id = account_id.clone();
                        state.advance(AccountReceivePhase::WaitingForImport { account_id });
                    }
                    AccountReceivePhase::WaitingForImport { account_id } => {
                        match account_receive_mailbox_gate(
                            account_receive_mailbox_observation(&ui, account_id.as_str()),
                            AccountReceiveMailboxExpectation::Single,
                        ) {
                            AccountReceiveGate::Waiting => {}
                            AccountReceiveGate::Failed(reason) => {
                                fail_account_receive_stress(&ui, &timer, reason, true);
                                state.phase = AccountReceivePhase::Complete;
                            }
                            AccountReceiveGate::Ready => {
                                let account_id = account_id.clone();
                                let Some(message) = ui.get_mails().row_data(0) else {
                                    fail_account_receive_stress(
                                        &ui,
                                        &timer,
                                        "imported_message_missing",
                                        true,
                                    );
                                    state.phase = AccountReceivePhase::Complete;
                                    return;
                                };
                                let message_id = message.id;
                                match account_receive_database_gate(
                                    &database_path,
                                    &content_path,
                                    account_id.as_str(),
                                    message_id.as_str(),
                                ) {
                                    AccountReceiveGate::Waiting => return,
                                    AccountReceiveGate::Failed(reason) => {
                                        fail_account_receive_stress(&ui, &timer, reason, true);
                                        state.phase = AccountReceivePhase::Complete;
                                        return;
                                    }
                                    AccountReceiveGate::Ready => {}
                                }
                                ui.set_detail_open(true);
                                ui.invoke_select_mail(message_id.clone());
                                state.advance(AccountReceivePhase::Opening {
                                    account_id,
                                    message_id,
                                });
                            }
                        }
                    }
                    AccountReceivePhase::Opening {
                        account_id,
                        message_id,
                    } => {
                        let detail = ui.get_selected_mail();
                        let gate = account_receive_reader_gate(AccountReceiveReaderObservation {
                            detail_open: ui.get_detail_open(),
                            loading: ui.get_detail_loading(),
                            error: ui.get_detail_error(),
                            selected_id_matches: ui.get_selected_id().as_str()
                                == message_id.as_str(),
                            detail_id_matches: detail.id.as_str() == message_id.as_str(),
                            subject_matches_fixture: detail.subject.as_str()
                                == ACCOUNT_RECEIVE_EXPECTED_SUBJECT,
                            body_matches_fixture: detail
                                .body
                                .as_str()
                                .trim_end_matches(['\r', '\n'])
                                == ACCOUNT_RECEIVE_EXPECTED_BODY,
                        });
                        match gate {
                            AccountReceiveGate::Waiting => {}
                            AccountReceiveGate::Failed(reason) => {
                                fail_account_receive_stress(&ui, &timer, reason, true);
                                state.phase = AccountReceivePhase::Complete;
                            }
                            AccountReceiveGate::Ready => {
                                let account_id = account_id.clone();
                                ui.invoke_switch_account(SharedString::default());
                                state.advance(AccountReceivePhase::ClosingReader { account_id });
                            }
                        }
                    }
                    AccountReceivePhase::ClosingReader { account_id } => {
                        if !ui.get_active_account_id().is_empty()
                            || ui.get_mailbox_loading()
                            || ui.get_mailbox_navigation_loading()
                            || ui.get_detail_open()
                            || ui.get_detail_loading()
                            || !ui.get_selected_id().is_empty()
                            || !ui.get_selected_mail().id.is_empty()
                        {
                            return;
                        }
                        let account_id = account_id.clone();
                        ui.invoke_remove_account(account_id.clone());
                        state.advance(AccountReceivePhase::Removing { account_id });
                    }
                    AccountReceivePhase::Removing { account_id } => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_receive_stress(&ui, &timer, "removal_failed", true);
                            state.phase = AccountReceivePhase::Complete;
                            return;
                        }
                        let account_id = account_id.clone();
                        state.advance(AccountReceivePhase::WaitingForRemoval { account_id });
                    }
                    AccountReceivePhase::WaitingForRemoval { account_id } => {
                        if account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        state.cleanup_required = false;
                        ui.set_status_text("Account receive memory stress complete".into());
                        eprintln!(
                            "NIVALIS_STRESS_RESULT scenario=account-receive steps=1 manual_sync=1 database=1 ui=1 reader=1 imported=1 opened=1 closed=1 removed=1 elapsed_ms={}",
                            state.started.elapsed().as_millis()
                        );
                        stop_stress(&ui, &timer);
                        state.phase = AccountReceivePhase::Complete;
                    }
                    AccountReceivePhase::Complete => {}
                }
            },
        );
    });

    Some(timer)
}

fn account_receive_mailbox_observation(
    ui: &AppWindow,
    expected_account_id: &str,
) -> AccountReceiveMailboxObservation {
    let mails = ui.get_mails();
    let first = mails.row_data(0);
    AccountReceiveMailboxObservation {
        account_selected: ui.get_active_account_id().as_str() == expected_account_id,
        loading: ui.get_initial_loading()
            || ui.get_mailbox_loading()
            || ui.get_mailbox_navigation_loading(),
        error: ui.get_mailbox_error(),
        total_known: ui.get_total_known(),
        message_total: ui.get_message_total(),
        rows: mails.row_count(),
        has_previous: ui.get_has_previous_mailbox_page(),
        has_next: ui.get_has_next_mailbox_page(),
        first_account_matches: first
            .as_ref()
            .is_some_and(|mail| mail.account_id.as_str() == expected_account_id),
        first_id_present: first.as_ref().is_some_and(|mail| !mail.id.is_empty()),
        first_subject_matches: first
            .is_some_and(|mail| mail.subject.as_str() == ACCOUNT_RECEIVE_EXPECTED_SUBJECT),
    }
}

fn fail_account_receive_stress(
    ui: &AppWindow,
    timer: &Timer,
    reason: &str,
    cleanup_required: bool,
) {
    ui.set_status_text("Account receive memory stress failed".into());
    eprintln!(
        "NIVALIS_STRESS_ERROR scenario=account-receive reason={reason} cleanup_required={}",
        u8::from(cleanup_required)
    );
    stop_stress(ui, timer);
}

const ACCOUNT_SEND_MAX_TO_BYTES: usize = 64 * 322;
const ACCOUNT_SEND_MAX_SUBJECT_BYTES: usize = 998;
const ACCOUNT_SEND_MAX_BODY_BYTES: usize = 1024 * 1024;

struct AccountSendMessage {
    to: String,
    subject: String,
    body: String,
}

impl AccountSendMessage {
    fn load() -> Result<Self, &'static str> {
        let to = std::env::var("NIVALIS_STRESS_SEND_TO")
            .unwrap_or_else(|_| "recipient@localhost".into());
        let subject = std::env::var("NIVALIS_STRESS_SEND_SUBJECT")
            .unwrap_or_else(|_| "Nivalis bounded send fixture".into());
        let body = std::env::var("NIVALIS_STRESS_SEND_BODY")
            .unwrap_or_else(|_| "Bounded loopback SMTP body.".into());
        if to.is_empty()
            || to.len() > ACCOUNT_SEND_MAX_TO_BYTES
            || to.bytes().any(|byte| byte < b' ' || byte == 0x7f)
        {
            return Err("send_recipient_invalid");
        }
        if subject.len() > ACCOUNT_SEND_MAX_SUBJECT_BYTES
            || subject.bytes().any(|byte| byte < b' ' || byte == 0x7f)
        {
            return Err("send_subject_invalid");
        }
        if body.len() > ACCOUNT_SEND_MAX_BODY_BYTES {
            return Err("send_body_too_large");
        }
        Ok(Self { to, subject, body })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountSendDeliveryGate {
    Waiting,
    Delivered,
    Failed(&'static str),
}

fn account_send_delivery_gate(
    composer_open: bool,
    composer_loading: bool,
    composer_error: &str,
    status: &str,
    snackbar_visible: bool,
    snackbar: &str,
) -> AccountSendDeliveryGate {
    if !composer_error.is_empty() {
        return AccountSendDeliveryGate::Failed("queue_failed");
    }
    if composer_open || composer_loading {
        return AccountSendDeliveryGate::Waiting;
    }
    if status == "Message sent" || (snackbar_visible && snackbar == "Message sent") {
        return AccountSendDeliveryGate::Delivered;
    }
    match status {
        "Message delivery needs review" => AccountSendDeliveryGate::Failed("delivery_uncertain"),
        "Message could not be sent" => {
            AccountSendDeliveryGate::Failed("delivery_permanent_failure")
        }
        _ => AccountSendDeliveryGate::Waiting,
    }
}

#[derive(Clone, Copy)]
struct AccountSendSentObservation {
    account_selected: bool,
    sent_selected: bool,
    loading: bool,
    error: bool,
    total_known: bool,
    message_total: i32,
    rows: usize,
    drafts: i32,
    has_previous: bool,
    has_next: bool,
    first_account_matches: bool,
    first_subject_matches: bool,
    first_id_present: bool,
}

fn account_send_sent_gate(observation: AccountSendSentObservation) -> AccountReceiveGate {
    if !observation.account_selected || !observation.sent_selected || observation.loading {
        return AccountReceiveGate::Waiting;
    }
    if observation.error {
        return AccountReceiveGate::Failed("sent_mailbox_error");
    }
    if observation.message_total < 0 || observation.drafts < 0 {
        return AccountReceiveGate::Failed("sent_mailbox_count_invalid");
    }
    if observation.message_total > 1 || observation.rows > 1 {
        return AccountReceiveGate::Failed("sent_fixture_not_empty");
    }
    if observation.total_known
        && observation.message_total == 1
        && observation.rows == 1
        && observation.drafts == 0
        && !observation.has_previous
        && !observation.has_next
    {
        if observation.first_account_matches
            && observation.first_subject_matches
            && observation.first_id_present
        {
            AccountReceiveGate::Ready
        } else {
            AccountReceiveGate::Failed("sent_message_invalid")
        }
    } else {
        AccountReceiveGate::Waiting
    }
}

#[derive(Debug)]
enum AccountSendPhase {
    WaitingForInitialState,
    Diagnosing,
    WaitingForCatalog { account_id: SharedString },
    WaitingForAccountMailbox { account_id: SharedString },
    LoadingComposer { account_id: SharedString },
    Queueing { account_id: SharedString },
    WaitingForSent { account_id: SharedString },
    ClosingSent { account_id: SharedString },
    Removing { account_id: SharedString },
    WaitingForRemoval { account_id: SharedString },
    Complete,
}

struct AccountSendStress {
    phase: AccountSendPhase,
    message: Option<AccountSendMessage>,
    started: Instant,
    deadline: Instant,
    transition_timeout: Duration,
    cleanup_required: bool,
}

impl AccountSendStress {
    fn advance(&mut self, phase: AccountSendPhase) {
        self.phase = phase;
        self.deadline = Instant::now() + self.transition_timeout;
    }
}

fn install_account_send_stress(
    ui: &AppWindow,
    steps: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    if steps != 1 {
        eprintln!(
            "NIVALIS_STRESS_ERROR scenario=account-send reason=steps_must_equal_one steps={steps} cleanup_required=0"
        );
        return None;
    }
    let timeout = std::env::var("NIVALIS_STRESS_TRANSITION_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(ACCOUNT_DIAGNOSTIC_DEFAULT_TIMEOUT_MS)
        .max(1);
    let transition_timeout = Duration::from_millis(timeout);
    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(timer) = timer_weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        let state = Rc::new(RefCell::new(AccountSendStress {
            phase: AccountSendPhase::WaitingForInitialState,
            message: None,
            started,
            deadline: started + transition_timeout,
            transition_timeout,
            cleanup_required: false,
        }));
        let state_for_timer = state.clone();
        let timer_for_callback = Rc::downgrade(&timer);
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval.max(25)),
            move || {
                let Some(timer) = timer_for_callback.upgrade() else {
                    return;
                };
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                let mut state = state_for_timer.borrow_mut();
                if Instant::now() >= state.deadline {
                    fail_account_send_stress(
                        &ui,
                        &timer,
                        "transition_timeout",
                        state.cleanup_required,
                    );
                    state.phase = AccountSendPhase::Complete;
                    return;
                }

                match &state.phase {
                    AccountSendPhase::WaitingForInitialState => {
                        if ui.get_initial_loading() || ui.get_mailbox_loading() {
                            return;
                        }
                        if ui.get_mailbox_error() {
                            fail_account_send_stress(&ui, &timer, "initial_mailbox_error", false);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        if ui.get_has_accounts() {
                            fail_account_send_stress(&ui, &timer, "fixture_not_empty", false);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        let message = match AccountSendMessage::load() {
                            Ok(message) => message,
                            Err(reason) => {
                                fail_account_send_stress(&ui, &timer, reason, false);
                                state.phase = AccountSendPhase::Complete;
                                return;
                            }
                        };
                        let config = match AccountDiagnosticConfig::load() {
                            Ok(config) => config,
                            Err(reason) => {
                                fail_account_send_stress(&ui, &timer, reason, false);
                                state.phase = AccountSendPhase::Complete;
                                return;
                            }
                        };
                        let secret = match std::str::from_utf8(&config.secret) {
                            Ok(secret) => SharedString::from(secret),
                            Err(_) => {
                                fail_account_send_stress(&ui, &timer, "secret_file_invalid", false);
                                state.phase = AccountSendPhase::Complete;
                                return;
                            }
                        };
                        state.message = Some(message);
                        ui.invoke_add_account(
                            config.name.into(),
                            config.address.into(),
                            config.login.into(),
                            config.imap_host.into(),
                            config.imap_port.into(),
                            config.smtp_host.into(),
                            config.smtp_port.into(),
                            secret,
                        );
                        state.cleanup_required = true;
                        state.advance(AccountSendPhase::Diagnosing);
                    }
                    AccountSendPhase::Diagnosing => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_send_stress(&ui, &timer, "diagnostic_failed", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        let account_id = ui.get_managed_account_id();
                        if account_id.is_empty() {
                            fail_account_send_stress(&ui, &timer, "account_identity_missing", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        if classify_account_diagnostic(
                            ui.get_managed_account_status().as_str(),
                            ui.get_managed_account_has_error(),
                        ) != Some(AccountDiagnosticExpectation::Ready)
                        {
                            fail_account_send_stress(&ui, &timer, "diagnostic_mismatch", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        state.advance(AccountSendPhase::WaitingForCatalog { account_id });
                    }
                    AccountSendPhase::WaitingForCatalog { account_id } => {
                        if !account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        let account_id = account_id.clone();
                        if ui.get_active_account_id().as_str() != account_id.as_str() {
                            ui.invoke_switch_account(account_id.clone());
                        }
                        state.advance(AccountSendPhase::WaitingForAccountMailbox { account_id });
                    }
                    AccountSendPhase::WaitingForAccountMailbox { account_id } => {
                        match account_receive_mailbox_gate(
                            account_receive_mailbox_observation(&ui, account_id.as_str()),
                            AccountReceiveMailboxExpectation::Empty,
                        ) {
                            AccountReceiveGate::Waiting => {}
                            AccountReceiveGate::Failed(reason) => {
                                fail_account_send_stress(&ui, &timer, reason, true);
                                state.phase = AccountSendPhase::Complete;
                            }
                            AccountReceiveGate::Ready => {
                                if !ui.get_compose_enabled() {
                                    fail_account_send_stress(
                                        &ui,
                                        &timer,
                                        "compose_not_enabled",
                                        true,
                                    );
                                    state.phase = AccountSendPhase::Complete;
                                    return;
                                }
                                let account_id = account_id.clone();
                                ui.set_composer_open(true);
                                ui.invoke_open_composer();
                                state.advance(AccountSendPhase::LoadingComposer { account_id });
                            }
                        }
                    }
                    AccountSendPhase::LoadingComposer { account_id } => {
                        if ui.get_composer_loading() {
                            return;
                        }
                        if !ui.get_composer_error().is_empty() || !ui.get_composer_open() {
                            fail_account_send_stress(&ui, &timer, "composer_load_failed", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        let Some(message) = state.message.as_ref() else {
                            fail_account_send_stress(&ui, &timer, "send_message_missing", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        };
                        let account_id = account_id.clone();
                        ui.invoke_send_message(
                            message.to.clone().into(),
                            message.subject.clone().into(),
                            message.body.clone().into(),
                        );
                        state.advance(AccountSendPhase::Queueing { account_id });
                    }
                    AccountSendPhase::Queueing { account_id } => {
                        if ui.get_composer_loading() {
                            return;
                        }
                        if !ui.get_composer_error().is_empty() {
                            fail_account_send_stress(&ui, &timer, "queue_failed", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        if ui.get_composer_open() {
                            return;
                        }
                        let account_id = account_id.clone();
                        ui.invoke_filter_folder("Sent".into());
                        state.advance(AccountSendPhase::WaitingForSent { account_id });
                    }
                    AccountSendPhase::WaitingForSent { account_id } => {
                        if let AccountSendDeliveryGate::Failed(reason) = account_send_delivery_gate(
                            ui.get_composer_open(),
                            ui.get_composer_loading(),
                            ui.get_composer_error().as_str(),
                            ui.get_status_text().as_str(),
                            ui.get_snackbar_visible(),
                            ui.get_snackbar_text().as_str(),
                        ) {
                            fail_account_send_stress(&ui, &timer, reason, true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        let mails = ui.get_mails();
                        let first = mails.row_data(0);
                        let subject = state
                            .message
                            .as_ref()
                            .map(|message| message.subject.as_str())
                            .unwrap_or_default();
                        let gate = account_send_sent_gate(AccountSendSentObservation {
                            account_selected: ui.get_active_account_id().as_str()
                                == account_id.as_str(),
                            sent_selected: ui.get_active_folder().as_str() == "Sent",
                            loading: ui.get_initial_loading()
                                || ui.get_mailbox_loading()
                                || ui.get_mailbox_navigation_loading(),
                            error: ui.get_mailbox_error(),
                            total_known: ui.get_total_known(),
                            message_total: ui.get_message_total(),
                            rows: mails.row_count(),
                            drafts: ui.get_draft_count(),
                            has_previous: ui.get_has_previous_mailbox_page(),
                            has_next: ui.get_has_next_mailbox_page(),
                            first_account_matches: first.as_ref().is_some_and(|mail| {
                                mail.account_id.as_str() == account_id.as_str()
                            }),
                            first_subject_matches: first
                                .as_ref()
                                .is_some_and(|mail| mail.subject.as_str() == subject),
                            first_id_present: first.is_some_and(|mail| !mail.id.is_empty()),
                        });
                        match gate {
                            AccountReceiveGate::Waiting => {}
                            AccountReceiveGate::Failed(reason) => {
                                fail_account_send_stress(&ui, &timer, reason, true);
                                state.phase = AccountSendPhase::Complete;
                            }
                            AccountReceiveGate::Ready => {
                                let account_id = account_id.clone();
                                ui.invoke_switch_account(SharedString::default());
                                state.advance(AccountSendPhase::ClosingSent { account_id });
                            }
                        }
                    }
                    AccountSendPhase::ClosingSent { account_id } => {
                        if !ui.get_active_account_id().is_empty()
                            || ui.get_mailbox_loading()
                            || ui.get_mailbox_navigation_loading()
                        {
                            return;
                        }
                        let account_id = account_id.clone();
                        ui.invoke_remove_account(account_id.clone());
                        state.advance(AccountSendPhase::Removing { account_id });
                    }
                    AccountSendPhase::Removing { account_id } => {
                        if ui.get_account_operation_loading() {
                            return;
                        }
                        if !ui.get_account_operation_error().is_empty() {
                            fail_account_send_stress(&ui, &timer, "removal_failed", true);
                            state.phase = AccountSendPhase::Complete;
                            return;
                        }
                        let account_id = account_id.clone();
                        state.advance(AccountSendPhase::WaitingForRemoval { account_id });
                    }
                    AccountSendPhase::WaitingForRemoval { account_id } => {
                        if account_model_contains(&ui, account_id.as_str()) {
                            return;
                        }
                        state.cleanup_required = false;
                        ui.set_status_text("Account send memory stress complete".into());
                        eprintln!("{}", account_send_result_marker(state.started.elapsed()));
                        stop_stress(&ui, &timer);
                        state.phase = AccountSendPhase::Complete;
                    }
                    AccountSendPhase::Complete => {}
                }
            },
        );
    });

    Some(timer)
}

fn account_send_result_marker(elapsed: Duration) -> String {
    format!(
        "NIVALIS_STRESS_RESULT scenario=account-send steps=1 queued=1 delivered=1 sent_visible=1 drafts=0 removed=1 elapsed_ms={}",
        elapsed.as_millis()
    )
}

fn fail_account_send_stress(ui: &AppWindow, timer: &Timer, reason: &str, cleanup_required: bool) {
    ui.set_status_text("Account send memory stress failed".into());
    eprintln!(
        "NIVALIS_STRESS_ERROR scenario=account-send reason={reason} cleanup_required={}",
        u8::from(cleanup_required)
    );
    stop_stress(ui, timer);
}

const CONTENT_TARGET_MESSAGE_ID: i64 = 51;
const CONTENT_TARGET_ACCOUNT_ID: i64 = 51;
const CONTENT_ATTACHMENT_BYTES: usize = 256 * 1024;
const CONTENT_GC_LIMIT: usize = 16;
const CONTENT_BOUNDARY: &str = "nivalis-bounded-content-stress";

struct ContentStressResult {
    cycles: usize,
    gc_examined: usize,
    gc_removed: usize,
    gc_missing: usize,
    elapsed_ms: u128,
}

struct ContentCycle {
    generation: i64,
    gc: FileGcOutcome,
}

fn install_content_stress(
    ui: &AppWindow,
    database: DatabaseClient,
    content_path: PathBuf,
    cycles: usize,
    delay: u64,
) -> Option<Rc<Timer>> {
    let staging = match ContentStaging::open(content_path) {
        Ok(staging) => Arc::new(staging),
        Err(_) => {
            eprintln!("NIVALIS_STRESS_ERROR scenario=content reason=content_root_unavailable");
            return None;
        }
    };
    let timer = Rc::new(Timer::default());
    let ui_weak = ui.as_weak();
    timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(delay),
        move || {
            let worker_ui = ui_weak.clone();
            let worker_database = database.clone();
            let worker_staging = staging.clone();
            let worker = std::thread::Builder::new()
                .name("nivalis-content-stress".into())
                .spawn(move || {
                    let result = run_content_stress(&worker_database, &worker_staging, cycles);
                    let _ = worker_ui.upgrade_in_event_loop(move |ui| {
                        match result {
                            Ok(result) => {
                                ui.set_status_text("Content memory stress complete".into());
                                eprintln!(
                                    "NIVALIS_STRESS_RESULT scenario=content cycles={} imports={} body_opens={} attachment_opens={} gc_runs={} gc_examined={} gc_removed={} gc_missing={} files_per_import=2 target_id={} elapsed_ms={}",
                                    result.cycles,
                                    result.cycles,
                                    result.cycles,
                                    result.cycles,
                                    result.cycles,
                                    result.gc_examined,
                                    result.gc_removed,
                                    result.gc_missing,
                                    CONTENT_TARGET_MESSAGE_ID,
                                    result.elapsed_ms
                                );
                            }
                            Err((cycle, reason)) => {
                                ui.set_status_text("Content memory stress failed".into());
                                eprintln!(
                                    "NIVALIS_STRESS_ERROR scenario=content cycle={} reason={reason}",
                                    cycle + 1
                                );
                            }
                        }
                        finish_stress(&ui);
                    });
                });
            if worker.is_err() {
                eprintln!(
                    "NIVALIS_STRESS_ERROR scenario=content reason=worker_start_failed"
                );
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_text("Content memory stress failed".into());
                    finish_stress(&ui);
                }
            }
        },
    );

    Some(timer)
}

fn run_content_stress(
    database: &DatabaseClient,
    staging: &Arc<ContentStaging>,
    cycles: usize,
) -> Result<ContentStressResult, (usize, &'static str)> {
    let message_id = MessageId::new(CONTENT_TARGET_MESSAGE_ID)
        .expect("content benchmark message identity is positive");
    let started = Instant::now();
    let mut last_generation = None;
    let mut gc_examined = 0_usize;
    let mut gc_removed = 0_usize;
    let mut gc_missing = 0_usize;
    for cycle_index in 0..cycles {
        let outcome = run_content_cycle(database, staging, message_id, cycle_index)
            .map_err(|reason| (cycle_index, reason))?;
        if last_generation
            .is_some_and(|generation: i64| generation.checked_add(1) != Some(outcome.generation))
        {
            return Err((cycle_index, "generation_gap"));
        }
        let examined = usize::from(outcome.gc.examined);
        let removed = usize::from(outcome.gc.removed);
        let missing = usize::from(outcome.gc.missing);
        let expected_gc_files = if cycle_index == 0 { 1..=2 } else { 2..=2 };
        if !expected_gc_files.contains(&examined)
            || examined != removed + missing
            || outcome.gc.referenced != 0
            || outcome.gc.invalid_keys != 0
            || outcome.gc.io_errors != 0
        {
            return Err((cycle_index, "gc_mismatch"));
        }
        last_generation = Some(outcome.generation);
        gc_examined += examined;
        gc_removed += removed;
        gc_missing += missing;
    }
    Ok(ContentStressResult {
        cycles,
        gc_examined,
        gc_removed,
        gc_missing,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn run_content_cycle(
    database: &DatabaseClient,
    staging: &Arc<ContentStaging>,
    message_id: MessageId,
    cycle_index: usize,
) -> Result<ContentCycle, &'static str> {
    let raw = content_fixture(cycle_index);
    let limits = ContentLimits::default();
    let prepared = prepare_content(&raw, staging, limits).map_err(|_| "mime_prepare_failed")?;
    drop(raw);
    let published = prepared.publish().map_err(|_| "content_publish_failed")?;
    let record = published.record();
    let Some(body_key) = record.body_file_key.clone() else {
        return Err("body_file_missing");
    };
    if record.attachments.len() != 1 {
        return Err("attachment_count_mismatch");
    }
    let attachment = &record.attachments[0];
    let attachment_key = attachment.file_key.clone();
    let attachment_bytes =
        usize::try_from(attachment.byte_count).map_err(|_| "attachment_size_overflow")?;
    if attachment_bytes != CONTENT_ATTACHMENT_BYTES {
        return Err("attachment_size_mismatch");
    }

    let submission = Box::new(ContentImportSubmission::new(
        message_id,
        AccountId::new(CONTENT_TARGET_ACCOUNT_ID)
            .expect("content benchmark account identity is positive"),
        AccountGeneration::new(1).expect("content benchmark generation is positive"),
        published,
    ));
    let import_reply = database
        .try_import_content(submission)
        .map_err(|failure| match failure.reason() {
            DatabaseSubmitError::Busy => "content_import_busy",
            DatabaseSubmitError::Closed => "content_import_closed",
        })?
        .blocking_recv()
        .map_err(|_| "content_import_reply_closed")?
        .map_err(|_| "content_import_failed")?;

    read_published_file(
        staging,
        &body_key,
        None,
        limits.stored_body_bytes,
        "body_read_failed",
    )?;
    read_published_file(
        staging,
        &attachment_key,
        Some(attachment_bytes),
        limits.decoded_part_bytes,
        "attachment_read_failed",
    )?;

    let gc = database
        .try_run_file_gc(staging, CONTENT_GC_LIMIT)
        .map_err(|_| "content_gc_submit_failed")?
        .blocking_recv()
        .map_err(|_| "content_gc_reply_closed")?
        .map_err(|_| "content_gc_failed")?;

    Ok(ContentCycle {
        generation: import_reply.generation,
        gc,
    })
}

fn content_fixture(cycle_index: usize) -> Vec<u8> {
    const BODY_LINE: &[u8] = b"Bounded local content remains readable.\r\n";
    const BODY_LINES: usize = 1_024;
    let mut raw =
        Vec::with_capacity(BODY_LINE.len() * BODY_LINES + CONTENT_ATTACHMENT_BYTES + 1_024);
    raw.extend_from_slice(b"From: Memory Sender <memory@example.test>\r\n");
    raw.extend_from_slice(b"To: Reader <reader@example.test>\r\n");
    raw.extend_from_slice(format!("Subject: Bounded content cycle {cycle_index}\r\n").as_bytes());
    raw.extend_from_slice(b"Date: Tue, 14 Nov 2023 22:13:20 +0000\r\n");
    raw.extend_from_slice(b"MIME-Version: 1.0\r\n");
    raw.extend_from_slice(
        format!("Content-Type: multipart/mixed; boundary=\"{CONTENT_BOUNDARY}\"\r\n\r\n")
            .as_bytes(),
    );
    raw.extend_from_slice(format!("--{CONTENT_BOUNDARY}\r\n").as_bytes());
    raw.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
    raw.extend_from_slice(b"Content-Transfer-Encoding: 8bit\r\n\r\n");
    for _ in 0..BODY_LINES {
        raw.extend_from_slice(BODY_LINE);
    }
    raw.extend_from_slice(format!("--{CONTENT_BOUNDARY}\r\n").as_bytes());
    raw.extend_from_slice(b"Content-Type: application/octet-stream\r\n");
    raw.extend_from_slice(b"Content-Disposition: attachment; filename=\"payload.bin\"\r\n");
    raw.extend_from_slice(b"Content-Transfer-Encoding: binary\r\n\r\n");
    let attachment_start = raw.len();
    let attachment_end = attachment_start + CONTENT_ATTACHMENT_BYTES;
    raw.resize(
        attachment_end,
        b'a' + u8::try_from(cycle_index % 26).unwrap_or(0),
    );
    raw.extend_from_slice(format!("\r\n--{CONTENT_BOUNDARY}--\r\n").as_bytes());
    raw
}

fn read_published_file(
    staging: &ContentStaging,
    key: &FileKey,
    expected_bytes: Option<usize>,
    maximum_bytes: usize,
    failure: &'static str,
) -> Result<(), &'static str> {
    let mut file = staging.open_file(key).map_err(|_| failure)?;
    let file_bytes =
        usize::try_from(file.metadata().map_err(|_| failure)?.len()).map_err(|_| failure)?;
    if file_bytes == 0
        || file_bytes > maximum_bytes
        || expected_bytes.is_some_and(|expected| expected != file_bytes)
    {
        return Err(failure);
    }
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_usize;
    loop {
        let read = file.read(&mut buffer).map_err(|_| failure)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read)
            .filter(|total| *total <= file_bytes)
            .ok_or(failure)?;
    }
    if total != file_bytes {
        return Err(failure);
    }
    Ok(())
}

fn install_mixed_stress(
    ui: &AppWindow,
    steps: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();
    let step = Rc::new(Cell::new(0usize));

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(timer) = timer_weak.upgrade() else {
            return;
        };
        let started = Instant::now();
        let timer_for_callback = Rc::downgrade(&timer);
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval),
            move || {
                let Some(ui) = ui_weak.upgrade() else {
                    return;
                };
                let current = step.get();
                if current >= steps {
                    ui.set_settings_open(false);
                    ui.set_account_menu_open(false);
                    ui.set_more_menu_open(false);
                    ui.set_message_menu_open(false);
                    ui.set_delete_dialog_open(false);
                    ui.set_detail_open(false);
                    ui.set_search_query("".into());
                    ui.invoke_query_mail("".into());
                    ui.invoke_filter_folder("Inbox".into());
                    ui.set_status_text("Memory stress complete".into());
                    eprintln!(
                        "NIVALIS_STRESS_RESULT scenario=mixed steps={steps} elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    if let Some(timer) = timer_for_callback.upgrade() {
                        timer.stop();
                    }
                    if std::env::var("NIVALIS_STRESS_EXIT").as_deref() == Ok("1") {
                        let _ = ui.hide();
                        let _ = slint::quit_event_loop();
                    }
                    return;
                }

                const FOLDERS: [&str; 5] = ["Inbox", "Unread", "Starred", "Archive", "Trash"];
                match current % 8 {
                    0 => ui.invoke_filter_folder(FOLDERS[current % FOLDERS.len()].into()),
                    1 => {
                        let query = if current % 16 == 1 { "mail" } else { "" };
                        ui.set_search_query(query.into());
                        ui.invoke_query_mail(query.into());
                    }
                    2 => {
                        ui.set_account_menu_open(false);
                        ui.set_settings_open(true);
                    }
                    3 => {
                        ui.set_settings_open(false);
                        ui.set_account_menu_open(true);
                    }
                    4 => {
                        ui.set_account_menu_open(false);
                        ui.set_more_menu_open(true);
                    }
                    5 => {
                        ui.set_more_menu_open(false);
                        ui.set_message_menu_open(true);
                    }
                    6 => {
                        ui.set_message_menu_open(false);
                        ui.set_delete_dialog_open(true);
                    }
                    _ => {
                        ui.set_delete_dialog_open(false);
                        let mails = ui.get_mails();
                        if let Some(mail) = mails.row_data(current % mails.row_count().max(1)) {
                            ui.set_detail_open(true);
                            ui.invoke_select_mail(mail.id);
                        } else {
                            ui.set_detail_open(false);
                        }
                    }
                }
                step.set(current + 1);
            },
        );
    });

    Some(timer)
}

const WRITE_SEARCH_TARGET_ID: &str = "51";
const WRITE_SEARCH_QUERY: &str = "message 51";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteSearchPhase {
    Initial,
    Write,
    Search,
    Clear,
}

struct WriteSearchStress {
    phase: WriteSearchPhase,
    cycles: usize,
    writes: usize,
    searches: usize,
    clears: usize,
    initial_starred: bool,
    expected_starred: bool,
    baseline: Option<MailboxQueryCounts>,
    deadline: Instant,
    started: Instant,
}

enum WriteSearchAction {
    Wait,
    ToggleStar,
    Search,
    Clear,
    Complete(MailboxQueryCounts),
    Fail(Box<str>),
}

fn install_write_search_stress(
    ui: &AppWindow,
    cycles: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    if !cycles.is_multiple_of(2) {
        eprintln!(
            "NIVALIS_STRESS_ERROR scenario=write-search reason=cycles_must_be_even cycles={cycles}"
        );
        return None;
    }
    let timeout = std::env::var("NIVALIS_STRESS_TRANSITION_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000)
        .max(1);

    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();
    let target_id = SharedString::from(WRITE_SEARCH_TARGET_ID);
    let search_query = SharedString::from(WRITE_SEARCH_QUERY);
    let clear_query = SharedString::default();
    Timer::single_shot(Duration::from_millis(delay), move || {
        let (Some(timer), Some(ui)) = (timer_weak.upgrade(), ui_weak.upgrade()) else {
            return;
        };
        let started = Instant::now();
        let state = Rc::new(RefCell::new(WriteSearchStress {
            phase: WriteSearchPhase::Initial,
            cycles: 0,
            writes: 0,
            searches: 0,
            clears: 0,
            initial_starred: false,
            expected_starred: false,
            baseline: None,
            deadline: started + Duration::from_millis(timeout),
            started,
        }));
        let timer_for_callback = Rc::downgrade(&timer);
        let ui_weak = ui.as_weak();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval),
            move || {
                let (Some(timer), Some(ui)) =
                    (timer_for_callback.upgrade(), ui_weak.upgrade())
                else {
                    return;
                };
                let action = write_search_action(
                    &ui,
                    &mut state.borrow_mut(),
                    cycles,
                    Duration::from_millis(timeout),
                );
                match action {
                    WriteSearchAction::Wait => {}
                    WriteSearchAction::ToggleStar => {
                        ui.invoke_toggle_star(target_id.clone());
                    }
                    WriteSearchAction::Search => {
                        ui.set_search_query(search_query.clone());
                        ui.invoke_query_mail(search_query.clone());
                    }
                    WriteSearchAction::Clear => {
                        ui.set_search_query(clear_query.clone());
                        ui.invoke_query_mail(clear_query.clone());
                    }
                    WriteSearchAction::Complete(delta) => {
                        let state = state.borrow();
                        ui.set_status_text("Write and search memory stress complete".into());
                        eprintln!(
                            "NIVALIS_STRESS_RESULT scenario=write-search cycles={} writes={} searches={} clears={} first_queries={} after_queries={} before_queries={} target_id={} final_page=1 final_query=empty final_starred={} elapsed_ms={}",
                            state.cycles,
                            state.writes,
                            state.searches,
                            state.clears,
                            delta.first,
                            delta.after,
                            delta.before,
                            WRITE_SEARCH_TARGET_ID,
                            state.expected_starred,
                            state.started.elapsed().as_millis()
                        );
                        drop(state);
                        stop_stress(&ui, &timer);
                    }
                    WriteSearchAction::Fail(reason) => {
                        let state = state.borrow();
                        ui.set_status_text("Write and search memory stress failed".into());
                        eprintln!(
                            "NIVALIS_STRESS_ERROR scenario=write-search cycles={} writes={} searches={} clears={} reason={reason}",
                            state.cycles, state.writes, state.searches, state.clears
                        );
                        drop(state);
                        stop_stress(&ui, &timer);
                    }
                }
            },
        );
    });

    Some(timer)
}

fn write_search_action(
    ui: &AppWindow,
    state: &mut WriteSearchStress,
    target_cycles: usize,
    timeout: Duration,
) -> WriteSearchAction {
    if ui.get_mailbox_error() {
        return WriteSearchAction::Fail("mailbox_error".into());
    }
    let now = Instant::now();

    match state.phase {
        WriteSearchPhase::Initial => {
            if now >= state.deadline {
                return WriteSearchAction::Fail(write_search_mismatch(
                    "initial_page_timeout",
                    ui,
                    state.expected_starred,
                ));
            }
            if !write_search_page_matches(ui) {
                return WriteSearchAction::Wait;
            }
            let Some(starred) = write_search_target_starred(ui) else {
                return WriteSearchAction::Fail("target_message_missing".into());
            };
            state.baseline = Some(mailbox_query_counts());
            state.initial_starred = starred;
            state.expected_starred = !starred;
            state.phase = WriteSearchPhase::Write;
            state.deadline = now + timeout;
            WriteSearchAction::ToggleStar
        }
        WriteSearchPhase::Write => {
            if now >= state.deadline {
                return WriteSearchAction::Fail(write_search_mismatch(
                    "write_timeout",
                    ui,
                    state.expected_starred,
                ));
            }
            if ui.get_mutation_loading()
                || !write_search_page_matches(ui)
                || write_search_target_starred(ui) != Some(state.expected_starred)
            {
                return WriteSearchAction::Wait;
            }
            if let Err(reason) = write_search_query_delta(state, 1) {
                return WriteSearchAction::Fail(reason);
            }
            state.writes += 1;
            state.phase = WriteSearchPhase::Search;
            state.deadline = now + timeout;
            WriteSearchAction::Search
        }
        WriteSearchPhase::Search => {
            if now >= state.deadline {
                return WriteSearchAction::Fail(write_search_mismatch(
                    "search_timeout",
                    ui,
                    state.expected_starred,
                ));
            }
            if !write_search_result_matches(ui, state.expected_starred) {
                return WriteSearchAction::Wait;
            }
            if let Err(reason) = write_search_query_delta(state, 2) {
                return WriteSearchAction::Fail(reason);
            }
            state.searches += 1;
            state.phase = WriteSearchPhase::Clear;
            state.deadline = now + timeout;
            WriteSearchAction::Clear
        }
        WriteSearchPhase::Clear => {
            if now >= state.deadline {
                return WriteSearchAction::Fail(write_search_mismatch(
                    "clear_timeout",
                    ui,
                    state.expected_starred,
                ));
            }
            if !write_search_page_matches(ui)
                || write_search_target_starred(ui) != Some(state.expected_starred)
            {
                return WriteSearchAction::Wait;
            }
            let delta = match write_search_query_delta(state, 3) {
                Ok(delta) => delta,
                Err(reason) => return WriteSearchAction::Fail(reason),
            };
            state.clears += 1;
            state.cycles += 1;
            if state.cycles == target_cycles {
                if state.expected_starred != state.initial_starred {
                    return WriteSearchAction::Fail("final_starred_state_mismatch".into());
                }
                WriteSearchAction::Complete(delta)
            } else {
                state.expected_starred = !state.expected_starred;
                state.phase = WriteSearchPhase::Write;
                state.deadline = now + timeout;
                WriteSearchAction::ToggleStar
            }
        }
    }
}

fn write_search_page_matches(ui: &AppWindow) -> bool {
    ui.get_search_query().is_empty()
        && ui.get_mail_actions_enabled()
        && !ui.get_mutation_loading()
        && page_one_matches(ui)
}

fn write_search_result_matches(ui: &AppWindow, expected_starred: bool) -> bool {
    if ui.get_search_query().as_str() != WRITE_SEARCH_QUERY
        || ui.get_mailbox_loading()
        || ui.get_mailbox_navigation_loading()
        || ui.get_mutation_loading()
        || ui.get_mailbox_error()
        || ui.get_mailbox_page_number() != 1
        || ui.get_total_known()
        || ui.get_message_total() != 0
        || ui.get_has_previous_mailbox_page()
        || ui.get_has_next_mailbox_page()
    {
        return false;
    }
    let mails = ui.get_mails();
    mails.row_count() == 1
        && mails.row_data(0).is_some_and(|mail| {
            mail.id.as_str() == WRITE_SEARCH_TARGET_ID && mail.starred == expected_starred
        })
}

fn write_search_target_starred(ui: &AppWindow) -> Option<bool> {
    let mails = ui.get_mails();
    (0..mails.row_count()).find_map(|index| {
        let mail = mails.row_data(index)?;
        (mail.id.as_str() == WRITE_SEARCH_TARGET_ID).then_some(mail.starred)
    })
}

fn write_search_query_delta(
    state: &WriteSearchStress,
    queries_in_cycle: usize,
) -> Result<MailboxQueryCounts, Box<str>> {
    validate_write_search_query_delta(
        state.baseline,
        mailbox_query_counts(),
        state.cycles,
        queries_in_cycle,
    )
}

fn validate_write_search_query_delta(
    baseline: Option<MailboxQueryCounts>,
    current: MailboxQueryCounts,
    completed_cycles: usize,
    queries_in_cycle: usize,
) -> Result<MailboxQueryCounts, Box<str>> {
    let Some(actual) = query_count_delta(baseline, current) else {
        return Err("mailbox_query_counter_regressed".into());
    };
    let Some(expected_first) =
        expected_write_search_first_queries(completed_cycles, queries_in_cycle)
    else {
        return Err("write_search_query_count_overflow".into());
    };
    if actual.first != expected_first || actual.after != 0 || actual.before != 0 {
        return Err(format!(
            "query_count_mismatch expected_first={expected_first} expected_after=0 expected_before=0 actual_first={} actual_after={} actual_before={}",
            actual.first, actual.after, actual.before
        )
        .into_boxed_str());
    }
    Ok(actual)
}

fn expected_write_search_first_queries(
    completed_cycles: usize,
    queries_in_cycle: usize,
) -> Option<u64> {
    if !(1..=3).contains(&queries_in_cycle) {
        return None;
    }
    let count = completed_cycles
        .checked_mul(3)?
        .checked_add(queries_in_cycle)?;
    u64::try_from(count).ok()
}

fn write_search_mismatch(stage: &str, ui: &AppWindow, expected_starred: bool) -> Box<str> {
    format!(
        "{stage} query={:?} rows={} page={} total_known={} message_total={} starred_total={} mailbox_error={} mailbox_loading={} navigation_loading={} mutation_loading={} actions_enabled={} target_starred={:?} expected_starred={expected_starred}",
        ui.get_search_query().as_str(),
        ui.get_mails().row_count(),
        ui.get_mailbox_page_number(),
        ui.get_total_known(),
        ui.get_message_total(),
        ui.get_starred_count(),
        ui.get_mailbox_error(),
        ui.get_mailbox_loading(),
        ui.get_mailbox_navigation_loading(),
        ui.get_mutation_loading(),
        ui.get_mail_actions_enabled(),
        write_search_target_starred(ui)
    )
    .into_boxed_str()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaginationPhase {
    Initial,
    Next,
    Previous,
}

struct PaginationStress {
    phase: PaginationPhase,
    transitions: usize,
    baseline: Option<MailboxQueryCounts>,
    deadline: Instant,
    started: Instant,
}

enum PaginationAction {
    Wait,
    Next,
    Previous,
    Complete(MailboxQueryCounts),
    Fail(Box<str>),
}

fn install_pagination_stress(
    ui: &AppWindow,
    steps: usize,
    delay: u64,
    interval: u64,
) -> Option<Rc<Timer>> {
    if !steps.is_multiple_of(2) {
        eprintln!(
            "NIVALIS_STRESS_ERROR scenario=pagination reason=transitions_must_be_even transitions={steps}"
        );
        return None;
    }
    let timeout = std::env::var("NIVALIS_STRESS_TRANSITION_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000)
        .max(1);

    let timer = Rc::new(Timer::default());
    let timer_weak = Rc::downgrade(&timer);
    let ui_weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(delay), move || {
        let (Some(timer), Some(ui)) = (timer_weak.upgrade(), ui_weak.upgrade()) else {
            return;
        };
        let started = Instant::now();
        let state = Rc::new(RefCell::new(PaginationStress {
            phase: PaginationPhase::Initial,
            transitions: 0,
            baseline: None,
            deadline: started + Duration::from_millis(timeout),
            started,
        }));
        let timer_for_callback = Rc::downgrade(&timer);
        let ui_weak = ui.as_weak();
        timer.start(
            TimerMode::Repeated,
            Duration::from_millis(interval),
            move || {
                let (Some(timer), Some(ui)) =
                    (timer_for_callback.upgrade(), ui_weak.upgrade())
                else {
                    return;
                };
                let action = pagination_action(
                    &ui,
                    &mut state.borrow_mut(),
                    steps,
                    Duration::from_millis(timeout),
                );
                match action {
                    PaginationAction::Wait => {}
                    PaginationAction::Next => ui.invoke_next_mailbox_page(),
                    PaginationAction::Previous => ui.invoke_previous_mailbox_page(),
                    PaginationAction::Complete(delta) => {
                        let elapsed_ms = state.borrow().started.elapsed().as_millis();
                        ui.set_status_text("Pagination memory stress complete".into());
                        eprintln!(
                            "NIVALIS_STRESS_RESULT scenario=pagination transitions={steps} after={} before={} final_page=1 elapsed_ms={elapsed_ms}",
                            delta.after, delta.before
                        );
                        stop_stress(&ui, &timer);
                    }
                    PaginationAction::Fail(reason) => {
                        ui.set_status_text("Pagination memory stress failed".into());
                        eprintln!(
                            "NIVALIS_STRESS_ERROR scenario=pagination transitions={} reason={reason}",
                            state.borrow().transitions
                        );
                        stop_stress(&ui, &timer);
                    }
                }
            },
        );
    });

    Some(timer)
}

fn pagination_action(
    ui: &AppWindow,
    state: &mut PaginationStress,
    target_transitions: usize,
    timeout: Duration,
) -> PaginationAction {
    if ui.get_mailbox_error() {
        return PaginationAction::Fail("mailbox_error".into());
    }
    let now = Instant::now();

    match state.phase {
        PaginationPhase::Initial => {
            if now >= state.deadline {
                PaginationAction::Fail(page_mismatch("initial_page_timeout", ui))
            } else if page_one_matches(ui) {
                state.baseline = Some(mailbox_query_counts());
                state.phase = PaginationPhase::Next;
                state.deadline = now + timeout;
                PaginationAction::Next
            } else {
                PaginationAction::Wait
            }
        }
        PaginationPhase::Next => {
            if now >= state.deadline {
                return PaginationAction::Fail("next_page_timeout".into());
            }
            if ui.get_mailbox_navigation_loading() {
                return PaginationAction::Wait;
            }
            if !page_two_matches(ui) {
                return PaginationAction::Fail(page_mismatch("next_page", ui));
            }
            let Some(delta) = query_count_delta(state.baseline, mailbox_query_counts()) else {
                return PaginationAction::Fail("mailbox_query_counter_regressed".into());
            };
            let expected_after = u64::try_from(state.transitions / 2 + 1).unwrap_or(u64::MAX);
            let expected_before = u64::try_from(state.transitions / 2).unwrap_or(u64::MAX);
            if delta.first != 0 || delta.after != expected_after || delta.before != expected_before
            {
                return PaginationAction::Fail(counter_mismatch(
                    expected_after,
                    expected_before,
                    delta,
                ));
            }
            state.transitions += 1;
            state.phase = PaginationPhase::Previous;
            state.deadline = now + timeout;
            PaginationAction::Previous
        }
        PaginationPhase::Previous => {
            if now >= state.deadline {
                return PaginationAction::Fail("previous_page_timeout".into());
            }
            if ui.get_mailbox_navigation_loading() {
                return PaginationAction::Wait;
            }
            if !page_one_matches(ui) {
                return PaginationAction::Fail(page_mismatch("previous_page", ui));
            }
            let Some(delta) = query_count_delta(state.baseline, mailbox_query_counts()) else {
                return PaginationAction::Fail("mailbox_query_counter_regressed".into());
            };
            let expected = u64::try_from(state.transitions.div_ceil(2)).unwrap_or(u64::MAX);
            if delta.first != 0 || delta.after != expected || delta.before != expected {
                return PaginationAction::Fail(counter_mismatch(expected, expected, delta));
            }
            state.transitions += 1;
            if state.transitions == target_transitions {
                PaginationAction::Complete(delta)
            } else {
                state.phase = PaginationPhase::Next;
                state.deadline = now + timeout;
                PaginationAction::Next
            }
        }
    }
}

fn page_one_matches(ui: &AppWindow) -> bool {
    page_matches(ui, 1, 50, "51", "2", false, true)
}

fn page_two_matches(ui: &AppWindow) -> bool {
    page_matches(ui, 2, 1, "1", "1", true, false)
}

fn page_matches(
    ui: &AppWindow,
    page_number: i32,
    row_count: usize,
    first_id: &str,
    last_id: &str,
    has_previous: bool,
    has_next: bool,
) -> bool {
    if ui.get_mailbox_loading()
        || ui.get_mailbox_navigation_loading()
        || ui.get_mailbox_error()
        || ui.get_mailbox_page_number() != page_number
        || ui.get_has_previous_mailbox_page() != has_previous
        || ui.get_has_next_mailbox_page() != has_next
        || !ui.get_total_known()
        || ui.get_message_total() != 51
    {
        return false;
    }
    let mails = ui.get_mails();
    mails.row_count() == row_count
        && mails
            .row_data(0)
            .is_some_and(|mail| mail.id.as_str() == first_id)
        && mails
            .row_data(row_count - 1)
            .is_some_and(|mail| mail.id.as_str() == last_id)
}

fn page_mismatch(stage: &str, ui: &AppWindow) -> Box<str> {
    let mails = ui.get_mails();
    let first = mails
        .row_data(0)
        .map(|mail| mail.id.to_string())
        .unwrap_or_else(|| "none".to_owned());
    let last = mails
        .row_data(mails.row_count().saturating_sub(1))
        .map(|mail| mail.id.to_string())
        .unwrap_or_else(|| "none".to_owned());
    format!(
        "{stage}_signature page={} rows={} first={} last={} previous={} next={} mailbox_loading={} navigation_loading={}",
        ui.get_mailbox_page_number(),
        mails.row_count(),
        first,
        last,
        ui.get_has_previous_mailbox_page(),
        ui.get_has_next_mailbox_page(),
        ui.get_mailbox_loading(),
        ui.get_mailbox_navigation_loading()
    )
    .into_boxed_str()
}

fn query_count_delta(
    baseline: Option<MailboxQueryCounts>,
    current: MailboxQueryCounts,
) -> Option<MailboxQueryCounts> {
    let baseline = baseline?;
    Some(MailboxQueryCounts {
        first: current.first.checked_sub(baseline.first)?,
        after: current.after.checked_sub(baseline.after)?,
        before: current.before.checked_sub(baseline.before)?,
    })
}

fn counter_mismatch(
    expected_after: u64,
    expected_before: u64,
    actual: MailboxQueryCounts,
) -> Box<str> {
    format!(
        "query_count_mismatch expected_first=0 expected_after={expected_after} expected_before={expected_before} actual_first={} actual_after={} actual_before={}",
        actual.first, actual.after, actual.before
    )
    .into_boxed_str()
}

fn stop_stress(ui: &AppWindow, timer: &Timer) {
    timer.stop();
    finish_stress(ui);
}

fn finish_stress(ui: &AppWindow) {
    if std::env::var("NIVALIS_STRESS_EXIT").as_deref() == Ok("1") {
        let _ = ui.hide();
        let _ = slint::quit_event_loop();
    }
}

pub(crate) fn install_maximize_stress(ui: &AppWindow) {
    if std::env::var("NIVALIS_MAXIMIZE_STRESS").as_deref() != Ok("1") {
        return;
    }

    let delay = std::env::var("NIVALIS_MAXIMIZE_STRESS_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let duration = std::env::var("NIVALIS_MAXIMIZE_STRESS_DURATION_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(5_000);
    let ui_weak = ui.as_weak();

    Timer::single_shot(Duration::from_millis(delay), move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        ui.window().set_maximized(true);

        let ui_weak = ui.as_weak();
        Timer::single_shot(Duration::from_millis(duration), move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.window().set_maximized(false);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_scenarios_explicitly_isolate_manual_protocol_work() {
        assert!(automatic_sync_enabled_for_scenario(None));
        assert!(automatic_sync_enabled_for_scenario(Some("mixed")));
        assert!(!automatic_sync_enabled_for_scenario(Some(
            "account-diagnostic"
        )));
        assert!(!automatic_sync_enabled_for_scenario(Some(
            "account-receive"
        )));
        assert!(!automatic_sync_enabled_for_scenario(Some("account-send")));
    }

    #[test]
    fn account_diagnostic_result_classification_is_strict() {
        assert_eq!(
            AccountDiagnosticExpectation::parse("ready"),
            Some(AccountDiagnosticExpectation::Ready)
        );
        assert_eq!(AccountDiagnosticExpectation::parse("authentication"), None);
        assert_eq!(AccountDiagnosticExpectation::parse("offline"), None);
        assert_eq!(
            classify_account_diagnostic("Connected", false),
            Some(AccountDiagnosticExpectation::Ready)
        );
        assert_eq!(
            classify_account_diagnostic("Ready", false),
            Some(AccountDiagnosticExpectation::Ready)
        );
        assert_eq!(
            classify_account_diagnostic("Sign-in was rejected", true),
            None
        );
        assert_eq!(classify_account_diagnostic("Connected", true), None);
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("imap.example.test"));
    }

    #[test]
    fn account_receive_mailbox_gate_requires_exact_bounded_signatures() {
        let empty = AccountReceiveMailboxObservation {
            account_selected: true,
            loading: false,
            error: false,
            total_known: true,
            message_total: 0,
            rows: 0,
            has_previous: false,
            has_next: false,
            first_account_matches: false,
            first_id_present: false,
            first_subject_matches: false,
        };
        assert_eq!(
            account_receive_mailbox_gate(empty, AccountReceiveMailboxExpectation::Empty),
            AccountReceiveGate::Ready
        );
        assert_eq!(
            account_receive_mailbox_gate(empty, AccountReceiveMailboxExpectation::Single),
            AccountReceiveGate::Waiting
        );

        let single = AccountReceiveMailboxObservation {
            message_total: 1,
            rows: 1,
            first_account_matches: true,
            first_id_present: true,
            first_subject_matches: true,
            ..empty
        };
        assert_eq!(
            account_receive_mailbox_gate(single, AccountReceiveMailboxExpectation::Single),
            AccountReceiveGate::Ready
        );
        assert_eq!(
            account_receive_mailbox_gate(single, AccountReceiveMailboxExpectation::Empty),
            AccountReceiveGate::Failed("fixture_not_empty")
        );
        assert_eq!(
            account_receive_mailbox_gate(
                AccountReceiveMailboxObservation {
                    rows: 2,
                    message_total: 2,
                    ..single
                },
                AccountReceiveMailboxExpectation::Single,
            ),
            AccountReceiveGate::Failed("import_count_mismatch")
        );
        assert_eq!(
            account_receive_mailbox_gate(
                AccountReceiveMailboxObservation {
                    first_account_matches: false,
                    ..single
                },
                AccountReceiveMailboxExpectation::Single,
            ),
            AccountReceiveGate::Failed("imported_message_invalid")
        );
        assert_eq!(
            account_receive_mailbox_gate(
                AccountReceiveMailboxObservation {
                    loading: true,
                    error: true,
                    ..single
                },
                AccountReceiveMailboxExpectation::Single,
            ),
            AccountReceiveGate::Waiting
        );
    }

    #[test]
    fn account_receive_reader_gate_rejects_unreadable_or_wrong_details() {
        let ready = AccountReceiveReaderObservation {
            detail_open: true,
            loading: false,
            error: false,
            selected_id_matches: true,
            detail_id_matches: true,
            subject_matches_fixture: true,
            body_matches_fixture: true,
        };
        assert_eq!(
            account_receive_reader_gate(ready),
            AccountReceiveGate::Ready
        );
        assert_eq!(
            account_receive_reader_gate(AccountReceiveReaderObservation {
                loading: true,
                ..ready
            }),
            AccountReceiveGate::Waiting
        );
        assert_eq!(
            account_receive_reader_gate(AccountReceiveReaderObservation {
                body_matches_fixture: false,
                ..ready
            }),
            AccountReceiveGate::Failed("reader_fixture_mismatch")
        );
        assert_eq!(
            account_receive_reader_gate(AccountReceiveReaderObservation {
                detail_id_matches: false,
                ..ready
            }),
            AccountReceiveGate::Failed("opened_message_mismatch")
        );
        assert_eq!(
            account_receive_reader_gate(AccountReceiveReaderObservation {
                error: true,
                ..ready
            }),
            AccountReceiveGate::Failed("detail_error")
        );
    }

    #[test]
    fn account_receive_database_gate_requires_one_private_durable_body() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "nivalis-account-receive-database-{}-{timestamp}",
            std::process::id()
        ));
        let content = root.join("content");
        let body_directory = content.join("body");
        std::fs::create_dir_all(&body_directory).unwrap();
        let database = root.join("mail.sqlite3");
        let body_key = "body/11111111111111111111111111111111.txt";
        std::fs::write(
            content.join(body_key),
            format!("{ACCOUNT_RECEIVE_EXPECTED_BODY}\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(&content, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::set_permissions(&body_directory, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::set_permissions(
                content.join(body_key),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (
                     id INTEGER PRIMARY KEY,
                     account_id INTEGER NOT NULL,
                     subject TEXT NOT NULL
                 );
                 CREATE TABLE folders (
                     id INTEGER PRIMARY KEY,
                     account_id INTEGER NOT NULL,
                     role TEXT NOT NULL
                 );
                 CREATE TABLE message_folders (
                     message_id INTEGER NOT NULL,
                     folder_id INTEGER NOT NULL,
                     account_id INTEGER NOT NULL
                 );
                 CREATE TABLE message_content (
                     message_id INTEGER PRIMARY KEY,
                     body_file_key TEXT,
                     body_byte_count INTEGER NOT NULL,
                     reader_excerpt TEXT NOT NULL
                 );
                 INSERT INTO messages VALUES (7, 3, 'Received memory fixture');
                 INSERT INTO folders VALUES (11, 3, 'inbox');
                 INSERT INTO message_folders VALUES (7, 11, 3);
                 INSERT INTO message_content VALUES (
                     7, 'body/11111111111111111111111111111111.txt',
                     22, 'Bounded receive body.\n'
                 );",
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            account_receive_database_gate(&database, &content, "3", "7"),
            AccountReceiveGate::Ready
        );
        std::fs::write(content.join(body_key), b"").unwrap();
        assert_eq!(
            account_receive_database_gate(&database, &content, "3", "7"),
            AccountReceiveGate::Failed("database_body_file_mismatch")
        );
        std::fs::write(content.join(body_key), b"Different body.\n").unwrap();
        assert_eq!(
            account_receive_database_gate(&database, &content, "3", "7"),
            AccountReceiveGate::Failed("database_body_excerpt_mismatch")
        );
        std::fs::write(
            content.join(body_key),
            format!("{ACCOUNT_RECEIVE_EXPECTED_BODY}\n"),
        )
        .unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("UPDATE messages SET subject = 'Different subject'", [])
            .unwrap();
        drop(connection);
        assert_eq!(
            account_receive_database_gate(&database, &content, "3", "7"),
            AccountReceiveGate::Failed("database_fixture_mismatch")
        );
        assert_eq!(
            account_receive_database_gate(&database, &content, "4", "7"),
            AccountReceiveGate::Failed("database_account_mismatch")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};

            let connection = Connection::open(&database).unwrap();
            connection
                .execute(
                    "UPDATE messages SET subject = 'Received memory fixture'",
                    [],
                )
                .unwrap();
            drop(connection);
            std::fs::set_permissions(
                content.join(body_key),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert_eq!(
                account_receive_database_gate(&database, &content, "3", "7"),
                AccountReceiveGate::Failed("database_body_not_private")
            );

            let external = root.join("external-body.txt");
            std::fs::write(&external, format!("{ACCOUNT_RECEIVE_EXPECTED_BODY}\n")).unwrap();
            std::fs::remove_file(content.join(body_key)).unwrap();
            symlink(&external, content.join(body_key)).unwrap();
            assert_eq!(
                account_receive_database_gate(&database, &content, "3", "7"),
                AccountReceiveGate::Failed("database_body_file_mismatch")
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn account_send_delivery_gate_accepts_only_fenced_delivered_feedback() {
        assert_eq!(
            account_send_delivery_gate(false, false, "", "Message sent", false, ""),
            AccountSendDeliveryGate::Delivered
        );
        assert_eq!(
            account_send_delivery_gate(false, false, "", "Inbox ready", true, "Message sent"),
            AccountSendDeliveryGate::Delivered
        );
        assert_eq!(
            account_send_delivery_gate(false, false, "", "Message queued", false, ""),
            AccountSendDeliveryGate::Waiting
        );
        assert_eq!(
            account_send_delivery_gate(
                false,
                false,
                "",
                "Message delivery needs review",
                false,
                "",
            ),
            AccountSendDeliveryGate::Failed("delivery_uncertain")
        );
        assert_eq!(
            account_send_delivery_gate(false, false, "invalid", "Message sent", false, ""),
            AccountSendDeliveryGate::Failed("queue_failed")
        );
    }

    #[test]
    fn account_send_sent_gate_requires_one_sent_row_and_no_draft() {
        let delivered = AccountSendSentObservation {
            account_selected: true,
            sent_selected: true,
            loading: false,
            error: false,
            total_known: true,
            message_total: 1,
            rows: 1,
            drafts: 0,
            has_previous: false,
            has_next: false,
            first_account_matches: true,
            first_subject_matches: true,
            first_id_present: true,
        };
        assert_eq!(account_send_sent_gate(delivered), AccountReceiveGate::Ready);
        assert_eq!(
            account_send_sent_gate(AccountSendSentObservation {
                loading: true,
                ..delivered
            }),
            AccountReceiveGate::Waiting
        );
        assert_eq!(
            account_send_sent_gate(AccountSendSentObservation {
                drafts: 1,
                ..delivered
            }),
            AccountReceiveGate::Waiting
        );
        assert_eq!(
            account_send_sent_gate(AccountSendSentObservation {
                first_subject_matches: false,
                ..delivered
            }),
            AccountReceiveGate::Failed("sent_message_invalid")
        );
        assert_eq!(
            account_send_sent_gate(AccountSendSentObservation {
                message_total: 2,
                rows: 2,
                ..delivered
            }),
            AccountReceiveGate::Failed("sent_fixture_not_empty")
        );
        assert_eq!(
            account_send_result_marker(Duration::from_millis(17)),
            "NIVALIS_STRESS_RESULT scenario=account-send steps=1 queued=1 delivered=1 sent_visible=1 drafts=0 removed=1 elapsed_ms=17"
        );
    }

    #[cfg(unix)]
    #[test]
    fn account_diagnostic_secret_file_is_private_bounded_and_utf8() {
        use std::os::unix::fs::PermissionsExt;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "nivalis-account-diagnostic-secret-{}-{timestamp}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("secret");
        std::fs::write(&path, b"one-time-app-password").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let secret = read_account_diagnostic_secret(&path).unwrap();
        assert_eq!(&*secret, b"one-time-app-password");
        drop(secret);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            read_account_diagnostic_secret(&path),
            Err("secret_file_permissions")
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, [0xff]).unwrap();
        assert!(matches!(
            read_account_diagnostic_secret(&path),
            Err("secret_file_invalid")
        ));
        std::fs::write(
            &path,
            vec![b'x'; usize::try_from(ACCOUNT_DIAGNOSTIC_SECRET_LIMIT).unwrap() + 1],
        )
        .unwrap();
        assert!(matches!(
            read_account_diagnostic_secret(&path),
            Err("secret_file_size")
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_fixture_projects_one_bounded_body_and_attachment() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "nivalis-content-benchmark-{}-{timestamp}",
            std::process::id()
        ));
        let staging = ContentStaging::open(root.clone()).unwrap();
        let raw = content_fixture(7);
        let limits = ContentLimits::default();
        let published = prepare_content(&raw, &staging, limits)
            .unwrap()
            .publish()
            .unwrap();
        let record = published.record();

        assert!(record.body_byte_count > 0);
        assert_eq!(record.attachments.len(), 1);
        assert_eq!(
            usize::try_from(record.attachments[0].byte_count).unwrap(),
            CONTENT_ATTACHMENT_BYTES
        );
        read_published_file(
            &staging,
            record.body_file_key.as_ref().unwrap(),
            None,
            limits.stored_body_bytes,
            "body",
        )
        .unwrap();
        read_published_file(
            &staging,
            &record.attachments[0].file_key,
            Some(CONTENT_ATTACHMENT_BYTES),
            limits.decoded_part_bytes,
            "attachment",
        )
        .unwrap();

        drop(published);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mailbox_query_deltas_are_checked_without_wrapping() {
        let baseline = MailboxQueryCounts {
            first: 4,
            after: 8,
            before: 12,
        };
        assert_eq!(
            query_count_delta(
                Some(baseline),
                MailboxQueryCounts {
                    first: 4,
                    after: 11,
                    before: 14,
                },
            ),
            Some(MailboxQueryCounts {
                first: 0,
                after: 3,
                before: 2,
            })
        );
        assert_eq!(
            query_count_delta(
                Some(baseline),
                MailboxQueryCounts {
                    first: 3,
                    after: 11,
                    before: 14,
                },
            ),
            None
        );
        assert_eq!(query_count_delta(None, baseline), None);
    }

    #[test]
    fn counter_mismatch_reports_expected_and_observed_classes() {
        let message = counter_mismatch(
            5,
            4,
            MailboxQueryCounts {
                first: 1,
                after: 3,
                before: 2,
            },
        );
        assert!(message.contains("expected_first=0"));
        assert!(message.contains("expected_after=5"));
        assert!(message.contains("expected_before=4"));
        assert!(message.contains("actual_first=1"));
        assert!(message.contains("actual_after=3"));
        assert!(message.contains("actual_before=2"));
    }

    #[test]
    fn write_search_query_expectations_are_exact_and_checked() {
        assert_eq!(expected_write_search_first_queries(0, 1), Some(1));
        assert_eq!(expected_write_search_first_queries(0, 3), Some(3));
        assert_eq!(expected_write_search_first_queries(7, 2), Some(23));
        assert_eq!(expected_write_search_first_queries(7, 0), None);
        assert_eq!(expected_write_search_first_queries(7, 4), None);
        assert_eq!(expected_write_search_first_queries(usize::MAX, 3), None);

        let baseline = MailboxQueryCounts {
            first: 10,
            after: 4,
            before: 2,
        };
        assert_eq!(
            validate_write_search_query_delta(
                Some(baseline),
                MailboxQueryCounts {
                    first: 15,
                    after: 4,
                    before: 2,
                },
                1,
                2,
            ),
            Ok(MailboxQueryCounts {
                first: 5,
                after: 0,
                before: 0,
            })
        );

        for current in [
            MailboxQueryCounts {
                first: 14,
                after: 4,
                before: 2,
            },
            MailboxQueryCounts {
                first: 16,
                after: 4,
                before: 2,
            },
            MailboxQueryCounts {
                first: 15,
                after: 5,
                before: 2,
            },
            MailboxQueryCounts {
                first: 15,
                after: 4,
                before: 3,
            },
            MailboxQueryCounts {
                first: 9,
                after: 4,
                before: 2,
            },
        ] {
            assert!(validate_write_search_query_delta(Some(baseline), current, 1, 2).is_err());
        }
        assert!(validate_write_search_query_delta(None, baseline, 1, 2).is_err());
    }
}
