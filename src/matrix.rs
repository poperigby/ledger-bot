use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use chrono::{DateTime, Datelike, Utc};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    authentication::{
        SessionTokens,
        matrix::{MatrixSession, MatrixSession as PersistedMatrixSession},
    },
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, RoomId, UserId,
        api::client::uiaa,
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent},
            message::{
                MessageFormat, MessageType, OriginalSyncRoomMessageEvent, Relation,
                RoomMessageEventContent,
            },
            redaction::OriginalSyncRoomRedactionEvent,
        },
        owned_device_id,
        room_version_rules::RedactionRules,
    },
    store::RoomLoadSettings,
};
use mime::Mime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::{
    config::{Config, MatrixConfig, resolve_optional_secret},
    ledger::{self, Balance, LedgerEntry, PairHistory},
    parser::{BalanceDetail, BotCommand, ParseError, parse_command},
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

    // Without cross-signing, clients flag every message the bot sends as coming
    // from an unverified device. That is a warning, not a failure, so keep running.
    if let Err(error) = bootstrap_cross_signing(&client, &config.matrix).await {
        warn!(?error, "failed to set up cross-signing");
    }

    // Cosmetic, so a failure here should not keep the bot from running.
    if let Err(error) = sync_profile(&client, &config.matrix, &data_dir).await {
        warn!(?error, "failed to update Matrix profile");
    }

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

/// Creates and uploads cross-signing keys if the account has none, which also
/// signs the bot's current device with them. Does nothing if the account
/// already has cross-signing keys.
async fn bootstrap_cross_signing(client: &Client, config: &MatrixConfig) -> anyhow::Result<()> {
    let encryption = client.encryption();

    // Some servers allow the first upload of an account's cross-signing keys
    // without interactive auth (MSC3967).
    let error = match encryption.bootstrap_cross_signing_if_needed(None).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let Some(uiaa) = error.as_uiaa_response() else {
        return Err(error.into());
    };

    let Some(password) = resolve_optional_secret(config.password.as_ref(), "matrix.password")?
    else {
        warn!(
            "could not set up cross-signing: the homeserver requires interactive auth to upload \
             cross-signing keys. Configure matrix.password or LEDGER__MATRIX_PASSWORD for a \
             one-time bootstrap; it can be removed once cross-signing is set up. Until then, \
             clients will show the bot's messages as sent from an unverified device"
        );
        return Ok(());
    };

    let user_id = client
        .user_id()
        .context("client has no user ID after authentication")?;
    let mut auth = uiaa::Password::new(
        uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(user_id.to_string())),
        password,
    );
    auth.session = uiaa.session.clone();

    encryption
        .bootstrap_cross_signing_if_needed(Some(uiaa::AuthData::Password(auth)))
        .await
        .context("failed to upload cross-signing keys with password auth")?;
    info!("set up cross-signing");
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct AvatarState {
    sha256: String,
    mxc_uri: OwnedMxcUri,
}

async fn sync_profile(
    client: &Client,
    config: &MatrixConfig,
    data_dir: &Path,
) -> anyhow::Result<()> {
    let account = client.account();

    if let Some(display_name) = &config.display_name
        && account.get_display_name().await?.as_deref() != Some(display_name.as_str())
    {
        account.set_display_name(Some(display_name)).await?;
        info!(%display_name, "set Matrix display name");
    }

    let Some(avatar_path) = &config.avatar else {
        return Ok(());
    };

    let content_type = avatar_mime(avatar_path)?;
    let data = fs::read(avatar_path)
        .with_context(|| format!("failed to read avatar {}", avatar_path.display()))?;
    let sha256 = format!("{:x}", Sha256::digest(&data));

    // Uploading on every start would pile up copies of the same image in the
    // homeserver's media repo, so only upload when the file changed or the
    // avatar was replaced from somewhere else.
    let current = account.get_avatar_url().await?;
    let state_path = data_dir.join("avatar-state.json");
    let unchanged = fs::read_to_string(&state_path)
        .ok()
        .and_then(|data| serde_json::from_str::<AvatarState>(&data).ok())
        .is_some_and(|state| state.sha256 == sha256 && current.as_ref() == Some(&state.mxc_uri));
    if unchanged {
        return Ok(());
    }

    let mxc_uri = account.upload_avatar(&content_type, data).await?;
    info!(%mxc_uri, path = %avatar_path.display(), "set Matrix avatar");
    fs::write(
        &state_path,
        serde_json::to_string_pretty(&AvatarState { sha256, mxc_uri })?,
    )
    .with_context(|| format!("failed to write {}", state_path.display()))?;
    Ok(())
}

fn avatar_mime(path: &Path) -> anyhow::Result<Mime> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    Ok(match extension.as_deref() {
        Some("png") => mime::IMAGE_PNG,
        Some("jpg" | "jpeg") => mime::IMAGE_JPEG,
        Some("gif") => mime::IMAGE_GIF,
        Some("webp") => "image/webp".parse()?,
        _ => bail!(
            "unsupported avatar file type {}; use .png, .jpg, .jpeg, .gif, or .webp",
            path.display()
        ),
    })
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
            && body.is_some_and(|body| parse_command(&body).is_ok())
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

    let command = match parse_command(&body) {
        Ok(command) => command,
        Err(ParseError::NotACommand) => return Ok(()),
        Err(error) => {
            reply(room, format!("Could not parse ledger command: {error}")).await?;
            return Ok(());
        }
    };

    let command = match command {
        BotCommand::Entry(command) => command,
        BotCommand::Balance { user, detail } => {
            let user = match user {
                Some(raw) => match resolve_user(room, &raw, &state.bot_user_id, false).await? {
                    Ok(user_id) => Some(user_id),
                    Err(message) => {
                        reply(room, message).await?;
                        return Ok(());
                    }
                },
                None => None,
            };
            let user = user.as_ref().map(|user| user.as_str());
            if detail == BalanceDetail::Summary {
                let balances = ledger::balances(&state.pool, room_id.as_str()).await?;
                reply(room, format_balances(&balances, user)).await?;
            } else {
                let entries = ledger::entries(&state.pool, room_id.as_str()).await?;
                let pairs = ledger::histories(entries, detail == BalanceDetail::SinceSettled);
                reply_html(room, format_balance_details(&pairs, user, Utc::now())).await?;
            }
            return Ok(());
        }
        BotCommand::Help => {
            reply_html(room, help_text()).await?;
            return Ok(());
        }
    };

    // Whichever side the command leaves out is the sender.
    let mut parties = Vec::with_capacity(2);
    for raw in [&command.from, &command.to] {
        let user_id = match raw {
            Some(raw) => match resolve_user(room, raw, &state.bot_user_id, true).await? {
                Ok(user_id) => user_id,
                Err(message) => {
                    reply(room, message).await?;
                    return Ok(());
                }
            },
            None => ev.sender.clone(),
        };
        parties.push(user_id);
    }
    let [from, to] = <[OwnedUserId; 2]>::try_from(parties).expect("two parties");
    if from == to {
        reply(
            room,
            format!("Both sides of that entry are {from}. Name two different people."),
        )
        .await?;
        return Ok(());
    }

    let created_at = ev
        .origin_server_ts
        .to_system_time()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);
    let entry_for = |event_id: &str| LedgerEntry {
        room_id: room_id.to_string(),
        matrix_event_id: event_id.to_owned(),
        kind: command.kind,
        actor_mxid: ev.sender.to_string(),
        from_mxid: from.to_string(),
        to_mxid: to.to_string(),
        amount_cents: command.amount_cents,
        description: command.description.clone(),
        created_at,
    };

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

        let entry = entry_for(original_event_id.as_str());
        ledger::upsert_edited_entry(&state.pool, &entry)
            .await?
            .then_some(entry)
    } else {
        let entry = entry_for(ev.event_id.as_str());
        ledger::insert_entry(&state.pool, &entry)
            .await?
            .then_some(entry)
    };

    if let Some(entry) = changed {
        let (a, b) = if entry.from_mxid < entry.to_mxid {
            (&entry.from_mxid, &entry.to_mxid)
        } else {
            (&entry.to_mxid, &entry.from_mxid)
        };
        let a_owes_b_cents = ledger::pair_balance(&state.pool, room_id.as_str(), a, b).await?;
        reply_html(
            room,
            format_recorded(&entry, replaces_event_id.is_some(), a_owes_b_cents),
        )
        .await?;
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

async fn reply_html(room: &Room, (plain, html): (String, String)) -> anyhow::Result<()> {
    room.send(RoomMessageEventContent::notice_html(plain, html))
        .await?;
    Ok(())
}

/// A titled table of commands: title, second column heading, rows.
type HelpSection = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const HELP_SECTIONS: [HelpSection; 2] = [
    (
        "Recording",
        "Meaning",
        &[
            ("!owes @y 12.34 lunch", "you owe y"),
            ("@x !owes 12.34 lunch", "x owes you"),
            ("@x !owes @y 12.34 lunch", "x owes y"),
            ("!paid …", "same forms, for payments"),
        ],
    ),
    (
        "Balances",
        "Shows",
        &[
            ("!balance", "who owes whom"),
            ("!balance @x", "only x's balances"),
            ("!balance details", "entries since last settled up"),
            ("!balance all", "every entry ever"),
        ],
    ),
];

const HELP_FOOTER: &str = "Users can be @user:server, or @user if it matches one person in the \
     room. Edit or delete your message to change or remove an entry.";

/// The plain-text and HTML bodies of `!help`.
fn help_text() -> (String, String) {
    let mut plain = String::new();
    let mut html = String::new();
    for (title, meaning, commands) in HELP_SECTIONS {
        let width = commands
            .iter()
            .map(|(command, _)| command.chars().count())
            .max()
            .unwrap_or(0);
        plain.push_str(title);
        html.push_str(&format!(
            "<p><strong>{title}</strong></p>\
             <table><thead><tr><th>Command</th><th>{meaning}</th></tr></thead><tbody>"
        ));
        for (command, description) in commands {
            plain.push_str(&format!("\n  {command:width$}  {description}"));
            html.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                escape_html(command),
                escape_html(description)
            ));
        }
        plain.push_str("\n\n");
        html.push_str("</tbody></table>");
    }
    plain.push_str(HELP_FOOTER);
    html.push_str(
        "<p>Users can be <code>@user:server</code>, or <code>@user</code> if it matches one \
         person in the room. Edit or delete your message to change or remove an entry.</p>",
    );
    (plain, html)
}

/// Confirms a new or edited entry. `a_owes_b_cents` is the pair's balance
/// afterwards, with `a_mxid` the party that sorts first.
fn format_recorded(entry: &LedgerEntry, edited: bool, a_owes_b_cents: i64) -> (String, String) {
    let (icon, verb) = if edited {
        ("✏️", "Updated: ")
    } else {
        ("✅", "")
    };
    let from = localpart(&entry.from_mxid);
    let to = localpart(&entry.to_mxid);
    let amount = format_cents(entry.amount_cents);
    let kind = entry.kind.as_str();

    let mut plain = format!("{icon} {verb}{from} {kind} {to} {amount}");
    let mut html = format!(
        "{icon} {verb}<strong>{}</strong> {kind} <strong>{}</strong> <strong>{}</strong>",
        escape_html(from),
        escape_html(to),
        escape_html(&amount)
    );
    if let Some(description) = &entry.description {
        plain.push_str(&format!(" — {description}"));
        html.push_str(&format!(" — {}", escape_html(description)));
    }

    let (a, b) = if entry.from_mxid < entry.to_mxid {
        (from, to)
    } else {
        (to, from)
    };
    let balance = match a_owes_b_cents {
        0 => format!("{a} and {b} are now settled up"),
        cents if cents > 0 => format!("{a} now owes {b} {}", format_cents(cents)),
        cents => format!("{b} now owes {a} {}", format_cents(-cents)),
    };
    plain.push('\n');
    plain.push_str(&balance);
    html.push_str(&format!("<br><sub>{}</sub>", escape_html(&balance)));
    (plain, html)
}

/// Lists who owes whom. With `user`, only pairs involving that user are
/// shown, including settled ones.
fn format_balances(balances: &[Balance], user: Option<&str>) -> String {
    let lines = balances
        .iter()
        .filter(|balance| match user {
            Some(user) => balance.a_mxid == user || balance.b_mxid == user,
            None => balance.a_owes_b_cents != 0,
        })
        .map(balance_line)
        .collect::<Vec<_>>();

    let heading = balances_heading(user);
    if lines.is_empty() {
        return format!("{heading}\nNothing owed.");
    }
    format!("{heading}\n{}", lines.join("\n"))
}

fn balances_heading(user: Option<&str>) -> String {
    match user {
        Some(user) => format!("Balances for {user}"),
        None => "Balances".to_owned(),
    }
}

fn balance_line(balance: &Balance) -> String {
    let (debtor, creditor) = if balance.a_owes_b_cents >= 0 {
        (&balance.a_mxid, &balance.b_mxid)
    } else {
        (&balance.b_mxid, &balance.a_mxid)
    };
    if balance.a_owes_b_cents == 0 {
        format!("{debtor} and {creditor} are settled up")
    } else {
        format!(
            "{debtor} owes {creditor} {}",
            format_cents(balance.a_owes_b_cents.abs())
        )
    }
}

/// One table row of `!balance details`.
struct DetailRow {
    date: String,
    entry: String,
    amount: String,
    description: String,
}

/// Lists each pair's balance with a table of the entries behind it. Amounts
/// are signed relative to the pair's net balance, so they add up to the
/// total. Returns the plain-text body and the HTML body.
fn format_balance_details(
    pairs: &[PairHistory],
    user: Option<&str>,
    now: DateTime<Utc>,
) -> (String, String) {
    let pairs: Vec<_> = pairs
        .iter()
        .filter(|pair| match user {
            Some(user) => pair.balance.a_mxid == user || pair.balance.b_mxid == user,
            None => pair.balance.a_owes_b_cents != 0 || !pair.entries.is_empty(),
        })
        .collect();

    let heading = balances_heading(user);
    let mut plain = heading.clone();
    let mut html = format!("<p><strong>{}</strong></p>", escape_html(&heading));
    if pairs.is_empty() {
        plain.push_str("\nNothing owed.");
        html.push_str("<p>Nothing owed.</p>");
        return (plain, html);
    }

    for pair in pairs {
        let line = balance_line(&pair.balance);
        plain.push_str("\n\n");
        plain.push_str(&line);
        html.push_str(&format!("<p><strong>{}</strong></p>", escape_html(&line)));
        if pair.entries.is_empty() {
            continue;
        }

        let direction = if pair.balance.a_owes_b_cents < 0 {
            -1
        } else {
            1
        };
        let rows: Vec<_> = pair
            .entries
            .iter()
            .map(|entry| DetailRow {
                date: format_date(entry.created_at, now),
                entry: format!(
                    "{} {} {}",
                    localpart(&entry.from_mxid),
                    entry.kind.as_str(),
                    localpart(&entry.to_mxid)
                ),
                amount: format_signed_cents(
                    ledger::signed_cents(entry, &pair.balance.a_mxid) * direction,
                ),
                description: entry.description.clone().unwrap_or_default(),
            })
            .collect();
        let total = format_cents(pair.balance.a_owes_b_cents.abs());

        plain.push_str(&plain_detail_table(&rows, &total));
        html.push_str(&html_detail_table(&rows, &total));
    }
    (plain, html)
}

fn plain_detail_table(rows: &[DetailRow], total: &str) -> String {
    let width = |text: &str| text.chars().count();
    let date_width = rows.iter().map(|row| width(&row.date)).max().unwrap_or(0);
    let entry_width = rows.iter().map(|row| width(&row.entry)).max().unwrap_or(0);
    let amount_width = rows
        .iter()
        .map(|row| width(&row.amount))
        .chain([width(total)])
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for row in rows {
        let line = format!(
            "  {:date_width$}  {:entry_width$}  {:>amount_width$}  {}",
            row.date, row.entry, row.amount, row.description
        );
        out.push('\n');
        out.push_str(line.trim_end());
    }
    let label_width = date_width + 2 + entry_width;
    out.push_str(&format!(
        "\n  {}\n  {:label_width$}  {total:>amount_width$}",
        "-".repeat(label_width + 2 + amount_width),
        "Total"
    ));
    out
}

/// Matrix clients strip `tfoot` and `colspan`, so the total is a header-style
/// row in its own body, which clients draw apart from the entries.
fn html_detail_table(rows: &[DetailRow], total: &str) -> String {
    let mut out = String::from(
        "<table><thead><tr><th>Date</th><th>Entry</th><th>Amount</th><th>Description</th></tr></thead><tbody>",
    );
    for row in rows {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&row.date),
            escape_html(&row.entry),
            escape_html(&row.amount),
            escape_html(&row.description)
        ));
    }
    out.push_str(&format!(
        "</tbody><tbody><tr><th>Total</th><th></th><th>{}</th><th></th></tr></tbody></table>",
        escape_html(total)
    ));
    out
}

/// `Nov 14`, with the year added for dates outside the current year.
fn format_date(date: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if date.year() == now.year() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%b %-d, %Y").to_string()
    }
}

fn localpart(mxid: &str) -> &str {
    let mxid = mxid.strip_prefix('@').unwrap_or(mxid);
    mxid.split_once(':')
        .map_or(mxid, |(localpart, _)| localpart)
}

fn format_signed_cents(amount_cents: i64) -> String {
    if amount_cents > 0 {
        format!("+{}", format_cents(amount_cents))
    } else {
        format_cents(amount_cents)
    }
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn format_cents(amount_cents: i64) -> String {
    let sign = if amount_cents < 0 { "-" } else { "" };
    let amount = amount_cents.abs();
    format!("{sign}${}.{:02}", amount / 100, amount % 100)
}

/// Turns a user as typed into a full Matrix ID. A bare `@handle` is looked up
/// among the room's members. A full ID must also belong to a member when
/// `require_member` is set, which catches typos before they become entries.
/// The inner `Err` is a message to show the sender.
async fn resolve_user(
    room: &Room,
    raw: &str,
    bot_user_id: &UserId,
    require_member: bool,
) -> anyhow::Result<Result<OwnedUserId, String>> {
    if raw.contains(':') {
        let Ok(user_id) = UserId::parse(raw) else {
            return Ok(Err(format!("{raw} is not a valid Matrix user ID.")));
        };
        if user_id == bot_user_id {
            return Ok(Err("The bot can't be part of a ledger entry.".to_owned()));
        }
        if require_member
            && room
                .get_member(&user_id)
                .await?
                .is_none_or(|member| !is_active(member.membership()))
        {
            return Ok(Err(format!("{user_id} isn't in this room.")));
        }
        return Ok(Ok(user_id));
    }

    let handle = raw.trim_start_matches('@');
    let members = room.members(RoomMemberships::ACTIVE).await?;
    Ok(match_member(
        handle,
        members
            .iter()
            .filter(|member| member.user_id() != bot_user_id)
            .map(|member| (member.user_id(), member.display_name())),
    )
    .map(ToOwned::to_owned))
}

fn is_active(membership: &MembershipState) -> bool {
    matches!(membership, MembershipState::Join | MembershipState::Invite)
}

/// Matches `handle` against member localparts first, then display names,
/// both case-insensitively. Exactly one match is required.
fn match_member<'a>(
    handle: &str,
    members: impl IntoIterator<Item = (&'a UserId, Option<&'a str>)>,
) -> Result<&'a UserId, String> {
    let members: Vec<_> = members.into_iter().collect();
    let by_localpart: Vec<_> = members
        .iter()
        .filter(|(user_id, _)| user_id.localpart().eq_ignore_ascii_case(handle))
        .map(|(user_id, _)| *user_id)
        .collect();
    let matches = if by_localpart.is_empty() {
        members
            .iter()
            .filter(|(_, name)| name.is_some_and(|name| name.eq_ignore_ascii_case(handle)))
            .map(|(user_id, _)| *user_id)
            .collect()
    } else {
        by_localpart
    };

    match matches.as_slice() {
        [user_id] => Ok(user_id),
        [] => Err(format!(
            "No one in this room matches @{handle}. Use their full ID, like @{handle}:server."
        )),
        several => Err(format!(
            "@{handle} matches more than one person: {}. Use their full ID.",
            several
                .iter()
                .map(|user_id| user_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Returns the text to parse and, for an edit, the event it replaces.
fn message_body_and_replacement(
    content: &RoomMessageEventContent,
) -> (Option<Cow<'_, str>>, Option<&OwnedEventId>) {
    if let Some(Relation::Replacement(replacement)) = &content.relates_to {
        return (
            message_text(&replacement.new_content.msgtype),
            Some(&replacement.event_id),
        );
    }
    (message_text(&content.msgtype), None)
}

/// Uses the plain body, unless the HTML body has mention pills. A pill's plain
/// text is the person's display name, so the HTML is the only place the user
/// ID survives.
fn message_text(msgtype: &MessageType) -> Option<Cow<'_, str>> {
    let (body, formatted) = match msgtype {
        MessageType::Text(text) => (&text.body, text.formatted.as_ref()),
        MessageType::Notice(notice) => (&notice.body, notice.formatted.as_ref()),
        _ => return None,
    };
    match formatted {
        Some(formatted)
            if formatted.format == MessageFormat::Html
                && formatted.body.contains("matrix.to/#/") =>
        {
            Some(Cow::Owned(html_with_mentions_as_ids(&formatted.body)))
        }
        _ => Some(Cow::Borrowed(body)),
    }
}

/// Flattens message HTML to text, writing each user mention pill as the
/// user's Matrix ID and dropping any reply fallback.
fn html_with_mentions_as_ids(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..start]));
        let Some(len) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[start + 1..start + len];
        rest = &rest[start + len + 1..];

        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|ch: char| ch.is_whitespace() || ch == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "mx-reply" if !closing => rest = skip_past(rest, "</mx-reply>"),
            "a" if !closing => {
                if let Some(user_id) = mention_target(tag) {
                    // Pad so a pill touching the next word still splits.
                    out.push_str(&format!(" {user_id} "));
                    rest = skip_past(rest, "</a>");
                }
            }
            "br" | "p" | "div" | "li" => out.push(' '),
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    out
}

fn skip_past<'a>(text: &'a str, needle: &str) -> &'a str {
    match text.to_ascii_lowercase().find(needle) {
        Some(index) => &text[index + needle.len()..],
        None => "",
    }
}

/// The user ID from a `<a href="https://matrix.to/#/@user:server">` tag.
fn mention_target(tag: &str) -> Option<String> {
    let href = tag.find("href=").map(|index| &tag[index + 5..])?;
    let quote = href.chars().next().filter(|ch| *ch == '"' || *ch == '\'')?;
    let href = &href[1..];
    let href = &href[..href.find(quote)?];
    let target = href.strip_prefix("https://matrix.to/#/")?;
    let target = target.split('?').next()?;
    let user_id = percent_decode(target)?;
    UserId::parse(&user_id)
        .ok()
        .map(|user_id| user_id.to_string())
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
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
    fn matches_member_by_localpart_then_display_name() {
        let sh = <&UserId>::try_from("@shwilson2:matrix.org").unwrap();
        let alex = <&UserId>::try_from("@alex:example").unwrap();
        let alex_other = <&UserId>::try_from("@alex:other").unwrap();
        let members = [
            (sh, Some("Shawn")),
            (alex, None),
            (alex_other, Some("Alex O")),
        ];

        assert_eq!(match_member("ShWilson2", members), Ok(sh));
        assert_eq!(match_member("shawn", members), Ok(sh));
        assert!(
            match_member("alex", members)
                .unwrap_err()
                .contains("@alex:example, @alex:other")
        );
        assert!(
            match_member("nobody", members)
                .unwrap_err()
                .starts_with("No one in this room matches @nobody")
        );
    }

    #[test]
    fn picks_avatar_mime_from_extension() {
        assert_eq!(avatar_mime(Path::new("a.PNG")).unwrap(), mime::IMAGE_PNG);
        assert_eq!(avatar_mime(Path::new("a.jpeg")).unwrap(), mime::IMAGE_JPEG);
        assert_eq!(
            avatar_mime(Path::new("a.webp")).unwrap().essence_str(),
            "image/webp"
        );
        assert!(avatar_mime(Path::new("a.bmp")).is_err());
        assert!(avatar_mime(Path::new("avatar")).is_err());
    }

    #[test]
    fn formats_cents() {
        assert_eq!(format_cents(1234), "$12.34");
        assert_eq!(format_cents(-99), "-$0.99");
    }

    fn balance(a: &str, b: &str, a_owes_b_cents: i64) -> Balance {
        Balance {
            a_mxid: a.to_owned(),
            b_mxid: b.to_owned(),
            a_owes_b_cents,
        }
    }

    #[test]
    fn formats_empty_balances() {
        assert_eq!(format_balances(&[], None), "Balances\nNothing owed.");
        assert_eq!(
            format_balances(&[], Some("@alex:example")),
            "Balances for @alex:example\nNothing owed."
        );
    }

    #[test]
    fn formats_balances_in_both_directions() {
        let balances = [
            balance("@alex:example", "@blair:example", 1234),
            balance("@alex:example", "@casey:example", 0),
            balance("@blair:example", "@casey:example", -500),
        ];

        assert_eq!(
            format_balances(&balances, None),
            "Balances\n\
             @alex:example owes @blair:example $12.34\n\
             @casey:example owes @blair:example $5.00"
        );
        assert_eq!(
            format_balances(&balances, Some("@casey:example")),
            "Balances for @casey:example\n\
             @alex:example and @casey:example are settled up\n\
             @casey:example owes @blair:example $5.00"
        );
    }

    fn detail_entry(from: &str, to: &str, amount_cents: i64, description: &str) -> LedgerEntry {
        LedgerEntry {
            room_id: "!room:example".to_owned(),
            matrix_event_id: format!("${description}"),
            kind: crate::parser::LedgerKind::Owes,
            actor_mxid: from.to_owned(),
            from_mxid: from.to_owned(),
            to_mxid: to.to_owned(),
            amount_cents,
            description: Some(description.to_owned()),
            created_at: "2026-11-14T12:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn formats_balance_details() {
        let me = "@poperigby:haddock.cc";
        let them = "@shwilson2:matrix.org";
        let pairs = ledger::histories(
            vec![
                detail_entry(me, them, 2400, "Deodorant"),
                detail_entry(me, them, 600, "Headphones <cable>"),
                detail_entry(them, me, 5000, "Teriyaki"),
            ],
            true,
        );
        let now = "2026-11-16T00:00:00Z".parse().unwrap();
        let (plain, html) = format_balance_details(&pairs, None, now);

        assert_eq!(
            plain,
            "Balances\n\
             \n\
             @shwilson2:matrix.org owes @poperigby:haddock.cc $20.00\n  \
             Nov 14  poperigby owes shwilson2  -$24.00  Deodorant\n  \
             Nov 14  poperigby owes shwilson2   -$6.00  Headphones <cable>\n  \
             Nov 14  shwilson2 owes poperigby  +$50.00  Teriyaki\n  \
             -----------------------------------------\n  \
             Total                              $20.00"
        );
        assert!(html.contains("<td>Headphones &lt;cable&gt;</td>"));
        assert!(html.contains("<th>Total</th><th></th><th>$20.00</th>"));
    }

    #[test]
    fn details_hide_settled_pairs_unless_asked_about() {
        let pairs = ledger::histories(
            vec![
                detail_entry("@a:x", "@b:x", 500, "lunch"),
                detail_entry("@b:x", "@a:x", 500, "dinner"),
            ],
            true,
        );
        let now = Utc::now();
        assert_eq!(
            format_balance_details(&pairs, None, now).0,
            "Balances\nNothing owed."
        );
        assert_eq!(
            format_balance_details(&pairs, Some("@a:x"), now).0,
            "Balances for @a:x\n\n@a:x and @b:x are settled up"
        );
    }

    #[test]
    fn formats_recorded_entry() {
        let entry = LedgerEntry {
            room_id: "!room:example".to_owned(),
            matrix_event_id: "$event".to_owned(),
            kind: crate::parser::LedgerKind::Owes,
            actor_mxid: "@blair:example".to_owned(),
            from_mxid: "@alex:example".to_owned(),
            to_mxid: "@blair:example".to_owned(),
            amount_cents: 1234,
            description: Some("lunch".to_owned()),
            created_at: Utc::now(),
        };

        let (plain, html) = format_recorded(&entry, false, 2600);
        assert_eq!(
            plain,
            "✅ alex owes blair $12.34 — lunch\nalex now owes blair $26.00"
        );
        assert_eq!(
            html,
            "✅ <strong>alex</strong> owes <strong>blair</strong> <strong>$12.34</strong> — lunch\
             <br><sub>alex now owes blair $26.00</sub>"
        );

        let entry = LedgerEntry {
            from_mxid: "@blair:example".to_owned(),
            to_mxid: "@alex:example".to_owned(),
            description: None,
            ..entry
        };
        assert_eq!(
            format_recorded(&entry, true, -500).0,
            "✏️ Updated: blair owes alex $12.34\nblair now owes alex $5.00"
        );
        assert_eq!(
            format_recorded(&entry, true, 0).0,
            "✏️ Updated: blair owes alex $12.34\nalex and blair are now settled up"
        );
    }

    #[test]
    fn formats_help() {
        let (plain, html) = help_text();
        assert!(plain.starts_with("Recording\n  !owes @y 12.34 lunch     you owe y\n"));
        assert!(
            html.contains("<tr><td><code>!balance @x</code></td><td>only x's balances</td></tr>")
        );
    }

    #[test]
    fn replaces_mention_pills_with_user_ids() {
        let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let html = "<mx-reply><blockquote>earlier</blockquote></mx-reply>\
                    <a href=\"https://matrix.to/#/%40shwilson2%3Amatrix.org\">Shawn W</a>\
                    !owes <a href='https://matrix.to/#/@poperigby:matrix.org?via=x'>Cassidy</a> \
                    24 Soap &amp; deodorant";
        assert_eq!(
            normalize(&html_with_mentions_as_ids(html)),
            "@shwilson2:matrix.org !owes @poperigby:matrix.org 24 Soap & deodorant"
        );
        assert_eq!(
            normalize(&html_with_mentions_as_ids(
                "<a href=\"https://example.com\">link</a> text"
            )),
            "link text"
        );
    }
}
