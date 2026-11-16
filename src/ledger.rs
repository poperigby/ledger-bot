use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use crate::parser::LedgerKind;

/// One ledger line: `from_mxid` owes, or paid, `to_mxid` the amount.
/// `actor_mxid` is whoever sent the command, which may be neither party.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerEntry {
    pub room_id: String,
    pub matrix_event_id: String,
    pub kind: LedgerKind,
    pub actor_mxid: String,
    pub from_mxid: String,
    pub to_mxid: String,
    pub amount_cents: i64,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Net position between two users, with `a_mxid` sorting before `b_mxid`.
/// Positive means `a` owes `b`; negative means `b` owes `a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    pub a_mxid: String,
    pub b_mxid: String,
    pub a_owes_b_cents: i64,
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
create table if not exists ledger_entries (
    id bigserial primary key,
    room_id text not null,
    matrix_event_id text not null unique,
    kind text not null check (kind in ('owes', 'paid')),
    actor_mxid text not null,
    from_mxid text not null,
    to_mxid text not null,
    amount_cents bigint not null check (amount_cents > 0),
    description text,
    created_at timestamptz not null,
    redacted boolean not null default false,
    check (from_mxid <> to_mxid)
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
    let result = sqlx::query(
        r#"
insert into ledger_entries (
    room_id,
    matrix_event_id,
    kind,
    actor_mxid,
    from_mxid,
    to_mxid,
    amount_cents,
    description,
    created_at
) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
on conflict (matrix_event_id) do nothing
"#,
    )
    .bind(&entry.room_id)
    .bind(&entry.matrix_event_id)
    .bind(entry.kind.as_str())
    .bind(&entry.actor_mxid)
    .bind(&entry.from_mxid)
    .bind(&entry.to_mxid)
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
    from_mxid,
    to_mxid,
    amount_cents,
    description,
    created_at
) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)
on conflict (matrix_event_id) do update
set kind = excluded.kind,
    from_mxid = excluded.from_mxid,
    to_mxid = excluded.to_mxid,
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
    .bind(&entry.from_mxid)
    .bind(&entry.to_mxid)
    .bind(entry.amount_cents)
    .bind(&entry.description)
    .bind(entry.created_at)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// Nets every pair of users in the room. `!owes` moves the debt towards
/// `from` owing `to`; `!paid` moves it back.
pub async fn balances(pool: &PgPool, room_id: &str) -> anyhow::Result<Vec<Balance>> {
    let rows = sqlx::query(
        r#"
select a_mxid,
       b_mxid,
       sum(signed_cents)::bigint as a_owes_b_cents
from (
    select least(from_mxid, to_mxid) as a_mxid,
           greatest(from_mxid, to_mxid) as b_mxid,
           (case kind when 'owes' then amount_cents else -amount_cents end)
             * (case when from_mxid < to_mxid then 1 else -1 end) as signed_cents
    from ledger_entries
    where redacted = false and room_id = $1
) as signed
group by a_mxid, b_mxid
order by a_mxid, b_mxid
"#,
    )
    .bind(room_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(Balance {
                a_mxid: row.try_get("a_mxid")?,
                b_mxid: row.try_get("b_mxid")?,
                a_owes_b_cents: row.try_get("a_owes_b_cents")?,
            })
        })
        .collect()
}
