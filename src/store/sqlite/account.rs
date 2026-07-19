use std::{fmt, net::IpAddr, num::NonZeroI64, str::FromStr};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::domain::{AccountId, DbFailure, MessageId};

const MAX_NAME_BYTES: usize = 320;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_LOGIN_BYTES: usize = 320;
const MAX_HOST_BYTES: usize = 253;
const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const ACCOUNT_PURGE_MESSAGE_BATCH: usize = 16;
const ACCOUNT_PURGE_ATTACHMENT_BATCH: usize = 16;
const ACCOUNT_PURGE_STAGING_BATCH: usize = 16;
const PENDING_REMOVAL_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountAuthKind {
    AppPassword,
    OAuth2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountLifecycle {
    Active,
    Disabled,
    RemovingCredentials,
    RemovingCache,
}

impl AccountLifecycle {
    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            "removing_credentials" => Ok(Self::RemovingCredentials),
            "removing_cache" => Ok(Self::RemovingCache),
            _ => Err(DbFailure::database("invalid configured account state")),
        }
    }

    pub(crate) fn enabled(self) -> bool {
        self == Self::Active
    }
}

impl AccountAuthKind {
    fn database_value(self) -> &'static str {
        match self {
            Self::AppPassword => "app_password",
            Self::OAuth2 => "oauth2",
        }
    }

    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "app_password" => Ok(Self::AppPassword),
            "oauth2" => Ok(Self::OAuth2),
            _ => Err(DbFailure::database("invalid account authentication kind")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountGeneration(NonZeroI64);

impl AccountGeneration {
    pub(crate) fn new(value: i64) -> Result<Self, AccountValidationError> {
        NonZeroI64::new(value)
            .filter(|value| value.get() > 0)
            .map(Self)
            .ok_or(AccountValidationError::Generation)
    }

    fn from_database(value: i64) -> Result<Self, DbFailure> {
        NonZeroI64::new(value)
            .filter(|value| value.get() > 0)
            .map(Self)
            .ok_or_else(|| DbFailure::database("invalid account configuration generation"))
    }

    pub(crate) fn get(self) -> i64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticEpoch(NonZeroI64);

impl DiagnosticEpoch {
    fn from_database(value: i64) -> Result<Self, DbFailure> {
        NonZeroI64::new(value)
            .filter(|value| value.get() > 0)
            .map(Self)
            .ok_or_else(|| DbFailure::database("invalid account diagnostic epoch"))
    }

    pub(crate) fn get(self) -> i64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountDiagnosticKind {
    Authentication,
    Permission,
    Certificate,
    Timeout,
    Offline,
    Protocol,
}

impl AccountDiagnosticKind {
    fn database_value(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Permission => "permission",
            Self::Certificate => "certificate",
            Self::Timeout => "timeout",
            Self::Offline => "offline",
            Self::Protocol => "protocol",
        }
    }

    fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "authentication" => Ok(Self::Authentication),
            "permission" => Ok(Self::Permission),
            "certificate" => Ok(Self::Certificate),
            "timeout" => Ok(Self::Timeout),
            "offline" => Ok(Self::Offline),
            "protocol" => Ok(Self::Protocol),
            _ => Err(DbFailure::database("invalid account diagnostic kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountDiagnostic {
    Never,
    Ready {
        checked_at_ms: i64,
    },
    Failed {
        kind: AccountDiagnosticKind,
        checked_at_ms: i64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountConfigInput {
    pub(crate) credential_key: Box<str>,
    pub(crate) name: Box<str>,
    pub(crate) address: Box<str>,
    pub(crate) auth_kind: AccountAuthKind,
    pub(crate) login_name: Box<str>,
    pub(crate) imap_host: Box<str>,
    pub(crate) imap_port: u16,
    pub(crate) accent_rgb: u32,
}

impl AccountConfigInput {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        credential_key: &str,
        name: &str,
        address: &str,
        auth_kind: AccountAuthKind,
        login_name: &str,
        imap_host: &str,
        imap_port: u16,
        accent_rgb: u32,
    ) -> Result<Self, AccountValidationError> {
        Ok(Self {
            credential_key: validate_credential_key(credential_key)?.into(),
            name: validate_text(name, MAX_NAME_BYTES, AccountValidationError::Name)?.into(),
            address: validate_address(address)?.into(),
            auth_kind,
            login_name: validate_text(login_name, MAX_LOGIN_BYTES, AccountValidationError::Login)?
                .into(),
            imap_host: validate_host(imap_host)?.into(),
            imap_port: (imap_port != 0)
                .then_some(imap_port)
                .ok_or(AccountValidationError::Port)?,
            accent_rgb: (accent_rgb <= 0x00ff_ffff)
                .then_some(accent_rgb)
                .ok_or(AccountValidationError::Accent)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountConfiguration {
    pub(crate) account_id: AccountId,
    pub(crate) generation: AccountGeneration,
    pub(crate) credential_key: Box<str>,
    pub(crate) name: Box<str>,
    pub(crate) address: Box<str>,
    pub(crate) auth_kind: AccountAuthKind,
    pub(crate) login_name: Box<str>,
    pub(crate) imap_host: Box<str>,
    pub(crate) imap_port: u16,
    pub(crate) accent_rgb: u32,
    pub(crate) lifecycle: AccountLifecycle,
    pub(crate) diagnostic: AccountDiagnostic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountSetupTarget {
    pub(crate) account_id: AccountId,
    pub(crate) generation: AccountGeneration,
    pub(crate) provider: Box<str>,
    pub(crate) name: Box<str>,
    pub(crate) address: Box<str>,
    pub(crate) accent_rgb: u32,
    pub(crate) removal_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountRecord {
    NeedsSetup(AccountSetupTarget),
    Configured(AccountConfiguration),
}

impl AccountRecord {
    fn generation(&self) -> AccountGeneration {
        match self {
            Self::NeedsSetup(target) => target.generation,
            Self::Configured(configuration) => configuration.generation,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticRecord {
    Ready {
        checked_at_ms: i64,
    },
    Failed {
        kind: AccountDiagnosticKind,
        checked_at_ms: i64,
    },
}

impl DiagnosticRecord {
    pub(crate) fn ready(checked_at_ms: i64) -> Result<Self, AccountValidationError> {
        validate_timestamp(checked_at_ms)?;
        Ok(Self::Ready { checked_at_ms })
    }

    pub(crate) fn failed(
        kind: AccountDiagnosticKind,
        checked_at_ms: i64,
    ) -> Result<Self, AccountValidationError> {
        validate_timestamp(checked_at_ms)?;
        Ok(Self::Failed {
            kind,
            checked_at_ms,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemovedAccount {
    pub(crate) account_id: AccountId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountRemovalTicket {
    pub(crate) account_id: AccountId,
    pub(crate) generation: AccountGeneration,
    pub(crate) credential_key: Option<Box<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingCredentialRemoval {
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) credential_key: Box<str>,
    pub(crate) auth_kind: AccountAuthKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingCacheRemoval {
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticTicket {
    pub(crate) account_id: AccountId,
    pub(crate) configuration_generation: AccountGeneration,
    pub(crate) epoch: DiagnosticEpoch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountPurgeOutcome {
    Pending {
        removed_messages: u8,
        removed_attachments: u8,
        removed_staging_files: u8,
        queued_files: u16,
    },
    Complete(RemovedAccount),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticCommit {
    Recorded,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountWrite {
    Create(AccountConfigInput),
    ConfigureExisting {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        input: AccountConfigInput,
    },
    Update {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        input: AccountConfigInput,
    },
    SetEnabled {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        enabled: bool,
    },
    BeginDiagnostic {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    RecordDiagnostic {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        epoch: DiagnosticEpoch,
        record: DiagnosticRecord,
    },
    BeginRemove {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    ConfirmCredentialsRemoved {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    PurgeRemovedAccount {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AccountWriteOutcome {
    Saved(AccountConfiguration),
    DiagnosticStarted(DiagnosticTicket),
    Diagnostic(DiagnosticCommit),
    RemovalStarted(AccountRemovalTicket),
    Purged(AccountPurgeOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountValidationError {
    CredentialKey,
    Name,
    Address,
    Login,
    Host,
    Port,
    Accent,
    Generation,
    Timestamp,
}

impl fmt::Display for AccountValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CredentialKey => "credential key must be 32 lowercase hexadecimal bytes",
            Self::Name => "account name is empty or exceeds 320 bytes",
            Self::Address => "mail address is invalid or exceeds 320 bytes",
            Self::Login => "login name is empty or exceeds 320 bytes",
            Self::Host => "IMAP host is not a valid DNS name or IP address",
            Self::Port => "IMAP port must be between 1 and 65535",
            Self::Accent => "account accent is outside the 24-bit RGB range",
            Self::Generation => "account generation must be positive",
            Self::Timestamp => "diagnostic timestamp is outside the supported range",
        })
    }
}

impl std::error::Error for AccountValidationError {}

pub(super) fn create_account(
    connection: &mut Connection,
    input: &AccountConfigInput,
) -> Result<AccountConfiguration, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    transaction
        .execute(
            "INSERT INTO accounts
                 (provider, remote_key, name, address, sort_order, state, accent_rgb)
             SELECT 'imap', ?1, ?2, ?3,
                    coalesce(max(sort_order), -1) + 1, 'active', ?4
             FROM accounts",
            params![
                input.credential_key.as_ref(),
                input.name.as_ref(),
                input.address.as_ref(),
                input.accent_rgb,
            ],
        )
        .map_err(map_account_write_error)?;
    let account_id = AccountId::new(transaction.last_insert_rowid())
        .map_err(|error| DbFailure::database(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO account_connections
                 (account_id, credential_key, auth_kind, login_name, imap_host, imap_port)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                account_id.get(),
                input.credential_key.as_ref(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
            ],
        )
        .map_err(map_account_write_error)?;
    let configuration = load_account_from(&transaction, account_id)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(configuration)
}

pub(super) fn configure_existing_account(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    input: &AccountConfigInput,
) -> Result<AccountConfiguration, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let target = require_setup_fence(&transaction, account_id, expected_generation)?;
    if target.removal_pending {
        return Err(DbFailure::conflict(
            "account removal is already in progress",
        ));
    }
    if target.provider.as_ref() != "imap" {
        return Err(DbFailure::conflict(
            "only IMAP accounts can be configured in this milestone",
        ));
    }
    increment_account_generation(
        &transaction,
        account_id,
        "UPDATE accounts
         SET configuration_generation = configuration_generation + 1,
             name = ?2, address = ?3, state = 'active', accent_rgb = ?4
         WHERE id = ?1 AND configuration_generation < 9223372036854775807",
        params![
            account_id.get(),
            input.name.as_ref(),
            input.address.as_ref(),
            input.accent_rgb,
        ],
    )?;
    transaction
        .execute(
            "INSERT INTO account_connections
                 (account_id, credential_key, auth_kind, login_name, imap_host, imap_port)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                account_id.get(),
                input.credential_key.as_ref(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
            ],
        )
        .map_err(map_account_write_error)?;
    let configuration = load_account_from(&transaction, account_id)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(configuration)
}

pub(super) fn load_account(
    connection: &Connection,
    account_id: AccountId,
) -> Result<AccountRecord, DbFailure> {
    load_account_record_from(connection, account_id)
}

pub(super) fn load_pending_credential_removals(
    connection: &Connection,
) -> Result<Box<[PendingCredentialRemoval]>, DbFailure> {
    type StoredRemoval = (i64, i64, String, String);

    let mut statement = connection
        .prepare(
            "SELECT account.id, account.configuration_generation,
                    connection.credential_key, connection.auth_kind
             FROM accounts AS account
             JOIN account_connections AS connection ON connection.account_id = account.id
             WHERE account.state = 'removing_credentials'
             ORDER BY account.id ASC
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([PENDING_REMOVAL_LIMIT as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(DbFailure::database)?;
    let mut removals = Vec::with_capacity(PENDING_REMOVAL_LIMIT);
    for row in rows {
        let (account_id, generation, credential_key, auth_kind): StoredRemoval =
            row.map_err(DbFailure::database)?;
        let credential_key = validate_credential_key(&credential_key)
            .map_err(|_| DbFailure::database("invalid pending credential key"))?;
        removals.push(PendingCredentialRemoval {
            account_id: AccountId::new(account_id)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            configuration_generation: AccountGeneration::from_database(generation)?,
            credential_key: credential_key.into(),
            auth_kind: AccountAuthKind::from_database(&auth_kind)?,
        });
    }
    Ok(removals.into_boxed_slice())
}

pub(super) fn load_pending_cache_removals(
    connection: &Connection,
) -> Result<Box<[PendingCacheRemoval]>, DbFailure> {
    let mut statement = connection
        .prepare(
            "SELECT id, configuration_generation
             FROM accounts
             WHERE state = 'removing_cache'
             ORDER BY id ASC
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([PENDING_REMOVAL_LIMIT as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(DbFailure::database)?;
    let mut removals = Vec::with_capacity(PENDING_REMOVAL_LIMIT);
    for row in rows {
        let (account_id, generation) = row.map_err(DbFailure::database)?;
        removals.push(PendingCacheRemoval {
            account_id: AccountId::new(account_id)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            configuration_generation: AccountGeneration::from_database(generation)?,
        });
    }
    Ok(removals.into_boxed_slice())
}

pub(super) fn update_account(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    input: &AccountConfigInput,
) -> Result<AccountConfiguration, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let configuration = require_configuration_fence(
        &transaction,
        account_id,
        expected_generation,
        Some(&input.credential_key),
    )?;
    require_mutable(&configuration)?;
    if configuration.auth_kind != input.auth_kind {
        return Err(DbFailure::conflict(
            "account authentication kind cannot be changed in place",
        ));
    }
    increment_account_generation(
        &transaction,
        account_id,
        "UPDATE accounts
         SET configuration_generation = configuration_generation + 1,
             name = ?2, address = ?3, accent_rgb = ?4
         WHERE id = ?1 AND configuration_generation < 9223372036854775807",
        params![
            account_id.get(),
            input.name.as_ref(),
            input.address.as_ref(),
            input.accent_rgb,
        ],
    )?;
    transaction
        .execute(
            "UPDATE account_connections
             SET auth_kind = ?2,
                 login_name = ?3,
                 imap_host = ?4,
                 imap_port = ?5,
                 diagnostic_state = 'never',
                 last_checked_at_ms = NULL
             WHERE account_id = ?1",
            params![
                account_id.get(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
            ],
        )
        .map_err(map_account_write_error)?;
    let configuration = load_account_from(&transaction, account_id)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(configuration)
}

pub(super) fn set_account_enabled(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    enabled: bool,
) -> Result<AccountConfiguration, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let configuration =
        require_configuration_fence(&transaction, account_id, expected_generation, None)?;
    require_mutable(&configuration)?;
    increment_account_generation(
        &transaction,
        account_id,
        "UPDATE accounts
         SET configuration_generation = configuration_generation + 1, state = ?2
         WHERE id = ?1 AND configuration_generation < 9223372036854775807",
        params![
            account_id.get(),
            if enabled { "active" } else { "disabled" }
        ],
    )?;
    let configuration = load_account_from(&transaction, account_id)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(configuration)
}

pub(super) fn begin_diagnostic(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<DiagnosticTicket, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let configuration =
        require_configuration_fence(&transaction, account_id, expected_generation, None)?;
    if configuration.lifecycle != AccountLifecycle::Active {
        return Err(DbFailure::conflict(
            "only an active account can start a connection diagnostic",
        ));
    }
    let changed = transaction
        .execute(
            "UPDATE account_connections
             SET diagnostic_generation = diagnostic_generation + 1
             WHERE account_id = ?1
               AND diagnostic_generation < 9223372036854775807",
            [account_id.get()],
        )
        .map_err(map_account_write_error)?;
    if changed != 1 {
        return Err(DbFailure::resource_limit(
            "account diagnostic generation is exhausted",
        ));
    }
    let raw_epoch: i64 = transaction
        .query_row(
            "SELECT diagnostic_generation
             FROM account_connections WHERE account_id = ?1",
            [account_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let ticket = DiagnosticTicket {
        account_id,
        configuration_generation: expected_generation,
        epoch: DiagnosticEpoch::from_database(raw_epoch)?,
    };
    transaction.commit().map_err(DbFailure::database)?;
    Ok(ticket)
}

pub(super) fn record_diagnostic(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    epoch: DiagnosticEpoch,
    record: &DiagnosticRecord,
) -> Result<DiagnosticCommit, DbFailure> {
    let (state, checked_at_ms) = match record {
        DiagnosticRecord::Ready { checked_at_ms } => ("ready", *checked_at_ms),
        DiagnosticRecord::Failed {
            kind,
            checked_at_ms,
        } => (kind.database_value(), *checked_at_ms),
    };
    let changed = connection
        .execute(
            "UPDATE account_connections
             SET diagnostic_state = ?3,
                 last_checked_at_ms = ?5
             WHERE account_id = ?1
               AND diagnostic_generation = ?4
               AND EXISTS (
                   SELECT 1 FROM accounts
                   WHERE id = ?1 AND configuration_generation = ?2
                     AND state = 'active'
               )",
            params![
                account_id.get(),
                expected_generation.get(),
                state,
                epoch.get(),
                checked_at_ms,
            ],
        )
        .map_err(map_account_write_error)?;
    if changed == 1 {
        return Ok(DiagnosticCommit::Recorded);
    }
    if account_exists(connection, account_id)? {
        Ok(DiagnosticCommit::Stale)
    } else {
        Err(DbFailure::not_found(
            "account configuration no longer exists",
        ))
    }
}

pub(super) fn begin_account_removal(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<AccountRemovalTicket, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let record = require_account_fence(&transaction, account_id, expected_generation)?;
    let (state, credential_key) = match record {
        AccountRecord::Configured(configuration) => {
            require_mutable(&configuration)?;
            ("removing_credentials", Some(configuration.credential_key))
        }
        AccountRecord::NeedsSetup(target) => {
            if target.removal_pending {
                return Err(DbFailure::conflict(
                    "account removal is already in progress",
                ));
            }
            ("removing_cache", None)
        }
    };
    increment_account_generation(
        &transaction,
        account_id,
        "UPDATE accounts
         SET configuration_generation = configuration_generation + 1, state = ?2
         WHERE id = ?1 AND configuration_generation < 9223372036854775807",
        params![account_id.get(), state],
    )?;
    let generation = load_account_record_from(&transaction, account_id)?.generation();
    transaction.commit().map_err(DbFailure::database)?;
    Ok(AccountRemovalTicket {
        account_id,
        generation,
        credential_key,
    })
}

pub(super) fn confirm_account_credentials_removed(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<AccountConfiguration, DbFailure> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let configuration =
        require_configuration_fence(&transaction, account_id, expected_generation, None)?;
    if configuration.lifecycle != AccountLifecycle::RemovingCredentials {
        return Err(DbFailure::conflict(
            "account credentials must be pending removal before confirmation",
        ));
    }
    increment_account_generation(
        &transaction,
        account_id,
        "UPDATE accounts
         SET configuration_generation = configuration_generation + 1,
             state = 'removing_cache'
         WHERE id = ?1 AND configuration_generation < 9223372036854775807",
        [account_id.get()],
    )?;
    let removing = load_account_from(&transaction, account_id)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(removing)
}

pub(super) fn purge_removed_account(
    connection: &mut Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    queued_at_ms: i64,
) -> Result<AccountPurgeOutcome, DbFailure> {
    validate_database_timestamp(queued_at_ms)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let record = require_account_fence(&transaction, account_id, expected_generation)?;
    let removing_cache = match &record {
        AccountRecord::Configured(configuration) => {
            configuration.lifecycle == AccountLifecycle::RemovingCache
        }
        AccountRecord::NeedsSetup(target) => target.removal_pending,
    };
    if !removing_cache {
        return Err(DbFailure::conflict(
            "account credentials must be removed before cache cleanup",
        ));
    }

    let message_ids = {
        let mut statement = transaction
            .prepare(
                "SELECT id FROM messages
                 WHERE account_id = ?1
                 ORDER BY received_at_ms DESC, id DESC
                 LIMIT ?2",
            )
            .map_err(DbFailure::database)?;
        let rows = statement
            .query_map(
                params![account_id.get(), ACCOUNT_PURGE_MESSAGE_BATCH as i64],
                |row| row.get::<_, i64>(0),
            )
            .map_err(DbFailure::database)?;
        let mut ids = Vec::with_capacity(ACCOUNT_PURGE_MESSAGE_BATCH);
        for row in rows {
            ids.push(
                MessageId::new(row.map_err(DbFailure::database)?)
                    .map_err(|error| DbFailure::database(error.to_string()))?,
            );
        }
        ids
    };
    let mut queued_files = 0_usize;
    let mut removed_messages = 0_usize;
    let mut removed_attachments = 0_usize;
    for message_id in &message_ids {
        let attachments = {
            let mut statement = transaction
                .prepare(
                    "SELECT id, file_key FROM attachments
                     WHERE message_id = ?1
                     ORDER BY ordinal
                     LIMIT ?2",
                )
                .map_err(DbFailure::database)?;
            let rows = statement
                .query_map(
                    params![message_id.get(), ACCOUNT_PURGE_ATTACHMENT_BATCH as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .map_err(DbFailure::database)?;
            let mut attachments = Vec::with_capacity(ACCOUNT_PURGE_ATTACHMENT_BATCH);
            for row in rows {
                attachments.push(row.map_err(DbFailure::database)?);
            }
            attachments
        };
        for (attachment_id, file_key) in &attachments {
            if let Some(file_key) = file_key {
                queued_files = queued_files
                    .checked_add(queue_file(&transaction, file_key, queued_at_ms)?)
                    .ok_or_else(|| {
                        DbFailure::resource_limit("account file cleanup count overflow")
                    })?;
            }
            transaction
                .execute("DELETE FROM attachments WHERE id = ?1", [attachment_id])
                .map_err(map_account_write_error)?;
        }
        removed_attachments = removed_attachments
            .checked_add(attachments.len())
            .ok_or_else(|| DbFailure::resource_limit("account attachment cleanup overflow"))?;
        if !attachments.is_empty() {
            let attachments_remain: bool = transaction
                .query_row(
                    "SELECT EXISTS (
                         SELECT 1 FROM attachments WHERE message_id = ?1
                     )",
                    [message_id.get()],
                    |row| row.get(0),
                )
                .map_err(DbFailure::database)?;
            if attachments_remain {
                break;
            }
        }
        queued_files = queued_files
            .checked_add(queue_message_singleton_files(
                &transaction,
                *message_id,
                queued_at_ms,
            )?)
            .ok_or_else(|| DbFailure::resource_limit("account file cleanup count overflow"))?;
        transaction
            .execute("DELETE FROM messages WHERE id = ?1", [message_id.get()])
            .map_err(map_account_write_error)?;
        removed_messages += 1;
        if !attachments.is_empty() {
            break;
        }
    }

    let staged_keys = {
        let mut statement = transaction
            .prepare(
                "SELECT file_key FROM file_staging
                 WHERE account_id = ?1
                 ORDER BY file_key
                 LIMIT ?2",
            )
            .map_err(DbFailure::database)?;
        let rows = statement
            .query_map(
                params![account_id.get(), ACCOUNT_PURGE_STAGING_BATCH as i64],
                |row| row.get::<_, String>(0),
            )
            .map_err(DbFailure::database)?;
        let mut keys = Vec::with_capacity(ACCOUNT_PURGE_STAGING_BATCH);
        for row in rows {
            keys.push(row.map_err(DbFailure::database)?);
        }
        keys
    };
    for key in &staged_keys {
        queued_files = queued_files
            .checked_add(
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
                         VALUES (?1, ?2)",
                        params![key, queued_at_ms],
                    )
                    .map_err(map_account_write_error)?,
            )
            .ok_or_else(|| DbFailure::resource_limit("account file cleanup count overflow"))?;
        transaction
            .execute("DELETE FROM file_staging WHERE file_key = ?1", [key])
            .map_err(map_account_write_error)?;
    }

    let remaining: bool = transaction
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM messages WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM file_staging WHERE account_id = ?1
             )",
            [account_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if remaining {
        let outcome = AccountPurgeOutcome::Pending {
            removed_messages: u8::try_from(removed_messages)
                .expect("account purge message batch fits u8"),
            removed_attachments: u8::try_from(removed_attachments)
                .expect("account purge attachment batch fits u8"),
            removed_staging_files: u8::try_from(staged_keys.len())
                .expect("account purge staging batch fits u8"),
            queued_files: u16::try_from(queued_files)
                .map_err(|_| DbFailure::resource_limit("account file cleanup count overflow"))?,
        };
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(outcome);
    }

    let removed = transaction
        .execute("DELETE FROM accounts WHERE id = ?1", [account_id.get()])
        .map_err(map_account_write_error)?;
    if removed != 1 {
        return Err(DbFailure::conflict("account changed during removal"));
    }
    transaction.commit().map_err(DbFailure::database)?;
    Ok(AccountPurgeOutcome::Complete(RemovedAccount { account_id }))
}

fn queue_message_singleton_files(
    connection: &Connection,
    message_id: MessageId,
    queued_at_ms: i64,
) -> Result<usize, DbFailure> {
    let mut queued = 0_usize;
    for sql in [
        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
         SELECT body_file_key, ?2 FROM message_content
         WHERE message_id = ?1 AND body_file_key IS NOT NULL",
        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
         SELECT mime_file_key, ?2 FROM outbox WHERE message_id = ?1",
    ] {
        queued = queued
            .checked_add(
                connection
                    .execute(sql, params![message_id.get(), queued_at_ms])
                    .map_err(map_account_write_error)?,
            )
            .ok_or_else(|| DbFailure::resource_limit("account file cleanup count overflow"))?;
    }
    Ok(queued)
}

fn queue_file(
    connection: &Connection,
    file_key: &str,
    queued_at_ms: i64,
) -> Result<usize, DbFailure> {
    connection
        .execute(
            "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
             VALUES (?1, ?2)",
            params![file_key, queued_at_ms],
        )
        .map_err(map_account_write_error)
}

fn load_account_from(
    connection: &Connection,
    account_id: AccountId,
) -> Result<AccountConfiguration, DbFailure> {
    type StoredAccount = (
        i64,
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        Option<i64>,
        i64,
    );
    let stored: Option<StoredAccount> = connection
        .query_row(
            "SELECT account.id, account.configuration_generation, connection.credential_key,
                    account.name, account.address, connection.auth_kind,
                    connection.login_name, connection.imap_host, connection.imap_port,
                    account.state, connection.diagnostic_state,
                    connection.last_checked_at_ms,
                    account.accent_rgb
             FROM accounts AS account
             JOIN account_connections AS connection ON connection.account_id = account.id
             WHERE account.id = ?1",
            [account_id.get()],
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
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((
        raw_id,
        generation,
        credential_key,
        name,
        address,
        auth_kind,
        login_name,
        imap_host,
        imap_port,
        state,
        diagnostic_state,
        last_checked_at_ms,
        accent_rgb,
    )) = stored
    else {
        return Err(DbFailure::not_found(
            "account configuration no longer exists",
        ));
    };
    let diagnostic = diagnostic_from_database(&diagnostic_state, last_checked_at_ms)?;
    Ok(AccountConfiguration {
        account_id: AccountId::new(raw_id)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        generation: AccountGeneration::from_database(generation)?,
        credential_key: credential_key.into_boxed_str(),
        name: name.into_boxed_str(),
        address: address.into_boxed_str(),
        auth_kind: AccountAuthKind::from_database(&auth_kind)?,
        login_name: login_name.into_boxed_str(),
        imap_host: imap_host.into_boxed_str(),
        imap_port: u16::try_from(imap_port)
            .map_err(|_| DbFailure::database("invalid IMAP port in account configuration"))?,
        accent_rgb: u32::try_from(accent_rgb)
            .map_err(|_| DbFailure::database("invalid account accent in configuration"))?,
        lifecycle: AccountLifecycle::from_database(&state)?,
        diagnostic,
    })
}

fn load_account_record_from(
    connection: &Connection,
    account_id: AccountId,
) -> Result<AccountRecord, DbFailure> {
    let configured = connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM account_connections WHERE account_id = account.id
             )
             FROM accounts AS account WHERE account.id = ?1",
            [account_id.get()],
            |row| row.get::<_, bool>(0),
        )
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::not_found("mail account no longer exists"))?;
    if configured {
        load_account_from(connection, account_id).map(AccountRecord::Configured)
    } else {
        load_setup_target_from(connection, account_id).map(AccountRecord::NeedsSetup)
    }
}

fn load_setup_target_from(
    connection: &Connection,
    account_id: AccountId,
) -> Result<AccountSetupTarget, DbFailure> {
    type StoredTarget = (i64, i64, String, String, String, i64, String);
    let stored: Option<StoredTarget> = connection
        .query_row(
            "SELECT id, configuration_generation, provider, name, address, accent_rgb, state
             FROM accounts
             WHERE id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM account_connections WHERE account_id = accounts.id
               )",
            [account_id.get()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    let Some((raw_id, generation, provider, name, address, accent_rgb, state)) = stored else {
        return Err(DbFailure::not_found(
            "account setup target no longer exists",
        ));
    };
    if state == "removing_credentials" {
        return Err(DbFailure::database(
            "unconfigured account cannot be removing credentials",
        ));
    }
    Ok(AccountSetupTarget {
        account_id: AccountId::new(raw_id)
            .map_err(|error| DbFailure::database(error.to_string()))?,
        generation: AccountGeneration::from_database(generation)?,
        provider: provider.into_boxed_str(),
        name: name.into_boxed_str(),
        address: address.into_boxed_str(),
        accent_rgb: u32::try_from(accent_rgb)
            .map_err(|_| DbFailure::database("invalid account accent in setup target"))?,
        removal_pending: state == "removing_cache",
    })
}

fn require_account_fence(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<AccountRecord, DbFailure> {
    let record = load_account_record_from(connection, account_id)?;
    if record.generation() != expected_generation {
        return Err(DbFailure::conflict(
            "account configuration changed; reload and retry",
        ));
    }
    Ok(record)
}

fn require_configuration_fence(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
    expected_credential_key: Option<&str>,
) -> Result<AccountConfiguration, DbFailure> {
    let record = require_account_fence(connection, account_id, expected_generation)?;
    let AccountRecord::Configured(configuration) = record else {
        return Err(DbFailure::conflict("mail account still requires setup"));
    };
    if expected_credential_key.is_some_and(|key| configuration.credential_key.as_ref() != key) {
        return Err(DbFailure::conflict(
            "account credential identity cannot be changed",
        ));
    }
    Ok(configuration)
}

fn require_setup_fence(
    connection: &Connection,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<AccountSetupTarget, DbFailure> {
    let record = require_account_fence(connection, account_id, expected_generation)?;
    let AccountRecord::NeedsSetup(target) = record else {
        return Err(DbFailure::conflict("mail account is already configured"));
    };
    Ok(target)
}

fn increment_account_generation<P: rusqlite::Params>(
    connection: &Connection,
    _account_id: AccountId,
    sql: &str,
    params: P,
) -> Result<(), DbFailure> {
    let changed = connection
        .execute(sql, params)
        .map_err(map_account_write_error)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(DbFailure::resource_limit(
            "account configuration generation is exhausted",
        ))
    }
}

fn require_mutable(configuration: &AccountConfiguration) -> Result<(), DbFailure> {
    match configuration.lifecycle {
        AccountLifecycle::Active | AccountLifecycle::Disabled => Ok(()),
        AccountLifecycle::RemovingCredentials | AccountLifecycle::RemovingCache => Err(
            DbFailure::conflict("account removal is already in progress"),
        ),
    }
}

fn account_exists(connection: &Connection, account_id: AccountId) -> Result<bool, DbFailure> {
    connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM accounts WHERE id = ?1
             )",
            [account_id.get()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(DbFailure::database)
}

fn diagnostic_from_database(
    state: &str,
    checked_at_ms: Option<i64>,
) -> Result<AccountDiagnostic, DbFailure> {
    match (state, checked_at_ms) {
        ("never", None) => Ok(AccountDiagnostic::Never),
        ("ready", Some(checked_at_ms)) => Ok(AccountDiagnostic::Ready { checked_at_ms }),
        (state, Some(checked_at_ms)) => Ok(AccountDiagnostic::Failed {
            kind: AccountDiagnosticKind::from_database(state)?,
            checked_at_ms,
        }),
        _ => Err(DbFailure::database("inconsistent account diagnostic state")),
    }
}

fn validate_credential_key(value: &str) -> Result<&str, AccountValidationError> {
    let value = value.trim();
    if value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(value)
    } else {
        Err(AccountValidationError::CredentialKey)
    }
}

fn validate_text(
    value: &str,
    maximum: usize,
    error: AccountValidationError,
) -> Result<&str, AccountValidationError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > maximum
        || value.chars().any(|character| character.is_control())
    {
        Err(error)
    } else {
        Ok(value)
    }
}

fn validate_address(value: &str) -> Result<&str, AccountValidationError> {
    let value = validate_text(value, MAX_ADDRESS_BYTES, AccountValidationError::Address)?;
    let Some((local, domain)) = value.rsplit_once('@') else {
        return Err(AccountValidationError::Address);
    };
    if local.is_empty()
        || local.len() > 64
        || local.contains('@')
        || local.chars().any(char::is_whitespace)
        || validate_host(domain).is_err()
    {
        Err(AccountValidationError::Address)
    } else {
        Ok(value)
    }
}

fn validate_host(value: &str) -> Result<&str, AccountValidationError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_HOST_BYTES
        || !value.is_ascii()
        || value.chars().any(char::is_whitespace)
    {
        return Err(AccountValidationError::Host);
    }
    let ip_candidate = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);
    if IpAddr::from_str(ip_candidate).is_ok() {
        return Ok(ip_candidate);
    }
    let dns_name = value.strip_suffix('.').unwrap_or(value);
    if dns_name.is_empty()
        || dns_name.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        Err(AccountValidationError::Host)
    } else {
        Ok(dns_name)
    }
}

fn validate_timestamp(timestamp_ms: i64) -> Result<(), AccountValidationError> {
    if (MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&timestamp_ms) {
        Ok(())
    } else {
        Err(AccountValidationError::Timestamp)
    }
}

fn validate_database_timestamp(timestamp_ms: i64) -> Result<(), DbFailure> {
    validate_timestamp(timestamp_ms).map_err(|error| DbFailure::resource_limit(error.to_string()))
}

fn map_account_write_error(error: rusqlite::Error) -> DbFailure {
    match &error {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            if error.to_string().contains("mail account limit exceeded") {
                DbFailure::resource_limit("at most 64 mail accounts can be configured")
            } else {
                DbFailure::conflict("account configuration conflicts with existing data")
            }
        }
        _ => DbFailure::database(error),
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::store::sqlite::{
        FailureKind,
        domain::{AccountScope, FolderScope, PageBoundary, PageSpec},
        migrations::migrate,
        query::query_account_directory,
        stats::query_mailbox_stats,
    };

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open database");
        migrate(&mut connection).expect("migrate database");
        connection
    }

    fn input() -> AccountConfigInput {
        input_with_key(KEY)
    }

    fn input_with_key(key: &str) -> AccountConfigInput {
        AccountConfigInput::new(
            key,
            "Personal",
            "owner@example.test",
            AccountAuthKind::AppPassword,
            "owner@example.test",
            "imap.example.test",
            993,
            0x335244,
        )
        .expect("valid account input")
    }

    #[test]
    fn input_validation_rejects_unbounded_or_ambiguous_values() {
        assert!(matches!(
            AccountConfigInput::new(
                "not-a-key",
                "Personal",
                "owner@example.test",
                AccountAuthKind::AppPassword,
                "owner",
                "imap.example.test",
                993,
                0,
            ),
            Err(AccountValidationError::CredentialKey)
        ));
        assert!(matches!(
            AccountConfigInput::new(
                KEY,
                "Personal",
                "owner@example.test",
                AccountAuthKind::AppPassword,
                "owner",
                "-invalid.example",
                993,
                0,
            ),
            Err(AccountValidationError::Host)
        ));
        assert!(DiagnosticRecord::failed(AccountDiagnosticKind::Protocol, i64::MAX).is_err());
    }

    #[test]
    fn generation_fences_updates_and_diagnostics() {
        let mut connection = database();
        let created = create_account(&mut connection, &input()).expect("create account");
        assert_eq!(created.generation.get(), 1);
        assert!(created.lifecycle.enabled());
        assert_eq!(created.diagnostic, AccountDiagnostic::Never);

        let older =
            begin_diagnostic(&mut connection, created.account_id, created.generation).unwrap();
        let newer =
            begin_diagnostic(&mut connection, created.account_id, created.generation).unwrap();
        assert!(newer.epoch.get() > older.epoch.get());
        assert_eq!(
            record_diagnostic(
                &mut connection,
                created.account_id,
                created.generation,
                newer.epoch,
                &DiagnosticRecord::ready(100).unwrap(),
            )
            .unwrap(),
            DiagnosticCommit::Recorded
        );
        assert_eq!(
            record_diagnostic(
                &mut connection,
                created.account_id,
                created.generation,
                older.epoch,
                &DiagnosticRecord::failed(AccountDiagnosticKind::Timeout, 200).unwrap(),
            )
            .unwrap(),
            DiagnosticCommit::Stale
        );
        let mut changed = input();
        changed.name = "Updated".into();
        let updated = update_account(
            &mut connection,
            created.account_id,
            created.generation,
            &changed,
        )
        .expect("update account");
        assert_eq!(updated.generation.get(), 2);
        assert_eq!(updated.name.as_ref(), "Updated");
        assert_eq!(updated.diagnostic, AccountDiagnostic::Never);

        let mut changed_auth = changed.clone();
        changed_auth.auth_kind = AccountAuthKind::OAuth2;
        let failure = update_account(
            &mut connection,
            created.account_id,
            updated.generation,
            &changed_auth,
        )
        .expect_err("credential kind changes require an explicit rollover protocol");
        assert_eq!(failure.kind, FailureKind::Conflict);
        assert_eq!(
            load_account_from(&connection, created.account_id)
                .unwrap()
                .generation,
            updated.generation
        );

        assert_eq!(
            record_diagnostic(
                &mut connection,
                created.account_id,
                created.generation,
                newer.epoch,
                &DiagnosticRecord::failed(AccountDiagnosticKind::Timeout, 200).unwrap(),
            )
            .unwrap(),
            DiagnosticCommit::Stale
        );
        let failure = update_account(
            &mut connection,
            created.account_id,
            created.generation,
            &changed,
        )
        .expect_err("stale update must fail");
        assert_eq!(failure.kind, FailureKind::Conflict);

        let disabled = set_account_enabled(
            &mut connection,
            created.account_id,
            updated.generation,
            false,
        )
        .expect("disable account");
        assert_eq!(disabled.lifecycle, AccountLifecycle::Disabled);
        assert_eq!(disabled.generation.get(), 3);
        let failure = begin_diagnostic(&mut connection, disabled.account_id, disabled.generation)
            .expect_err("disabled account cannot start a diagnostic");
        assert_eq!(failure.kind, FailureKind::Conflict);
    }

    #[test]
    fn legacy_account_can_be_configured_or_removed_without_recreation() {
        let mut connection = database();
        connection
            .execute_batch(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES
                     (1, 'imap', 'legacy-one', 'Legacy', 'legacy@example.test',
                      'disabled', 1),
                     (2, 'imap', 'legacy-two', 'Retired', 'retired@example.test',
                      'offline', 2);",
            )
            .unwrap();

        let AccountRecord::NeedsSetup(target) =
            load_account(&connection, AccountId::new(1).unwrap())
                .expect("load legacy setup target")
        else {
            panic!("legacy account must not invent connection configuration");
        };
        assert_eq!(target.generation.get(), 1);
        assert!(!target.removal_pending);
        let configured = configure_existing_account(
            &mut connection,
            target.account_id,
            target.generation,
            &input(),
        )
        .expect("configure legacy account in place");
        assert_eq!(configured.account_id, target.account_id);
        assert_eq!(configured.generation.get(), 2);
        let remote_key: String = connection
            .query_row(
                "SELECT remote_key FROM accounts WHERE id = ?1",
                [target.account_id.get()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remote_key, "legacy-one");

        let retired_id = AccountId::new(2).unwrap();
        let AccountRecord::NeedsSetup(retired) = load_account(&connection, retired_id).unwrap()
        else {
            panic!("second legacy account must require setup");
        };
        let removal =
            begin_account_removal(&mut connection, retired.account_id, retired.generation)
                .expect("begin cache-only removal");
        assert!(removal.credential_key.is_none());
        assert!(matches!(
            purge_removed_account(&mut connection, removal.account_id, removal.generation, 10,)
                .unwrap(),
            AccountPurgeOutcome::Complete(_)
        ));
    }

    #[test]
    fn pending_credential_removals_are_bounded_filtered_and_stably_ordered() {
        let connection = database();
        connection
            .execute("DROP TRIGGER reject_account_limit_insert", [])
            .unwrap();

        for id in (1_i64..=68).rev() {
            let state = match id {
                2 => "active",
                3 => "removing_cache",
                4 => "disabled",
                _ => "removing_credentials",
            };
            let credential_key = format!("{id:032x}");
            let auth_kind = if id % 2 == 0 {
                "app_password"
            } else {
                "oauth2"
            };
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, state, accent_rgb,
                          configuration_generation)
                     VALUES (?1, 'imap', ?2, ?2, ?2, ?3, 0, ?4)",
                    params![id, format!("account-{id}"), state, 1_000 + id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO account_connections
                         (account_id, credential_key, auth_kind, login_name, imap_host, imap_port)
                     VALUES (?1, ?2, ?3, 'owner@example.test', 'imap.example.test', 993)",
                    params![id, credential_key, auth_kind],
                )
                .unwrap();
        }

        let pending = load_pending_credential_removals(&connection).unwrap();
        assert_eq!(pending.len(), PENDING_REMOVAL_LIMIT);
        let expected_ids: Vec<_> = (1_i64..=68)
            .filter(|id| !matches!(id, 2..=4))
            .take(PENDING_REMOVAL_LIMIT)
            .collect();
        assert_eq!(
            pending
                .iter()
                .map(|removal| removal.account_id.get())
                .collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(pending[0].configuration_generation.get(), 1_001);
        assert_eq!(
            pending[0].credential_key.as_ref(),
            "00000000000000000000000000000001"
        );
        assert_eq!(pending[0].auth_kind, AccountAuthKind::OAuth2);
        assert_eq!(pending[63].account_id.get(), 67);
        assert!(!pending.iter().any(|removal| removal.account_id.get() == 68));
    }

    #[test]
    fn pending_cache_removals_are_bounded_for_configured_and_legacy_accounts() {
        let connection = database();
        connection
            .execute("DROP TRIGGER reject_account_limit_insert", [])
            .unwrap();

        for id in (1_i64..=68).rev() {
            let state = match id {
                2 => "active",
                3 => "removing_credentials",
                4 => "disabled",
                _ => "removing_cache",
            };
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, state, accent_rgb,
                          configuration_generation)
                     VALUES (?1, 'imap', ?2, ?2, ?2, ?3, 0, ?4)",
                    params![id, format!("cache-account-{id}"), state, 2_000 + id],
                )
                .unwrap();
            if id % 2 == 0 || id == 3 {
                connection
                    .execute(
                        "INSERT INTO account_connections
                             (account_id, credential_key, auth_kind, login_name,
                              imap_host, imap_port)
                         VALUES (?1, ?2, 'app_password', 'owner@example.test',
                                 'imap.example.test', 993)",
                        params![id, format!("{id:032x}")],
                    )
                    .unwrap();
            }
        }

        let pending = load_pending_cache_removals(&connection).unwrap();
        assert_eq!(pending.len(), PENDING_REMOVAL_LIMIT);
        assert_eq!(
            pending
                .iter()
                .map(|removal| removal.account_id.get())
                .collect::<Vec<_>>(),
            (1_i64..=68)
                .filter(|id| !matches!(id, 2..=4))
                .take(PENDING_REMOVAL_LIMIT)
                .collect::<Vec<_>>()
        );
        assert_eq!(pending[0].configuration_generation.get(), 2_001);
        assert_eq!(pending[63].account_id.get(), 67);
        assert!(!pending.iter().any(|removal| removal.account_id.get() == 68));

        let AccountRecord::NeedsSetup(legacy) =
            load_account(&connection, AccountId::new(1).unwrap()).unwrap()
        else {
            panic!("odd cache-removal account must remain a legacy setup target");
        };
        assert!(legacy.removal_pending);
        let AccountRecord::Configured(configured) =
            load_account(&connection, AccountId::new(6).unwrap()).unwrap()
        else {
            panic!("even cache-removal account must remain configured");
        };
        assert_eq!(configured.lifecycle, AccountLifecycle::RemovingCache);
    }

    #[test]
    fn removal_queues_all_private_files_before_cascade() {
        let mut connection = database();
        let created = create_account(&mut connection, &input()).expect("create account");
        connection
            .execute(
                "INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms)
                 VALUES (1, ?1, 'message-1', 0)",
                [created.account_id.get()],
            )
            .unwrap();
        let body_key = "body/00000000000000000000000000000001.txt";
        let attachment_key = "attachment/00000000000000000000000000000002.bin";
        let outbox_key = "attachment/00000000000000000000000000000003.bin";
        let staged_key = "body/00000000000000000000000000000004.txt";
        connection
            .execute(
                "INSERT INTO message_content
                     (message_id, body_file_key)
                 VALUES (1, ?1)",
                [body_key],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attachments
                     (id, message_id, ordinal, file_key)
                 VALUES (1, 1, 0, ?1)",
                [attachment_key],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO outbox
                     (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (1, ?1, 'owner@example.test', 1, 'pending')",
                [outbox_key],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO file_staging
                     (file_key, batch_token, message_id, account_id, content_generation,
                      file_kind, part_ordinal, created_at_ms, expires_at_ms)
                 VALUES (?1, '00000000000000000000000000000001', 1, ?2, 1,
                         'body', NULL, 0, 1)",
                params![staged_key, created.account_id.get()],
            )
            .unwrap();

        let removing_credentials =
            begin_account_removal(&mut connection, created.account_id, created.generation)
                .expect("begin account removal");
        assert_eq!(removing_credentials.credential_key.as_deref(), Some(KEY));
        let early = purge_removed_account(
            &mut connection,
            created.account_id,
            removing_credentials.generation,
            10,
        )
        .expect_err("cache purge must wait for credential removal");
        assert_eq!(early.kind, FailureKind::Conflict);
        let removing_cache = confirm_account_credentials_removed(
            &mut connection,
            created.account_id,
            removing_credentials.generation,
        )
        .expect("confirm credential removal");
        let AccountPurgeOutcome::Complete(removed) = purge_removed_account(
            &mut connection,
            created.account_id,
            removing_cache.generation,
            10,
        )
        .expect("purge account") else {
            panic!("one-message account should finish in one bounded batch");
        };
        assert_eq!(removed.account_id, created.account_id);
        let queued: i64 = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 4);
        let accounts: i64 = connection
            .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accounts, 0);
        let staging: i64 = connection
            .query_row("SELECT count(*) FROM file_staging", [], |row| row.get(0))
            .unwrap();
        assert_eq!(staging, 0);
    }

    #[test]
    fn removal_purges_messages_in_fixed_batches() {
        let mut connection = database();
        let created = create_account(&mut connection, &input()).expect("create account");
        for id in 1..=ACCOUNT_PURGE_MESSAGE_BATCH as i64 + 1 {
            connection
                .execute(
                    "INSERT INTO messages
                         (id, account_id, remote_key, received_at_ms)
                     VALUES (?1, ?2, ?3, 0)",
                    params![id, created.account_id.get(), format!("message-{id}")],
                )
                .unwrap();
        }
        let removing_credentials =
            begin_account_removal(&mut connection, created.account_id, created.generation).unwrap();
        let removing_cache = confirm_account_credentials_removed(
            &mut connection,
            created.account_id,
            removing_credentials.generation,
        )
        .unwrap();

        let first = purge_removed_account(
            &mut connection,
            created.account_id,
            removing_cache.generation,
            10,
        )
        .unwrap();
        assert_eq!(
            first,
            AccountPurgeOutcome::Pending {
                removed_messages: ACCOUNT_PURGE_MESSAGE_BATCH as u8,
                removed_attachments: 0,
                removed_staging_files: 0,
                queued_files: 0,
            }
        );
        let remaining: i64 = connection
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        let directory = query_account_directory(&connection)
            .expect("removing account must not poison the account directory");
        assert_eq!(directory.rows[0].state.as_ref(), "removing");
        assert_eq!(directory.rows[0].inbox_unread, 0);
        let spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let stats = query_mailbox_stats(&connection, &spec)
            .expect("removing account must be excluded from aggregate statistics");
        assert!(stats.account_unread.is_empty());
        assert_eq!(stats.selected_total, Some(0));
        assert!(matches!(
            purge_removed_account(
                &mut connection,
                created.account_id,
                removing_cache.generation,
                11,
            )
            .unwrap(),
            AccountPurgeOutcome::Complete(_)
        ));
    }

    #[test]
    fn removal_purges_legal_attachment_fanout_in_fixed_batches() {
        let mut connection = database();
        let created = create_account(&mut connection, &input()).expect("create account");
        connection
            .execute(
                "INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms)
                 VALUES (1, ?1, 'message-1', 0)",
                [created.account_id.get()],
            )
            .unwrap();
        for ordinal in 0..=ACCOUNT_PURGE_ATTACHMENT_BATCH as i64 {
            connection
                .execute(
                    "INSERT INTO attachments (id, message_id, ordinal, file_key)
                     VALUES (?1, 1, ?2, ?3)",
                    params![
                        ordinal + 1,
                        ordinal,
                        format!("attachment/{ordinal:032x}.bin")
                    ],
                )
                .unwrap();
        }
        let removal =
            begin_account_removal(&mut connection, created.account_id, created.generation).unwrap();
        let removing_cache = confirm_account_credentials_removed(
            &mut connection,
            created.account_id,
            removal.generation,
        )
        .unwrap();

        assert_eq!(
            purge_removed_account(
                &mut connection,
                created.account_id,
                removing_cache.generation,
                10,
            )
            .unwrap(),
            AccountPurgeOutcome::Pending {
                removed_messages: 0,
                removed_attachments: ACCOUNT_PURGE_ATTACHMENT_BATCH as u8,
                removed_staging_files: 0,
                queued_files: ACCOUNT_PURGE_ATTACHMENT_BATCH as u16,
            }
        );
        let attachments: i64 = connection
            .query_row("SELECT count(*) FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(attachments, 1);
        assert!(matches!(
            purge_removed_account(
                &mut connection,
                created.account_id,
                removing_cache.generation,
                11,
            )
            .unwrap(),
            AccountPurgeOutcome::Complete(_)
        ));
        let queued: i64 = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, ACCOUNT_PURGE_ATTACHMENT_BATCH as i64 + 1);
    }

    #[test]
    fn account_limit_is_reported_as_a_resource_limit() {
        let mut connection = database();
        for index in 0..64_u128 {
            create_account(&mut connection, &input_with_key(&format!("{index:032x}"))).unwrap();
        }
        let failure = create_account(
            &mut connection,
            &input_with_key("ffffffffffffffffffffffffffffffff"),
        )
        .unwrap_err();
        assert_eq!(failure.kind, FailureKind::ResourceLimit);
    }
}
