use crate::AppWindow;
use crate::store::sqlite::{MailboxQueryCounts, mailbox_query_counts};
use slint::{ComponentHandle, Model, SharedString, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub(crate) fn install_memory_stress(ui: &AppWindow) -> Option<Rc<Timer>> {
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
        Ok("mixed") | Err(_) => install_mixed_stress(ui, steps, delay, interval),
        Ok(scenario) => {
            eprintln!(
                "NIVALIS_STRESS_ERROR scenario={scenario} reason=unsupported_stress_scenario"
            );
            None
        }
    }
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
