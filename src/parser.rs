use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerKind {
    Purchase,
    Payment,
}

impl LedgerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Purchase => "purchase",
            Self::Payment => "payment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerCommand {
    pub kind: LedgerKind,
    pub counterparty_mxid: String,
    pub amount_cents: i64,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    Entry(LedgerCommand),
    Balance { counterparty_mxid: Option<String> },
    Help,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("expected !purchase, !payment, !balance, or !help")]
    MissingCommand,
    #[error("expected a Matrix user ID like @user:server")]
    MissingCounterparty,
    #[error("expected an amount like 12.34")]
    MissingAmount,
    #[error("unexpected argument: {0}")]
    UnexpectedArgument(String),
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
}

pub fn parse_command(input: &str) -> Result<BotCommand, ParseError> {
    let mut parts = input.trim().splitn(4, char::is_whitespace);
    let command = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or(ParseError::MissingCommand)?;

    match command {
        "!purchase" | "purchase" => parse_entry(LedgerKind::Purchase, parts),
        "!payment" | "payment" => parse_entry(LedgerKind::Payment, parts),
        "!balance" | "balance" => parse_balance(parts),
        "!help" | "help" => parse_help(parts),
        other if other.starts_with('!') => Err(ParseError::UnknownCommand(other.to_owned())),
        _ => Err(ParseError::MissingCommand),
    }
}

fn parse_entry<'a>(
    kind: LedgerKind,
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<BotCommand, ParseError> {
    let counterparty = parts.next().ok_or(ParseError::MissingCounterparty)?;
    let amount = parts.next().ok_or(ParseError::MissingAmount)?;
    let description = parts
        .next()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned);

    if !counterparty.starts_with('@') || !counterparty.contains(':') {
        return Err(ParseError::MissingCounterparty);
    }

    Ok(BotCommand::Entry(LedgerCommand {
        kind,
        counterparty_mxid: counterparty.to_owned(),
        amount_cents: parse_cents(amount)?,
        description,
    }))
}

fn parse_balance<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<BotCommand, ParseError> {
    let counterparty = parts.next();
    if let Some(extra) = parts.next() {
        return Err(ParseError::UnexpectedArgument(extra.to_owned()));
    }

    if let Some(counterparty) = counterparty {
        if !counterparty.starts_with('@') || !counterparty.contains(':') {
            return Err(ParseError::MissingCounterparty);
        }
        Ok(BotCommand::Balance {
            counterparty_mxid: Some(counterparty.to_owned()),
        })
    } else {
        Ok(BotCommand::Balance {
            counterparty_mxid: None,
        })
    }
}

fn parse_help<'a>(mut parts: impl Iterator<Item = &'a str>) -> Result<BotCommand, ParseError> {
    if let Some(extra) = parts.next() {
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

    #[test]
    fn parses_purchase() {
        let command = parse_command("!purchase @alex:example $12.30 groceries").unwrap();
        assert_eq!(
            command,
            BotCommand::Entry(LedgerCommand {
                kind: LedgerKind::Purchase,
                counterparty_mxid: "@alex:example".to_owned(),
                amount_cents: 1230,
                description: Some("groceries".to_owned()),
            })
        );
    }

    #[test]
    fn parses_payment_without_description() {
        let command = parse_command("payment @alex:example 12").unwrap();
        assert_eq!(
            command,
            BotCommand::Entry(LedgerCommand {
                kind: LedgerKind::Payment,
                counterparty_mxid: "@alex:example".to_owned(),
                amount_cents: 1200,
                description: None,
            })
        );
    }

    #[test]
    fn parses_balance() {
        assert_eq!(
            parse_command("!balance").unwrap(),
            BotCommand::Balance {
                counterparty_mxid: None
            }
        );
        assert_eq!(
            parse_command("balance @alex:example").unwrap(),
            BotCommand::Balance {
                counterparty_mxid: Some("@alex:example".to_owned())
            }
        );
    }

    #[test]
    fn parses_help() {
        assert_eq!(parse_command("!help").unwrap(), BotCommand::Help);
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
