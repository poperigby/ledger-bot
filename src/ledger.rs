use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::parser::{LedgerCommand, LedgerKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub room_id: String,
    pub matrix_event_id: String,
    pub kind: LedgerKind,
    pub actor_mxid: String,
    pub counterparty_mxid: String,
    pub amount_cents: i64,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    pub counterparty_mxid: String,
    pub balance_cents: i64,
}

impl LedgerEntry {
    pub fn from_command(
        room_id: impl Into<String>,
        matrix_event_id: impl Into<String>,
        actor_mxid: impl Into<String>,
        command: LedgerCommand,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            room_id: room_id.into(),
            matrix_event_id: matrix_event_id.into(),
            kind: command.kind,
            actor_mxid: actor_mxid.into(),
            counterparty_mxid: command.counterparty_mxid,
            amount_cents: command.amount_cents,
            description: command.description,
            created_at,
        }
    }
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
create table if not exists ledger_entries (
    id bigserial primary key,
    room_id text not null,
    matrix_event_id text not null unique,
    kind text not null check (kind in ('purchase', 'payment')),
    actor_mxid text not null,
    counterparty_mxid text not null,
    amount_cents bigint not null check (amount_cents > 0),
    description text,
    created_at timestamptz not null,
    redacted boolean not null default false
);
"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
create table if not exists processed_events (
    event_id text primary key,
    room_id text not null,
    processed_at timestamptz not null
);
"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("alter table processed_events add column if not exists sender_mxid text")
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn insert_entry(pool: &PgPool, entry: &LedgerEntry) -> anyhow::Result<bool> {
    let kind = entry.kind.as_str();
    let result = sqlx::query(
        r#"
insert into ledger_entries (
    room_id,
    matrix_event_id,
    kind,
    actor_mxid,
    counterparty_mxid,
    amount_cents,
    description,
    created_at
) values ($1, $2, $3, $4, $5, $6, $7, $8)
on conflict (matrix_event_id) do nothing
"#,
    )
    .bind(&entry.room_id)
    .bind(&entry.matrix_event_id)
    .bind(kind)
    .bind(&entry.actor_mxid)
    .bind(&entry.counterparty_mxid)
    .bind(entry.amount_cents)
    .bind(&entry.description)
    .bind(entry.created_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

pub async fn mark_processed(
    pool: &PgPool,
    event_id: &str,
    room_id: &str,
    sender_mxid: &str,
    processed_at: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"
insert into processed_events (event_id, room_id, sender_mxid, processed_at)
values ($1, $2, $3, $4)
on conflict (event_id) do nothing
"#,
    )
    .bind(event_id)
    .bind(room_id)
    .bind(sender_mxid)
    .bind(processed_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

pub async fn redact_entry(pool: &PgPool, matrix_event_id: &str) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"
update ledger_entries
set redacted = true
where matrix_event_id = $1 and redacted = false
"#,
    )
    .bind(matrix_event_id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Returns the sender of an event the bot has seen in `room_id`, if known.
pub async fn event_sender(
    pool: &PgPool,
    event_id: &str,
    room_id: &str,
) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        r#"
select sender_mxid
from processed_events
where event_id = $1 and room_id = $2 and sender_mxid is not null
"#,
    )
    .bind(event_id)
    .bind(room_id)
    .fetch_optional(pool)
    .await?)
}

/// Applies an edit. `entry.matrix_event_id` must be the original event ID, since
/// Matrix edits always point at the original rather than at the previous edit.
///
/// Inserts the entry if the original was not a ledger command, otherwise updates
/// the existing row in place. Redacted entries and entries recorded by a
/// different sender are left untouched.
pub async fn upsert_edited_entry(pool: &PgPool, entry: &LedgerEntry) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"
insert into ledger_entries (
    room_id,
    matrix_event_id,
    kind,
    actor_mxid,
    counterparty_mxid,
    amount_cents,
    description,
    created_at
) values ($1, $2, $3, $4, $5, $6, $7, $8)
on conflict (matrix_event_id) do update
set kind = excluded.kind,
    counterparty_mxid = excluded.counterparty_mxid,
    amount_cents = excluded.amount_cents,
    description = excluded.description
where ledger_entries.redacted = false
  and ledger_entries.room_id = excluded.room_id
  and ledger_entries.actor_mxid = excluded.actor_mxid
"#,
    )
    .bind(&entry.room_id)
    .bind(&entry.matrix_event_id)
    .bind(entry.kind.as_str())
    .bind(&entry.actor_mxid)
    .bind(&entry.counterparty_mxid)
    .bind(entry.amount_cents)
    .bind(&entry.description)
    .bind(entry.created_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

pub async fn balances(pool: &PgPool, room_id: &str) -> anyhow::Result<Vec<Balance>> {
    let rows = sqlx::query(
        r#"
select counterparty_mxid,
       coalesce(sum(case kind
           when 'purchase' then amount_cents
           when 'payment' then -amount_cents
       end), 0) as balance_cents
from ledger_entries
where redacted = false and room_id = $1
group by counterparty_mxid
order by counterparty_mxid
"#,
    )
    .bind(room_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(Balance {
                counterparty_mxid: row.try_get("counterparty_mxid")?,
                balance_cents: row.try_get("balance_cents")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::parser::{BotCommand, parse_command};

    #[test]
    fn builds_entry_from_command() {
        let BotCommand::Entry(command) =
            parse_command("!purchase @cassidy:example $12.34 lunch").unwrap()
        else {
            panic!("expected entry command");
        };
        let created_at = Utc.with_ymd_and_hms(2026, 8, 16, 18, 0, 0).unwrap();
        let entry = LedgerEntry::from_command(
            "!room:example",
            "$event",
            "@bot:example",
            command,
            created_at,
        );

        assert_eq!(entry.kind, LedgerKind::Purchase);
        assert_eq!(entry.amount_cents, 1234);
        assert_eq!(entry.description, Some("lunch".to_owned()));
    }
}
