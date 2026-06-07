use chrono::{DateTime, Datelike, NaiveDate, Utc};
use uuid::Uuid;

#[derive(Debug, Copy, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct UserId(Uuid);

impl From<Uuid> for UserId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct TelegramId(u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct User {
    pub id: UserId,
    pub telegram_id: u64,
    /// Only the month and day are significant; the year is an arbitrary anchor
    /// (a leap year, so Feb 29 is representable).
    pub birthdate: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    /// Whether this user's birthday is celebrated on `date`.
    /// Feb 29 birthdays are celebrated on Feb 28 in non-leap years.
    pub fn celebrates_on(&self, date: NaiveDate) -> bool {
        let (birth_month, birth_day) = (self.birthdate.month(), self.birthdate.day());
        if (birth_month, birth_day) == (date.month(), date.day()) {
            return true;
        }
        let is_leap_year = NaiveDate::from_ymd_opt(date.year(), 2, 29).is_some();
        (birth_month, birth_day) == (2, 29) && (date.month(), date.day()) == (2, 28) && !is_leap_year
    }

    pub fn new(id: UserId, telegram_id: u64, birthdate: NaiveDate) -> Self {
        let current_date = Utc::now();
        User {
            id,
            telegram_id,
            birthdate,
            created_at: current_date,
            updated_at: current_date,
        }
    }
}

#[async_trait::async_trait]
pub trait UserRepository {
    async fn find_birthdays_for_date(&self, date: NaiveDate) -> Result<Vec<User>, RepositoryError>;
    /// Users whose birthday falls within `from..=from + days`, paired with the
    /// date it falls on, sorted by soonest first.
    async fn find_birthdays_within(
        &self,
        from: NaiveDate,
        days: u32,
    ) -> Result<Vec<(NaiveDate, User)>, RepositoryError>;
    async fn add_birthday(
        &self,
        telegram_id: u64,
        birthday: NaiveDate,
    ) -> Result<(), RepositoryError>;
    /// Removes the stored birthday for `telegram_id`. Returns whether one existed.
    async fn remove_birthday(&self, telegram_id: u64) -> Result<bool, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_born_on(month: u32, day: u32) -> User {
        let birthdate = NaiveDate::from_ymd_opt(2000, month, day).unwrap();
        User::new(Uuid::now_v7().into(), 1, birthdate)
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn regular_birthday_matches_month_and_day_in_any_year() {
        let user = user_born_on(6, 6);
        assert!(user.celebrates_on(date(2026, 6, 6)));
        assert!(user.celebrates_on(date(2024, 6, 6)));
        assert!(!user.celebrates_on(date(2026, 6, 7)));
        assert!(!user.celebrates_on(date(2026, 7, 6)));
    }

    #[test]
    fn feb_29_birthday_matches_feb_29_in_leap_years() {
        let user = user_born_on(2, 29);
        assert!(user.celebrates_on(date(2024, 2, 29)));
        // Not on Feb 28 when Feb 29 exists that year.
        assert!(!user.celebrates_on(date(2024, 2, 28)));
    }

    #[test]
    fn feb_29_birthday_falls_back_to_feb_28_in_non_leap_years() {
        let user = user_born_on(2, 29);
        assert!(user.celebrates_on(date(2025, 2, 28)));
        assert!(!user.celebrates_on(date(2025, 3, 1)));
        // 2100 is divisible by 4 but not a leap year.
        assert!(user.celebrates_on(date(2100, 2, 28)));
    }

    #[test]
    fn feb_28_birthday_is_unaffected_by_leap_rules() {
        let user = user_born_on(2, 28);
        assert!(user.celebrates_on(date(2024, 2, 28)));
        assert!(user.celebrates_on(date(2025, 2, 28)));
        assert!(!user.celebrates_on(date(2024, 2, 29)));
    }
}
