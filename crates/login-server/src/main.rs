//! # login-server
//!
//! Handles account registration, login (bcrypt + JWT), realm list,
//! and character CRUD.
//! Separate process from the game server — no game logic here.
//!
//! ## Environment variables
//!
//! - `DATABASE_URL` — Postgres connection string (**required**)
//! - `JWT_SECRET`   — HMAC-SHA256 key for signing JWTs (default: insecure dev key)
//! - `PORT`         — HTTP listen port (default: 8080)

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use axum::http::header::AUTHORIZATION;
use common::{CharacterInfo, Class, Race};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use protocol::AuthClaims;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;

// ─────────────────────────────────────────────────────────────────────────────
// App state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    jwt_secret: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum AuthError {
    UsernameConflict,
    InvalidCredentials,
    Unauthorized,
    BadRequest(String),
    NotFound,
    Internal(String),
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AuthError::UsernameConflict => {
                (StatusCode::CONFLICT, "username already taken".to_string())
            }
            AuthError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, "invalid username or password".to_string())
            }
            AuthError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "missing or invalid authorization".to_string())
            }
            AuthError::BadRequest(ref reason) => {
                (StatusCode::BAD_REQUEST, reason.clone())
            }
            AuthError::NotFound => {
                (StatusCode::NOT_FOUND, "not found".to_string())
            }
            AuthError::Internal(ref e) => {
                tracing::error!(error = %e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error".to_string())
            }
        };
        (status, msg).into_response()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Request / response types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    account_id: String,
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
}

#[derive(Serialize)]
struct Realm {
    id: u32,
    name: String,
    address: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Character request/response types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateCharacterRequest {
    name: String,
    race: String,
    class: String,
}

#[derive(Serialize)]
struct CharacterResponse {
    id: String,
    name: String,
    race: String,
    class: String,
    level: u32,
    zone_id: u32,
}

impl From<CharacterInfo> for CharacterResponse {
    fn from(c: CharacterInfo) -> Self {
        Self {
            id: c.id.get().to_string(),
            name: c.name,
            race: format!("{}", c.race),
            class: format!("{}", c.class),
            level: c.level,
            zone_id: c.zone_id.get(),
        }
    }
}

fn parse_race(s: &str) -> Result<Race, AuthError> {
    match s.to_lowercase().as_str() {
        "human" | "0" => Ok(Race::Human),
        "orc" | "1" => Ok(Race::Orc),
        "dwarf" | "2" => Ok(Race::Dwarf),
        "elf" | "3" => Ok(Race::Elf),
        _ => Err(AuthError::BadRequest(format!("unknown race: {s}"))),
    }
}

fn parse_class(s: &str) -> Result<Class, AuthError> {
    match s.to_lowercase().as_str() {
        "warrior" | "0" => Ok(Class::Warrior),
        "mage" | "1" => Ok(Class::Mage),
        "priest" | "2" => Ok(Class::Priest),
        "rogue" | "3" => Ok(Class::Rogue),
        _ => Err(AuthError::BadRequest(format!("unknown class: {s}"))),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JWT extraction helper
// ─────────────────────────────────────────────────────────────────────────────

/// Extract the account UUID from the Authorization: Bearer <jwt> header.
fn extract_account_id(
    headers: &axum::http::HeaderMap,
    jwt_secret: &str,
) -> Result<String, AuthError> {
    let auth_header = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Unauthorized)?;

    let token = auth_header
        .strip_prefix("Bearer ")
        .ok_or(AuthError::Unauthorized)?;

    let token_data = decode::<AuthClaims>(
        token,
        &DecodingKey::from_secret(jwt_secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AuthError::Unauthorized)?;

    Ok(token_data.claims.sub)
}

// ─────────────────────────────────────────────────────────────────────────────
// Route handlers
// ─────────────────────────────────────────────────────────────────────────────

/// POST /register — create a new account with a bcrypt-hashed password.
async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, AuthError> {
    let hash = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)
        .map_err(|e| AuthError::Internal(e.to_string()))?;

    let row: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO accounts (username, password_hash) VALUES ($1, $2) RETURNING id",
    )
    .bind(&req.username)
    .bind(&hash)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref db_err) = e {
            if db_err.constraint() == Some("accounts_username_key") {
                return AuthError::UsernameConflict;
            }
        }
        AuthError::Internal(e.to_string())
    })?;

    Ok(Json(RegisterResponse {
        account_id: row.0.to_string(),
    }))
}

/// POST /login — verify credentials and issue a JWT.
async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AuthError> {
    let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT id, password_hash FROM accounts WHERE username = $1",
    )
    .bind(&req.username)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    let (account_id, password_hash) = row.ok_or(AuthError::InvalidCredentials)?;

    let valid = bcrypt::verify(&req.password, &password_hash)
        .map_err(|e| AuthError::Internal(e.to_string()))?;
    if !valid {
        return Err(AuthError::InvalidCredentials);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as usize;

    let claims = AuthClaims {
        sub: account_id.to_string(),
        jti: uuid::Uuid::new_v4().to_string(),
        iat: now,
        exp: now + 86400, // 24 hours
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.jwt_secret.as_bytes()),
    )
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    Ok(Json(LoginResponse { token }))
}

/// GET /realms — hardcoded realm list.
async fn list_realms() -> Json<Vec<Realm>> {
    Json(vec![Realm {
        id: 1,
        name: "Realm 1".to_string(),
        address: "127.0.0.1:7878".to_string(),
    }])
}

// ─────────────────────────────────────────────────────────────────────────────
// Character CRUD handlers
// ─────────────────────────────────────────────────────────────────────────────

const MAX_CHARACTERS_PER_ACCOUNT: i64 = 10;

/// GET /characters — list all characters for the authenticated account.
async fn list_characters(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<CharacterResponse>>, AuthError> {
    let account_id = extract_account_id(&headers, &state.jwt_secret)?;
    let account_uuid: uuid::Uuid = account_id
        .parse()
        .map_err(|_| AuthError::Internal("invalid account UUID in token".into()))?;

    let rows = sqlx::query_as::<_, (uuid::Uuid, String, i16, i16, i32, i32)>(
        "SELECT id, name, race, class, level, zone_id FROM characters WHERE account_id = $1 ORDER BY created_at",
    )
    .bind(account_uuid)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    let characters: Vec<CharacterResponse> = rows
        .into_iter()
        .map(|(id, name, race, class, level, zone_id)| {
            CharacterResponse {
                id: id.to_string(),
                name,
                race: Race::from_u8(race as u8)
                    .map(|r| format!("{r}"))
                    .unwrap_or_else(|| format!("Unknown({race})")),
                class: Class::from_u8(class as u8)
                    .map(|c| format!("{c}"))
                    .unwrap_or_else(|| format!("Unknown({class})")),
                level: level as u32,
                zone_id: zone_id as u32,
            }
        })
        .collect();

    Ok(Json(characters))
}

/// POST /characters — create a new character for the authenticated account.
async fn create_character(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateCharacterRequest>,
) -> Result<(StatusCode, Json<CharacterResponse>), AuthError> {
    let account_id = extract_account_id(&headers, &state.jwt_secret)?;
    let account_uuid: uuid::Uuid = account_id
        .parse()
        .map_err(|_| AuthError::Internal("invalid account UUID in token".into()))?;

    // Validate name (2-12 chars, alphanumeric).
    if req.name.len() < 2 || req.name.len() > 12 {
        return Err(AuthError::BadRequest("name must be 2-12 characters".into()));
    }
    if !req.name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(AuthError::BadRequest("name must be alphanumeric".into()));
    }

    let race = parse_race(&req.race)?;
    let class = parse_class(&req.class)?;

    // Check max characters.
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM characters WHERE account_id = $1",
    )
    .bind(account_uuid)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    if count.0 >= MAX_CHARACTERS_PER_ACCOUNT {
        return Err(AuthError::BadRequest(
            "maximum characters per account reached".into(),
        ));
    }

    let row: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO characters (account_id, name, race, class) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(account_uuid)
    .bind(&req.name)
    .bind(race.to_u8() as i16)
    .bind(class.to_u8() as i16)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref db_err) = e {
            if db_err.constraint() == Some("characters_name_key") {
                return AuthError::BadRequest("character name already taken".into());
            }
        }
        AuthError::Internal(e.to_string())
    })?;

    Ok((
        StatusCode::CREATED,
        Json(CharacterResponse {
            id: row.0.to_string(),
            name: req.name,
            race: format!("{race}"),
            class: format!("{class}"),
            level: 1,
            zone_id: 1,
        }),
    ))
}

/// DELETE /characters/:id — delete a character owned by the authenticated account.
async fn delete_character(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(character_id): Path<String>,
) -> Result<StatusCode, AuthError> {
    let account_id = extract_account_id(&headers, &state.jwt_secret)?;
    let account_uuid: uuid::Uuid = account_id
        .parse()
        .map_err(|_| AuthError::Internal("invalid account UUID in token".into()))?;
    let char_uuid: uuid::Uuid = character_id
        .parse()
        .map_err(|_| AuthError::NotFound)?;

    let result = sqlx::query(
        "DELETE FROM characters WHERE id = $1 AND account_id = $2",
    )
    .bind(char_uuid)
    .bind(account_uuid)
    .execute(&state.pool)
    .await
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// GET /characters/:id — get full character info for the authenticated account.
async fn get_character(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(character_id): Path<String>,
) -> Result<Json<CharacterResponse>, AuthError> {
    let account_id = extract_account_id(&headers, &state.jwt_secret)?;
    let account_uuid: uuid::Uuid = account_id
        .parse()
        .map_err(|_| AuthError::Internal("invalid account UUID in token".into()))?;
    let char_uuid: uuid::Uuid = character_id
        .parse()
        .map_err(|_| AuthError::NotFound)?;

    let row: Option<(uuid::Uuid, String, i16, i16, i32, i32)> = sqlx::query_as(
        "SELECT id, name, race, class, level, zone_id FROM characters WHERE id = $1 AND account_id = $2",
    )
    .bind(char_uuid)
    .bind(account_uuid)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AuthError::Internal(e.to_string()))?;

    let (id, name, race, class, level, zone_id) = row.ok_or(AuthError::NotFound)?;

    Ok(Json(CharacterResponse {
        id: id.to_string(),
        name,
        race: Race::from_u8(race as u8)
            .map(|r| format!("{r}"))
            .unwrap_or_else(|| format!("Unknown({race})")),
        class: Class::from_u8(class as u8)
            .map(|c| format!("{c}"))
            .unwrap_or_else(|| format!("Unknown({class})")),
        level: level as u32,
        zone_id: zone_id as u32,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// main
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load .env file if present (silently skip if missing).
    dotenvy::dotenv().ok();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| {
        tracing::warn!("JWT_SECRET not set — using insecure default");
        "dev-secret-change-me".to_string()
    });
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a valid port number");

    let pool = PgPool::connect(&database_url).await?;

    // Run migrations on startup.
    sqlx::migrate!("../../migrations").run(&pool).await?;

    let state = AppState { pool, jwt_secret };

    let app = Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/realms", get(list_realms))
        .route("/characters", get(list_characters).post(create_character))
        .route(
            "/characters/:id",
            get(get_character).delete(delete_character),
        )
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    info!("Login server: http://localhost:{port}");
    println!("Login server: http://localhost:{port}");

    axum::serve(listener, app).await?;

    Ok(())
}
