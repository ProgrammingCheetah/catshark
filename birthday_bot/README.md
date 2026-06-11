# Birthday Bot

A Telegram bot that remembers the birthdays of one group chat's members and
celebrates them: a daily greeting at a configurable hour, and an on-demand
list of what's coming up.

Birthdays are month + day only — no year is ever stored. Feb 29 birthdays
are celebrated on Feb 28 in non-leap years.

## Commands

| Command | What it does |
|---|---|
| `/add_birthday MM-DD [@username]` | Save your birthday — or, as a chat admin, someone else's |
| `/remove_birthday [@username]` | Remove your birthday — or, as a chat admin, someone else's |
| `/soon [days]` | Upcoming birthdays in the next `days` days (default 15) |
| `/celebrate` | Post today's birthday wishes now (doesn't affect the daily greeting) |
| `/ping` | Check that the bot is alive |
| `/help` | Show the command list |

Rules of the house:

- Only chat members can have birthdays saved; only admins can act on other
  people. A failed admin check denies (fails closed).
- Removal works for people who already left the chat — that's deliberate, so
  admins can clean up and departed members can remove their own data.
- Members who leave are skipped by listings and greetings automatically.
- `@username` targets are resolved from messages the bot has observed; a
  member the bot has never seen post can't be targeted by username yet
  (text mentions of users without a username work directly).
- Birthday wishes are pinned without a notification, if the bot is an admin
  with the pin right; without it the wishes still post, just unpinned.

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) (`/newbot`) and
   note the token.
2. **Disable privacy mode**: BotFather → `/setprivacy` → your bot →
   `Disable`. Without this (or admin rights in the group), Telegram only
   delivers commands to the bot — it never sees regular messages, so the
   `@username` resolution above silently never learns anyone.
3. Add the bot to your group.
4. Find the group's chat id (negative number for groups). Easiest ways: open
   the group in Telegram Web and read the id from the URL, or use a helper
   bot like @userinfobot on a forwarded message.
5. Configure and run:

```sh
cp .env.example .env   # fill in TELOXIDE_TOKEN and CHAT_ID
cargo run -p telegram_bot
```

## Configuration

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `TELOXIDE_TOKEN` | yes | — | Bot API token from BotFather |
| `CHAT_ID` | yes | — | The group chat the bot serves |
| `BIRTHDAY_HOUR_UTC` | no | `9` | Hour of day (UTC, 0–23) for the daily greeting |
| `DB_PATH` | no | `birthdays.db` | SQLite database file |
| `RUST_LOG` | no | `info` | Log filter; `info` is a clean audit trail (every command received with chat and user id, saves, removals, denials, daily greetings), `debug` adds decisions, unrecognized commands and SQL statements, `trace` adds per-message username observation |

Data lives in a single SQLite file: birthdays, learned usernames, and the
date of the last daily greeting — so a bot that was down at greeting time
catches up when it comes back, and a restart later the same day doesn't
greet twice.

## Run with Docker

```sh
cp .env.example .env   # fill in TELOXIDE_TOKEN and CHAT_ID
docker compose up -d --build
docker compose logs -f
```

Works identically with podman (`podman compose ...`). The database lives on
the `bot-data` volume, so `docker compose down` and image rebuilds keep it;
only deleting the volume loses it. To back it up:

```sh
docker compose cp birthday-bot:/data/birthdays.db ./birthdays-backup.db
```

The container runs as a non-root user, needs no inbound ports (long
polling), and must be the only instance running against the token.

## Architecture

A small hexagonal workspace:

- `crates/domain` — entities, celebration rules (the Feb 29 logic), and the
  storage ports
- `crates/persistence` — SQLite adapter (and an in-memory one for tests)
- `crates/application` — `BirthdayService`: authorization policy and use
  cases, against abstract ports
- `crates/telegram_bot` — the binary: teloxide handlers, message formatting,
  and the daily scheduler

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```
