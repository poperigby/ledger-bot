use std::collections::BTreeMap;

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

/// Net balance between two users in the room. Positive means `a_mxid` owes
/// `b_mxid`.
pub async fn pair_balance(
    pool: &PgPool,
    room_id: &str,
    a_mxid: &str,
    b_mxid: &str,
) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        r#"
select coalesce(sum(
           (case kind when 'owes' then amount_cents else -amount_cents end)
             * (case when from_mxid = $2 then 1 else -1 end)
       ), 0)::bigint
from ledger_entries
where redacted = false
  and room_id = $1
  and ((from_mxid = $2 and to_mxid = $3) or (from_mxid = $3 and to_mxid = $2))
"#,
    )
    .bind(room_id)
    .bind(a_mxid)
    .bind(b_mxid)
    .fetch_one(pool)
    .await?)
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

/// One pair's net balance plus the entries behind it, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairHistory {
    pub balance: Balance,
    pub entries: Vec<LedgerEntry>,
}

/// How much `entry` moves the debt towards `a_mxid` owing the other party.
pub fn signed_cents(entry: &LedgerEntry, a_mxid: &str) -> i64 {
    let amount = match entry.kind {
        LedgerKind::Owes => entry.amount_cents,
        LedgerKind::Paid => -entry.amount_cents,
    };
    if entry.from_mxid == a_mxid {
        amount
    } else {
        -amount
    }
}

/// Every unredacted entry in the room, oldest first.
pub async fn entries(pool: &PgPool, room_id: &str) -> anyhow::Result<Vec<LedgerEntry>> {
    let rows = sqlx::query(
        r#"
select room_id, matrix_event_id, kind, actor_mxid, from_mxid, to_mxid,
       amount_cents, description, created_at
from ledger_entries
where redacted = false and room_id = $1
order by created_at, id
"#,
    )
    .bind(room_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let kind: String = row.try_get("kind")?;
            Ok(LedgerEntry {
                room_id: row.try_get("room_id")?,
                matrix_event_id: row.try_get("matrix_event_id")?,
                kind: match kind.as_str() {
                    "owes" => LedgerKind::Owes,
                    "paid" => LedgerKind::Paid,
                    other => anyhow::bail!("unknown ledger entry kind {other}"),
                },
                actor_mxid: row.try_get("actor_mxid")?,
                from_mxid: row.try_get("from_mxid")?,
                to_mxid: row.try_get("to_mxid")?,
                amount_cents: row.try_get("amount_cents")?,
                description: row.try_get("description")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

/// Groups entries, which must be oldest first, by pair of users. With
/// `since_settled`, each pair keeps only the entries after its balance last
/// reached zero, so a settled pair keeps none.
pub fn histories(entries: Vec<LedgerEntry>, since_settled: bool) -> Vec<PairHistory> {
    let mut pairs: BTreeMap<(String, String), PairHistory> = BTreeMap::new();
    for entry in entries {
        let (a, b) = if entry.from_mxid < entry.to_mxid {
            (&entry.from_mxid, &entry.to_mxid)
        } else {
            (&entry.to_mxid, &entry.from_mxid)
        };
        let pair = pairs
            .entry((a.clone(), b.clone()))
            .or_insert_with(|| PairHistory {
                balance: Balance {
                    a_mxid: a.clone(),
                    b_mxid: b.clone(),
                    a_owes_b_cents: 0,
                },
                entries: Vec::new(),
            });
        pair.balance.a_owes_b_cents += signed_cents(&entry, &pair.balance.a_mxid);
        pair.entries.push(entry);
        if since_settled && pair.balance.a_owes_b_cents == 0 {
            pair.entries.clear();
        }
    }
    pairs.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: LedgerKind, from: &str, to: &str, amount_cents: i64) -> LedgerEntry {
        LedgerEntry {
            room_id: "!room:example".to_owned(),
            matrix_event_id: format!("${from}{to}{amount_cents}"),
            kind,
            actor_mxid: from.to_owned(),
            from_mxid: from.to_owned(),
            to_mxid: to.to_owned(),
            amount_cents,
            description: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn nets_each_pair() {
        let pairs = histories(
            vec![
                entry(LedgerKind::Owes, "@b", "@a", 2400),
                entry(LedgerKind::Owes, "@b", "@a", 600),
                entry(LedgerKind::Owes, "@a", "@b", 5000),
                entry(LedgerKind::Owes, "@a", "@c", 100),
            ],
            true,
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].balance.a_owes_b_cents, 2000);
        assert_eq!(pairs[0].entries.len(), 3);
        assert_eq!(pairs[1].balance.b_mxid, "@c");
    }

    #[test]
    fn drops_entries_before_the_last_settle_up() {
        let entries = vec![
            entry(LedgerKind::Owes, "@a", "@b", 1000),
            entry(LedgerKind::Paid, "@a", "@b", 1000),
            entry(LedgerKind::Owes, "@b", "@a", 300),
        ];

        let pairs = histories(entries.clone(), true);
        assert_eq!(pairs[0].balance.a_owes_b_cents, -300);
        assert_eq!(pairs[0].entries, entries[2..]);

        let pairs = histories(entries.clone(), false);
        assert_eq!(pairs[0].entries, entries);
    }

    #[test]
    fn settled_pair_keeps_no_entries() {
        let pairs = histories(
            vec![
                entry(LedgerKind::Owes, "@a", "@b", 1000),
                entry(LedgerKind::Owes, "@b", "@a", 1000),
            ],
            true,
        );
        assert_eq!(pairs[0].balance.a_owes_b_cents, 0);
        assert!(pairs[0].entries.is_empty());
    }
}
