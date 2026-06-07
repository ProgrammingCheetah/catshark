use chrono::{NaiveDate, Utc};
use domain::elements::{RepositoryError, User, UserRepository, UsernameDirectory};
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        telegram_id INTEGER NOT NULL UNIQUE,
        birthdate TEXT NOT NULL,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS usernames (
        username TEXT PRIMARY KEY,
        telegram_id INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS bot_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );";

/// SQLite-backed store for birthdays and the username directory. Cheap to
/// clone; clones share one connection pool.
#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn open(path: &str) -> Result<Self, RepositoryError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(storage_error)?;
        Self::from_pool(pool).await
    }

    /// An ephemeral store, for tests. The pool is pinned to a single
    /// never-recycled connection: every fresh connection to an in-memory
    /// database would be a new, empty database.
    pub async fn open_in_memory() -> Result<Self, RepositoryError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .map_err(storage_error)?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self, RepositoryError> {
        sqlx::raw_sql(SCHEMA)
            .execute(&pool)
            .await
            .map_err(storage_error)?;
        Ok(Self { pool })
    }

    /// The date the daily birthday announcement last completed (it counts
    /// even when nobody celebrates), used to catch up after downtime.
    pub async fn last_announced(&self) -> Result<Option<NaiveDate>, RepositoryError> {
        sqlx::query("SELECT value FROM bot_state WHERE key = 'last_announced'")
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_error)?
            .map(|row| row.try_get(0))
            .transpose()
            .map_err(storage_error)
    }

    pub async fn set_last_announced(&self, date: NaiveDate) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO bot_state (key, value) VALUES ('last_announced', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(date)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Celebration rules (the Feb 29 fallback) live in the domain, so date
    /// filtering happens in Rust; a single chat's user table stays tiny.
    async fn all_users(&self) -> Result<Vec<User>, RepositoryError> {
        sqlx::query(
            "SELECT id, telegram_id, birthdate, created_at, updated_at
             FROM users ORDER BY telegram_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage_error)?
        .iter()
        .map(row_to_user)
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)
    }
}

fn storage_error(err: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Storage(err.to_string())
}

fn row_to_user(row: &SqliteRow) -> Result<User, sqlx::Error> {
    let id: String = row.try_get("id")?;
    let id = uuid::Uuid::parse_str(&id).map_err(|err| sqlx::Error::ColumnDecode {
        index: "id".into(),
        source: Box::new(err),
    })?;
    let telegram_id: i64 = row.try_get("telegram_id")?;
    Ok(User {
        id: id.into(),
        telegram_id: telegram_id as u64,
        birthdate: row.try_get("birthdate")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[async_trait::async_trait]
impl UserRepository for SqliteStore {
    async fn find_birthdays_for_date(&self, date: NaiveDate) -> Result<Vec<User>, RepositoryError> {
        let users = self.all_users().await?;
        Ok(users
            .into_iter()
            .filter(|user| user.celebrates_on(date))
            .collect())
    }

    async fn find_birthdays_within(
        &self,
        from: NaiveDate,
        days: u32,
    ) -> Result<Vec<(NaiveDate, User)>, RepositoryError> {
        let users = self.all_users().await?;
        let mut upcoming: Vec<(NaiveDate, User)> = users
            .into_iter()
            .filter_map(|user| {
                user.celebration_within(from, days)
                    .map(|date| (date, user))
            })
            .collect();
        upcoming.sort_by_key(|(date, _)| *date);
        Ok(upcoming)
    }

    async fn add_birthday(
        &self,
        telegram_id: u64,
        birthdate: NaiveDate,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO users (id, telegram_id, birthdate, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(telegram_id) DO UPDATE SET
                 birthdate = excluded.birthdate,
                 updated_at = excluded.updated_at",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(telegram_id as i64)
        .bind(birthdate)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    async fn remove_birthday(&self, telegram_id: u64) -> Result<bool, RepositoryError> {
        let result = sqlx::query("DELETE FROM users WHERE telegram_id = ?1")
            .bind(telegram_id as i64)
            .execute(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(result.rows_affected() > 0)
    }
}

#[async_trait::async_trait]
impl UsernameDirectory for SqliteStore {
    async fn record_username(
        &self,
        username: &str,
        telegram_id: u64,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO usernames (username, telegram_id) VALUES (?1, ?2)
             ON CONFLICT(username) DO UPDATE SET telegram_id = excluded.telegram_id",
        )
        .bind(username)
        .bind(telegram_id as i64)
        .execute(&self.pool)
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    async fn resolve_username(&self, username: &str) -> Result<Option<u64>, RepositoryError> {
        sqlx::query("SELECT telegram_id FROM usernames WHERE username = ?1")
            .bind(username)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage_error)?
            .map(|row| row.try_get::<i64, _>(0))
            .transpose()
            .map_err(storage_error)
            .map(|id| id.map(|id| id as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn finds_birthdays_only_on_their_day() {
        let store = store().await;
        store.add_birthday(1, date(2000, 6, 6)).await.unwrap();

        let found = store
            .find_birthdays_for_date(date(2026, 6, 6))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].telegram_id, 1);
        assert_eq!(found[0].birthdate, date(2000, 6, 6));

        assert!(
            store
                .find_birthdays_for_date(date(2026, 6, 7))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn adding_again_updates_instead_of_duplicating() {
        let store = store().await;
        store.add_birthday(1, date(2000, 6, 6)).await.unwrap();
        store.add_birthday(1, date(2000, 7, 7)).await.unwrap();

        assert!(
            store
                .find_birthdays_for_date(date(2026, 6, 6))
                .await
                .unwrap()
                .is_empty()
        );
        let found = store
            .find_birthdays_for_date(date(2026, 7, 7))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
    }

    #[tokio::test]
    async fn upsert_preserves_identity_and_creation_time() {
        let store = store().await;
        store.add_birthday(1, date(2000, 6, 6)).await.unwrap();
        let before = store
            .find_birthdays_for_date(date(2026, 6, 6))
            .await
            .unwrap()[0]
            .clone();

        store.add_birthday(1, date(2000, 7, 7)).await.unwrap();
        let after = store
            .find_birthdays_for_date(date(2026, 7, 7))
            .await
            .unwrap()[0]
            .clone();

        assert_eq!(after.id, before.id);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.birthdate, date(2000, 7, 7));
    }

    #[tokio::test]
    async fn feb_29_birthdate_roundtrips() {
        let store = store().await;
        store.add_birthday(1, date(2000, 2, 29)).await.unwrap();

        // Celebrated on Feb 28 in non-leap years...
        let found = store
            .find_birthdays_for_date(date(2025, 2, 28))
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].birthdate, date(2000, 2, 29));

        // ...and only on the real date in leap years.
        assert!(
            store
                .find_birthdays_for_date(date(2024, 2, 28))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .find_birthdays_for_date(date(2024, 2, 29))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn within_sorts_soonest_first_and_wraps_the_year() {
        let store = store().await;
        store.add_birthday(1, date(2000, 1, 2)).await.unwrap();
        store.add_birthday(2, date(2000, 12, 30)).await.unwrap();

        let found = store
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
    async fn remove_deletes_the_birthday_and_reports_whether_it_existed() {
        let store = store().await;
        store.add_birthday(1, date(2000, 6, 6)).await.unwrap();

        assert!(store.remove_birthday(1).await.unwrap());
        assert!(
            store
                .find_birthdays_for_date(date(2026, 6, 6))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!store.remove_birthday(1).await.unwrap());
    }

    #[tokio::test]
    async fn last_announced_roundtrips() {
        let store = store().await;
        assert_eq!(store.last_announced().await.unwrap(), None);

        store.set_last_announced(date(2026, 6, 6)).await.unwrap();
        assert_eq!(
            store.last_announced().await.unwrap(),
            Some(date(2026, 6, 6))
        );

        store.set_last_announced(date(2026, 6, 7)).await.unwrap();
        assert_eq!(
            store.last_announced().await.unwrap(),
            Some(date(2026, 6, 7))
        );
    }

    #[tokio::test]
    async fn usernames_resolve_and_can_be_reassigned() {
        let store = store().await;
        assert_eq!(store.resolve_username("bob").await.unwrap(), None);

        store.record_username("bob", 2).await.unwrap();
        assert_eq!(store.resolve_username("bob").await.unwrap(), Some(2));

        // Telegram usernames can change hands; the directory follows.
        store.record_username("bob", 3).await.unwrap();
        assert_eq!(store.resolve_username("bob").await.unwrap(), Some(3));
    }
}
