use chrono::NaiveDate;
use domain::elements::{RepositoryError, UserRepository, UsernameDirectory};

/// Birthdays carry no real year; they are stored anchored to this leap year
/// so Feb 29 is representable.
pub const ANCHOR_YEAR: i32 = 2000;

/// Default horizon for upcoming-birthday lookups, in days.
pub const DEFAULT_SOON_DAYS: u32 = 15;

/// Upper bound on the upcoming-birthday horizon.
pub const MAX_SOON_DAYS: u32 = 366;

/// Parses a month-day birthday ("MM-DD") into its `ANCHOR_YEAR`-anchored date.
pub fn parse_birthday(input: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&format!("{ANCHOR_YEAR}-{}", input.trim()), "%Y-%m-%d").ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMemberInfo {
    pub telegram_id: u64,
    pub username: Option<String>,
    pub full_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("chat API error: {0}")]
pub struct ChatError(pub String);

/// The single chat this bot serves, as seen from the application layer.
#[async_trait::async_trait]
pub trait ChatPort: Send + Sync {
    /// Member info if the user is currently present in the chat.
    async fn present_member(&self, telegram_id: u64) -> Result<Option<ChatMemberInfo>, ChatError>;
    /// Whether the user is the chat owner or an administrator.
    async fn is_admin(&self, telegram_id: u64) -> Result<bool, ChatError>;
}

/// Who a birthday is being added for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Already resolved to a user (e.g. a Telegram text mention).
    User { telegram_id: u64, name: String },
    /// A plain @username that still needs resolving via observed messages.
    Username(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Added {
    ForSelf,
    ForOther { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    ForSelf { existed: bool },
    ForOther { name: String, existed: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BirthdayError {
    #[error("unknown username {0}")]
    UnknownUsername(String),
    #[error("{0} is not in the chat")]
    NotInChat(String),
    #[error("the actor is not in the chat")]
    ActorNotInChat,
    #[error("only admins may manage birthdays for others")]
    NotAdmin,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Chat(#[from] ChatError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpcomingBirthday {
    pub date: NaiveDate,
    pub member: ChatMemberInfo,
}

pub struct BirthdayService<R, C> {
    repo: R,
    chat: C,
}

impl<R, C> BirthdayService<R, C>
where
    R: UserRepository + UsernameDirectory + Send + Sync,
    C: ChatPort,
{
    pub fn new(repo: R, chat: C) -> Self {
        Self { repo, chat }
    }

    /// Remembers which Telegram ID a username belongs to.
    pub async fn record_username(
        &self,
        username: &str,
        telegram_id: u64,
    ) -> Result<(), RepositoryError> {
        self.repo
            .record_username(&username.to_lowercase(), telegram_id)
            .await
    }

    /// Resolves a target to a Telegram ID and display name, enforcing that
    /// only chat admins may act on someone other than themselves. A failed
    /// admin lookup fails closed.
    async fn resolve_target(
        &self,
        actor_id: u64,
        target: Target,
    ) -> Result<(u64, String), BirthdayError> {
        let (telegram_id, name) = match target {
            Target::User { telegram_id, name } => (telegram_id, name),
            Target::Username(raw) => {
                let username = raw.trim_start_matches('@').to_lowercase();
                let id = self
                    .repo
                    .resolve_username(&username)
                    .await?
                    .ok_or(BirthdayError::UnknownUsername(raw))?;
                (id, format!("@{username}"))
            }
        };

        if telegram_id != actor_id && !self.chat.is_admin(actor_id).await.unwrap_or(false) {
            return Err(BirthdayError::NotAdmin);
        }
        Ok((telegram_id, name))
    }

    /// Adds a birthday for the actor themselves (no target) or, if the actor
    /// is a chat admin, for another chat member. Either way the person whose
    /// birthday it is must be in the chat — strangers messaging the bot
    /// directly should not end up in the database.
    pub async fn add_birthday(
        &self,
        actor_id: u64,
        target: Option<Target>,
        birthdate: NaiveDate,
    ) -> Result<Added, BirthdayError> {
        let Some(target) = target else {
            if self.chat.present_member(actor_id).await?.is_none() {
                return Err(BirthdayError::ActorNotInChat);
            }
            self.repo.add_birthday(actor_id, birthdate).await?;
            return Ok(Added::ForSelf);
        };

        let (telegram_id, name) = self.resolve_target(actor_id, target).await?;

        if self.chat.present_member(telegram_id).await?.is_none() {
            return Err(BirthdayError::NotInChat(name));
        }

        self.repo.add_birthday(telegram_id, birthdate).await?;
        Ok(if telegram_id == actor_id {
            Added::ForSelf
        } else {
            Added::ForOther { name }
        })
    }

    /// Removes the actor's own birthday (no target) or, if the actor is a
    /// chat admin, someone else's. Unlike adding, the target does not have to
    /// be in the chat: cleaning up after departed members is the point.
    pub async fn remove_birthday(
        &self,
        actor_id: u64,
        target: Option<Target>,
    ) -> Result<Removed, BirthdayError> {
        let Some(target) = target else {
            let existed = self.repo.remove_birthday(actor_id).await?;
            return Ok(Removed::ForSelf { existed });
        };

        let (telegram_id, name) = self.resolve_target(actor_id, target).await?;

        let existed = self.repo.remove_birthday(telegram_id).await?;
        Ok(if telegram_id == actor_id {
            Removed::ForSelf { existed }
        } else {
            Removed::ForOther { name, existed }
        })
    }

    /// Chat members with a birthday within `from..=from + days`, soonest
    /// first. Users who are no longer in the chat are skipped.
    pub async fn upcoming_birthdays(
        &self,
        from: NaiveDate,
        days: u32,
    ) -> Result<Vec<UpcomingBirthday>, RepositoryError> {
        let upcoming = self
            .repo
            .find_birthdays_within(from, days.min(MAX_SOON_DAYS))
            .await?;
        let mut result = Vec::new();
        for (date, user) in upcoming {
            if let Some(member) = self.member_or_skip(user.telegram_id).await {
                result.push(UpcomingBirthday { date, member });
            }
        }
        Ok(result)
    }

    /// Chat members whose birthday is celebrated today. Users who are no
    /// longer in the chat are skipped.
    pub async fn todays_celebrants(
        &self,
        today: NaiveDate,
    ) -> Result<Vec<ChatMemberInfo>, RepositoryError> {
        let users = self.repo.find_birthdays_for_date(today).await?;
        let mut result = Vec::new();
        for user in users {
            if let Some(member) = self.member_or_skip(user.telegram_id).await {
                result.push(member);
            }
        }
        Ok(result)
    }

    async fn member_or_skip(&self, telegram_id: u64) -> Option<ChatMemberInfo> {
        match self.chat.present_member(telegram_id).await {
            Ok(Some(member)) => Some(member),
            Ok(None) => {
                log::info!("user {telegram_id} is no longer in the chat, skipping");
                None
            }
            Err(err) => {
                log::warn!("chat lookup failed for {telegram_id}, skipping: {err}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use persistence::inmemory::InMemoryUserRepository;

    use super::*;

    const ALICE: u64 = 1;
    const BOB: u64 = 2;

    #[derive(Default)]
    struct FakeChat {
        members: HashMap<u64, ChatMemberInfo>,
        admins: HashSet<u64>,
        /// When set, every chat API call fails.
        broken: bool,
    }

    impl FakeChat {
        fn with_member(mut self, id: u64, username: Option<&str>, name: &str) -> Self {
            self.members.insert(
                id,
                ChatMemberInfo {
                    telegram_id: id,
                    username: username.map(str::to_string),
                    full_name: name.to_string(),
                },
            );
            self
        }

        fn with_admin(mut self, id: u64) -> Self {
            self.admins.insert(id);
            self
        }

        fn broken(mut self) -> Self {
            self.broken = true;
            self
        }

        fn check(&self) -> Result<(), ChatError> {
            if self.broken {
                Err(ChatError("chat API down".to_string()))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl ChatPort for FakeChat {
        async fn present_member(
            &self,
            telegram_id: u64,
        ) -> Result<Option<ChatMemberInfo>, ChatError> {
            self.check()?;
            Ok(self.members.get(&telegram_id).cloned())
        }

        async fn is_admin(&self, telegram_id: u64) -> Result<bool, ChatError> {
            self.check()?;
            Ok(self.admins.contains(&telegram_id))
        }
    }

    fn service(chat: FakeChat) -> BirthdayService<InMemoryUserRepository, FakeChat> {
        BirthdayService::new(InMemoryUserRepository::new(), chat)
    }

    fn alice_and_bob() -> FakeChat {
        FakeChat::default()
            .with_member(ALICE, Some("alice"), "Alice")
            .with_member(BOB, Some("bob"), "Bob")
    }

    fn birthday(month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(ANCHOR_YEAR, month, day).unwrap()
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn parse_birthday_accepts_month_day_only() {
        assert_eq!(parse_birthday("06-06"), Some(birthday(6, 6)));
        assert_eq!(parse_birthday("6-6"), Some(birthday(6, 6)));
        assert_eq!(parse_birthday(" 12-31 "), Some(birthday(12, 31)));
        assert_eq!(parse_birthday("02-29"), Some(birthday(2, 29)));
        assert_eq!(parse_birthday("13-01"), None);
        assert_eq!(parse_birthday("02-30"), None);
        assert_eq!(parse_birthday("2001-06-06"), None);
        assert_eq!(parse_birthday("birthday"), None);
        assert_eq!(parse_birthday(""), None);
    }

    #[tokio::test]
    async fn member_adds_their_own_birthday() {
        let service = service(alice_and_bob());

        let added = service.add_birthday(ALICE, None, birthday(6, 6)).await.unwrap();

        assert_eq!(added, Added::ForSelf);
        let celebrants = service.todays_celebrants(date(2026, 6, 6)).await.unwrap();
        assert_eq!(celebrants.len(), 1);
        assert_eq!(celebrants[0].telegram_id, ALICE);
    }

    #[tokio::test]
    async fn non_member_cannot_add_their_own_birthday() {
        const CAROL: u64 = 3;
        let service = service(alice_and_bob());

        let result = service.add_birthday(CAROL, None, birthday(6, 6)).await;

        assert_eq!(result, Err(BirthdayError::ActorNotInChat));
    }

    #[tokio::test]
    async fn member_cannot_add_for_someone_else() {
        let service = service(alice_and_bob());
        service.record_username("bob", BOB).await.unwrap();

        let result = service
            .add_birthday(ALICE, Some(Target::Username("@bob".into())), birthday(6, 6))
            .await;

        assert_eq!(result, Err(BirthdayError::NotAdmin));
    }

    #[tokio::test]
    async fn admin_adds_for_someone_else() {
        let service = service(alice_and_bob().with_admin(ALICE));
        service.record_username("bob", BOB).await.unwrap();

        let added = service
            .add_birthday(ALICE, Some(Target::Username("@bob".into())), birthday(6, 6))
            .await
            .unwrap();

        assert_eq!(added, Added::ForOther { name: "@bob".into() });
        let celebrants = service.todays_celebrants(date(2026, 6, 6)).await.unwrap();
        assert_eq!(celebrants[0].telegram_id, BOB);
    }

    #[tokio::test]
    async fn targeting_yourself_needs_no_admin() {
        let service = service(alice_and_bob());
        service.record_username("alice", ALICE).await.unwrap();

        let added = service
            .add_birthday(ALICE, Some(Target::Username("@alice".into())), birthday(6, 6))
            .await
            .unwrap();

        assert_eq!(added, Added::ForSelf);
    }

    #[tokio::test]
    async fn unknown_username_is_rejected() {
        let service = service(alice_and_bob().with_admin(ALICE));

        let result = service
            .add_birthday(ALICE, Some(Target::Username("@bob".into())), birthday(6, 6))
            .await;

        assert_eq!(result, Err(BirthdayError::UnknownUsername("@bob".into())));
    }

    #[tokio::test]
    async fn username_resolution_is_case_insensitive() {
        let service = service(alice_and_bob().with_admin(ALICE));
        service.record_username("Bob", BOB).await.unwrap();

        let added = service
            .add_birthday(ALICE, Some(Target::Username("@BOB".into())), birthday(6, 6))
            .await
            .unwrap();

        assert_eq!(added, Added::ForOther { name: "@bob".into() });
    }

    #[tokio::test]
    async fn target_must_be_in_the_chat() {
        const CAROL: u64 = 3;
        let service = service(alice_and_bob().with_admin(ALICE));
        service.record_username("carol", CAROL).await.unwrap();

        let result = service
            .add_birthday(ALICE, Some(Target::Username("@carol".into())), birthday(6, 6))
            .await;

        assert_eq!(result, Err(BirthdayError::NotInChat("@carol".into())));
    }

    #[tokio::test]
    async fn text_mention_target_needs_no_username_cache() {
        let service = service(alice_and_bob().with_admin(ALICE));

        let target = Target::User { telegram_id: BOB, name: "Bob".into() };
        let added = service
            .add_birthday(ALICE, Some(target), birthday(6, 6))
            .await
            .unwrap();

        assert_eq!(added, Added::ForOther { name: "Bob".into() });
    }

    #[tokio::test]
    async fn adding_again_overwrites_the_previous_birthday() {
        let service = service(alice_and_bob());

        service.add_birthday(ALICE, None, birthday(6, 6)).await.unwrap();
        service.add_birthday(ALICE, None, birthday(7, 7)).await.unwrap();

        assert!(service.todays_celebrants(date(2026, 6, 6)).await.unwrap().is_empty());
        assert_eq!(service.todays_celebrants(date(2026, 7, 7)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn member_removes_their_own_birthday() {
        let service = service(alice_and_bob());
        service.add_birthday(ALICE, None, birthday(6, 6)).await.unwrap();

        let removed = service.remove_birthday(ALICE, None).await.unwrap();

        assert_eq!(removed, Removed::ForSelf { existed: true });
        assert!(service.todays_celebrants(date(2026, 6, 6)).await.unwrap().is_empty());

        // Removing again reports there was nothing to remove.
        let removed = service.remove_birthday(ALICE, None).await.unwrap();
        assert_eq!(removed, Removed::ForSelf { existed: false });
    }

    #[tokio::test]
    async fn member_cannot_remove_for_someone_else() {
        let service = service(alice_and_bob());
        service.record_username("bob", BOB).await.unwrap();
        service.add_birthday(BOB, None, birthday(6, 6)).await.unwrap();

        let result = service
            .remove_birthday(ALICE, Some(Target::Username("@bob".into())))
            .await;

        assert_eq!(result, Err(BirthdayError::NotAdmin));
    }

    #[tokio::test]
    async fn admin_removes_for_a_member_who_left() {
        const CAROL: u64 = 3;
        // Carol saved a birthday, then left the chat.
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(CAROL, birthday(6, 6)).await.unwrap();
        let service = BirthdayService::new(repo, alice_and_bob().with_admin(ALICE));
        service.record_username("carol", CAROL).await.unwrap();

        let removed = service
            .remove_birthday(ALICE, Some(Target::Username("@carol".into())))
            .await
            .unwrap();

        assert_eq!(
            removed,
            Removed::ForOther { name: "@carol".into(), existed: true }
        );
    }

    #[tokio::test]
    async fn upcoming_is_sorted_and_skips_users_who_left() {
        const CAROL: u64 = 3;
        // Carol saved a birthday while she was in the chat, then left.
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(CAROL, birthday(6, 9)).await.unwrap();
        let service = BirthdayService::new(repo, alice_and_bob());
        service.add_birthday(ALICE, None, birthday(6, 10)).await.unwrap();
        service.add_birthday(BOB, None, birthday(6, 8)).await.unwrap();

        let upcoming = service.upcoming_birthdays(date(2026, 6, 6), 15).await.unwrap();

        let ids: Vec<u64> = upcoming.iter().map(|entry| entry.member.telegram_id).collect();
        assert_eq!(ids, vec![BOB, ALICE]);
        assert_eq!(upcoming[0].date, date(2026, 6, 8));
        assert_eq!(upcoming[1].date, date(2026, 6, 10));
    }

    #[tokio::test]
    async fn failed_admin_lookup_fails_closed() {
        // Alice IS an admin, but the chat API cannot confirm it.
        let service = service(alice_and_bob().with_admin(ALICE).broken());
        service.record_username("bob", BOB).await.unwrap();

        let result = service
            .add_birthday(ALICE, Some(Target::Username("@bob".into())), birthday(6, 6))
            .await;

        assert_eq!(result, Err(BirthdayError::NotAdmin));
    }

    #[tokio::test]
    async fn chat_errors_skip_users_in_listings_instead_of_failing() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(ALICE, birthday(6, 6)).await.unwrap();
        let service = BirthdayService::new(repo, alice_and_bob().broken());

        let celebrants = service.todays_celebrants(date(2026, 6, 6)).await.unwrap();
        assert!(celebrants.is_empty());

        let upcoming = service.upcoming_birthdays(date(2026, 6, 1), 15).await.unwrap();
        assert!(upcoming.is_empty());
    }

    #[tokio::test]
    async fn huge_horizons_list_each_member_once_with_the_soonest_date() {
        let service = service(alice_and_bob());
        service.add_birthday(ALICE, None, birthday(6, 6)).await.unwrap();

        // Far beyond MAX_SOON_DAYS; a yearly birthday recurs many times in
        // 10,000 days but must appear once, at its next occurrence.
        let upcoming = service
            .upcoming_birthdays(date(2026, 6, 7), 10_000)
            .await
            .unwrap();

        assert_eq!(upcoming.len(), 1);
        assert_eq!(upcoming[0].date, date(2027, 6, 6));
    }

    #[tokio::test]
    async fn upcoming_includes_today_and_the_horizon_edge() {
        let service = service(alice_and_bob());
        service.add_birthday(ALICE, None, birthday(6, 6)).await.unwrap();
        service.add_birthday(BOB, None, birthday(6, 21)).await.unwrap();

        let upcoming = service.upcoming_birthdays(date(2026, 6, 6), 15).await.unwrap();
        assert_eq!(upcoming.len(), 2);

        let upcoming = service.upcoming_birthdays(date(2026, 6, 6), 14).await.unwrap();
        assert_eq!(upcoming.len(), 1);
    }

    #[tokio::test]
    async fn feb_29_is_celebrated_on_feb_28_in_non_leap_years() {
        let service = service(alice_and_bob());
        service.add_birthday(ALICE, None, birthday(2, 29)).await.unwrap();

        let celebrants = service.todays_celebrants(date(2025, 2, 28)).await.unwrap();
        assert_eq!(celebrants.len(), 1);

        let upcoming = service.upcoming_birthdays(date(2025, 2, 20), 15).await.unwrap();
        assert_eq!(upcoming[0].date, date(2025, 2, 28));

        // ...and on the real date in leap years.
        let upcoming = service.upcoming_birthdays(date(2028, 2, 20), 15).await.unwrap();
        assert_eq!(upcoming[0].date, date(2028, 2, 29));
    }
}
