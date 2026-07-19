use crate::{
    AppWindow,
    content::{ContentLimits, ContentStaging, FileKey, prepare_content},
    store::sqlite::{
        ContentImportSubmission, DatabaseClient, DatabaseSubmitError, FileGcOutcome,
        MailboxQueryCounts, MessageId, mailbox_query_counts,
    },
};
use slint::{ComponentHandle, Model, SharedString, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub(crate) fn install_memory_stress(
    ui: &AppWindow,
    database: DatabaseClient,
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
    host: String,
    port: String,
    secret: Zeroizing<Vec<u8>>,
    expected: AccountDiagnosticExpectation,
}

impl AccountDiagnosticConfig {
    fn load() -> Result<Self, &'static str> {
        let secret_path = required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_SECRET_FILE")?;
        let host = required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_IMAP_HOST")?;
        // The harness copies through Slint to exercise production UI, so it only accepts fake
        // credentials for a loopback fixture rather than a real provider secret.
        if !is_loopback_imap_host(&host) {
            return Err("nonlocal_host_rejected");
        }
        Ok(Self {
            name: std::env::var("NIVALIS_STRESS_ACCOUNT_NAME")
                .unwrap_or_else(|_| "Memory diagnostic".into()),
            address: required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_ADDRESS")?,
            login: required_account_diagnostic_env("NIVALIS_STRESS_ACCOUNT_LOGIN")?,
            host,
            port: std::env::var("NIVALIS_STRESS_ACCOUNT_IMAP_PORT")
                .unwrap_or_else(|_| "993".into()),
            secret: read_account_diagnostic_secret(&PathBuf::from(secret_path))?,
            expected: std::env::var("NIVALIS_STRESS_ACCOUNT_EXPECTED_RESULT")
                .ok()
                .as_deref()
                .and_then(AccountDiagnosticExpectation::parse)
                .ok_or("invalid_expected_result")?,
        })
    }
}

fn is_loopback_imap_host(host: &str) -> bool {
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
                            config.host.into(),
                            config.port.into(),
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
        ("Connected", false) => Some(AccountDiagnosticExpectation::Ready),
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
        CONTENT_TARGET_ACCOUNT_ID,
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
            classify_account_diagnostic("Sign-in was rejected", true),
            None
        );
        assert_eq!(classify_account_diagnostic("Connected", true), None);
        assert!(is_loopback_imap_host("localhost"));
        assert!(is_loopback_imap_host("127.0.0.1"));
        assert!(is_loopback_imap_host("::1"));
        assert!(!is_loopback_imap_host("imap.example.test"));
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
