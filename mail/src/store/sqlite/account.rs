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
const ACCOUNT_PURGE_PROVIDER_BATCH: usize = 16;
const PENDING_REMOVAL_LIMIT: usize = 64;
const ACCOUNT_SYNC_TARGET_LIMIT: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountAuthKind {
    AppPassword,
    OAuth2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpSecurity {
    ImplicitTls,
    StartTls,
}

impl SmtpSecurity {
    fn database_value(self) -> &'static str {
        match self {
            Self::ImplicitTls => "implicit_tls",
            Self::StartTls => "starttls",
        }
    }

    pub(super) fn from_database(value: &str) -> Result<Self, DbFailure> {
        match value {
            "implicit_tls" => Ok(Self::ImplicitTls),
            "starttls" => Ok(Self::StartTls),
            _ => Err(DbFailure::database("invalid SMTP security mode")),
        }
    }
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
    pub(crate) smtp_host: Box<str>,
    pub(crate) smtp_port: u16,
    pub(crate) smtp_security: SmtpSecurity,
    pub(crate) smtp_explicit: bool,
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
        Self::new_with_smtp(
            credential_key,
            name,
            address,
            auth_kind,
            login_name,
            imap_host,
            imap_port,
            imap_host,
            465,
            SmtpSecurity::ImplicitTls,
            false,
            accent_rgb,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_smtp(
        credential_key: &str,
        name: &str,
        address: &str,
        auth_kind: AccountAuthKind,
        login_name: &str,
        imap_host: &str,
        imap_port: u16,
        smtp_host: &str,
        smtp_port: u16,
        smtp_security: SmtpSecurity,
        smtp_explicit: bool,
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
            smtp_host: validate_host(smtp_host)?.into(),
            smtp_port: (smtp_port != 0)
                .then_some(smtp_port)
                .ok_or(AccountValidationError::Port)?,
            smtp_security,
            smtp_explicit,
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
    pub(crate) smtp_host: Box<str>,
    pub(crate) smtp_port: u16,
    pub(crate) smtp_security: SmtpSecurity,
    pub(crate) smtp_configured: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountSyncTarget {
    pub(crate) account_id: AccountId,
    pub(crate) generation: AccountGeneration,
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
                 (account_id, credential_key, auth_kind, login_name, imap_host, imap_port,
                  smtp_host, smtp_port, smtp_security, smtp_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                account_id.get(),
                input.credential_key.as_ref(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
                input.smtp_host.as_ref(),
                i64::from(input.smtp_port),
                input.smtp_security.database_value(),
                if input.smtp_explicit {
                    "configured"
                } else {
                    "needs_configuration"
                },
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
                 (account_id, credential_key, auth_kind, login_name, imap_host, imap_port,
                  smtp_host, smtp_port, smtp_security, smtp_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                account_id.get(),
                input.credential_key.as_ref(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
                input.smtp_host.as_ref(),
                i64::from(input.smtp_port),
                input.smtp_security.database_value(),
                if input.smtp_explicit {
                    "configured"
                } else {
                    "needs_configuration"
                },
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

pub(super) fn load_sync_targets(
    connection: &Connection,
) -> Result<Box<[AccountSyncTarget]>, DbFailure> {
    let row_limit = i64::try_from(ACCOUNT_SYNC_TARGET_LIMIT + 1)
        .map_err(|_| DbFailure::resource_limit("account sync target limit is invalid"))?;
    let mut statement = connection
        .prepare(
            "SELECT account.id, account.configuration_generation
             FROM accounts AS account
             JOIN account_connections AS connection ON connection.account_id = account.id
             WHERE account.state = 'active'
             ORDER BY account.sort_order, account.id
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([row_limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(DbFailure::database)?;

    let mut targets = Vec::with_capacity(ACCOUNT_SYNC_TARGET_LIMIT + 1);
    for row in rows {
        let (account_id, generation) = row.map_err(DbFailure::database)?;
        targets.push(AccountSyncTarget {
            account_id: AccountId::new(account_id)
                .map_err(|error| DbFailure::database(error.to_string()))?,
            generation: AccountGeneration::from_database(generation)?,
        });
    }
    if targets.len() > ACCOUNT_SYNC_TARGET_LIMIT {
        return Err(DbFailure::resource_limit(format!(
            "account sync target count exceeds the {ACCOUNT_SYNC_TARGET_LIMIT}-account limit"
        )));
    }
    Ok(targets.into_boxed_slice())
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
                 smtp_host = ?6,
                 smtp_port = ?7,
                 smtp_security = ?8,
                 smtp_state = ?9,
                 diagnostic_state = 'never',
                 last_checked_at_ms = NULL
             WHERE account_id = ?1",
            params![
                account_id.get(),
                input.auth_kind.database_value(),
                input.login_name.as_ref(),
                input.imap_host.as_ref(),
                i64::from(input.imap_port),
                input.smtp_host.as_ref(),
                i64::from(input.smtp_port),
                input.smtp_security.database_value(),
                if input.smtp_explicit {
                    "configured"
                } else {
                    "needs_configuration"
                },
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
    reject_account_with_undelivered_outbox(&transaction, account_id)?;
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
    reject_account_with_undelivered_outbox(&transaction, account_id)?;
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
        purge_message_provider_rows(&transaction, *message_id)?;
        if message_provider_rows_remain(&transaction, *message_id)? {
            break;
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

    let local_cache_remaining: bool = transaction
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
    let provider_cleanup = if local_cache_remaining {
        None
    } else {
        Some(purge_account_provider_rows(&transaction, account_id)?)
    };
    if provider_cleanup
        .as_ref()
        .is_some_and(|cleanup| cleanup.remaining && cleanup.removed == 0)
    {
        return Err(DbFailure::conflict(
            "account provider cache cleanup could not make progress",
        ));
    }
    if local_cache_remaining
        || provider_cleanup
            .as_ref()
            .is_some_and(|cleanup| cleanup.remaining)
    {
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

    transaction
        .execute(
            "DELETE FROM account_connections WHERE account_id = ?1",
            [account_id.get()],
        )
        .map_err(map_account_write_error)?;
    transaction
        .execute(
            "DELETE FROM account_mailbox_stats WHERE account_id = ?1",
            [account_id.get()],
        )
        .map_err(map_account_write_error)?;
    if account_direct_children_remain(&transaction, account_id)? {
        return Err(DbFailure::conflict(
            "account cache still has rows that require bounded cleanup",
        ));
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

fn purge_message_provider_rows(
    connection: &Connection,
    message_id: MessageId,
) -> Result<(), DbFailure> {
    let limit = ACCOUNT_PURGE_PROVIDER_BATCH as i64;
    connection
        .execute(
            "DELETE FROM imap_message_locations
             WHERE (message_id, folder_id) IN (
                 SELECT message_id, folder_id
                 FROM imap_message_locations
                 WHERE message_id = ?1
                 ORDER BY folder_id
                 LIMIT ?2
             )",
            params![message_id.get(), limit],
        )
        .map_err(map_account_write_error)?;
    connection
        .execute(
            "DELETE FROM message_folders
             WHERE (message_id, folder_id) IN (
                 SELECT membership.message_id, membership.folder_id
                 FROM message_folders AS membership
                 WHERE membership.message_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM imap_message_locations AS location
                       WHERE location.message_id = membership.message_id
                         AND location.folder_id = membership.folder_id
                   )
                 ORDER BY membership.folder_id
                 LIMIT ?2
             )",
            params![message_id.get(), limit],
        )
        .map_err(map_account_write_error)?;
    Ok(())
}

fn message_provider_rows_remain(
    connection: &Connection,
    message_id: MessageId,
) -> Result<bool, DbFailure> {
    connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM imap_message_locations WHERE message_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM message_folders WHERE message_id = ?1
             )",
            [message_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

struct ProviderCleanup {
    removed: usize,
    remaining: bool,
}

fn purge_account_provider_rows(
    connection: &Connection,
    account_id: AccountId,
) -> Result<ProviderCleanup, DbFailure> {
    let limit = ACCOUNT_PURGE_PROVIDER_BATCH as i64;
    let mut removed = 0_usize;
    for sql in [
        "DELETE FROM remote_change_intent_folders
         WHERE (intent_id, side, folder_key) IN (
             SELECT child.intent_id, child.side, child.folder_key
             FROM remote_change_intent_folders AS child
             JOIN remote_change_intents AS intent ON intent.id = child.intent_id
             WHERE intent.account_id = ?1
             ORDER BY child.intent_id, child.side, child.folder_key
             LIMIT ?2
         )",
        "DELETE FROM remote_change_intent_imap_sources
         WHERE (intent_id, folder_key, uid_validity, uid) IN (
             SELECT child.intent_id, child.folder_key, child.uid_validity, child.uid
             FROM remote_change_intent_imap_sources AS child
             JOIN remote_change_intents AS intent ON intent.id = child.intent_id
             WHERE intent.account_id = ?1
             ORDER BY child.intent_id, child.folder_key, child.uid_validity, child.uid
             LIMIT ?2
         )",
        "DELETE FROM remote_change_intents
         WHERE id IN (
             SELECT intent.id
             FROM remote_change_intents AS intent
             WHERE intent.account_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM remote_change_intent_folders AS child
                   WHERE child.intent_id = intent.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM remote_change_intent_imap_sources AS child
                   WHERE child.intent_id = intent.id
               )
             ORDER BY intent.id
             LIMIT ?2
         )",
        "DELETE FROM message_tombstone_imap_locations
         WHERE (account_id, target_key, folder_key, uid_validity, uid) IN (
             SELECT account_id, target_key, folder_key, uid_validity, uid
             FROM message_tombstone_imap_locations
             WHERE account_id = ?1
             ORDER BY target_key, folder_key, uid_validity, uid
             LIMIT ?2
         )",
        "DELETE FROM message_tombstones
         WHERE (account_id, remote_key) IN (
             SELECT tombstone.account_id, tombstone.remote_key
             FROM message_tombstones AS tombstone
             WHERE tombstone.account_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM message_tombstone_imap_locations AS child
                   WHERE child.account_id = tombstone.account_id
                     AND child.target_key = tombstone.remote_key
               )
             ORDER BY tombstone.remote_key
             LIMIT ?2
         )",
        "DELETE FROM imap_message_locations
         WHERE (message_id, folder_id) IN (
             SELECT message_id, folder_id
             FROM imap_message_locations
             WHERE account_id = ?1
             ORDER BY message_id, folder_id
             LIMIT ?2
         )",
        "DELETE FROM sync_state
         WHERE folder_id IN (
             SELECT state.folder_id
             FROM sync_state AS state
             JOIN folders AS folder ON folder.id = state.folder_id
             WHERE folder.account_id = ?1
             ORDER BY state.folder_id
             LIMIT ?2
         )",
        "DELETE FROM folders
         WHERE id IN (
             SELECT folder.id
             FROM folders AS folder
             WHERE folder.account_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM message_folders AS membership
                   WHERE membership.folder_id = folder.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM imap_message_locations AS location
                   WHERE location.folder_id = folder.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM sync_state AS state
                   WHERE state.folder_id = folder.id
               )
             ORDER BY folder.id
             LIMIT ?2
         )",
        "DELETE FROM account_object_states
         WHERE (account_id, object_kind) IN (
             SELECT account_id, object_kind
             FROM account_object_states
             WHERE account_id = ?1
             ORDER BY object_kind
             LIMIT ?2
         )",
        "DELETE FROM remote_account_reconciliations
         WHERE account_id IN (
             SELECT account_id
             FROM remote_account_reconciliations
             WHERE account_id = ?1
             LIMIT ?2
         )",
    ] {
        removed = removed
            .checked_add(
                connection
                    .execute(sql, params![account_id.get(), limit])
                    .map_err(map_account_write_error)?,
            )
            .ok_or_else(|| DbFailure::resource_limit("account provider cleanup count overflow"))?;
    }
    Ok(ProviderCleanup {
        removed,
        remaining: account_provider_rows_remain(connection, account_id)?,
    })
}

fn account_provider_rows_remain(
    connection: &Connection,
    account_id: AccountId,
) -> Result<bool, DbFailure> {
    connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM folders WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM imap_message_locations WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM message_tombstones WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM remote_change_intents WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM account_object_states WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM remote_account_reconciliations WHERE account_id = ?1
             )",
            [account_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn reject_account_with_undelivered_outbox(
    connection: &Connection,
    account_id: AccountId,
) -> Result<(), DbFailure> {
    let protected: bool = connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM outbox
                 WHERE account_id = ?1 AND state <> 'delivered'
             )",
            [account_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if protected {
        Err(DbFailure::conflict("account has undelivered outbox mail"))
    } else {
        Ok(())
    }
}

fn account_direct_children_remain(
    connection: &Connection,
    account_id: AccountId,
) -> Result<bool, DbFailure> {
    connection
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM account_connections WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM account_mailbox_stats WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM messages WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM file_staging WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM folders WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM message_tombstones WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM account_object_states WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM remote_account_reconciliations WHERE account_id = ?1
             ) OR EXISTS (
                 SELECT 1 FROM remote_change_intents WHERE account_id = ?1
             )",
            [account_id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
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
        i64,
        String,
        String,
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
                    connection.smtp_host, connection.smtp_port, connection.smtp_security,
                    connection.smtp_state,
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
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
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
        smtp_host,
        smtp_port,
        smtp_security,
        smtp_state,
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
        smtp_host: smtp_host.into_boxed_str(),
        smtp_port: u16::try_from(smtp_port)
            .map_err(|_| DbFailure::database("invalid SMTP port in account configuration"))?,
        smtp_security: SmtpSecurity::from_database(&smtp_security)?,
        smtp_configured: match smtp_state.as_str() {
            "configured" => true,
            "needs_configuration" => false,
            _ => return Err(DbFailure::database("invalid SMTP configuration state")),
        },
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
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

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

    static NEXT_DATABASE_PATH: AtomicU64 = AtomicU64::new(1);

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open database");
        migrate(&mut connection).expect("migrate database");
        connection
    }

    fn file_database(label: &str) -> (PathBuf, Connection) {
        let sequence = NEXT_DATABASE_PATH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nivalis-account-{label}-{}-{sequence}.sqlite",
            std::process::id()
        ));
        let mut connection = Connection::open(&path).expect("open file database");
        migrate(&mut connection).expect("migrate file database");
        (path, connection)
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
    fn sync_targets_are_bounded_filtered_and_stably_ordered() {
        let connection = database();
        for (id, generation, sort_order, state, configured) in [
            (9_i64, 19_i64, 1_i64, "active", true),
            (2, 2, 0, "disabled", true),
            (7, 17, 0, "active", false),
            (5, 15, 0, "active", true),
            (8, 18, 0, "removing_credentials", true),
            (6, 16, 0, "removing_cache", true),
            (4, 14, 1, "active", true),
        ] {
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, sort_order, state,
                          accent_rgb, configuration_generation)
                     VALUES (?1, 'imap', ?2, ?2, ?2, ?3, ?4, 0, ?5)",
                    params![
                        id,
                        format!("sync-account-{id}"),
                        sort_order,
                        state,
                        generation
                    ],
                )
                .unwrap();
            if configured {
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

        let targets = load_sync_targets(&connection).unwrap();
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.account_id.get(), target.generation.get()))
                .collect::<Vec<_>>(),
            vec![(5, 15), (4, 14), (9, 19)]
        );

        connection
            .execute("DROP TRIGGER reject_account_limit_insert", [])
            .unwrap();
        for id in 10_i64..=70 {
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, sort_order, state,
                          accent_rgb, configuration_generation)
                     VALUES (?1, 'imap', ?2, ?2, ?2, ?1, 'active', 0, ?1)",
                    params![id, format!("overflow-account-{id}")],
                )
                .unwrap();
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
        assert_eq!(
            load_sync_targets(&connection).unwrap().len(),
            ACCOUNT_SYNC_TARGET_LIMIT
        );
        let id = 71_i64;
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, sort_order, state,
                      accent_rgb, configuration_generation)
                 VALUES (?1, 'imap', ?2, ?2, ?2, ?1, 'active', 0, ?1)",
                params![id, format!("overflow-account-{id}")],
            )
            .unwrap();
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
        let failure = load_sync_targets(&connection).unwrap_err();
        assert_eq!(failure.kind, FailureKind::ResourceLimit);
    }

    #[test]
    fn sync_targets_reject_invalid_database_identity_and_generation() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE accounts (
                     id INTEGER,
                     configuration_generation INTEGER,
                     sort_order INTEGER,
                     state TEXT
                 );
                 CREATE TABLE account_connections (account_id INTEGER);
                 INSERT INTO accounts VALUES (0, 1, 0, 'active');
                 INSERT INTO account_connections VALUES (0);",
            )
            .unwrap();

        let invalid_id = load_sync_targets(&connection).unwrap_err();
        assert_eq!(invalid_id.kind, FailureKind::Database);

        connection.execute("DELETE FROM accounts", []).unwrap();
        connection
            .execute("DELETE FROM account_connections", [])
            .unwrap();
        connection
            .execute("INSERT INTO accounts VALUES (1, 0, 0, 'active')", [])
            .unwrap();
        connection
            .execute("INSERT INTO account_connections VALUES (1)", [])
            .unwrap();
        let invalid_generation = load_sync_targets(&connection).unwrap_err();
        assert_eq!(invalid_generation.kind, FailureKind::Database);
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
                     (message_id, account_id, configuration_generation, artifact_generation,
                      draft_revision, mime_file_key, rfc_message_id, envelope_from,
                      wire_byte_count, state, created_at_ms, updated_at_ms, delivered_at_ms)
                 VALUES (1, ?1, ?2, 1, 0, ?3, '<1@example.test>',
                         'owner@example.test', 1, 'delivered', 0, 0, 0)",
                params![
                    created.account_id.get(),
                    created.generation.get(),
                    outbox_key
                ],
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
    fn removal_resumes_bounded_provider_cleanup_without_final_cascade() {
        let (path, mut connection) = file_database("provider-purge");
        let created = create_account(&mut connection, &input()).expect("create account");
        let account_id = created.account_id.get();
        let row_count = ACCOUNT_PURGE_PROVIDER_BATCH as i64 + 1;

        connection
            .execute(
                "INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms)
                 VALUES (1, ?1, 'live-message', 0)",
                [account_id],
            )
            .unwrap();
        for index in 1..=row_count {
            let folder_key = format!("folder-{index:02}");
            connection
                .execute(
                    "INSERT INTO folders (id, account_id, remote_key, name, role)
                     VALUES (?1, ?2, ?3, ?3, 'custom')",
                    params![index, account_id, folder_key],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO sync_state
                         (folder_id, uid_validity, change_cursor, last_sync_at_ms)
                     VALUES (?1, 7, ?2, 0)",
                    params![index, index.to_string()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO message_folders (message_id, folder_id, account_id)
                     VALUES (1, ?1, ?2)",
                    params![index, account_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO imap_message_locations
                         (message_id, folder_id, account_id, uid_validity, uid,
                          remote_seen, remote_flagged)
                     VALUES (1, ?1, ?2, 7, ?1, 0, 0)",
                    params![index, account_id],
                )
                .unwrap();
        }

        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (?1, 'pending-message', 0, 1, 0, 0, 0)",
                [account_id],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        for index in 1..=row_count {
            let folder_key = format!("folder-{index:02}");
            connection
                .execute(
                    "INSERT INTO remote_change_intent_folders
                         (intent_id, side, folder_key)
                     VALUES (?1, 'base', ?2)",
                    params![intent_id, folder_key],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO remote_change_intent_imap_sources
                         (intent_id, folder_key, uid_validity, uid,
                          remote_seen, remote_flagged)
                     VALUES (?1, ?2, 7, ?3, 0, 0)",
                    params![intent_id, folder_key, index],
                )
                .unwrap();
        }

        connection
            .execute(
                "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
                 VALUES (?1, 'deleted-message', 0)",
                [account_id],
            )
            .unwrap();
        for index in 1..=row_count {
            connection
                .execute(
                    "INSERT INTO message_tombstone_imap_locations
                         (account_id, target_key, folder_key, uid_validity, uid)
                     VALUES (?1, 'deleted-message', ?2, 7, ?3)",
                    params![account_id, format!("folder-{index:02}"), index],
                )
                .unwrap();
        }
        for kind in ["email", "mailbox", "thread"] {
            connection
                .execute(
                    "INSERT INTO account_object_states
                         (account_id, object_kind, state_token, updated_at_ms)
                     VALUES (?1, ?2, ?3, 0)",
                    params![account_id, kind, format!("state-{kind}")],
                )
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO remote_account_reconciliations
                     (account_id, reason, requested_at_ms)
                 VALUES (?1, 'legacy_journal_bootstrap', 0)",
                [account_id],
            )
            .unwrap();

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
                removed_attachments: 0,
                removed_staging_files: 0,
                queued_files: 0,
            }
        );
        let (locations, memberships, messages): (i64, i64, i64) = connection
            .query_row(
                "SELECT
                     (SELECT count(*) FROM imap_message_locations),
                     (SELECT count(*) FROM message_folders),
                     (SELECT count(*) FROM messages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((locations, memberships, messages), (1, 1, 1));

        assert_eq!(
            purge_removed_account(
                &mut connection,
                created.account_id,
                removing_cache.generation,
                11,
            )
            .unwrap(),
            AccountPurgeOutcome::Pending {
                removed_messages: 1,
                removed_attachments: 0,
                removed_staging_files: 0,
                queued_files: 0,
            }
        );
        let (folders, intent_children, tombstone_children, journal_children): (i64, i64, i64, i64) =
            connection
                .query_row(
                    "SELECT
                         (SELECT count(*) FROM folders),
                         (SELECT count(*) FROM remote_change_intent_folders) +
                             (SELECT count(*) FROM remote_change_intent_imap_sources),
                         (SELECT count(*) FROM message_tombstone_imap_locations),
                         (SELECT child_count FROM remote_journal_usage WHERE singleton = 1)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
        assert_eq!((folders, intent_children, tombstone_children), (1, 2, 1));
        assert_eq!(journal_children, 3);

        drop(connection);
        let mut connection = Connection::open(&path).expect("reopen removal database");
        migrate(&mut connection).expect("restore database invariants");
        let stale_generation = AccountGeneration::new(removing_cache.generation.get() - 1).unwrap();
        let stale =
            purge_removed_account(&mut connection, created.account_id, stale_generation, 12)
                .expect_err("restart must preserve the removal generation fence");
        assert_eq!(stale.kind, FailureKind::Conflict);
        assert!(matches!(
            purge_removed_account(
                &mut connection,
                created.account_id,
                removing_cache.generation,
                13,
            )
            .unwrap(),
            AccountPurgeOutcome::Complete(_)
        ));
        let remaining: i64 = connection
            .query_row(
                "SELECT
                     (SELECT count(*) FROM accounts) +
                     (SELECT count(*) FROM folders) +
                     (SELECT count(*) FROM sync_state) +
                     (SELECT count(*) FROM imap_message_locations) +
                     (SELECT count(*) FROM remote_change_intents) +
                     (SELECT count(*) FROM remote_change_intent_folders) +
                     (SELECT count(*) FROM remote_change_intent_imap_sources) +
                     (SELECT count(*) FROM message_tombstones) +
                     (SELECT count(*) FROM message_tombstone_imap_locations) +
                     (SELECT count(*) FROM account_object_states) +
                     (SELECT count(*) FROM remote_account_reconciliations)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        let journal_usage: (i64, i64) = connection
            .query_row(
                "SELECT child_count, reserved_count
                 FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(journal_usage, (0, 0));
        drop(connection);
        fs::remove_file(path).expect("remove file database");
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
