use std::sync::{Arc, Mutex};

use chrono::{NaiveDate, Utc};
use domain::elements::{RepositoryError, User, UserRepository, UsernameDirectory};
use rusqlite::{Connection, OptionalExtension, params};

/// SQLite-backed store for birthdays and the username directory. Cheap to
/// clone; clones share one connection.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    pub fn open(path: &str) -> Result<Self, RepositoryError> {
        Self::from_connection(Connection::open(path).map_err(storage_error)?)
    }

    /// An ephemeral store, for tests.
    pub fn open_in_memory() -> Result<Self, RepositoryError> {
        Self::from_connection(Connection::open_in_memory().map_err(storage_error)?)
    }

    fn from_connection(conn: Connection) -> Result<Self, RepositoryError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                telegram_id INTEGER NOT NULL UNIQUE,
                birthdate TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usernames (
                username TEXT PRIMARY KEY,
                telegram_id INTEGER NOT NULL
            );",
        )
        .map_err(storage_error)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs a query on the blocking thread pool, since rusqlite is synchronous.
    async fn with_conn<T, F>(&self, f: F) -> Result<T, RepositoryError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || f(&conn.lock().unwrap()))
            .await
            .map_err(|err| RepositoryError::Storage(err.to_string()))?
            .map_err(storage_error)
    }

    /// Celebration rules (the Feb 29 fallback) live in the domain, so date
    /// filtering happens in Rust; a single chat's user table stays tiny.
    fn all_users(conn: &Connection) -> rusqlite::Result<Vec<User>> {
        conn.prepare(
            "SELECT id, telegram_id, birthdate, created_at, updated_at
             FROM users ORDER BY telegram_id",
        )?
        .query_map([], row_to_user)?
        .collect()
    }
}

fn storage_error(err: rusqlite::Error) -> RepositoryError {
    RepositoryError::Storage(err.to_string())
}

fn row_to_user(row: &rusqlite::Row) -> rusqlite::Result<User> {
    let id: String = row.get(0)?;
    let id = uuid::Uuid::parse_str(&id).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
    })?;
    let telegram_id: i64 = row.get(1)?;
    Ok(User {
        id: id.into(),
        telegram_id: telegram_id as u64,
        birthdate: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[async_trait::async_trait]
impl UserRepository for SqliteStore {
    async fn find_birthdays_for_date(&self, date: NaiveDate) -> Result<Vec<User>, RepositoryError> {
        let users = self.with_conn(Self::all_users).await?;
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
        let users = self.with_conn(Self::all_users).await?;
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
        let now = Utc::now();
        let id = uuid::Uuid::now_v7();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO users (id, telegram_id, birthdate, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(telegram_id) DO UPDATE SET
                     birthdate = excluded.birthdate,
                     updated_at = excluded.updated_at",
                params![id.to_string(), telegram_id as i64, birthdate, now],
            )
            .map(|_| ())
        })
        .await
    }

    async fn remove_birthday(&self, telegram_id: u64) -> Result<bool, RepositoryError> {
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM users WHERE telegram_id = ?1",
                [telegram_id as i64],
            )
            .map(|rows| rows > 0)
        })
        .await
    }
}

#[async_trait::async_trait]
impl UsernameDirectory for SqliteStore {
    async fn record_username(
        &self,
        username: &str,
        telegram_id: u64,
    ) -> Result<(), RepositoryError> {
        let username = username.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO usernames (username, telegram_id) VALUES (?1, ?2)
                 ON CONFLICT(username) DO UPDATE SET telegram_id = excluded.telegram_id",
                params![username, telegram_id as i64],
            )
            .map(|_| ())
        })
        .await
    }

    async fn resolve_username(&self, username: &str) -> Result<Option<u64>, RepositoryError> {
        let username = username.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT telegram_id FROM usernames WHERE username = ?1",
                [username],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })
        .await
        .map(|id| id.map(|id| id as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn finds_birthdays_only_on_their_day() {
        let store = store();
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
        let store = store();
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
    async fn within_sorts_soonest_first_and_wraps_the_year() {
        let store = store();
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
        let store = store();
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
    async fn usernames_resolve_and_can_be_reassigned() {
        let store = store();
        assert_eq!(store.resolve_username("bob").await.unwrap(), None);

        store.record_username("bob", 2).await.unwrap();
        assert_eq!(store.resolve_username("bob").await.unwrap(), Some(2));

        // Telegram usernames can change hands; the directory follows.
        store.record_username("bob", 3).await.unwrap();
        assert_eq!(store.resolve_username("bob").await.unwrap(), Some(3));
    }
}
