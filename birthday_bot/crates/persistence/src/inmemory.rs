use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{NaiveDate, Utc};
use domain::elements::{User, UserId, UserRepository, UsernameDirectory};

#[derive(Default)]
pub struct InMemoryUserRepository {
    users: RwLock<HashMap<UserId, User>>,
    usernames: RwLock<HashMap<String, u64>>,
}

impl InMemoryUserRepository {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl UserRepository for InMemoryUserRepository {
    async fn find_birthdays_for_date(
        &self,
        date: NaiveDate,
    ) -> Result<Vec<User>, domain::elements::RepositoryError> {
        let users = self.users.read().unwrap();
        let users = users
            .values()
            .filter(|user| user.celebrates_on(date))
            .cloned()
            .collect();
        Ok(users)
    }

    async fn find_birthdays_within(
        &self,
        from: NaiveDate,
        days: u32,
    ) -> Result<Vec<(NaiveDate, User)>, domain::elements::RepositoryError> {
        let users = self.users.read().unwrap();
        let mut upcoming: Vec<(NaiveDate, User)> = users
            .values()
            .filter_map(|user| {
                (0..=u64::from(days)).find_map(|offset| {
                    let date = from.checked_add_days(chrono::Days::new(offset))?;
                    user.celebrates_on(date).then(|| (date, user.clone()))
                })
            })
            .collect();
        upcoming.sort_by_key(|(date, _)| *date);
        Ok(upcoming)
    }

    async fn add_birthday(
        &self,
        telegram_id: u64,
        birthdate: NaiveDate,
    ) -> Result<(), domain::elements::RepositoryError> {
        let mut users = self.users.write().unwrap();
        match users
            .values_mut()
            .find(|user| user.telegram_id == telegram_id)
        {
            Some(user) => {
                user.birthdate = birthdate;
                user.updated_at = Utc::now();
            }
            None => {
                let key: UserId = uuid::Uuid::now_v7().into();
                users.insert(key, User::new(key, telegram_id, birthdate));
            }
        }
        Ok(())
    }

    async fn remove_birthday(
        &self,
        telegram_id: u64,
    ) -> Result<bool, domain::elements::RepositoryError> {
        let mut users = self.users.write().unwrap();
        let key = users
            .iter()
            .find(|(_, user)| user.telegram_id == telegram_id)
            .map(|(key, _)| *key);
        Ok(key.is_some_and(|key| users.remove(&key).is_some()))
    }
}

#[async_trait::async_trait]
impl UsernameDirectory for InMemoryUserRepository {
    async fn record_username(
        &self,
        username: &str,
        telegram_id: u64,
    ) -> Result<(), domain::elements::RepositoryError> {
        self.usernames
            .write()
            .unwrap()
            .insert(username.to_string(), telegram_id);
        Ok(())
    }

    async fn resolve_username(
        &self,
        username: &str,
    ) -> Result<Option<u64>, domain::elements::RepositoryError> {
        Ok(self.usernames.read().unwrap().get(username).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[tokio::test]
    async fn finds_birthdays_only_on_their_day() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(1, date(2000, 6, 6)).await.unwrap();

        let found = repo.find_birthdays_for_date(date(2026, 6, 6)).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].telegram_id, 1);

        assert!(repo.find_birthdays_for_date(date(2026, 6, 7)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn adding_again_updates_instead_of_duplicating() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(1, date(2000, 6, 6)).await.unwrap();
        repo.add_birthday(1, date(2000, 7, 7)).await.unwrap();

        assert!(repo.find_birthdays_for_date(date(2026, 6, 6)).await.unwrap().is_empty());
        assert_eq!(repo.find_birthdays_for_date(date(2026, 7, 7)).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn within_sorts_soonest_first_and_wraps_the_year() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(1, date(2000, 1, 2)).await.unwrap();
        repo.add_birthday(2, date(2000, 12, 30)).await.unwrap();

        let found = repo
            .find_birthdays_within(date(2026, 12, 28), 10)
            .await
            .unwrap();

        let summary: Vec<(NaiveDate, u64)> = found
            .iter()
            .map(|(date, user)| (*date, user.telegram_id))
            .collect();
        assert_eq!(
            summary,
            vec![(date(2026, 12, 30), 2), (date(2027, 1, 2), 1)]
        );
    }

    #[tokio::test]
    async fn within_includes_both_horizon_bounds() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(1, date(2000, 6, 6)).await.unwrap();
        repo.add_birthday(2, date(2000, 6, 16)).await.unwrap();
        repo.add_birthday(3, date(2000, 6, 17)).await.unwrap();

        let found = repo.find_birthdays_within(date(2026, 6, 6), 10).await.unwrap();

        let ids: Vec<u64> = found.iter().map(|(_, user)| user.telegram_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[tokio::test]
    async fn remove_deletes_the_birthday_and_reports_whether_it_existed() {
        let repo = InMemoryUserRepository::new();
        repo.add_birthday(1, date(2000, 6, 6)).await.unwrap();

        assert!(repo.remove_birthday(1).await.unwrap());
        assert!(repo.find_birthdays_for_date(date(2026, 6, 6)).await.unwrap().is_empty());

        assert!(!repo.remove_birthday(1).await.unwrap());
    }
}
