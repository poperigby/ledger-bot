use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerKind {
    /// `from` owes `to` the amount.
    Owes,
    /// `from` paid `to` the amount.
    Paid,
}

impl LedgerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owes => "owes",
            Self::Paid => "paid",
        }
    }
}

/// `<from> !owes|!paid <to> <amount> [description]`. At least one of `from`
/// and `to` is present; the missing side is the message's sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerCommand {
    pub kind: LedgerKind,
    pub from: Option<String>,
    pub to: Option<String>,
    pub amount_cents: i64,
    pub description: Option<String>,
}

/// How much of each pair's history `!balance` lists under the totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceDetail {
    /// Totals only.
    Summary,
    /// Entries since the pair was last settled up.
    SinceSettled,
    /// Every entry ever recorded.
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    Entry(LedgerCommand),
    Balance {
        user: Option<String>,
        detail: BalanceDetail,
    },
    Help,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The message is ordinary chat, not addressed to the bot.
    #[error("not a ledger command")]
    NotACommand,
    #[error("name at least one person, like `@user !owes 12.34` or `!owes @user 12.34`")]
    MissingParty,
    #[error("expected a user like @user or @user:server, got {0}")]
    InvalidUser(String),
    #[error("expected an amount like 12.34")]
    MissingAmount,
    #[error("unexpected argument: {0}")]
    UnexpectedArgument(String),
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
}

const ENTRY_COMMANDS: [&str; 2] = ["!owes", "!paid"];

pub fn parse_command(input: &str) -> Result<BotCommand, ParseError> {
    let (first, rest) = next_token(input).ok_or(ParseError::NotACommand)?;

    // `@user !owes ...` puts a person before the command. Anything else that
    // starts with a mention is chat, so stay quiet unless the next word is an
    // entry command.
    let (from, command, rest) = if first.starts_with('@') {
        match next_token(rest) {
            Some((command, rest)) if ENTRY_COMMANDS.contains(&command) => {
                (Some(parse_user(first)?), command, rest)
            }
            _ => return Err(ParseError::NotACommand),
        }
    } else {
        (None, first, rest)
    };

    match command {
        "!owes" => parse_entry(LedgerKind::Owes, from, rest),
        "!paid" => parse_entry(LedgerKind::Paid, from, rest),
        "!balance" => parse_balance(rest),
        "!help" => parse_help(rest),
        other if other.starts_with('!') => Err(ParseError::UnknownCommand(other.to_owned())),
        _ => Err(ParseError::NotACommand),
    }
}

/// Splits off the first whitespace-delimited token, skipping any run of
/// leading whitespace so repeated spaces never produce empty tokens.
fn next_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Some((&input[..end], &input[end..]))
}

/// A user is either a full Matrix ID (`@user:server`) or a bare handle
/// (`@user`) that the caller resolves against the room's members.
fn parse_user(token: &str) -> Result<String, ParseError> {
    match token.strip_prefix('@') {
        Some(rest) if !rest.is_empty() && !rest.starts_with(':') && !rest.ends_with(':') => {
            Ok(token.to_owned())
        }
        _ => Err(ParseError::InvalidUser(token.to_owned())),
    }
}

fn parse_entry(
    kind: LedgerKind,
    from: Option<String>,
    rest: &str,
) -> Result<BotCommand, ParseError> {
    let (to, rest) = match next_token(rest) {
        Some((token, rest)) if token.starts_with('@') => (Some(parse_user(token)?), rest),
        _ => (None, rest),
    };
    if from.is_none() && to.is_none() {
        return Err(ParseError::MissingParty);
    }

    let (amount, rest) = next_token(rest).ok_or(ParseError::MissingAmount)?;
    let description = Some(rest.trim())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned);

    Ok(BotCommand::Entry(LedgerCommand {
        kind,
        from,
        to,
        amount_cents: parse_cents(amount)?,
        description,
    }))
}

/// `!balance [@user] [details|all]`, with the arguments in either order.
fn parse_balance(mut rest: &str) -> Result<BotCommand, ParseError> {
    let mut user = None;
    let mut detail = None;
    while let Some((token, after)) = next_token(rest) {
        rest = after;
        match token {
            "details" if detail.is_none() => detail = Some(BalanceDetail::SinceSettled),
            "all" if detail.is_none() => detail = Some(BalanceDetail::All),
            "details" | "all" => return Err(ParseError::UnexpectedArgument(token.to_owned())),
            _ if user.is_none() => user = Some(parse_user(token)?),
            _ => return Err(ParseError::UnexpectedArgument(token.to_owned())),
        }
    }
    Ok(BotCommand::Balance {
        user,
        detail: detail.unwrap_or(BalanceDetail::Summary),
    })
}

fn parse_help(rest: &str) -> Result<BotCommand, ParseError> {
    if let Some((extra, _)) = next_token(rest) {
        return Err(ParseError::UnexpectedArgument(extra.to_owned()));
    }
    Ok(BotCommand::Help)
}

pub fn parse_cents(input: &str) -> Result<i64, ParseError> {
    let normalized = input.trim().trim_start_matches('$');
    if normalized.is_empty() || normalized.starts_with('-') {
        return Err(ParseError::InvalidAmount(input.to_owned()));
    }

    let (dollars, cents) = match normalized.split_once('.') {
        Some((dollars, cents)) => (dollars, cents),
        None => (normalized, "0"),
    };

    if dollars.is_empty()
        || !dollars.chars().all(|ch| ch.is_ascii_digit())
        || cents.len() > 2
        || !cents.chars().all(|ch| ch.is_ascii_digit())
    {
        return Err(ParseError::InvalidAmount(input.to_owned()));
    }

    let dollars: i64 = dollars
        .parse()
        .map_err(|_| ParseError::InvalidAmount(input.to_owned()))?;
    let cents = match cents.len() {
        0 => 0,
        1 => cents.parse::<i64>().unwrap() * 10,
        2 => cents.parse::<i64>().unwrap(),
        _ => unreachable!(),
    };
    let amount = dollars
        .checked_mul(100)
        .and_then(|value| value.checked_add(cents))
        .ok_or_else(|| ParseError::InvalidAmount(input.to_owned()))?;

    if amount == 0 {
        return Err(ParseError::InvalidAmount(input.to_owned()));
    }

    Ok(amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(input: &str) -> LedgerCommand {
        match parse_command(input).unwrap() {
            BotCommand::Entry(command) => command,
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn parses_both_parties() {
        assert_eq!(
            entry("@alex:example !owes @blair 12.30 groceries"),
            LedgerCommand {
                kind: LedgerKind::Owes,
                from: Some("@alex:example".to_owned()),
                to: Some("@blair".to_owned()),
                amount_cents: 1230,
                description: Some("groceries".to_owned()),
            }
        );
    }

    #[test]
    fn missing_side_is_left_for_the_sender() {
        let command = entry("@shwilson2 !owes 24 Deodorant pack");
        assert_eq!(command.from.as_deref(), Some("@shwilson2"));
        assert_eq!(command.to, None);

        let command = entry("!paid @shwilson2 $24");
        assert_eq!(command.kind, LedgerKind::Paid);
        assert_eq!(command.from, None);
        assert_eq!(command.to.as_deref(), Some("@shwilson2"));
        assert_eq!(command.description, None);
    }

    #[test]
    fn requires_a_party() {
        assert_eq!(parse_command("!owes 24"), Err(ParseError::MissingParty));
    }

    #[test]
    fn tolerates_repeated_whitespace() {
        assert_eq!(
            entry("  @alex   !owes  @blair:example   24  Deodorant pack "),
            LedgerCommand {
                kind: LedgerKind::Owes,
                from: Some("@alex".to_owned()),
                to: Some("@blair:example".to_owned()),
                amount_cents: 2400,
                description: Some("Deodorant pack".to_owned()),
            }
        );
    }

    #[test]
    fn ignores_chat_that_starts_with_a_mention() {
        assert_eq!(
            parse_command("@alex want lunch?"),
            Err(ParseError::NotACommand)
        );
        assert_eq!(parse_command("@alex !!!"), Err(ParseError::NotACommand));
        assert_eq!(
            parse_command("@alex !balance"),
            Err(ParseError::NotACommand)
        );
        assert_eq!(parse_command("owes @alex 5"), Err(ParseError::NotACommand));
    }

    #[test]
    fn rejects_bad_users() {
        assert_eq!(
            parse_command("!owes @ 24"),
            Err(ParseError::InvalidUser("@".to_owned()))
        );
        assert_eq!(
            parse_command("@alex: !owes 24"),
            Err(ParseError::InvalidUser("@alex:".to_owned()))
        );
        assert_eq!(
            parse_command("!balance alex"),
            Err(ParseError::InvalidUser("alex".to_owned()))
        );
    }

    #[test]
    fn parses_balance() {
        assert_eq!(
            parse_command("!balance").unwrap(),
            BotCommand::Balance {
                user: None,
                detail: BalanceDetail::Summary,
            }
        );
        assert_eq!(
            parse_command("!balance @alex").unwrap(),
            BotCommand::Balance {
                user: Some("@alex".to_owned()),
                detail: BalanceDetail::Summary,
            }
        );
        assert_eq!(
            parse_command("!balance details").unwrap(),
            BotCommand::Balance {
                user: None,
                detail: BalanceDetail::SinceSettled,
            }
        );
        assert_eq!(
            parse_command("!balance all @alex").unwrap(),
            BotCommand::Balance {
                user: Some("@alex".to_owned()),
                detail: BalanceDetail::All,
            }
        );
        assert_eq!(
            parse_command("!balance @alex details").unwrap(),
            BotCommand::Balance {
                user: Some("@alex".to_owned()),
                detail: BalanceDetail::SinceSettled,
            }
        );
    }

    #[test]
    fn rejects_repeated_balance_arguments() {
        assert_eq!(
            parse_command("!balance details all"),
            Err(ParseError::UnexpectedArgument("all".to_owned()))
        );
        assert_eq!(
            parse_command("!balance @alex @blair"),
            Err(ParseError::UnexpectedArgument("@blair".to_owned()))
        );
    }

    #[test]
    fn parses_help() {
        assert_eq!(parse_command("!help").unwrap(), BotCommand::Help);
    }

    #[test]
    fn rejects_unknown_commands() {
        assert_eq!(
            parse_command("!purchase @alex 5"),
            Err(ParseError::UnknownCommand("!purchase".to_owned()))
        );
    }

    #[test]
    fn rejects_fractional_cents() {
        assert_eq!(
            parse_cents("1.234"),
            Err(ParseError::InvalidAmount("1.234".to_owned()))
        );
    }

    #[test]
    fn rejects_zero() {
        assert_eq!(
            parse_cents("0"),
            Err(ParseError::InvalidAmount("0".to_owned()))
        );
    }
}
