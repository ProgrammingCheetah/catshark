use std::sync::Arc;

use application::service::{
    Added, BirthdayError, BirthdayService, ChatError, ChatMemberInfo, ChatPort,
    DEFAULT_SOON_DAYS, MAX_SOON_DAYS, Removed, Target, parse_birthday,
};
use chrono::{NaiveTime, Utc};
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
    let store = SqliteStore::open(&db_path).expect("failed to open the birthday database");

    let service: Service = Arc::new(BirthdayService::new(
        store,
        TelegramChat {
            bot: bot.clone(),
            chat_id,
        },
    ));

    // Register the command list so Telegram offers "/" autocompletion.
    if let Err(err) = bot.set_my_commands(Command::bot_commands()).await {
        log::warn!("failed to register bot commands: {err}");
    }

    tokio::spawn(birthday_scheduler(bot.clone(), service.clone(), chat_id));

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

    let lines: Vec<String> = upcoming
        .iter()
        .map(|entry| {
            let when = match (entry.date - today).num_days() {
                0 => "today! 🎉".to_string(),
                1 => "tomorrow".to_string(),
                n => format!("in {n} days"),
            };
            // Plain names on purpose: a list shouldn't ping everyone in it.
            format!(
                "🎂 {} — {} ({when})",
                entry.date.format("%d %B"),
                entry.member.full_name
            )
        })
        .collect();

    let reply = if lines.is_empty() {
        format!("No birthdays in the next {days} days.")
    } else {
        format!("Birthdays in the next {days} days:\n{}", lines.join("\n"))
    };
    bot.send_message(msg.chat.id, reply).await?;
    Ok(())
}

/// Runs forever, posting birthday wishes once a day at `BIRTHDAY_HOUR_UTC` (default 9:00 UTC).
async fn birthday_scheduler(bot: Bot, service: Service, chat_id: ChatId) {
    let hour = std::env::var("BIRTHDAY_HOUR_UTC")
        .ok()
        .and_then(|hour| hour.parse().ok())
        .unwrap_or(9);
    let target_time = NaiveTime::from_hms_opt(hour, 0, 0).expect("BIRTHDAY_HOUR_UTC must be 0-23");

    loop {
        let now = Utc::now();
        let mut next_run = now.date_naive().and_time(target_time).and_utc();
        if next_run <= now {
            next_run += chrono::Duration::days(1);
        }
        log::info!("next birthday check at {next_run}");
        tokio::time::sleep((next_run - now).to_std().unwrap_or_default()).await;

        if let Err(err) = post_birthday_wishes(&bot, &service, chat_id).await {
            log::error!("failed to post birthday wishes: {err}");
        }
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

    let mentions: Vec<String> = celebrants
        .iter()
        .map(|member| match &member.username {
            Some(username) => format!("@{username}"),
            None => html::user_mention(UserId(member.telegram_id), &member.full_name),
        })
        .collect();

    bot.send_message(
        chat_id,
        format!("🎉 Happy birthday, {}! 🎂🎈", mentions.join(", ")),
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(())
}
