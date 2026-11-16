# ledger-bot

Matrix ledger bot. Complete slop that shouldn't be used by anyone besides me.

The bot records who owes whom in Matrix rooms. Either side of an entry can be left out, in which case it is the sender:

- `@x !owes @y 12.34 description`: x owes y
- `@x !owes 12.34 description`: x owes the sender
- `!owes @y 12.34 description`: the sender owes y
- `!paid` takes the same forms and records a payment from x to y
- `!balance`: every outstanding balance in the room, netted per pair of people
- `!balance @x`: balances involving x
- `!help`

Users can be full IDs (`@user:server`), bare handles like `@alex` matched against room members by user ID localpart or display name, or mention pills. Full IDs in entries must belong to current room members.

Ledger entries, processed event IDs, edits, redactions, and balances are stored in PostgreSQL. Matrix client state and E2EE state are stored in the Matrix SDK SQLite store under `data_dir/matrix-store`.

## Development

Enter the flake dev shell:

```sh
nix develop
```

Run tests:

```sh
cargo test --locked
```

Build the package:

```sh
nix build .#ledger-bot
```

## Configuration

The default config path is `/etc/ledger-bot/config.toml`. Pass a different path as the first argument:

```sh
ledger-bot ./config.example.toml
```

Secrets must not be inline TOML strings. Use either a secret file table or the standardized environment variable.

Matrix access token:

```toml
[matrix.access_token]
file = "/run/secrets/ledger-bot/matrix-access-token"
```

or:

```sh
LEDGER__MATRIX_ACCESS_TOKEN=...
```

Matrix password:

```toml
[matrix.password]
file = "/run/secrets/ledger-bot/matrix-password"
```

or:

```sh
LEDGER__MATRIX_PASSWORD=...
```

On startup the bot sets up cross-signing for its account if it has none, so clients stop showing its messages as sent from an unverified device. If the homeserver requires interactive auth for that, the bot uses the password, even when an access token is configured. Without a password it logs a warning and keeps running. Once cross-signing is set up, the password can be removed again.

Postgres database URL:

```toml
[postgres.database_url]
file = "/run/secrets/ledger-bot/database-url"
```

or:

```sh
LEDGER__POSTGRES_DATABASE_URL=...
```

Optional Matrix SDK SQLite store passphrase:

```toml
[matrix.store_passphrase]
file = "/run/secrets/ledger-bot/matrix-store-passphrase"
```

or:

```sh
LEDGER__MATRIX_STORE_PASSPHRASE=...
```

Optional bot profile, applied on every startup:

```toml
[matrix]
display_name = "Ledger"
avatar = "/etc/ledger-bot/avatar.png"
```

The avatar can be PNG, JPEG, GIF, or WebP. It is only re-uploaded when the file's contents change (tracked in `data_dir/avatar-state.json`) or when the account's avatar was changed elsewhere.

See `config.example.toml` for a complete example.

## NixOS

Import the module from this flake and enable the service:

```nix
{
  imports = [
    inputs.ledger-bot.nixosModules.default
  ];

  services.ledger-bot = {
    enable = true;
    configFile = "/etc/ledger-bot/config.toml";
    # Must be a string, not a path literal, or Nix copies the secrets into the
    # world-readable store.
    environmentFile = "/run/secrets/ledger-bot/env";
  };
}
```

The environment file can contain:

```sh
LEDGER__MATRIX_ACCESS_TOKEN=...
LEDGER__POSTGRES_DATABASE_URL=...
LEDGER__MATRIX_STORE_PASSPHRASE=...
```

## Backup

Back up all of these:

- PostgreSQL database: ledger source of truth.
- `data_dir/matrix-store`: Matrix SDK state and E2EE SQLite store.
- `data_dir/matrix-session.json`: persisted Matrix session from password login. Not used when an access token is configured.
- Secret files or the secret manager entries that provide the environment variables.

Default `data_dir` is `/var/lib/ledger-bot`.

## Smoke Test

A real smoke test needs a Matrix account, a test room, and a Postgres database.

1. Create the Postgres database and role.
2. Configure `postgres.database_url` through a secret file or `LEDGER__POSTGRES_DATABASE_URL`.
3. Configure Matrix auth through an access token plus `device_id`, or through password login.
4. Allowlist the test room in `allowed_room_ids`.
5. Start the bot.
6. Send `!help`, then `!owes @user:server 1.23 test`, then `!balance`.
7. Edit the `!owes` message twice and confirm the balance reflects only the latest edit.
8. Redact the original `!owes` message and confirm the balance changes again.
