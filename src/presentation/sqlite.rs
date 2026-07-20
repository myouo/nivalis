use crate::store::sqlite::{
    AccountDirectory, AccountGeneration, AccountId, AccountSummaryDto, AccountUnreadDto,
    MailSummaryDto, MailboxPage, MailboxStatsDto, MessageDetail, PageCursor,
};
use crate::ui_identity::{AccountKey, EntityKey};
use crate::{AccountItem, MailDetail, MailSummary};
use slint::{Color, SharedString};
use std::{error::Error, fmt};

pub(crate) const MAX_CATALOG_ACCOUNTS: usize = 64;
pub(crate) const MAX_MAILBOX_ROWS: usize = 50;
const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;

#[derive(Clone, Debug)]
struct CatalogAccount {
    database_id: AccountId,
    configuration_generation: AccountGeneration,
    item: AccountItem,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountOperationTarget {
    pub(crate) account_id: AccountId,
    pub(crate) expected_generation: AccountGeneration,
}

#[derive(Clone, Debug)]
pub(crate) struct AccountCatalog {
    all_accounts: AccountItem,
    accounts: Box<[CatalogAccount]>,
}

impl AccountCatalog {
    pub(crate) fn try_from_directory(directory: AccountDirectory) -> Result<Self, ProjectionError> {
        if directory.rows.len() > MAX_CATALOG_ACCOUNTS {
            return Err(ProjectionError::TooManyAccounts {
                found: directory.rows.len(),
                maximum: MAX_CATALOG_ACCOUNTS,
            });
        }

        let mut accounts = Vec::with_capacity(directory.rows.len());
        let mut all_unread = 0_u64;
        let mut has_error = false;
        for row in directory.rows.into_vec() {
            if accounts
                .iter()
                .any(|account: &CatalogAccount| account.database_id.get() == row.id)
            {
                return Err(ProjectionError::DuplicateAccountId(row.id));
            }
            let projected = project_account(row)?;
            all_unread = all_unread.checked_add(projected.raw_unread).ok_or(
                ProjectionError::CountOverflow {
                    field: "all_accounts.inbox_unread",
                    value: u64::MAX,
                },
            )?;
            has_error |= projected.item.has_error;
            accounts.push(CatalogAccount {
                database_id: projected.database_id,
                configuration_generation: projected.configuration_generation,
                item: projected.item,
            });
        }

        let unread_count = project_count("all_accounts.inbox_unread", all_unread)?;
        let account_count = accounts.len();
        let all_accounts = AccountItem {
            id: AccountKey::All.encode(),
            name: "All inboxes".into(),
            address: format_account_count(account_count).into(),
            initials: "AI".into(),
            unread_count,
            status: if has_error {
                "Some accounts need attention".into()
            } else {
                "Ready".into()
            },
            avatar_color: Color::from_rgb_u8(51, 82, 68),
            has_error,
        };

        Ok(Self {
            all_accounts,
            accounts: accounts.into_boxed_slice(),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.accounts.len()
    }

    pub(crate) fn account_items(&self) -> Vec<AccountItem> {
        let mut items = Vec::with_capacity(self.accounts.len() + 1);
        items.push(self.all_accounts.clone());
        items.extend(self.accounts.iter().map(|account| account.item.clone()));
        items
    }

    pub(crate) fn contains(&self, key: AccountKey) -> bool {
        match key {
            AccountKey::All => true,
            AccountKey::Account(id) => self
                .accounts
                .iter()
                .any(|account| account.database_id.get() == id.get()),
        }
    }

    pub(crate) fn operation_target(&self, key: AccountKey) -> Option<AccountOperationTarget> {
        let AccountKey::Account(id) = key else {
            return None;
        };
        self.accounts
            .iter()
            .find(|account| account.database_id.get() == id.get())
            .map(|account| AccountOperationTarget {
                account_id: account.database_id,
                expected_generation: account.configuration_generation,
            })
    }

    pub(crate) fn active_item(&self, key: AccountKey) -> Option<AccountItem> {
        match key {
            AccountKey::All => Some(self.all_accounts.clone()),
            AccountKey::Account(id) => self
                .accounts
                .iter()
                .find(|account| account.database_id.get() == id.get())
                .map(|account| account.item.clone()),
        }
    }

    pub(crate) fn project_mailbox(
        &self,
        page: MailboxPage,
    ) -> Result<ProjectedMailbox, ProjectionError> {
        if page.rows.len() > MAX_MAILBOX_ROWS {
            return Err(ProjectionError::TooManyMailboxRows {
                found: page.rows.len(),
                maximum: MAX_MAILBOX_ROWS,
            });
        }

        let mut rows = Vec::with_capacity(page.rows.len());
        for row in page.rows.into_vec() {
            rows.push(self.project_summary(row)?);
        }
        let stats = self.project_stats(page.stats)?;

        Ok(ProjectedMailbox {
            rows,
            stats,
            next_cursor: page.next_cursor,
        })
    }

    pub(crate) fn project_detail(
        &self,
        detail: MessageDetail,
        folder: &str,
    ) -> Result<MailDetail, ProjectionError> {
        let id = project_entity_id("message", detail.id.get())?;
        let account = self.require_account(detail.account_id)?;
        let sender = display_sender(&detail.sender_name, &detail.sender_address);
        let initials = initials(sender.as_str());
        let subject = display_subject(detail.subject);
        let date = format_utc(detail.received_at_ms)?;

        Ok(MailDetail {
            id,
            sender,
            email: boxed_string(detail.sender_address),
            initials,
            subject,
            body: boxed_string(detail.reader_excerpt),
            body_truncated: detail.body_truncated,
            date,
            folder: folder.into(),
            starred: detail.starred,
            has_attachment: detail.has_attachment,
            avatar_color: account.item.avatar_color,
        })
    }

    fn project_summary(&self, row: MailSummaryDto) -> Result<MailSummary, ProjectionError> {
        let id = project_entity_id("message", row.id.get())?;
        let account = self.require_account(row.account_id)?;
        let sender = display_sender(&row.sender_name, &row.sender_address);
        let initials = initials(sender.as_str());

        Ok(MailSummary {
            id,
            account_id: account.item.id.clone(),
            account_label: account.item.name.clone(),
            sender,
            initials,
            subject: display_subject(row.subject),
            preview: display_preview(&row.preview),
            time: format_utc(row.received_at_ms)?,
            unread: row.unread,
            starred: row.starred,
            has_attachment: row.has_attachment,
            avatar_color: account.item.avatar_color,
        })
    }

    fn project_stats(
        &self,
        stats: MailboxStatsDto,
    ) -> Result<ProjectedMailboxStats, ProjectionError> {
        if stats.account_unread.len() > MAX_CATALOG_ACCOUNTS {
            return Err(ProjectionError::TooManyAccountStats {
                found: stats.account_unread.len(),
                maximum: MAX_CATALOG_ACCOUNTS,
            });
        }

        let mut account_unread = Vec::with_capacity(stats.account_unread.len());
        let mut all_inbox_unread = 0_u64;
        for unread in stats.account_unread.into_vec() {
            all_inbox_unread = all_inbox_unread.checked_add(unread.unread).ok_or(
                ProjectionError::CountOverflow {
                    field: "all_accounts.inbox_unread",
                    value: u64::MAX,
                },
            )?;
            account_unread.push(self.project_account_unread(unread)?);
        }

        Ok(ProjectedMailboxStats {
            selected_total: stats
                .selected_total
                .map(|value| project_count("mailbox.selected_total", value))
                .transpose()?,
            inbox_unread: project_count("mailbox.inbox_unread", stats.inbox_unread)?,
            starred_total: project_count("mailbox.starred_total", stats.starred_total)?,
            drafts_total: project_count("mailbox.drafts_total", stats.drafts_total)?,
            all_inbox_unread: project_count("all_accounts.inbox_unread", all_inbox_unread)?,
            account_unread: account_unread.into_boxed_slice(),
        })
    }

    fn project_account_unread(
        &self,
        unread: AccountUnreadDto,
    ) -> Result<ProjectedAccountUnread, ProjectionError> {
        let account = self.require_account(unread.account_id)?;
        Ok(ProjectedAccountUnread {
            account_id: account.item.id.clone(),
            unread_count: project_count("account.inbox_unread", unread.unread)?,
        })
    }

    fn require_account(&self, id: i64) -> Result<&CatalogAccount, ProjectionError> {
        if EntityKey::new(id).is_none() {
            return Err(ProjectionError::InvalidId {
                entity: "account",
                value: id,
            });
        }
        self.accounts
            .iter()
            .find(|account| account.database_id.get() == id)
            .ok_or(ProjectionError::UnknownAccount(id))
    }
}

#[derive(Debug)]
struct ProjectedAccount {
    database_id: AccountId,
    configuration_generation: AccountGeneration,
    raw_unread: u64,
    item: AccountItem,
}

fn project_account(row: AccountSummaryDto) -> Result<ProjectedAccount, ProjectionError> {
    let database_id = AccountId::new(row.id).map_err(|_| ProjectionError::InvalidId {
        entity: "account",
        value: row.id,
    })?;
    let key = EntityKey::new(row.id).expect("validated account id is a UI entity key");
    let configuration_generation =
        AccountGeneration::new(row.configuration_generation).map_err(|_| {
            ProjectionError::InvalidAccountGeneration {
                account_id: row.id,
                value: row.configuration_generation,
            }
        })?;
    let name = boxed_string(row.name);
    let address = boxed_string(row.address);
    let (status, has_error) = account_status(&row.state);
    let unread_count = project_count("account.inbox_unread", row.inbox_unread)?;

    Ok(ProjectedAccount {
        database_id,
        configuration_generation,
        raw_unread: row.inbox_unread,
        item: AccountItem {
            id: AccountKey::Account(key).encode(),
            name: name.clone(),
            address,
            initials: initials(name.as_str()),
            unread_count,
            status: status.into(),
            avatar_color: accent_color(row.accent_rgb),
            has_error,
        },
    })
}

fn account_status(state: &str) -> (&'static str, bool) {
    match state {
        "active" => ("Ready", false),
        "disabled" => ("Disabled", false),
        "removing" => ("Removing account", false),
        "needs_setup" => ("Setup required", true),
        "authentication" => ("Sign-in required", true),
        "permission" => ("Permission required", true),
        "certificate" => ("Certificate problem", true),
        "timeout" => ("Connection timed out", true),
        "offline" => ("Offline", true),
        "protocol" => ("Server problem", true),
        _ => ("Needs attention", true),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedAccountUnread {
    pub(crate) account_id: SharedString,
    pub(crate) unread_count: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedMailboxStats {
    pub(crate) selected_total: Option<i32>,
    pub(crate) inbox_unread: i32,
    pub(crate) starred_total: i32,
    pub(crate) drafts_total: i32,
    pub(crate) all_inbox_unread: i32,
    pub(crate) account_unread: Box<[ProjectedAccountUnread]>,
}

#[derive(Debug)]
pub(crate) struct ProjectedMailbox {
    pub(crate) rows: Vec<MailSummary>,
    pub(crate) stats: ProjectedMailboxStats,
    pub(crate) next_cursor: Option<PageCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProjectionError {
    TooManyAccounts { found: usize, maximum: usize },
    TooManyMailboxRows { found: usize, maximum: usize },
    TooManyAccountStats { found: usize, maximum: usize },
    InvalidId { entity: &'static str, value: i64 },
    DuplicateAccountId(i64),
    InvalidAccountGeneration { account_id: i64, value: i64 },
    UnknownAccount(i64),
    CountOverflow { field: &'static str, value: u64 },
    InvalidTimestamp(i64),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyAccounts { found, maximum } => {
                write!(
                    formatter,
                    "account catalog has {found} rows; maximum is {maximum}"
                )
            }
            Self::TooManyMailboxRows { found, maximum } => {
                write!(
                    formatter,
                    "mailbox page has {found} rows; maximum is {maximum}"
                )
            }
            Self::TooManyAccountStats { found, maximum } => write!(
                formatter,
                "mailbox statistics have {found} account rows; maximum is {maximum}"
            ),
            Self::InvalidId { entity, value } => {
                write!(formatter, "{entity} identity {value} is not positive")
            }
            Self::DuplicateAccountId(id) => {
                write!(formatter, "account identity {id} is duplicated")
            }
            Self::InvalidAccountGeneration { account_id, value } => write!(
                formatter,
                "account identity {account_id} has invalid configuration generation {value}"
            ),
            Self::UnknownAccount(id) => {
                write!(formatter, "account identity {id} is not catalogued")
            }
            Self::CountOverflow { field, value } => {
                write!(formatter, "{field} count {value} does not fit the UI model")
            }
            Self::InvalidTimestamp(value) => {
                write!(
                    formatter,
                    "timestamp {value} is outside the supported UTC range"
                )
            }
        }
    }
}

impl Error for ProjectionError {}

fn project_entity_id(entity: &'static str, value: i64) -> Result<SharedString, ProjectionError> {
    EntityKey::new(value)
        .map(EntityKey::encode)
        .ok_or(ProjectionError::InvalidId { entity, value })
}

fn project_count(field: &'static str, value: u64) -> Result<i32, ProjectionError> {
    i32::try_from(value).map_err(|_| ProjectionError::CountOverflow { field, value })
}

fn display_sender(name: &str, address: &str) -> SharedString {
    let name = name.trim();
    if !name.is_empty() {
        return name.into();
    }
    let address = address.trim();
    if !address.is_empty() {
        address.into()
    } else {
        "Unknown sender".into()
    }
}

fn display_subject(subject: Box<str>) -> SharedString {
    if subject.trim().is_empty() {
        "(No subject)".into()
    } else {
        boxed_string(subject)
    }
}

fn display_preview(preview: &str) -> SharedString {
    let mut normalized = String::with_capacity(preview.len());
    let mut pending_space = false;

    for character in preview.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.push(character);
    }

    normalized.into()
}

fn initials(value: &str) -> SharedString {
    let mut output = String::with_capacity(2);
    for word in value.split_whitespace() {
        let Some(character) = word.chars().find(|character| character.is_alphanumeric()) else {
            continue;
        };
        for upper in character.to_uppercase() {
            output.push(upper);
            if output.chars().count() == 2 {
                return output.into();
            }
        }
    }
    if output.is_empty() {
        "?".into()
    } else {
        output.into()
    }
}

fn boxed_string(value: Box<str>) -> SharedString {
    String::from(value).into()
}

fn accent_color(rgb: u32) -> Color {
    let red = u8::try_from((rgb >> 16) & 0xff).expect("masked red channel fits u8");
    let green = u8::try_from((rgb >> 8) & 0xff).expect("masked green channel fits u8");
    let blue = u8::try_from(rgb & 0xff).expect("masked blue channel fits u8");
    Color::from_rgb_u8(red, green, blue)
}

fn format_account_count(count: usize) -> String {
    if count == 1 {
        "1 account".into()
    } else {
        format!("{count} accounts")
    }
}

fn format_utc(timestamp_ms: i64) -> Result<SharedString, ProjectionError> {
    if !(MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&timestamp_ms) {
        return Err(ProjectionError::InvalidTimestamp(timestamp_ms));
    }

    let seconds = timestamp_ms.div_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let (year, month, day) = civil_from_unix_days(days);
    Ok(format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC").into())
}

fn civil_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::MessageId;

    fn account(id: i64, unread: u64) -> AccountSummaryDto {
        AccountSummaryDto {
            id,
            configuration_generation: 1,
            name: format!("Account {id}").into_boxed_str(),
            address: format!("account-{id}@example.test").into_boxed_str(),
            state: "active".into(),
            accent_rgb: 0x12_34_56,
            inbox_unread: unread,
        }
    }

    fn catalog_with(ids: impl IntoIterator<Item = i64>) -> AccountCatalog {
        AccountCatalog::try_from_directory(AccountDirectory {
            rows: ids
                .into_iter()
                .map(|id| account(id, 0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        })
        .unwrap()
    }

    #[test]
    fn configured_account_states_have_stable_non_secret_guidance() {
        let mut disabled = account(1, 0);
        disabled.state = "disabled".into();
        let mut authentication = account(2, 0);
        authentication.state = "authentication".into();
        let catalog = AccountCatalog::try_from_directory(AccountDirectory {
            rows: vec![disabled, authentication].into_boxed_slice(),
        })
        .unwrap();
        let items = catalog.account_items();

        assert_eq!(items[1].status, "Disabled");
        assert!(!items[1].has_error);
        assert_eq!(items[2].status, "Sign-in required");
        assert!(items[2].has_error);
        assert!(items[0].has_error);
    }

    fn summary(id: i64, account_id: i64) -> MailSummaryDto {
        MailSummaryDto {
            id: MessageId::new(id).unwrap(),
            account_id,
            sender_name: "Sender Name".into(),
            sender_address: "sender@example.test".into(),
            subject: "Subject".into(),
            preview: "Preview".into(),
            received_at_ms: 0,
            unread: true,
            starred: false,
            has_attachment: false,
        }
    }

    fn stats(selected_total: Option<u64>) -> MailboxStatsDto {
        MailboxStatsDto {
            selected_total,
            inbox_unread: 0,
            starred_total: 0,
            drafts_total: 0,
            account_unread: Box::new([]),
        }
    }

    fn page(rows: Vec<MailSummaryDto>, selected_total: Option<u64>) -> MailboxPage {
        MailboxPage {
            rows: rows.into_boxed_slice(),
            previous_cursor: None,
            next_cursor: None,
            stats: stats(selected_total),
        }
    }

    #[test]
    fn maximum_database_identity_projects_without_truncation() {
        let catalog = catalog_with([i64::MAX]);
        let projected = catalog
            .project_mailbox(page(vec![summary(i64::MAX, i64::MAX)], Some(1)))
            .unwrap();

        assert_eq!(catalog.account_items()[1].id, "9223372036854775807");
        assert_eq!(projected.rows[0].id, "9223372036854775807");
        assert_eq!(projected.rows[0].account_id, "9223372036854775807");
    }

    #[test]
    fn account_operation_target_keeps_the_private_generation_fence() {
        let mut row = account(7, 0);
        row.configuration_generation = 73;
        let catalog = AccountCatalog::try_from_directory(AccountDirectory {
            rows: vec![row].into_boxed_slice(),
        })
        .unwrap();

        let target = catalog
            .operation_target(AccountKey::Account(EntityKey::new(7).unwrap()))
            .expect("catalogued account has an operation target");
        assert_eq!(target.account_id.get(), 7);
        assert_eq!(target.expected_generation.get(), 73);
        assert_eq!(catalog.operation_target(AccountKey::All), None);
        assert_eq!(
            catalog.operation_target(AccountKey::Account(EntityKey::new(8).unwrap())),
            None
        );
    }

    #[test]
    fn account_projection_rejects_non_positive_generation_fences() {
        for value in [0, -1] {
            let mut row = account(7, 0);
            row.configuration_generation = value;

            assert_eq!(
                AccountCatalog::try_from_directory(AccountDirectory {
                    rows: vec![row].into_boxed_slice(),
                })
                .unwrap_err(),
                ProjectionError::InvalidAccountGeneration {
                    account_id: 7,
                    value,
                }
            );
        }
    }

    #[test]
    fn mailbox_projection_preserves_navigation_cursors() {
        let catalog = catalog_with([1]);
        let previous_cursor = PageCursor::new(2_000, 2).unwrap();
        let next_cursor = PageCursor::new(1_000, 1).unwrap();
        let projected = catalog
            .project_mailbox(MailboxPage {
                rows: Box::new([]),
                previous_cursor: Some(previous_cursor),
                next_cursor: Some(next_cursor),
                stats: stats(Some(0)),
            })
            .unwrap();

        assert_eq!(projected.next_cursor, Some(next_cursor));
    }

    #[test]
    fn account_and_mailbox_boundaries_are_enforced() {
        let catalog = catalog_with(1_i64..=64);
        assert_eq!(catalog.len(), 64);
        assert_eq!(catalog.account_items().len(), 65);

        let projected = catalog
            .project_mailbox(page(
                (1_i64..=50).map(|id| summary(id, 1)).collect(),
                Some(50),
            ))
            .unwrap();
        assert_eq!(projected.rows.len(), 50);
        assert!(projected.next_cursor.is_none());

        let too_many_accounts = AccountCatalog::try_from_directory(AccountDirectory {
            rows: (1_i64..=65)
                .map(|id| account(id, 0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        });
        assert!(matches!(
            too_many_accounts,
            Err(ProjectionError::TooManyAccounts { found: 65, .. })
        ));

        let too_many_rows = catalog.project_mailbox(page(
            (1_i64..=51).map(|id| summary(id, 1)).collect(),
            Some(51),
        ));
        assert!(matches!(
            too_many_rows,
            Err(ProjectionError::TooManyMailboxRows { found: 51, .. })
        ));
    }

    #[test]
    fn unknown_accounts_and_invalid_identities_are_explicit_errors() {
        let catalog = catalog_with([1]);
        let error = catalog
            .project_mailbox(page(vec![summary(1, 2)], Some(1)))
            .expect_err("unknown account must fail projection");
        assert_eq!(error, ProjectionError::UnknownAccount(2));
        assert!(matches!(
            AccountCatalog::try_from_directory(AccountDirectory {
                rows: vec![account(0, 0)].into_boxed_slice(),
            }),
            Err(ProjectionError::InvalidId {
                entity: "account",
                value: 0
            })
        ));
    }

    #[test]
    fn unknown_selected_total_is_preserved_and_counts_do_not_truncate() {
        let catalog = catalog_with([1]);
        let projected = catalog
            .project_mailbox(page(vec![summary(1, 1)], None))
            .unwrap();
        assert_eq!(projected.stats.selected_total, None);

        let overflow = catalog
            .project_mailbox(MailboxPage {
                rows: Box::new([]),
                previous_cursor: None,
                next_cursor: None,
                stats: MailboxStatsDto {
                    selected_total: Some(u64::try_from(i32::MAX).unwrap() + 1),
                    inbox_unread: 0,
                    starred_total: 0,
                    drafts_total: 0,
                    account_unread: Box::new([]),
                },
            })
            .expect_err("overflowing UI count must fail projection");
        assert_eq!(
            overflow,
            ProjectionError::CountOverflow {
                field: "mailbox.selected_total",
                value: u64::try_from(i32::MAX).unwrap() + 1,
            }
        );
    }

    #[test]
    fn empty_sender_and_subject_have_stable_fallbacks_and_utc_time() {
        let catalog = catalog_with([1]);
        let mut row = summary(1, 1);
        row.sender_name = "  ".into();
        row.sender_address = "fallback@example.test".into();
        row.subject = "".into();
        let projected = catalog.project_mailbox(page(vec![row], Some(1))).unwrap();

        assert_eq!(projected.rows[0].sender, "fallback@example.test");
        assert_eq!(projected.rows[0].subject, "(No subject)");
        assert_eq!(projected.rows[0].time, "1970-01-01 00:00 UTC");
    }

    #[test]
    fn mailbox_preview_is_projected_as_stable_single_line_text() {
        let catalog = catalog_with([1]);
        let mut row = summary(1, 1);
        row.preview = "  First line\r\n\tSecond\u{0007}line　中文  ".into();

        let projected = catalog.project_mailbox(page(vec![row], Some(1))).unwrap();

        assert_eq!(projected.rows[0].preview, "First line Second line 中文");
    }

    #[test]
    fn detail_uses_database_flags_account_and_requested_folder() {
        let catalog = catalog_with([1]);
        let detail = catalog
            .project_detail(
                MessageDetail {
                    id: MessageId::new(7).unwrap(),
                    account_id: 1,
                    sender_name: "Reader".into(),
                    sender_address: "reader@example.test".into(),
                    subject: "Details".into(),
                    received_at_ms: 0,
                    unread: false,
                    starred: true,
                    has_attachment: true,
                    reader_excerpt: "Bounded body".into(),
                    body_truncated: true,
                    body_byte_count: 80_000,
                    body_file_key: Some("body-key".into()),
                },
                "Archive",
            )
            .unwrap();

        assert_eq!(detail.id, "7");
        assert_eq!(detail.folder, "Archive");
        assert!(detail.starred);
        assert!(detail.has_attachment);
        assert!(detail.body_truncated);
    }

    #[test]
    fn utc_conversion_handles_negative_and_schema_edge_timestamps() {
        assert_eq!(format_utc(-1).unwrap(), "1969-12-31 23:59 UTC");
        assert_eq!(
            format_utc(MIN_TIMESTAMP_MS).unwrap(),
            "0001-01-01 00:00 UTC"
        );
        assert_eq!(
            format_utc(MAX_TIMESTAMP_MS).unwrap(),
            "9999-12-31 23:59 UTC"
        );
        assert_eq!(
            format_utc(MAX_TIMESTAMP_MS + 1),
            Err(ProjectionError::InvalidTimestamp(MAX_TIMESTAMP_MS + 1))
        );
    }
}
