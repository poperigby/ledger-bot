use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use matrix_sdk::{
    Client, Room, RoomState,
    authentication::{
        SessionTokens,
        matrix::{MatrixSession, MatrixSession as PersistedMatrixSession},
    },
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
        events::room::{
            member::StrippedRoomMemberEvent,
            message::{
                MessageType, OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent,
            },
            redaction::OriginalSyncRoomRedactionEvent,
        },
        owned_device_id,
        room_version_rules::RedactionRules,
    },
    store::RoomLoadSettings,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::{
    config::{Config, MatrixConfig, resolve_optional_secret},
    ledger::{self, Balance, LedgerEntry},
    parser::{BotCommand, LedgerKind, parse_command},
};

#[derive(Clone)]
struct BotState {
    pool: PgPool,
    bot_user_id: OwnedUserId,
    allowed_room_ids: Arc<Vec<OwnedRoomId>>,
    allowed_inviter_mxids: Arc<Vec<OwnedUserId>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionFile {
    #[serde(flatten)]
    session: PersistedMatrixSession,
}

pub async fn run(config: Config, pool: PgPool) -> anyhow::Result<()> {
    let data_dir = PathBuf::from(&config.data_dir);
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
    let store_passphrase = resolve_optional_secret(
        config.matrix.store_passphrase.as_ref(),
        "matrix.store_passphrase",
    )?;

    let client = Client::builder()
        .homeserver_url(&config.matrix.homeserver_url)
        .sqlite_store(data_dir.join("matrix-store"), store_passphrase.as_deref())
        .build()
        .await?;

    authenticate(&client, &config.matrix, &data_dir).await?;

    let bot_user_id = client
        .user_id()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| parse_user_id(&config.matrix.user_id).expect("validated during auth"));

    let state = BotState {
        pool,
        bot_user_id: bot_user_id.clone(),
        allowed_room_ids: Arc::new(parse_room_ids(&config.matrix.allowed_room_ids)?),
        allowed_inviter_mxids: Arc::new(parse_user_ids(&config.matrix.allowed_inviter_mxids)?),
    };

    client.add_event_handler_context(state);
    client.add_event_handler(handle_room_message);
    client.add_event_handler(handle_room_redaction);
    client.add_event_handler(handle_invited_room);

    info!(
        homeserver_url = %config.matrix.homeserver_url,
        user_id = %bot_user_id,
        data_dir = %config.data_dir,
        "starting Matrix sync"
    );

    client
        .sync(SyncSettings::default().timeout(Duration::from_secs(30)))
        .await?;
    Ok(())
}

async fn authenticate(
    client: &Client,
    config: &MatrixConfig,
    data_dir: &Path,
) -> anyhow::Result<()> {
    // A configured access token always wins so that rotating it takes effect.
    // The persisted session only exists to avoid a fresh password login (and a
    // new device) on every start.
    if let Some(access_token) =
        resolve_optional_secret(config.access_token.as_ref(), "matrix.access_token")?
    {
        let device_id = config
            .device_id
            .as_deref()
            .context("matrix.device_id is required when matrix.access_token is configured")?;
        let session = MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: parse_user_id(&config.user_id)?,
                device_id: owned_device_id!(device_id),
            },
            tokens: SessionTokens {
                access_token,
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session.clone(), RoomLoadSettings::default())
            .await
            .context("failed to restore access-token Matrix session")?;
        return Ok(());
    }

    if let Some(session) = load_session(data_dir)? {
        client
            .matrix_auth()
            .restore_session(session.session, RoomLoadSettings::default())
            .await
            .context("failed to restore persisted Matrix session")?;
        return Ok(());
    }

    if let Some(password) = resolve_optional_secret(config.password.as_ref(), "matrix.password")? {
        let response = client
            .matrix_auth()
            .login_username(&config.user_id, &password)
            .initial_device_display_name("ledger-bot")
            .send()
            .await
            .context("failed to log in to Matrix with password")?;
        let session = MatrixSession::from(&response);
        save_session(data_dir, &session)?;
        return Ok(());
    }

    bail!(
        "configure either matrix.access_token with matrix.device_id, LEDGER__MATRIX_ACCESS_TOKEN with matrix.device_id, matrix.password, or LEDGER__MATRIX_PASSWORD"
    );
}

async fn handle_room_message(
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
    state: matrix_sdk::event_handler::Ctx<BotState>,
) {
    if let Err(error) = process_room_message(&ev, &room, &state).await {
        error!(?error, "failed to process room message");

        // The sync token has already moved past this event, so it will not be
        // redelivered. Tell the sender instead of silently dropping a command.
        let (body, _) = message_body_and_replacement(&ev.content);
        if state.should_process_room(room.room_id())
            && body.is_some_and(|body| parse_command(body).is_ok())
            && let Err(error) = reply(
                &room,
                "Something went wrong processing that command. Check !balance before sending it again.",
            )
            .await
        {
            error!(?error, "failed to send failure notice");
        }
    }
}

async fn process_room_message(
    ev: &OriginalSyncRoomMessageEvent,
    room: &Room,
    state: &BotState,
) -> anyhow::Result<()> {
    let room_id = room.room_id();
    if !state.should_process_room(room_id) || ev.sender == state.bot_user_id {
        return Ok(());
    }

    let (body, replaces_event_id) = message_body_and_replacement(&ev.content);
    let Some(body) = body else {
        return Ok(());
    };

    let inserted = ledger::mark_processed(
        &state.pool,
        ev.event_id.as_str(),
        room_id.as_str(),
        ev.sender.as_str(),
        Utc::now(),
    )
    .await?;
    if !inserted {
        return Ok(());
    }

    let command = match parse_command(body) {
        Ok(command) => command,
        Err(error) if body.trim_start().starts_with('!') => {
            reply(room, format!("Could not parse ledger command: {error}")).await?;
            return Ok(());
        }
        Err(_) => return Ok(()),
    };

    let command = match command {
        BotCommand::Entry(command) => command,
        BotCommand::Balance { counterparty_mxid } => {
            let balances = ledger::balances(&state.pool, room_id.as_str()).await?;
            reply(
                room,
                format_balances(&balances, counterparty_mxid.as_deref()),
            )
            .await?;
            return Ok(());
        }
        BotCommand::Help => {
            reply(room, help_text()).await?;
            return Ok(());
        }
    };

    let created_at = ev
        .origin_server_ts
        .to_system_time()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);

    let changed = if let Some(original_event_id) = replaces_event_id {
        // Homeservers do not check that an edit comes from the original sender,
        // so the bot has to.
        let original_sender =
            ledger::event_sender(&state.pool, original_event_id.as_str(), room_id.as_str()).await?;
        if original_sender.as_deref() != Some(ev.sender.as_str()) {
            warn!(
                room_id = %room_id,
                edit_event_id = %ev.event_id,
                original_event_id = %original_event_id,
                sender = %ev.sender,
                ?original_sender,
                "ignoring edit of an unknown event or one sent by someone else"
            );
            return Ok(());
        }

        let entry = LedgerEntry::from_command(
            room_id.as_str(),
            original_event_id.as_str(),
            ev.sender.as_str(),
            command,
            created_at,
        );
        ledger::upsert_edited_entry(&state.pool, &entry)
            .await?
            .then_some(entry)
    } else {
        let entry = LedgerEntry::from_command(
            room_id.as_str(),
            ev.event_id.as_str(),
            ev.sender.as_str(),
            command,
            created_at,
        );
        ledger::insert_entry(&state.pool, &entry)
            .await?
            .then_some(entry)
    };

    if let Some(entry) = changed {
        reply(room, format_recorded(&entry)).await?;
    }

    Ok(())
}

async fn handle_room_redaction(
    ev: OriginalSyncRoomRedactionEvent,
    room: Room,
    state: matrix_sdk::event_handler::Ctx<BotState>,
) {
    if let Err(error) = process_room_redaction(ev, room, &state).await {
        error!(?error, "failed to process room redaction");
    }
}

async fn process_room_redaction(
    ev: OriginalSyncRoomRedactionEvent,
    room: Room,
    state: &BotState,
) -> anyhow::Result<()> {
    if !state.should_process_room(room.room_id()) {
        return Ok(());
    }

    let redacted = ev.redacts(&RedactionRules::V11);
    if ledger::redact_entry(&state.pool, redacted.as_str()).await? {
        info!(event_id = %redacted, room_id = %room.room_id(), "marked ledger entry redacted");
    }

    Ok(())
}

async fn handle_invited_room(
    ev: StrippedRoomMemberEvent,
    room: Room,
    state: matrix_sdk::event_handler::Ctx<BotState>,
) {
    if let Err(error) = process_invited_room(ev, room, &state).await {
        error!(?error, "failed to process invited room");
    }
}

async fn process_invited_room(
    ev: StrippedRoomMemberEvent,
    room: Room,
    state: &BotState,
) -> anyhow::Result<()> {
    if room.state() != RoomState::Invited {
        return Ok(());
    }

    if !state.should_join_room(room.room_id()) {
        return Ok(());
    }

    if !state.allowed_inviter_mxids.is_empty()
        && !state
            .allowed_inviter_mxids
            .iter()
            .any(|user_id| user_id == &ev.sender)
    {
        warn!(
            room_id = %room.room_id(),
            inviter = %ev.sender,
            "ignoring invite from non-allowlisted user"
        );
        return Ok(());
    }

    room.join().await?;
    info!(room_id = %room.room_id(), "joined invited room");
    Ok(())
}

async fn reply(room: &Room, text: impl Into<String>) -> anyhow::Result<()> {
    room.send(RoomMessageEventContent::notice_plain(text.into()))
        .await?;
    Ok(())
}

fn help_text() -> &'static str {
    "Commands: !purchase @user:server 12.34 description, !payment @user:server 12.34 description, !balance, !balance @user:server"
}

fn format_recorded(entry: &LedgerEntry) -> String {
    let action = match entry.kind {
        LedgerKind::Purchase => "Purchase",
        LedgerKind::Payment => "Payment",
    };
    let mut message = format!("{action} recorded\n");
    message.push_str(&format!("User: {}\n", entry.counterparty_mxid));
    message.push_str(&format!("Amount: {}", format_cents(entry.amount_cents)));
    if let Some(description) = &entry.description {
        message.push_str("\nDescription: ");
        message.push_str(description);
    }
    message
}

fn format_balances(balances: &[Balance], counterparty_mxid: Option<&str>) -> String {
    let filtered = balances
        .iter()
        .filter(|balance| {
            counterparty_mxid
                .map(|counterparty| balance.counterparty_mxid == counterparty)
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();

    if filtered.is_empty() {
        return match counterparty_mxid {
            Some(counterparty) => {
                format!("Balance for {counterparty}\nNo entries recorded.")
            }
            None => "Balances\nNo entries recorded.".to_owned(),
        };
    }

    let mut message = match counterparty_mxid {
        Some(counterparty) => format!("Balance for {counterparty}"),
        None => "Balances".to_owned(),
    };
    let total = filtered
        .iter()
        .map(|balance| balance.balance_cents)
        .sum::<i64>();

    for balance in filtered {
        message.push('\n');
        message.push_str(&format!(
            "{}  {}  {}",
            balance_direction(balance.balance_cents),
            format_cents(balance.balance_cents),
            balance.counterparty_mxid
        ));
    }

    if counterparty_mxid.is_none() {
        message.push('\n');
        message.push_str(&format!("Total  {}", format_cents(total)));
    }

    message
}

fn format_cents(amount_cents: i64) -> String {
    let sign = if amount_cents < 0 { "-" } else { "" };
    let amount = amount_cents.abs();
    format!("{sign}${}.{:02}", amount / 100, amount % 100)
}

fn balance_direction(amount_cents: i64) -> &'static str {
    match amount_cents.cmp(&0) {
        std::cmp::Ordering::Greater => "Owed",
        std::cmp::Ordering::Less => "Paid",
        std::cmp::Ordering::Equal => "Settled",
    }
}

fn message_body_and_replacement(
    content: &RoomMessageEventContent,
) -> (Option<&str>, Option<&OwnedEventId>) {
    if let Some(Relation::Replacement(replacement)) = &content.relates_to {
        return (
            match &replacement.new_content.msgtype {
                MessageType::Text(text) => Some(text.body.as_str()),
                MessageType::Notice(notice) => Some(notice.body.as_str()),
                _ => None,
            },
            Some(&replacement.event_id),
        );
    }

    match &content.msgtype {
        MessageType::Text(text) => (Some(text.body.as_str()), None),
        MessageType::Notice(notice) => (Some(notice.body.as_str()), None),
        _ => (None, None),
    }
}

fn load_session(data_dir: &Path) -> anyhow::Result<Option<SessionFile>> {
    let path = session_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }

    let data = fs::read_to_string(&path)
        .with_context(|| format!("failed to read Matrix session file {}", path.display()))?;
    Ok(Some(serde_json::from_str(&data)?))
}

fn save_session(data_dir: &Path, session: &MatrixSession) -> anyhow::Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = session_path(data_dir);
    let data = serde_json::to_string_pretty(&SessionFile {
        session: session.clone(),
    })?;
    fs::write(&path, data)
        .with_context(|| format!("failed to write Matrix session file {}", path.display()))?;
    Ok(())
}

fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("matrix-session.json")
}

impl BotState {
    fn should_process_room(&self, room_id: &RoomId) -> bool {
        self.allowed_room_ids.is_empty()
            || self
                .allowed_room_ids
                .iter()
                .any(|allowed| allowed == room_id)
    }

    fn should_join_room(&self, room_id: &RoomId) -> bool {
        self.should_process_room(room_id)
    }
}

fn parse_user_id(value: &str) -> anyhow::Result<OwnedUserId> {
    UserId::parse(value).with_context(|| format!("invalid Matrix user ID: {value}"))
}

fn parse_user_ids(values: &[String]) -> anyhow::Result<Vec<OwnedUserId>> {
    values.iter().map(|value| parse_user_id(value)).collect()
}

fn parse_room_ids(values: &[String]) -> anyhow::Result<Vec<OwnedRoomId>> {
    values
        .iter()
        .map(|value| {
            RoomId::parse(value).with_context(|| format!("invalid Matrix room ID: {value}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_cents() {
        assert_eq!(format_cents(1234), "$12.34");
        assert_eq!(format_cents(-99), "-$0.99");
    }

    #[test]
    fn formats_empty_balances() {
        assert_eq!(format_balances(&[], None), "Balances\nNo entries recorded.");
        assert_eq!(
            format_balances(&[], Some("@alex:example")),
            "Balance for @alex:example\nNo entries recorded."
        );
    }

    #[test]
    fn filters_balances() {
        let balances = vec![
            Balance {
                counterparty_mxid: "@alex:example".to_owned(),
                balance_cents: 1234,
            },
            Balance {
                counterparty_mxid: "@blair:example".to_owned(),
                balance_cents: -500,
            },
        ];

        assert_eq!(
            format_balances(&balances, Some("@blair:example")),
            "Balance for @blair:example\nPaid  -$5.00  @blair:example"
        );
    }

    #[test]
    fn formats_all_balances_with_total() {
        let balances = vec![
            Balance {
                counterparty_mxid: "@alex:example".to_owned(),
                balance_cents: 1234,
            },
            Balance {
                counterparty_mxid: "@blair:example".to_owned(),
                balance_cents: -500,
            },
        ];

        assert_eq!(
            format_balances(&balances, None),
            "Balances\nOwed  $12.34  @alex:example\nPaid  -$5.00  @blair:example\nTotal  $7.34"
        );
    }

    #[test]
    fn formats_recorded_entry() {
        let entry = LedgerEntry {
            room_id: "!room:example".to_owned(),
            matrix_event_id: "$event".to_owned(),
            kind: LedgerKind::Purchase,
            actor_mxid: "@buyer:example".to_owned(),
            counterparty_mxid: "@alex:example".to_owned(),
            amount_cents: 1234,
            description: Some("lunch".to_owned()),
            created_at: Utc::now(),
        };

        assert_eq!(
            format_recorded(&entry),
            "Purchase recorded\nUser: @alex:example\nAmount: $12.34\nDescription: lunch"
        );
    }
}
