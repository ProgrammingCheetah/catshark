use std::sync::Arc;

use application::service::{
    Added, BirthdayError, BirthdayService, ChatError, ChatMemberInfo, ChatPort,
    DEFAULT_SOON_DAYS, MAX_SOON_DAYS, Removed, Target, UpcomingBirthday, parse_birthday,
};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use persistence::sqlite::SqliteStore;
use teloxide::prelude::*;
use teloxide::types::{MessageEntityKind, ParseMode, UserId};
use teloxide::utils::command::BotCommands;
use teloxide::utils::html;
use teloxide::{ApiError, RequestError};

type Service = Arc<BirthdayService<SqliteStore, TelegramChat>>;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "snake_case", description = "Birthday bot commands:")]
enum Command {
    #[command(description = "add a birthday: /add_birthday MM-DD [@username]")]
    AddBirthday(String),
    #[command(description = "remove a birthday: /remove_birthday [@username]")]
    RemoveBirthday(String),
    #[command(description = "show upcoming birthdays: /soon [days] (default 15)")]
    Soon(String),
    #[command(description = "show this help")]
    Help,
}

/// `ChatPort` adapter for the single configured Telegram chat.
struct TelegramChat {
    bot: Bot,
    chat_id: ChatId,
}

#[async_trait::async_trait]
impl ChatPort for TelegramChat {
    async fn present_member(&self, telegram_id: u64) -> Result<Option<ChatMemberInfo>, ChatError> {
        match self
            .bot
            .get_chat_member(self.chat_id, UserId(telegram_id))
            .await
        {
            Ok(member) => Ok(member.is_present().then(|| ChatMemberInfo {
                telegram_id,
                username: member.user.username.clone(),
                full_name: member.user.full_name(),
            })),
            Err(RequestError::Api(ApiError::UserNotFound)) => Ok(None),
            Err(err) => Err(ChatError(err.to_string())),
        }
    }

    async fn is_admin(&self, telegram_id: u64) -> Result<bool, ChatError> {
        match self
            .bot
            .get_chat_member(self.chat_id, UserId(telegram_id))
            .await
        {
            Ok(member) => Ok(member.is_privileged()),
            Err(RequestError::Api(ApiError::UserNotFound)) => Ok(false),
            Err(err) => Err(ChatError(err.to_string())),
        }
    }
}

#[tokio::main]
async fn main() {
    // Load a .env file if present; real environment variables take precedence.
    dotenvy::dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting birthday bot...");

    // Reads the token from TELOXIDE_TOKEN.
    let bot = Bot::from_env();
    let chat_id = ChatId(
        std::env::var("CHAT_ID")
            .expect("CHAT_ID env variable must be set")
            .parse()
            .expect("CHAT_ID must be a valid integer"),
    );

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "birthdays.db".to_string());
    let store = SqliteStore::open(&db_path)
        .await
        .expect("failed to open the birthday database");

    let service: Service = Arc::new(BirthdayService::new(
        store.clone(),
        TelegramChat {
            bot: bot.clone(),
            chat_id,
        },
    ));

    // Register the command list so Telegram offers "/" autocompletion.
    if let Err(err) = bot.set_my_commands(Command::bot_commands()).await {
        log::warn!("failed to register bot commands: {err}");
    }

    tokio::spawn(birthday_scheduler(
        bot.clone(),
        service.clone(),
        store,
        chat_id,
    ));

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        // Observe every other message so we can resolve @usernames later.
        .branch(Update::filter_message().endpoint(observe_message));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![service])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn record_sender(msg: &Message, service: &Service) {
    if let Some(user) = msg.from.as_ref()
        && let Some(username) = user.username.as_ref()
        && let Err(err) = service.record_username(username, user.id.0).await
    {
        log::warn!("failed to record username for {}: {err}", user.id);
    }
}

async fn observe_message(msg: Message, service: Service) -> ResponseResult<()> {
    record_sender(&msg, &service).await;
    Ok(())
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    service: Service,
) -> ResponseResult<()> {
    record_sender(&msg, &service).await;
    match cmd {
        Command::AddBirthday(args) => handle_add_birthday(bot, msg, args, service).await,
        Command::RemoveBirthday(args) => handle_remove_birthday(bot, msg, args, service).await,
        Command::Soon(args) => handle_soon(bot, msg, args, service).await,
        Command::Help => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await?;
            Ok(())
        }
    }
}

/// Interprets the non-date part of a command as who it acts on. A text
/// mention (users without a username) carries the user directly; otherwise
/// the raw @username is passed on for resolution.
fn parse_target(msg: &Message, target_str: &str) -> Option<Target> {
    if target_str.is_empty() {
        return None;
    }
    let mentioned = msg
        .entities()
        .unwrap_or_default()
        .iter()
        .find_map(|entity| match &entity.kind {
            MessageEntityKind::TextMention { user } => Some(Target::User {
                telegram_id: user.id.0,
                name: user.full_name(),
            }),
            _ => None,
        });
    Some(mentioned.unwrap_or_else(|| Target::Username(target_str.to_string())))
}

async fn handle_add_birthday(
    bot: Bot,
    msg: Message,
    args: String,
    service: Service,
) -> ResponseResult<()> {
    let args = args.trim();
    let (date_str, target_str) = match args.split_once(char::is_whitespace) {
        Some((date, rest)) => (date, rest.trim()),
        None => (args, ""),
    };

    let Some(birthdate) = parse_birthday(date_str) else {
        bot.send_message(
            msg.chat.id,
            "Please use the format: /add_birthday MM-DD [@username]",
        )
        .await?;
        return Ok(());
    };

    let Some(from) = msg.from.as_ref() else {
        return Ok(());
    };

    let target = parse_target(&msg, target_str);

    let reply = match service.add_birthday(from.id.0, target, birthdate).await {
        Ok(Added::ForSelf) => {
            format!("Saved your birthday: {} 🎂", birthdate.format("%d %B"))
        }
        Ok(Added::ForOther { name }) => {
            format!("Saved {name}'s birthday: {} 🎂", birthdate.format("%d %B"))
        }
        Err(BirthdayError::UnknownUsername(raw)) => {
            format!("I don't know who {raw} is yet — they need to send a message in the chat first.")
        }
        Err(BirthdayError::NotInChat(name)) => {
            format!("{name} doesn't seem to be in the chat.")
        }
        Err(BirthdayError::ActorNotInChat) => {
            "You need to be a member of the chat to save your birthday.".to_string()
        }
        Err(BirthdayError::NotAdmin) => "Only chat admins can add birthdays for other people. \
             You can add your own with /add_birthday MM-DD"
            .to_string(),
        Err(err) => {
            log::error!("failed to save birthday: {err}");
            "Something went wrong saving the birthday.".to_string()
        }
    };
    bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

async fn handle_remove_birthday(
    bot: Bot,
    msg: Message,
    args: String,
    service: Service,
) -> ResponseResult<()> {
    let Some(from) = msg.from.as_ref() else {
        return Ok(());
    };
    let target = parse_target(&msg, args.trim());

    let reply = match service.remove_birthday(from.id.0, target).await {
        Ok(Removed::ForSelf { existed: true }) => "Removed your birthday.".to_string(),
        Ok(Removed::ForSelf { existed: false }) => "You don't have a birthday saved.".to_string(),
        Ok(Removed::ForOther { name, existed: true }) => format!("Removed {name}'s birthday."),
        Ok(Removed::ForOther { name, existed: false }) => {
            format!("{name} doesn't have a birthday saved.")
        }
        Err(BirthdayError::UnknownUsername(raw)) => {
            format!("I don't know who {raw} is yet — they need to send a message in the chat first.")
        }
        Err(BirthdayError::NotAdmin) => "Only chat admins can remove birthdays for other people. \
             You can remove your own with /remove_birthday"
            .to_string(),
        Err(err) => {
            log::error!("failed to remove birthday: {err}");
            "Something went wrong removing the birthday.".to_string()
        }
    };
    bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

async fn handle_soon(bot: Bot, msg: Message, args: String, service: Service) -> ResponseResult<()> {
    let args = args.trim();
    let days = if args.is_empty() {
        DEFAULT_SOON_DAYS
    } else {
        match args.parse::<u32>() {
            Ok(days) => days.min(MAX_SOON_DAYS),
            Err(_) => {
                bot.send_message(msg.chat.id, "Please use the format: /soon [days]")
                    .await?;
                return Ok(());
            }
        }
    };

    let today = Utc::now().date_naive();
    let upcoming = match service.upcoming_birthdays(today, days).await {
        Ok(upcoming) => upcoming,
        Err(err) => {
            log::error!("failed to look up upcoming birthdays: {err}");
            bot.send_message(msg.chat.id, "Something went wrong looking up birthdays.")
                .await?;
            return Ok(());
        }
    };

    bot.send_message(msg.chat.id, soon_message(today, days, &upcoming))
        .await?;
    Ok(())
}

/// The `/soon` reply. Plain names on purpose: a list shouldn't ping everyone
/// in it.
fn soon_message(today: NaiveDate, days: u32, upcoming: &[UpcomingBirthday]) -> String {
    if upcoming.is_empty() {
        return format!("No birthdays in the next {days} days.");
    }
    let lines: Vec<String> = upcoming
        .iter()
        .map(|entry| {
            let when = match (entry.date - today).num_days() {
                0 => "today! 🎉".to_string(),
                1 => "tomorrow".to_string(),
                n => format!("in {n} days"),
            };
            format!(
                "🎂 {} — {} ({when})",
                entry.date.format("%d %B"),
                entry.member.full_name
            )
        })
        .collect();
    format!("Birthdays in the next {days} days:\n{}", lines.join("\n"))
}

/// Runs forever, posting birthday wishes once a day at `BIRTHDAY_HOUR_UTC`
/// (default 9:00 UTC). The last announcement date is persisted, so a bot that
/// was down when the hour struck catches up as soon as it comes back instead
/// of skipping the day, and a restart later the same day does not repeat it.
async fn birthday_scheduler(bot: Bot, service: Service, store: SqliteStore, chat_id: ChatId) {
    let hour = std::env::var("BIRTHDAY_HOUR_UTC")
        .ok()
        .and_then(|hour| hour.parse().ok())
        .unwrap_or(9);
    let target_time = NaiveTime::from_hms_opt(hour, 0, 0).expect("BIRTHDAY_HOUR_UTC must be 0-23");

    loop {
        let now = Utc::now();

        let announced = match store.last_announced().await {
            Ok(date) => date,
            Err(err) => {
                log::warn!("failed to read the last announcement date: {err}");
                None
            }
        };
        if announcement_due(now, target_time, announced) {
            match post_birthday_wishes(&bot, &service, chat_id).await {
                Ok(()) => {
                    if let Err(err) = store.set_last_announced(now.date_naive()).await {
                        log::warn!("failed to record the announcement date: {err}");
                    }
                }
                Err(err) => log::error!("failed to post birthday wishes: {err}"),
            }
        }

        // Always sleep to the next boundary; the check above decides whether
        // anything is due when we wake.
        let next_run = next_wakeup(now, target_time);
        log::info!("next birthday check at {next_run}");
        tokio::time::sleep((next_run - now).to_std().unwrap_or_default()).await;
    }
}

/// Today's strike of the announcement hour.
fn todays_due_time(now: DateTime<Utc>, target_time: NaiveTime) -> DateTime<Utc> {
    now.date_naive().and_time(target_time).and_utc()
}

/// Whether today's announcement is still owed: the hour has struck and the
/// last completed announcement was on an earlier day (or never).
fn announcement_due(
    now: DateTime<Utc>,
    target_time: NaiveTime,
    last_announced: Option<NaiveDate>,
) -> bool {
    now >= todays_due_time(now, target_time)
        && last_announced.is_none_or(|date| date < now.date_naive())
}

/// When to wake next: today's announcement hour if it is still ahead,
/// otherwise tomorrow's.
fn next_wakeup(now: DateTime<Utc>, target_time: NaiveTime) -> DateTime<Utc> {
    let due = todays_due_time(now, target_time);
    if now < due {
        due
    } else {
        due + chrono::Duration::days(1)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn member(id: u64, username: Option<&str>, name: &str) -> ChatMemberInfo {
        ChatMemberInfo {
            telegram_id: id,
            username: username.map(str::to_string),
            full_name: name.to_string(),
        }
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
    }

    fn nine() -> NaiveTime {
        NaiveTime::from_hms_opt(9, 0, 0).unwrap()
    }

    #[test]
    fn announcement_waits_for_the_hour() {
        assert!(!announcement_due(at(2026, 6, 6, 8), nine(), None));
        assert!(announcement_due(at(2026, 6, 6, 9), nine(), None));
    }

    #[test]
    fn announcement_catches_up_after_downtime() {
        // Down at 9:00, back at 15:00: still owed, whether the last run was
        // yesterday or never.
        assert!(announcement_due(
            at(2026, 6, 6, 15),
            nine(),
            Some(date(2026, 6, 5))
        ));
        assert!(announcement_due(at(2026, 6, 6, 15), nine(), None));
    }

    #[test]
    fn restart_after_announcing_does_not_repeat() {
        assert!(!announcement_due(
            at(2026, 6, 6, 15),
            nine(),
            Some(date(2026, 6, 6))
        ));
    }

    #[test]
    fn next_wakeup_is_todays_hour_or_tomorrows() {
        assert_eq!(next_wakeup(at(2026, 6, 6, 8), nine()), at(2026, 6, 6, 9));
        // At or past the hour, the next wake-up is tomorrow; the due check
        // for today already ran this iteration.
        assert_eq!(next_wakeup(at(2026, 6, 6, 9), nine()), at(2026, 6, 7, 9));
        assert_eq!(next_wakeup(at(2026, 6, 6, 23), nine()), at(2026, 6, 7, 9));
    }

    #[test]
    fn birthday_message_mentions_by_username_when_available() {
        let message = birthday_message(&[member(1, Some("alice"), "Alice")]);
        assert_eq!(message, "🎉 Happy birthday, @alice! 🎂🎈");
    }

    #[test]
    fn birthday_message_text_mentions_and_escapes_usernameless_members() {
        let message = birthday_message(&[member(2, None, "Bobby <3")]);
        assert!(message.contains("tg://user"), "{message}");
        assert!(message.contains("id=2"), "{message}");
        // The message is sent with HTML parse mode; names must be escaped.
        assert!(message.contains("Bobby &lt;3"), "{message}");
    }

    #[test]
    fn birthday_message_lists_all_celebrants() {
        let message = birthday_message(&[
            member(1, Some("alice"), "Alice"),
            member(2, Some("bob"), "Bob"),
        ]);
        assert_eq!(message, "🎉 Happy birthday, @alice, @bob! 🎂🎈");
    }

    #[test]
    fn soon_message_reports_an_empty_horizon() {
        assert_eq!(
            soon_message(date(2026, 6, 6), 15, &[]),
            "No birthdays in the next 15 days."
        );
    }

    #[test]
    fn soon_message_words_each_distance_naturally() {
        let today = date(2026, 6, 6);
        let upcoming = [
            UpcomingBirthday {
                date: today,
                member: member(1, Some("alice"), "Alice"),
            },
            UpcomingBirthday {
                date: date(2026, 6, 7),
                member: member(2, Some("bob"), "Bob"),
            },
            UpcomingBirthday {
                date: date(2026, 6, 11),
                member: member(3, None, "Carol"),
            },
        ];

        let message = soon_message(today, 15, &upcoming);

        assert_eq!(
            message,
            "Birthdays in the next 15 days:\n\
             🎂 06 June — Alice (today! 🎉)\n\
             🎂 07 June — Bob (tomorrow)\n\
             🎂 11 June — Carol (in 5 days)"
        );
    }
}

async fn post_birthday_wishes(
    bot: &Bot,
    service: &Service,
    chat_id: ChatId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let today = Utc::now().date_naive();
    let celebrants = service.todays_celebrants(today).await?;

    if celebrants.is_empty() {
        log::info!("no birthdays to celebrate today");
        return Ok(());
    }

    bot.send_message(chat_id, birthday_message(&celebrants))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// The daily greeting, pinging every celebrant: an @mention for members with
/// a username, an HTML text mention for the rest (hence HTML parse mode).
fn birthday_message(celebrants: &[ChatMemberInfo]) -> String {
    let mentions: Vec<String> = celebrants
        .iter()
        .map(|member| match &member.username {
            Some(username) => format!("@{username}"),
            None => html::user_mention(UserId(member.telegram_id), &member.full_name),
        })
        .collect();
    format!("🎉 Happy birthday, {}! 🎂🎈", mentions.join(", "))
}
