//! Character persistence abstraction.
//!
//! The [`CharacterStore`] trait defines how the gateway loads and manages
//! character data during the character selection phase. Two implementations
//! are provided:
//!
//! - [`InMemoryCharacterStore`] — for tests and dev mode (no database needed).
//! - [`DbCharacterStore`] — backed by sqlx + PostgreSQL.

use async_trait::async_trait;
use common::{CharacterId, CharacterFullInfo, CharacterInfo, Class, Race, Vec3, ZoneId};
use std::collections::HashMap;
use tokio::sync::Mutex;

/// Errors returned by [`CharacterStore`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharacterStoreError {
    /// Character name is already taken.
    NameTaken,
    /// Account has reached the maximum number of characters.
    MaxCharacters,
    /// Character not found or not owned by this account.
    NotFound,
    /// Name validation failed.
    InvalidName(String),
    /// An internal/database error occurred.
    Internal(String),
}

impl std::fmt::Display for CharacterStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameTaken => write!(f, "Character name is already taken"),
            Self::MaxCharacters => write!(f, "Maximum characters per account reached"),
            Self::NotFound => write!(f, "Character not found"),
            Self::InvalidName(reason) => write!(f, "Invalid name: {}", reason),
            Self::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for CharacterStoreError {}

/// Maximum characters per account.
pub const MAX_CHARACTERS_PER_ACCOUNT: usize = 10;

/// Abstraction over character persistence.
///
/// The gateway calls these methods during the character selection phase.
/// Production uses a DB-backed implementation; tests use [`InMemoryCharacterStore`].
#[async_trait]
pub trait CharacterStore: Send + Sync {
    /// List all characters belonging to the given account.
    async fn list(&self, account_id: &str) -> Result<Vec<CharacterInfo>, CharacterStoreError>;

    /// Create a new character on the given account.
    async fn create(
        &self,
        account_id: &str,
        name: &str,
        race: Race,
        class: Class,
    ) -> Result<CharacterInfo, CharacterStoreError>;

    /// Delete a character. Only succeeds if the character belongs to the account.
    async fn delete(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<(), CharacterStoreError>;

    /// Load full character info (including position) for entering the world.
    async fn get_full(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<CharacterFullInfo, CharacterStoreError>;
}

/// Validate a character name: 2-12 characters, alphanumeric only.
pub fn validate_name(name: &str) -> Result<(), CharacterStoreError> {
    if name.len() < 2 {
        return Err(CharacterStoreError::InvalidName(
            "Name must be at least 2 characters".to_string(),
        ));
    }
    if name.len() > 12 {
        return Err(CharacterStoreError::InvalidName(
            "Name must be at most 12 characters".to_string(),
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(CharacterStoreError::InvalidName(
            "Name must be alphanumeric".to_string(),
        ));
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// InMemoryCharacterStore
// ─────────────────────────────────────────────────────────────────────────────

/// In-memory character store for tests and dev mode.
///
/// Uses a `tokio::sync::Mutex` around a `HashMap`. This is a test utility
/// shared across connection tasks — not zone state — so Mutex is acceptable.
pub struct InMemoryCharacterStore {
    /// account_id -> list of characters
    data: Mutex<HashMap<String, Vec<CharacterFullInfo>>>,
}

impl InMemoryCharacterStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryCharacterStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CharacterStore for InMemoryCharacterStore {
    async fn list(&self, account_id: &str) -> Result<Vec<CharacterInfo>, CharacterStoreError> {
        let data = self.data.lock().await;
        let chars = data.get(account_id).cloned().unwrap_or_default();
        Ok(chars
            .into_iter()
            .map(|c| CharacterInfo {
                id: c.id,
                name: c.name,
                race: c.race,
                class: c.class,
                level: c.level,
                zone_id: c.zone_id,
            })
            .collect())
    }

    async fn create(
        &self,
        account_id: &str,
        name: &str,
        race: Race,
        class: Class,
    ) -> Result<CharacterInfo, CharacterStoreError> {
        validate_name(name)?;

        let mut data = self.data.lock().await;
        let chars = data.entry(account_id.to_string()).or_default();

        if chars.len() >= MAX_CHARACTERS_PER_ACCOUNT {
            return Err(CharacterStoreError::MaxCharacters);
        }

        // Check name uniqueness across all accounts
        for account_chars in data.values() {
            for c in account_chars {
                if c.name.eq_ignore_ascii_case(name) {
                    return Err(CharacterStoreError::NameTaken);
                }
            }
        }

        let id = CharacterId::new(uuid::Uuid::new_v4().to_string());
        let full = CharacterFullInfo {
            id: id.clone(),
            name: name.to_string(),
            race,
            class,
            level: 1,
            zone_id: ZoneId::new(1),
            position: Vec3::ZERO,
        };

        // Re-borrow after the name uniqueness check
        let chars = data.entry(account_id.to_string()).or_default();
        chars.push(full);

        Ok(CharacterInfo {
            id,
            name: name.to_string(),
            race,
            class,
            level: 1,
            zone_id: ZoneId::new(1),
        })
    }

    async fn delete(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<(), CharacterStoreError> {
        let mut data = self.data.lock().await;
        let chars = data
            .get_mut(account_id)
            .ok_or(CharacterStoreError::NotFound)?;

        let idx = chars
            .iter()
            .position(|c| c.id.get() == character_id)
            .ok_or(CharacterStoreError::NotFound)?;

        chars.remove(idx);
        Ok(())
    }

    async fn get_full(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<CharacterFullInfo, CharacterStoreError> {
        let data = self.data.lock().await;
        let chars = data.get(account_id).ok_or(CharacterStoreError::NotFound)?;

        chars
            .iter()
            .find(|c| c.id.get() == character_id)
            .cloned()
            .ok_or(CharacterStoreError::NotFound)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DbCharacterStore
// ─────────────────────────────────────────────────────────────────────────────

/// Database-backed character store using sqlx + PostgreSQL.
///
/// Expects the `characters` table from `migrations/002_characters.sql`.
/// Account IDs are UUID strings matching `accounts.id`.
pub struct DbCharacterStore {
    pool: sqlx::PgPool,
}

impl DbCharacterStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CharacterStore for DbCharacterStore {
    async fn list(&self, account_id: &str) -> Result<Vec<CharacterInfo>, CharacterStoreError> {
        let account_uuid: uuid::Uuid = account_id
            .parse()
            .map_err(|_| CharacterStoreError::Internal("invalid account UUID".into()))?;

        let rows = sqlx::query_as::<_, (uuid::Uuid, String, i16, i16, i32, i32)>(
            "SELECT id, name, race, class, level, zone_id FROM characters WHERE account_id = $1 ORDER BY created_at",
        )
        .bind(account_uuid)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CharacterStoreError::Internal(e.to_string()))?;

        rows.into_iter()
            .map(|(id, name, race, class, level, zone_id)| {
                Ok(CharacterInfo {
                    id: CharacterId::new(id.to_string()),
                    name,
                    race: Race::from_u8(race as u8)
                        .ok_or_else(|| CharacterStoreError::Internal("invalid race in DB".into()))?,
                    class: Class::from_u8(class as u8)
                        .ok_or_else(|| CharacterStoreError::Internal("invalid class in DB".into()))?,
                    level: level as u32,
                    zone_id: ZoneId::new(zone_id as u32),
                })
            })
            .collect()
    }

    async fn create(
        &self,
        account_id: &str,
        name: &str,
        race: Race,
        class: Class,
    ) -> Result<CharacterInfo, CharacterStoreError> {
        validate_name(name)?;

        let account_uuid: uuid::Uuid = account_id
            .parse()
            .map_err(|_| CharacterStoreError::Internal("invalid account UUID".into()))?;

        // Check max characters per account.
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM characters WHERE account_id = $1",
        )
        .bind(account_uuid)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| CharacterStoreError::Internal(e.to_string()))?;

        if count.0 as usize >= MAX_CHARACTERS_PER_ACCOUNT {
            return Err(CharacterStoreError::MaxCharacters);
        }

        let row: (uuid::Uuid,) = sqlx::query_as(
            "INSERT INTO characters (account_id, name, race, class) VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(account_uuid)
        .bind(name)
        .bind(race.to_u8() as i16)
        .bind(class.to_u8() as i16)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db_err) = e {
                if db_err.constraint() == Some("characters_name_key") {
                    return CharacterStoreError::NameTaken;
                }
            }
            CharacterStoreError::Internal(e.to_string())
        })?;

        Ok(CharacterInfo {
            id: CharacterId::new(row.0.to_string()),
            name: name.to_string(),
            race,
            class,
            level: 1,
            zone_id: ZoneId::new(1),
        })
    }

    async fn delete(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<(), CharacterStoreError> {
        let account_uuid: uuid::Uuid = account_id
            .parse()
            .map_err(|_| CharacterStoreError::Internal("invalid account UUID".into()))?;
        let char_uuid: uuid::Uuid = character_id
            .parse()
            .map_err(|_| CharacterStoreError::NotFound)?;

        let result = sqlx::query(
            "DELETE FROM characters WHERE id = $1 AND account_id = $2",
        )
        .bind(char_uuid)
        .bind(account_uuid)
        .execute(&self.pool)
        .await
        .map_err(|e| CharacterStoreError::Internal(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(CharacterStoreError::NotFound);
        }
        Ok(())
    }

    async fn get_full(
        &self,
        account_id: &str,
        character_id: &str,
    ) -> Result<CharacterFullInfo, CharacterStoreError> {
        let account_uuid: uuid::Uuid = account_id
            .parse()
            .map_err(|_| CharacterStoreError::Internal("invalid account UUID".into()))?;
        let char_uuid: uuid::Uuid = character_id
            .parse()
            .map_err(|_| CharacterStoreError::NotFound)?;

        let row: Option<(uuid::Uuid, String, i16, i16, i32, i32, f32, f32, f32)> = sqlx::query_as(
            "SELECT id, name, race, class, level, zone_id, position_x, position_y, position_z \
             FROM characters WHERE id = $1 AND account_id = $2",
        )
        .bind(char_uuid)
        .bind(account_uuid)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| CharacterStoreError::Internal(e.to_string()))?;

        let (id, name, race, class, level, zone_id, px, py, pz) =
            row.ok_or(CharacterStoreError::NotFound)?;

        Ok(CharacterFullInfo {
            id: CharacterId::new(id.to_string()),
            name,
            race: Race::from_u8(race as u8)
                .ok_or_else(|| CharacterStoreError::Internal("invalid race in DB".into()))?,
            class: Class::from_u8(class as u8)
                .ok_or_else(|| CharacterStoreError::Internal("invalid class in DB".into()))?,
            level: level as u32,
            zone_id: ZoneId::new(zone_id as u32),
            position: Vec3::new(px, py, pz),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_empty_account() {
        let store = InMemoryCharacterStore::new();
        let list = store.list("account-1").await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn create_and_list() {
        let store = InMemoryCharacterStore::new();
        let info = store
            .create("acc-1", "Arthas", Race::Human, Class::Warrior)
            .await
            .unwrap();
        assert_eq!(info.name, "Arthas");
        assert_eq!(info.race, Race::Human);
        assert_eq!(info.class, Class::Warrior);
        assert_eq!(info.level, 1);

        let list = store.list("acc-1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Arthas");
    }

    #[tokio::test]
    async fn create_duplicate_name_fails() {
        let store = InMemoryCharacterStore::new();
        store
            .create("acc-1", "Arthas", Race::Human, Class::Warrior)
            .await
            .unwrap();
        let err = store
            .create("acc-2", "arthas", Race::Orc, Class::Mage)
            .await
            .unwrap_err();
        assert_eq!(err, CharacterStoreError::NameTaken);
    }

    #[tokio::test]
    async fn create_max_characters_fails() {
        let store = InMemoryCharacterStore::new();
        for i in 0..MAX_CHARACTERS_PER_ACCOUNT {
            store
                .create("acc-1", &format!("Char{:02}", i), Race::Human, Class::Warrior)
                .await
                .unwrap();
        }
        let err = store
            .create("acc-1", "OneMore", Race::Elf, Class::Priest)
            .await
            .unwrap_err();
        assert_eq!(err, CharacterStoreError::MaxCharacters);
    }

    #[tokio::test]
    async fn delete_character() {
        let store = InMemoryCharacterStore::new();
        let info = store
            .create("acc-1", "Arthas", Race::Human, Class::Warrior)
            .await
            .unwrap();
        store.delete("acc-1", info.id.get()).await.unwrap();
        let list = store.list("acc-1").await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn delete_wrong_account_fails() {
        let store = InMemoryCharacterStore::new();
        let info = store
            .create("acc-1", "Arthas", Race::Human, Class::Warrior)
            .await
            .unwrap();
        let err = store.delete("acc-2", info.id.get()).await.unwrap_err();
        assert_eq!(err, CharacterStoreError::NotFound);
    }

    #[tokio::test]
    async fn get_full_returns_position() {
        let store = InMemoryCharacterStore::new();
        let info = store
            .create("acc-1", "Jaina", Race::Human, Class::Mage)
            .await
            .unwrap();
        let full = store.get_full("acc-1", info.id.get()).await.unwrap();
        assert_eq!(full.name, "Jaina");
        assert_eq!(full.position, Vec3::ZERO);
        assert_eq!(full.zone_id, ZoneId::new(1));
    }

    #[tokio::test]
    async fn get_full_wrong_account_fails() {
        let store = InMemoryCharacterStore::new();
        let info = store
            .create("acc-1", "Thrall", Race::Orc, Class::Warrior)
            .await
            .unwrap();
        let err = store.get_full("acc-2", info.id.get()).await.unwrap_err();
        assert_eq!(err, CharacterStoreError::NotFound);
    }

    #[tokio::test]
    async fn name_too_short() {
        let store = InMemoryCharacterStore::new();
        let err = store
            .create("acc-1", "A", Race::Human, Class::Warrior)
            .await
            .unwrap_err();
        assert!(matches!(err, CharacterStoreError::InvalidName(_)));
    }

    #[tokio::test]
    async fn name_too_long() {
        let store = InMemoryCharacterStore::new();
        let err = store
            .create("acc-1", "Abcdefghijklm", Race::Human, Class::Warrior)
            .await
            .unwrap_err();
        assert!(matches!(err, CharacterStoreError::InvalidName(_)));
    }

    #[tokio::test]
    async fn name_with_spaces_fails() {
        let store = InMemoryCharacterStore::new();
        let err = store
            .create("acc-1", "My Hero", Race::Human, Class::Warrior)
            .await
            .unwrap_err();
        assert!(matches!(err, CharacterStoreError::InvalidName(_)));
    }
}
