# Remote Synchronization Contract

## Status

This document is the normative boundary between Nivalis' SQLite actor and its provider adapters. Schema v8 implements the bounded locator, object-state, journal, legacy-reconciliation, and placement-rebase reservation storage described here. Local flags, Archive, Trash, Trash undo, and permanent deletion now reduce into that journal atomically, including frozen folder and locator snapshots, terminal tombstones, version compaction, and complete rollback on resource limits. The SQLite actor exposes bounded, versioned claim snapshots with one global lease, conservative expiry recovery, exact wake-up times, and reserved capacity for merging a confirmed placement with a newer local desired version. Fully fenced report processing now covers confirmation, satisfaction, progress renewal, retry, reconciliation, blocking, IMAP/JMAP checkpoints, newer-version merges, and transactional rollback. M4 implements a separate bounded, manually triggered inbound IMAP page; outbound provider execution and full bidirectional merge do not exist yet. The UI must not imply that queued local intent reached a server. Providers must not claim work or issue remote writes until synchronization merge/reconciliation and provider execution implement this contract.

The journal records the latest desired state of a logical message, not a history of UI actions. Local state, statistics, undo/tombstone data, and the journal change must commit in one SQLite `BEGIN IMMEDIATE` transaction.

## Identity

`messages.remote_key` is an account-scoped, stable logical target key. A JMAP account may use the opaque Email id directly. An IMAP account must use a locally stable opaque key or a stable `EMAILID` when the server supports the OBJECTID extension. A bare IMAP UID is never a logical target key.

An executable IMAP locator is the tuple:

```text
(mailbox identity, UIDVALIDITY, UID)
```

`UIDVALIDITY` and UID are non-zero 32-bit values. The locator may also retain a non-zero `MODSEQ`, `EMAILID`, and stable mailbox object id when the server provides them. Nivalis must select the mailbox and verify its current `UIDVALIDITY` before every write. A mismatch invalidates the locator and enters reconciliation; the old UID must never be tried against the new epoch. This follows the mailbox-scoped identity rules in [RFC 9051](https://www.rfc-editor.org/rfc/rfc9051.html).

A logical message may have multiple IMAP locations. Without a stable object id, the importer treats independent mailbox copies as independent logical messages. If copies are grouped by `EMAILID`, confirmed flags and `MODSEQ` remain per location; the provider cannot assume that one IMAP `STORE` changes every copy.

JMAP Email state is scoped to the account and object type, not to a mailbox. SQLite therefore stores JMAP Email, Mailbox, and Thread state tokens separately from the existing per-folder IMAP synchronization state. State tokens are opaque and are compared only for equality.

## Desired State

There is at most one journal intent for each `(account_id, logical_target_key)`. It may carry four independent dimensions:

- unread base and desired values;
- starred base and desired values;
- exact base and desired mailbox membership snapshots;
- terminal deletion.

The first local edit preserves the confirmed base for the affected dimension. Later local edits replace only its desired value. If a value returns to base before that dimension is dispatched, the dimension is removed. Once dispatched, a reversal remains as compensating desired state. Deletion supersedes flags and placement and retains frozen protocol locators after the local message row is removed.

Archive, Trash, and Trash undo reduce to desired mailbox membership; `undo` is not a provider command. A Trash intent is not eligible before the five-second undo deadline. Undo within that window can therefore remove an undispatched Trash-only intent without remote traffic.

Folder keys and IMAP source locators are copied into journal child rows rather than retained as local folder foreign keys. This keeps an intent executable after a folder refresh, local membership change, or permanent message deletion.

## Claim And Report

Each row has a monotonic desired version and claim epoch. Claiming work is a durable SQLite transition performed before network I/O. A provider report is accepted only when `(intent_id, leased_version, claim_epoch)` matches the current durable lease.

Only one version of an intent may be in flight. If a new local edit creates version 2 while version 1 is leased, the version-1 lease remains active and version 2 is not dispatched. When the version-1 report arrives, SQLite records any mandatory locator/cursor checkpoint, releases the lease, and retains version 2. This prevents older and newer remote writes from completing out of order.

Provider reports use these semantic dispositions:

- `Confirmed`: the server explicitly confirmed the requested effect;
- `Satisfied`: reconciliation proved that the desired effect already exists;
- `Progress`: a multi-phase operation reached a durable checkpoint but is not complete;
- `Retry`: the operation is safe to retry after a bounded delay;
- `Reconcile`: the send result or remote identity is ambiguous and remote state must be read first;
- `Blocked`: authentication, permissions, capability, or permanent data failure requires user or account action.

A stale report cannot delete or defer newer desired state. After process failure, abandoned flag-only/JMAP patch work may be replayed idempotently. Any in-flight IMAP transfer or delete with ambiguous outcome enters reconciliation before another write.

## IMAP Rules

- Address messages only by UID, never by sequence number.
- Map unread to the inverse of `\Seen` and starred to `\Flagged`. Use additive or subtractive `UID STORE ... FLAGS.SILENT`; never overwrite the complete flag set.
- With CONDSTORE, use `UNCHANGEDSINCE`. A `MODIFIED` response requires refetch and rebase.
- With MOVE support, use `UID MOVE`. A returned `COPYUID` updates the destination locator before the intent can be acknowledged.
- Without MOVE, the only safe fallback is `UID COPY`, a durable destination-locator checkpoint, `UID STORE +\Deleted`, then `UID EXPUNGE` when UID-scoped expunge is supported. Plain `EXPUNGE` is forbidden because it can remove another client's deleted messages.
- A successful COPY/MOVE response without a usable destination locator, or a connection loss after sending, is ambiguous. Synchronize/search the destination and reconcile before deleting the source or retrying. These transfer constraints follow [RFC 6851](https://www.rfc-editor.org/rfc/rfc6851.html).

## JMAP Rules

- Change Email state with `Email/set` PatchObject paths such as `keywords/$seen`, `keywords/$flagged`, and `mailboxIds/<id>`; do not replace complete keyword or mailbox maps.
- Always send the current Email state token as `ifInState`. On `stateMismatch`, fetch `Email/changes`, merge the server update, overlay unacknowledged local desired dimensions, store the new state, and retry. Do not fall back to an unconditional write.
- `updated[id]` or `destroyed[id]` confirms the corresponding operation. `notFound` satisfies deletion but is a reconcile/conflict result for an update.
- A JMAP method-call id correlates a response only; it is not a durable idempotency key.

JMAP Email ids remain stable across mailbox membership changes, and `Email/set` covers keyword, membership, and deletion updates as specified by [RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html). Generic state and `/set` behavior are defined by [RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html).

## Sync Merge And Tombstones

Incoming synchronization may update dimensions with no pending intent. For pending or blocked dimensions, the local desired value is overlaid after importing the remote base so offline work is not overwritten. Cursor/state-token advancement, locator changes, journal progress, and statistics rebuilds commit atomically.

The v7 migration does not expand every historical local revision into a target intent, because a real mailbox may exceed the live journal caps. It stores the migration-time revision in `messages.legacy_reconcile_revision`, keeps existing tombstones as the deletion backlog, and creates at most one reconciliation gate per account. A bounded feeder may materialize those targets only as journal capacity becomes available. Clearing a legacy revision, acknowledging its matching intent, advancing the remote cursor/state, and removing the account gate must follow the same version checks and transactional rules as normal synchronization.

Permanent local deletion writes a terminal intent, freezes all known locators, and creates a tombstone before deleting the message. Import checks tombstones before recreating a message. A tombstone is collected only after a causally newer remote cursor or state fence confirms absence and that confirmation commits with the journal acknowledgement.

## Resource Limits

- At most 4,096 intents per account and 16,384 globally.
- At most 256 base and 256 desired folder keys per intent.
- At most 65,536 frozen folder/locator child rows globally.
- Placement leases reserve their bounded rebase growth inside the same 65,536-row global budget.
- Claim one complete intent at a time; metadata prefetch may inspect at most 16 parent rows.
- A claimed provider payload, including folder keys and locators, is limited to 320 KiB.
- Attempts are capped at 1,000. Error code and detail fields are capped at 64 and 1,024 UTF-8 bytes.

Hitting any limit returns `ResourceLimit` and rolls back the complete local mutation. Updating an existing target remains possible when the parent-row cap is full, subject to its child and byte budgets. No queue scan materializes all pending intents in Rust, and the network actor owns at most one wake-up timer rather than one timer per row.

SQLite maintains the global child-row budget with one transactional usage row rather than scanning all child tables on each insert. Every application connection enables and verifies foreign keys and recursive triggers so cascades and replacement paths keep that counter exact.

## Non-Goals

The journal does not preserve an audit log, provider command transcript, or acknowledged history. It does not make ambiguous IMAP MOVE/COPY operations safe by blind replay. It also does not authorize a partial controller cutover: the in-memory and SQLite repositories must not both own visible mailbox state.
