use async_session::{CookieStore, SessionStore};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum_extra::extract::cookie::SignedCookieJar;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sqlx::mysql::{MySqlConnection, MySqlPool};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use num_traits::ToPrimitive;
use sqlx::types::BigDecimal;
use tracing::{error, info};
use uuid::Uuid;

const DEFAULT_SESSION_ID_KEY: &str = "SESSIONID";
const DEFUALT_SESSION_EXPIRES_KEY: &str = "EXPIRES";
const DEFAULT_USER_ID_KEY: &str = "USERID";
const DEFAULT_USERNAME_KEY: &str = "USERNAME";
const FALLBACK_IMAGE: &str = "../img/NoImage.jpg";

lazy_static::lazy_static! {
    static ref FALLBACK_IMAGE_BYTES: Vec<u8> = std::fs::read(FALLBACK_IMAGE).unwrap();
    static ref FALLBACK_IMAGE_HASH: String = {
        use sha2::digest::Digest as _;
        let icon_hash = sha2::Sha256::digest(&*FALLBACK_IMAGE_BYTES);
        let result: String = format!("{:x}", icon_hash);
        result
    };

    static ref ICON_HASH_CACHE: moka::future::Cache<i64, String> = moka::future::Cache::builder()
        .max_capacity(100_0000)
        .time_to_live(Duration::from_secs(90))
        .build();

    static ref TAG_MODELS: tokio::sync::RwLock<HashMap<i64, String>> = tokio::sync::RwLock::new(HashMap::new());
    static ref TAG_MAP: tokio::sync::RwLock<HashMap<String, i64>> = tokio::sync::RwLock::new(HashMap::new());
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("bcrypt error: {0}")]
    Bcrypt(#[from] bcrypt::BcryptError),
    #[error("async-session error: {0}")]
    AsyncSession(#[from] async_session::Error),
    #[error("{0}")]
    BadRequest(Cow<'static, str>),
    #[error("session error")]
    SessionError,
    #[error("unauthorized: {0}")]
    Unauthorized(Cow<'static, str>),
    #[error("forbidden: {0}")]
    Forbidden(Cow<'static, str>),
    #[error("not found: {0}")]
    NotFound(Cow<'static, str>),
    #[error("{0}")]
    InternalServerError(String),
}
impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        #[derive(Debug, serde::Serialize)]
        struct ErrorResponse {
            error: String,
        }

        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) | Self::SessionError => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Io(_)
            | Self::Sqlx(_)
            | Self::Bcrypt(_)
            | Self::AsyncSession(_)
            | Self::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        tracing::error!("{}", self);
        (
            status,
            axum::Json(ErrorResponse {
                error: format!("{}", self),
            }),
        )
            .into_response()
    }
}

#[derive(Clone)]
struct AppState {
    pool: MySqlPool,
    key: axum_extra::extract::cookie::Key,
    powerdns_subdomain_address: Arc<String>,
}
impl axum::extract::FromRef<AppState> for axum_extra::extract::cookie::Key {
    fn from_ref(state: &AppState) -> Self {
        state.key.clone()
    }
}

#[derive(Debug, serde::Serialize)]
struct InitializeResponse {
    language: &'static str,
}

fn build_mysql_options() -> sqlx::mysql::MySqlConnectOptions {
    let mut options = sqlx::mysql::MySqlConnectOptions::new()
        .host("127.0.0.1")
        .port(3306)
        .username("isucon")
        .password("isucon")
        .database("isupipe");
    if let Ok(host) = std::env::var("ISUCON13_MYSQL_DIALCONFIG_ADDRESS") {
        options = options.host(&host);
    }
    if let Some(port) = std::env::var("ISUCON13_MYSQL_DIALCONFIG_PORT")
        .ok()
        .and_then(|port_str| port_str.parse().ok())
    {
        options = options.port(port);
    }
    if let Ok(user) = std::env::var("ISUCON13_MYSQL_DIALCONFIG_USER") {
        options = options.username(&user);
    }
    if let Ok(password) = std::env::var("ISUCON13_MYSQL_DIALCONFIG_PASSWORD") {
        options = options.password(&password);
    }
    if let Ok(database) = std::env::var("ISUCON13_MYSQL_DIALCONFIG_DATABASE") {
        options = options.database(&database);
    }
    options
}

async fn initialize_handler() -> Result<axum::Json<InitializeResponse>, Error> {
    ICON_HASH_CACHE.invalidate_all();
    let output = tokio::process::Command::new("../sql/init.sh")
        .output()
        .await?;
    if !output.status.success() {
        return Err(Error::InternalServerError(format!(
            "init.sh failed with stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )));
    }

    Ok(axum::Json(InitializeResponse { language: "rust" }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info,axum::rejection=trace");
    }
    tracing_subscriber::fmt::init();

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .connect_with(build_mysql_options())
        .await
        .expect("failed to connect db");

    const DEFAULT_SECRET: &[u8] = b"isucon13_session_cookiestore_defaultsecret";
    let secret = if let Ok(secret) = std::env::var("ISUCON13_SESSION_SECRETKEY") {
        secret.into_bytes()
    } else {
        DEFAULT_SECRET.to_owned()
    };

    const POWERDNS_SUBDOMAIN_ADDRESS_ENV_KEY: &str = "ISUCON13_POWERDNS_SUBDOMAIN_ADDRESS";
    let Ok(powerdns_subdomain_address) = std::env::var(POWERDNS_SUBDOMAIN_ADDRESS_ENV_KEY) else {
        panic!(
            "environ {} must be provided",
            POWERDNS_SUBDOMAIN_ADDRESS_ENV_KEY
        );
    };

    {
        // Fetch all tags from db
        let tags: Vec<TagModel> = sqlx::query_as("SELECT * FROM tags")
            .fetch_all(&pool)
            .await?;
        let mut lock = TAG_MODELS.write().await;
        let mut lock2 = TAG_MAP.write().await;
        for tag in tags {
            lock.insert(tag.id, tag.name.clone());
            lock2.insert(tag.name, tag.id);
        }
    }

    let app = axum::Router::new()
        // 初期化
        .route("/api/initialize", axum::routing::post(initialize_handler))
        // top
        .route("/api/tag", axum::routing::get(get_tag_handler))
        .route(
            "/api/user/:username/theme",
            axum::routing::get(get_streamer_theme_handler),
        )
        // livestream
        // reserve livestream
        .route(
            "/api/livestream/reservation",
            axum::routing::post(reserve_livestream_handler),
        )
        // list livestream
        .route(
            "/api/livestream/search",
            axum::routing::get(search_livestreams_handler),
        )
        .route(
            "/api/livestream",
            axum::routing::get(get_my_livestreams_handler),
        )
        .route(
            "/api/user/:username/livestream",
            axum::routing::get(get_user_livestreams_handler),
        )
        // get livestream
        .route(
            "/api/livestream/:livestream_id",
            axum::routing::get(get_livestream_handler),
        )
        // get polling livecomment timeline
        // ライブコメント投稿
        .route(
            "/api/livestream/:livestream_id/livecomment",
            axum::routing::get(get_livecomments_handler).post(post_livecomment_handler),
        )
        .route(
            "/api/livestream/:livestream_id/reaction",
            axum::routing::get(get_reactions_handler).post(post_reaction_handler),
        )
        // (配信者向け)ライブコメントの報告一覧取得API
        .route(
            "/api/livestream/:livestream_id/report",
            axum::routing::get(get_livecomment_reports_handler),
        )
        .route(
            "/api/livestream/:livestream_id/ngwords",
            axum::routing::get(get_ngwords),
        )
        // ライブコメント報告
        .route(
            "/api/livestream/:livestream_id/livecomment/:livecomment_id/report",
            axum::routing::post(report_livecomment_handler),
        )
        // 配信者によるモデレーション (NGワード登録)
        .route(
            "/api/livestream/:livestream_id/moderate",
            axum::routing::post(moderate_handler),
        )
        // livestream_viewersにINSERTするため必要
        // ユーザ視聴開始 (viewer)
        .route(
            "/api/livestream/:livestream_id/enter",
            axum::routing::post(enter_livestream_handler),
        )
        // ユーザ視聴終了 (viewer)
        .route(
            "/api/livestream/:livestream_id/exit",
            axum::routing::delete(exit_livestream_handler),
        )
        // user
        .route("/api/register", axum::routing::post(register_handler))
        .route("/api/login", axum::routing::post(login_handler))
        .route("/api/user/me", axum::routing::get(get_me_handler))
        // フロントエンドで、配信予約のコラボレーターを指定する際に必要
        .route("/api/user/:username", axum::routing::get(get_user_handler))
        .route(
            "/api/user/:username/statistics",
            axum::routing::get(get_user_statistics_handler),
        )
        // icon情報
        .route(
            "/api/user/:username/icon",
            axum::routing::get(get_icon_handler),
        )
        .route("/api/icon", axum::routing::post(post_icon_handler))
        // stats
        // ライブ配信統計情報
        .route(
            "/api/livestream/:livestream_id/statistics",
            axum::routing::get(get_livestream_statistics_handler),
        )
        // 課金情報
        .route("/api/payment", axum::routing::get(get_payment_result))
        .with_state(AppState {
            pool,
            key: axum_extra::extract::cookie::Key::derive_from(&secret),
            powerdns_subdomain_address: Arc::new(powerdns_subdomain_address),
        })
        .layer(tower_http::trace::TraceLayer::new_for_http());

    // HTTPサーバ起動
    if let Some(tcp_listener) = listenfd::ListenFd::from_env().take_tcp_listener(0)? {
        axum::Server::from_tcp(tcp_listener)?
    } else {
        const LISTEN_PORT: u16 = 8080;
        axum::Server::bind(&std::net::SocketAddr::from(([0, 0, 0, 0], LISTEN_PORT)))
    }
    .serve(app.into_make_service())
    .await?;

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
struct Tag {
    id: i64,
    name: String,
}

#[derive(Debug, sqlx::FromRow)]
struct TagModel {
    id: i64,
    name: String,
}

#[derive(Debug, serde::Serialize)]
struct TagsResponse {
    tags: Vec<Tag>,
}

async fn get_tag_handler() -> Result<axum::Json<TagsResponse>, Error> {
    let tags = TAG_MODELS.read().await
        .iter()
        .map(|(&id, name)| Tag {
            id,
            name: name.clone(),
        })
        .collect();
    Ok(axum::Json(TagsResponse { tags }))
}

// 配信者のテーマ取得API
// GET /api/user/:username/theme
async fn get_streamer_theme_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((username,)): Path<(String,)>,
) -> Result<axum::Json<Theme>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE name = ?")
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound(
            "not found user that has the given username".into(),
        ))?;

    let theme_model: ThemeModel = sqlx::query_as("SELECT * FROM themes WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(axum::Json(Theme {
        id: theme_model.id,
        dark_mode: theme_model.dark_mode,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct ReserveLivestreamRequest {
    tags: Vec<i64>,
    title: String,
    description: String,
    playlist_url: String,
    thumbnail_url: String,
    start_at: i64,
    end_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LivestreamModel {
    id: i64,
    user_id: i64,
    title: String,
    description: String,
    playlist_url: String,
    thumbnail_url: String,
    start_at: i64,
    end_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Livestream {
    id: i64,
    owner: User,
    title: String,
    description: String,
    playlist_url: String,
    thumbnail_url: String,
    tags: Vec<Tag>,
    start_at: i64,
    end_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LivestreamTagModel {
    #[allow(unused)]
    id: i64,
    livestream_id: i64,
    tag_id: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct ReservationSlotModel {
    #[allow(unused)]
    id: i64,
    slot: i64,
    start_at: i64,
    end_at: i64,
}

async fn reserve_livestream_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    axum::Json(req): axum::Json<ReserveLivestreamRequest>,
) -> Result<(StatusCode, axum::Json<Livestream>), Error> {
    verify_user_session(&jar).await?;

    if req.tags.iter().any(|&tag_id| tag_id > 103) {
        tracing::error!("unexpected tags: {:?}", req);
    }

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    // 2023/11/25 10:00からの１年間の期間内であるかチェック
    let term_start_at = Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(2023, 11, 25)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap(),
    );
    let term_end_at = Utc.from_utc_datetime(
        &NaiveDate::from_ymd_opt(2024, 11, 25)
            .unwrap()
            .and_hms_opt(1, 0, 0)
            .unwrap(),
    );
    let reserve_start_at = DateTime::from_timestamp(req.start_at, 0).unwrap();
    let reserve_end_at = DateTime::from_timestamp(req.end_at, 0).unwrap();
    if reserve_start_at >= term_end_at || reserve_end_at <= term_start_at {
        return Err(Error::BadRequest("bad reservation time range".into()));
    }

    // 予約枠をみて、予約が可能か調べる
    // NOTE: 並列な予約のoverbooking防止にFOR UPDATEが必要
    let slots: Vec<ReservationSlotModel> = sqlx::query_as(
        "SELECT * FROM reservation_slots WHERE start_at >= ? AND end_at <= ? FOR UPDATE",
    )
    .bind(req.start_at)
    .bind(req.end_at)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| {
        tracing::warn!("予約枠一覧取得でエラー発生: {e:?}");
        e
    })?;
    for slot in slots {
        let count: i64 = sqlx::query_scalar(
            "SELECT slot FROM reservation_slots WHERE start_at = ? AND end_at = ?",
        )
        .bind(slot.start_at)
        .bind(slot.end_at)
        .fetch_one(&mut *tx)
        .await?;
        tracing::info!(
            "{} ~ {}予約枠の残数 = {}",
            slot.start_at,
            slot.end_at,
            slot.slot
        );
        if count < 1 {
            return Err(Error::BadRequest(
                format!(
                    "予約期間 {} ~ {}に対して、予約区間 {} ~ {}が予約できません",
                    term_start_at.timestamp(),
                    term_end_at.timestamp(),
                    req.start_at,
                    req.end_at
                )
                .into(),
            ));
        }
    }

    sqlx::query("UPDATE reservation_slots SET slot = slot - 1 WHERE start_at >= ? AND end_at <= ?")
        .bind(req.start_at)
        .bind(req.end_at)
        .execute(&mut *tx)
        .await?;

    let rs = sqlx::query("INSERT INTO livestreams (user_id, title, description, playlist_url, thumbnail_url, start_at, end_at) VALUES(?, ?, ?, ?, ?, ?, ?)")
        .bind(user_id)
        .bind(&req.title)
        .bind(&req.description)
        .bind(&req.playlist_url)
        .bind(&req.thumbnail_url)
        .bind(req.start_at)
        .bind(req.end_at)
        .execute(&mut *tx)
        .await?;
    let livestream_id = rs.last_insert_id() as i64;

    // タグ追加
    for tag_id in req.tags {
        sqlx::query("INSERT INTO livestream_tags (livestream_id, tag_id) VALUES (?, ?)")
            .bind(livestream_id)
            .bind(tag_id)
            .execute(&mut *tx)
            .await?;
    }

    let owner = get_user(&mut *tx, user_id).await?;

    let livestream = fill_livestream_response(
        &mut tx,
        &owner,
        LivestreamModel {
            id: livestream_id,
            user_id,
            title: req.title,
            description: req.description,
            playlist_url: req.playlist_url,
            thumbnail_url: req.thumbnail_url,
            start_at: req.start_at,
            end_at: req.end_at,
        },
    )
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, axum::Json(livestream)))
}

#[derive(Debug, serde::Deserialize)]
struct SearchLivestreamsQuery {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    limit: String,
}

async fn search_livestreams_handler(
    State(AppState { pool, .. }): State<AppState>,
    Query(SearchLivestreamsQuery {
        tag: key_tag_name,
        limit,
    }): Query<SearchLivestreamsQuery>,
) -> Result<axum::Json<Vec<Livestream>>, Error> {
    use sqlx::Execute;
    let mut tx = pool.begin().await?;

    let livestream_models: Vec<LivestreamModel> = if key_tag_name.is_empty() {
        // 検索条件なし
        let mut query = "SELECT * FROM livestreams ORDER BY id DESC".to_owned();
        if !limit.is_empty() {
            let limit: i64 = limit
                .parse()
                .map_err(|_| Error::BadRequest("failed to parse limit".into()))?;
            query = format!("{} LIMIT {}", query, limit);
        }
        sqlx::query_as(&query).fetch_all(&mut *tx).await?
    } else {
        // タグによる取得
        let tag_id = TAG_MAP.read().await.get(&key_tag_name).cloned();

        let mut query_builder = sqlx::query_builder::QueryBuilder::new(
            "SELECT * FROM livestream_tags WHERE tag_id IN (",
        );
        let mut separated = query_builder.separated(", ");
        if let Some(tag_id) = tag_id {
            separated.push_bind(tag_id);
        }
        separated.push_unseparated(") ORDER BY livestream_id DESC");
        let key_tagged_livestreams: Vec<LivestreamTagModel> =
            query_builder.build_query_as().fetch_all(&mut *tx).await?;

        let mut livestream_models = Vec::new();
        for key_tagged_livestream in key_tagged_livestreams {
            let ls = sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
                .bind(key_tagged_livestream.livestream_id)
                .fetch_one(&mut *tx)
                .await?;
            livestream_models.push(ls);
        }
        livestream_models
    };
    let mut tags_memo: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut owners_memo: HashMap<i64, User> = HashMap::new();
    for chunk in livestream_models.chunks(10000) {
        let livestream_ids: Vec<i64> = chunk.iter().map(|ls| ls.id).collect();
        let mut query_builder = sqlx::query_builder::QueryBuilder::new(
            "SELECT livestream_id, tag_id FROM livestream_tags WHERE livestream_id IN (",
        );
        let mut separated = query_builder.separated(", ");
        for livestream_id in livestream_ids {
            separated.push_bind(livestream_id);
        }
        separated.push_unseparated(")");
        let query = query_builder.build_query_as();
        let livestream_tag_models: Vec<(i64, i64)> =
            query.fetch_all(&mut *tx).await?;
        for livestream_tag_model in livestream_tag_models {
            tags_memo
                .entry(livestream_tag_model.0)
                .or_insert_with(Vec::new)
                .push(livestream_tag_model.1);
        }
        let user_ids: Vec<i64> = chunk.iter().map(|ls| ls.user_id).collect();
        let mut query_builder = sqlx::query_builder::QueryBuilder::new("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id IN(");
        let mut separated = query_builder.separated(", ");
        for user_id in user_ids {
            separated.push_bind(user_id);
        }
        separated.push_unseparated(")");
        let query = query_builder.build_query_as();
        let user_models: Vec<FilledUserModel> = query.fetch_all(&mut *tx).await?;
        for user_model in user_models {
            owners_memo.insert(user_model.id, user_model.into());
        }
    }


    let mut livestreams = Vec::with_capacity(livestream_models.len());
    for livestream_model in livestream_models {
        let owner = owners_memo.get(&livestream_model.user_id).unwrap();
        let livestream = fill_livestream_response2(owner, &tags_memo, livestream_model).await?;
        livestreams.push(livestream);
    }
    tx.commit().await?;

    Ok(axum::Json(livestreams))
}

async fn get_my_livestreams_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
) -> Result<axum::Json<Vec<Livestream>>, Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let livestream_models: Vec<LivestreamModel> =
        sqlx::query_as("SELECT * FROM livestreams WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&mut *tx)
            .await?;
    let mut livestreams = Vec::with_capacity(livestream_models.len());
    let owner = get_user(&mut tx, user_id).await?;
    for livestream_model in livestream_models {
        let livestream = fill_livestream_response(&mut tx, &owner, livestream_model).await?;
        livestreams.push(livestream);
    }

    tx.commit().await?;

    Ok(axum::Json(livestreams))
}

async fn get_user_livestreams_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((username,)): Path<(String,)>,
) -> Result<axum::Json<Vec<Livestream>>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let user: UserModel = sqlx::query_as("SELECT * FROM users WHERE name = ?")
        .bind(username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("user not found".into()))?;

    let livestream_models: Vec<LivestreamModel> =
        sqlx::query_as("SELECT * FROM livestreams WHERE user_id = ?")
            .bind(user.id)
            .fetch_all(&mut *tx)
            .await?;
    let mut livestreams = Vec::with_capacity(livestream_models.len());
    let owner = get_user(&mut tx, user.id).await?;
    for livestream_model in livestream_models {
        let livestream = fill_livestream_response(&mut tx, &owner, livestream_model).await?;
        livestreams.push(livestream);
    }

    tx.commit().await?;

    Ok(axum::Json(livestreams))
}

// viewerテーブルの廃止
async fn enter_livestream_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<(), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let created_at = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO livestream_viewers_history (user_id, livestream_id, created_at) VALUES(?, ?, ?)",
    )
    .bind(user_id)
    .bind(livestream_id)
    .bind(created_at)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

async fn exit_livestream_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<(), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM livestream_viewers_history WHERE user_id = ? AND livestream_id = ?")
        .bind(user_id)
        .bind(livestream_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(())
}

async fn get_livestream_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<axum::Json<Livestream>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(livestream_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound(
                "not found livestream that has the given id".into(),
            ))?;
    let user = get_user(&mut tx, livestream_model.user_id).await?;

    let livestream = fill_livestream_response(&mut tx, &user, livestream_model).await?;

    tx.commit().await?;

    Ok(axum::Json(livestream))
}

async fn get_livecomment_reports_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<axum::Json<Vec<LivecommentReport>>, Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(livestream_id)
            .fetch_one(&mut *tx)
            .await?;

    if livestream_model.user_id != user_id {
        return Err(Error::Forbidden(
            "can't get other streamer's livecomment reports".into(),
        ));
    }

    let report_models: Vec<LivecommentReportModel> =
        sqlx::query_as("SELECT * FROM livecomment_reports WHERE livestream_id = ?")
            .bind(livestream_id)
            .fetch_all(&mut *tx)
            .await?;

    let mut reports = Vec::with_capacity(report_models.len());
    for report_model in report_models {
        let report = fill_livecomment_report_response(&mut tx, report_model).await?;
        reports.push(report);
    }

    tx.commit().await?;

    Ok(axum::Json(reports))
}

async fn get_user(tx: &mut MySqlConnection, user_id: i64) -> sqlx::Result<User> {
    let user_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(user_id)
        .fetch_one(tx)
        .await?;
    Ok(user_model.into())
}

async fn fill_livestream_response2(
    owner: &User,
    tags_memo: &HashMap<i64, Vec<i64>>,
    livestream_model: LivestreamModel,
) -> sqlx::Result<Livestream> {
    let mut tags = Vec::new();
    let tag_ids = tags_memo.get(&livestream_model.id);
    if let Some(tag_ids) = tag_ids {
        for &tag_id in tag_ids {
            let tag_name = TAG_MODELS
                .read()
                .await
                .get(&tag_id)
                .cloned()
                .ok_or(sqlx::Error::RowNotFound)?;
            tags.push(Tag {
                id: tag_id,
                name: tag_name,
            });
        }
    }

    Ok(Livestream {
        id: livestream_model.id,
        owner: owner.clone(),
        title: livestream_model.title,
        tags,
        description: livestream_model.description,
        playlist_url: livestream_model.playlist_url,
        thumbnail_url: livestream_model.thumbnail_url,
        start_at: livestream_model.start_at,
        end_at: livestream_model.end_at,
    })
}
async fn fill_livestream_response(
    tx: &mut MySqlConnection,
    owner: &User,
    livestream_model: LivestreamModel,
) -> sqlx::Result<Livestream> {
    let livestream_tag_models: Vec<(i64,)> =
        sqlx::query_as("SELECT tag_id FROM livestream_tags WHERE livestream_id = ?")
            .bind(livestream_model.id)
            .fetch_all(&mut *tx)
            .await?;

    let mut tags = Vec::with_capacity(livestream_tag_models.len());
    for tag_id in livestream_tag_models {
        let tag_name = TAG_MODELS
            .read()
            .await
            .get(&tag_id.0)
            .cloned()
            .ok_or(sqlx::Error::RowNotFound)?;
        tags.push(Tag {
            id: tag_id.0,
            name: tag_name,
        });
    }

    Ok(Livestream {
        id: livestream_model.id,
        owner: owner.clone(),
        title: livestream_model.title,
        tags,
        description: livestream_model.description,
        playlist_url: livestream_model.playlist_url,
        thumbnail_url: livestream_model.thumbnail_url,
        start_at: livestream_model.start_at,
        end_at: livestream_model.end_at,
    })
}

#[derive(Debug, serde::Deserialize)]
struct PostLivecommentRequest {
    comment: String,
    tip: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LivecommentModel {
    id: i64,
    user_id: i64,
    livestream_id: i64,
    comment: String,
    tip: i64,
    created_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct Livecomment {
    id: i64,
    user: User,
    livestream: Livestream,
    comment: String,
    tip: i64,
    created_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct LivecommentReport {
    id: i64,
    reporter: User,
    livecomment: Livecomment,
    created_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct LivecommentReportModel {
    id: i64,
    user_id: i64,
    #[allow(unused)]
    livestream_id: i64,
    livecomment_id: i64,
    created_at: i64,
}

#[derive(Debug, serde::Deserialize)]
struct ModerateRequest {
    ng_word: String,
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
struct NgWord {
    id: i64,
    user_id: i64,
    livestream_id: i64,
    word: String,
    #[sqlx(default)]
    created_at: i64,
}

#[derive(Debug, serde::Deserialize)]
struct GetLivecommentsQuery {
    #[serde(default)]
    limit: String,
}

async fn get_livecomments_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
    Query(GetLivecommentsQuery { limit }): Query<GetLivecommentsQuery>,
) -> Result<axum::Json<Vec<Livecomment>>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let mut query =
        "SELECT * FROM livecomments WHERE livestream_id = ? ORDER BY created_at DESC".to_owned();
    if !limit.is_empty() {
        let limit: i64 = limit.parse().map_err(|_| Error::BadRequest("".into()))?;
        query = format!("{} LIMIT {}", query, limit);
    }

    let livecomment_models: Vec<LivecommentModel> = sqlx::query_as(&query)
        .bind(livestream_id)
        .fetch_all(&mut *tx)
        .await?;

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(livestream_id)
            .fetch_one(&mut *tx)
            .await?;
    let owner = get_user(&mut tx, livestream_model.user_id).await?;
    let livestream = fill_livestream_response(&mut tx, &owner, livestream_model).await?;

    let mut livecomments = Vec::with_capacity(livecomment_models.len());
    for livecomment_model in livecomment_models {
        let livecomment = fill_livecomment_response2(&mut tx, &livestream, livecomment_model).await?;
        livecomments.push(livecomment);
    }

    tx.commit().await?;

    Ok(axum::Json(livecomments))
}

async fn get_ngwords(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<axum::Json<Vec<NgWord>>, Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let ng_words: Vec<NgWord> = sqlx::query_as(
        "SELECT * FROM ng_words WHERE user_id = ? AND livestream_id = ? ORDER BY created_at DESC",
    )
    .bind(user_id)
    .bind(livestream_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(axum::Json(ng_words))
}

async fn post_livecomment_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
    axum::Json(req): axum::Json<PostLivecommentRequest>,
) -> Result<(StatusCode, axum::Json<Livecomment>), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(livestream_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound("livestream not found".into()))?;

    // スパム判定
    let ngwords: Vec<NgWord> =
        sqlx::query_as("SELECT id, user_id, livestream_id, word FROM ng_words WHERE user_id = ? AND livestream_id = ?")
            .bind(livestream_model.user_id)
            .bind(livestream_model.id)
            .fetch_all(&mut *tx)
            .await?;
    for ngword in &ngwords {
        let query = r#"
        SELECT COUNT(*)
        FROM
        (SELECT ? AS text) AS texts
        INNER JOIN
        (SELECT CONCAT('%', ?, '%')	AS pattern) AS patterns
        ON texts.text LIKE patterns.pattern;
        "#;
        let hit_spam: i64 = sqlx::query_scalar(query)
            .bind(&req.comment)
            .bind(&ngword.word)
            .fetch_one(&mut *tx)
            .await?;
        tracing::info!("[hit_spam={}] comment = {}", hit_spam, req.comment);
        if hit_spam >= 1 {
            return Err(Error::BadRequest(
                "このコメントがスパム判定されました".into(),
            ));
        }
    }

    let now = Utc::now().timestamp();

    let rs = sqlx::query(
        "INSERT INTO livecomments (user_id, livestream_id, comment, tip, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(livestream_id)
    .bind(&req.comment)
    .bind(req.tip)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let livecomment_id = rs.last_insert_id() as i64;

    let livecomment = fill_livecomment_response(
        &mut tx,
        LivecommentModel {
            id: livecomment_id,
            user_id,
            livestream_id,
            comment: req.comment,
            tip: req.tip,
            created_at: now,
        },
    )
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, axum::Json(livecomment)))
}

async fn report_livecomment_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id, livecomment_id)): Path<(i64, i64)>,
) -> Result<(StatusCode, axum::Json<LivecommentReport>), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let _: LivestreamModel = sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
        .bind(livestream_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("livestream not found".into()))?;

    let _: LivecommentModel = sqlx::query_as("SELECT * FROM livecomments WHERE id = ?")
        .bind(livecomment_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("livecomment not found".into()))?;

    let now = Utc::now().timestamp();
    let rs = sqlx::query(
        "INSERT INTO livecomment_reports(user_id, livestream_id, livecomment_id, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(livestream_id)
    .bind(livecomment_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let report_id = rs.last_insert_id() as i64;

    let report = fill_livecomment_report_response(
        &mut tx,
        LivecommentReportModel {
            id: report_id,
            user_id,
            livestream_id,
            livecomment_id,
            created_at: now,
        },
    )
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, axum::Json(report)))
}

#[derive(Debug, serde::Serialize)]
struct ModerateResponse {
    word_id: i64,
}

// NGワードを登録
async fn moderate_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
    axum::Json(req): axum::Json<ModerateRequest>,
) -> Result<(StatusCode, axum::Json<ModerateResponse>), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    // 配信者自身の配信に対するmoderateなのかを検証
    let owned_livestreams: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM livestreams WHERE id = ? AND user_id = ? LIMIT 1")
            .bind(livestream_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    if owned_livestreams.is_none() {
        return Err(Error::BadRequest(
            "A streamer can't moderate livestreams that other streamers own".into(),
        ));
    }

    let created_at = Utc::now().timestamp();
    let pattern = format!("%{}%", req.ng_word);
    let rs = sqlx::query(
        "INSERT INTO ng_words(user_id, livestream_id, word, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(livestream_id)
    .bind(req.ng_word)
    .bind(created_at)
    .execute(&mut *tx)
    .await?;
    let word_id = rs.last_insert_id() as i64;

    // NGワードにヒットする過去の投稿も全削除する
    let query = r#"
    DELETE FROM livecomments
    WHERE
    livestream_id = ? AND
    comment LIKE ?
    "#;
    sqlx::query(query)
        .bind(livestream_id)
        .bind(pattern)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        axum::Json(ModerateResponse { word_id }),
    ))
}

async fn fill_livecomment_response(
    tx: &mut MySqlConnection,
    livecomment_model: LivecommentModel,
) -> sqlx::Result<Livecomment> {
    let comment_owner_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(livecomment_model.user_id)
        .fetch_one(&mut *tx)
        .await?;
    let comment_owner = comment_owner_model.into();

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(livecomment_model.livestream_id)
            .fetch_one(&mut *tx)
            .await?;
    let owner = get_user(&mut *tx, livestream_model.user_id).await?;
    let livestream = fill_livestream_response(&mut *tx, &owner, livestream_model).await?;

    Ok(Livecomment {
        id: livecomment_model.id,
        user: comment_owner,
        livestream,
        comment: livecomment_model.comment,
        tip: livecomment_model.tip,
        created_at: livecomment_model.created_at,
    })
}

async fn fill_livecomment_response2(
    tx: &mut MySqlConnection,
    livestream: &Livestream,
    livecomment_model: LivecommentModel,
) -> sqlx::Result<Livecomment> {
    let comment_owner_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(livecomment_model.user_id)
        .fetch_one(&mut *tx)
        .await?;
    let comment_owner = comment_owner_model.into();

    Ok(Livecomment {
        id: livecomment_model.id,
        user: comment_owner,
        livestream: livestream.clone(),
        comment: livecomment_model.comment,
        tip: livecomment_model.tip,
        created_at: livecomment_model.created_at,
    })
}

async fn fill_livecomment_report_response(
    tx: &mut MySqlConnection,
    report_model: LivecommentReportModel,
) -> sqlx::Result<LivecommentReport> {
    let reporter_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(report_model.user_id)
        .fetch_one(&mut *tx)
        .await?;
    let reporter: User = reporter_model.into();

    let livecomment_model: LivecommentModel =
        sqlx::query_as("SELECT * FROM livecomments WHERE id = ?")
            .bind(report_model.livecomment_id)
            .fetch_one(&mut *tx)
            .await?;
    let livecomment = fill_livecomment_response(&mut *tx, livecomment_model).await?;

    Ok(LivecommentReport {
        id: report_model.id,
        reporter,
        livecomment,
        created_at: report_model.created_at,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct ReactionModel {
    id: i64,
    emoji_name: String,
    user_id: i64,
    livestream_id: i64,
    created_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct Reaction {
    id: i64,
    emoji_name: String,
    user: User,
    livestream: Livestream,
    created_at: i64,
}

#[derive(Debug, serde::Deserialize)]
struct PostReactionRequest {
    emoji_name: String,
}

#[derive(Debug, serde::Deserialize)]
struct GetReactionsQuery {
    #[serde(default)]
    limit: String,
}

async fn get_reactions_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
    Query(GetReactionsQuery { limit }): Query<GetReactionsQuery>,
) -> Result<axum::Json<Vec<Reaction>>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let mut query =
        "SELECT * FROM reactions WHERE livestream_id = ? ORDER BY created_at DESC".to_owned();
    if !limit.is_empty() {
        let limit: i64 = limit.parse().map_err(|_| Error::BadRequest("".into()))?;
        query = format!("{} LIMIT {}", query, limit);
    }

    let reaction_models: Vec<ReactionModel> = sqlx::query_as(&query)
        .bind(livestream_id)
        .fetch_all(&mut *tx)
        .await?;

    let mut reactions = Vec::with_capacity(reaction_models.len());
    for reaction_model in reaction_models {
        let reaction = fill_reaction_response(&mut tx, reaction_model).await?;
        reactions.push(reaction);
    }

    tx.commit().await?;

    Ok(axum::Json(reactions))
}

async fn post_reaction_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
    axum::Json(req): axum::Json<PostReactionRequest>,
) -> Result<(StatusCode, axum::Json<Reaction>), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let mut tx = pool.begin().await?;

    let created_at = Utc::now().timestamp();
    let result =
        sqlx::query("INSERT INTO reactions (user_id, livestream_id, emoji_name, created_at) VALUES (?, ?, ?, ?)")
            .bind(user_id)
            .bind(livestream_id)
            .bind(&req.emoji_name)
            .bind(created_at)
            .execute(&mut *tx)
            .await?;
    let reaction_id = result.last_insert_id() as i64;

    let reaction = fill_reaction_response(
        &mut tx,
        ReactionModel {
            id: reaction_id,
            user_id,
            livestream_id,
            emoji_name: req.emoji_name,
            created_at,
        },
    )
    .await?;

    tx.commit().await?;

    Ok((StatusCode::CREATED, axum::Json(reaction)))
}

async fn fill_reaction_response(
    tx: &mut MySqlConnection,
    reaction_model: ReactionModel,
) -> sqlx::Result<Reaction> {
    let user_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(reaction_model.user_id)
        .fetch_one(&mut *tx)
        .await?;
    let user: User = user_model.into();

    let livestream_model: LivestreamModel =
        sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
            .bind(reaction_model.livestream_id)
            .fetch_one(&mut *tx)
            .await?;
    let livestream = fill_livestream_response(&mut *tx, &user, livestream_model).await?;

    Ok(Reaction {
        id: reaction_model.id,
        emoji_name: reaction_model.emoji_name,
        user,
        livestream,
        created_at: reaction_model.created_at,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct UserModel {
    id: i64,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    #[sqlx(default, rename = "password")]
    hashed_password: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct FilledUserModel {
    id: i64,
    name: String,
    display_name: Option<String>,
    description: Option<String>,
    theme_id: i64,
    dark_mode: bool,
    icon_hash: Option<String>,
}

impl From<FilledUserModel> for User {
    fn from(model: FilledUserModel) -> Self {
        User {
            id: model.id,
            name: model.name,
            display_name: model.display_name,
            description: model.description,
            theme: Theme {
                id: model.theme_id,
                dark_mode: model.dark_mode,
            },
            icon_hash: model.icon_hash.unwrap_or_else(|| FALLBACK_IMAGE_HASH.clone()),
        }
    }

}

#[derive(Debug, Clone, serde::Serialize)]
struct User {
    id: i64,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    theme: Theme,
    icon_hash: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct Theme {
    id: i64,
    dark_mode: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct ThemeModel {
    id: i64,
    #[allow(unused)]
    user_id: i64,
    dark_mode: bool,
}

#[derive(Debug, serde::Deserialize)]
struct PostUserRequest {
    name: String,
    display_name: String,
    description: String,
    // password is non-hashed password.
    password: String,
    theme: PostUserRequestTheme,
}

#[derive(Debug, serde::Deserialize)]
struct PostUserRequestTheme {
    dark_mode: bool,
}

#[derive(Debug, serde::Deserialize)]
struct LoginRequest {
    username: String,
    // password is non-hashed password.
    password: String,
}

#[derive(Debug, serde::Deserialize)]
struct PostIconRequest {
    #[serde(deserialize_with = "from_base64")]
    image: Vec<u8>,
}
fn from_base64<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use base64::Engine as _;
    use serde::de::{Deserialize as _, Error as _};
    let value = String::deserialize(deserializer)?;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(D::Error::custom)
}

#[derive(Debug, serde::Serialize)]
struct PostIconResponse {
    id: i64,
}

async fn get_icon_handler(
    headers: HeaderMap,
    State(AppState { pool, .. }): State<AppState>,
    Path((username,)): Path<(String,)>,
) -> Result<axum::response::Response, Error> {
    use axum::response::IntoResponse as _;

    let mut tx = pool.begin().await?;

    let user: UserModel = sqlx::query_as("SELECT * FROM users WHERE name = ? LIMIT 1")
        .bind(username)
        .fetch_one(&mut *tx)
        .await?;

    if let Some(hash) = headers.get("If-None-Match") {
        //info!("If-None-Match: {:?}", hash);
        if let Ok(hash) = hash.to_str() {
            // remove double-quate
            let hash = hash.trim_matches('"');
            if let Some(cached_hash) = ICON_HASH_CACHE.get(&user.id).await {
                if hash == &cached_hash {
                    return Ok((StatusCode::NOT_MODIFIED, ()).into_response());
                }
            }
            let image_hash: Option<String> =
                sqlx::query_scalar("SELECT image_hash FROM icons WHERE user_id = ? LIMIT 1")
                    .bind(user.id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if image_hash == Some(hash.to_owned()) {
                return Ok((StatusCode::NOT_MODIFIED, ()).into_response());
            }
        }
    }

    let image: Option<(Vec<u8>, String)> =
        sqlx::query_as("SELECT image, image_hash FROM icons WHERE user_id = ? LIMIT 1")
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?;

    let headers = [(axum::http::header::CONTENT_TYPE, "image/jpeg")];
    if let Some(image) = image {
        ICON_HASH_CACHE.insert(user.id, image.1).await;
        Ok((headers, image.0).into_response())
    } else {
        Ok((headers, FALLBACK_IMAGE_BYTES.clone()).into_response())
    }
}

async fn post_icon_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    axum::Json(req): axum::Json<PostIconRequest>,
) -> Result<(StatusCode, axum::Json<PostIconResponse>), Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;
    use sha2::digest::Digest as _;
    let icon_hash = sha2::Sha256::digest(&req.image);
    let icon_hash = format!("{:x}", icon_hash);

    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM icons WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    let rs = sqlx::query("INSERT INTO icons (user_id, image, image_hash) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(req.image)
        .bind(icon_hash.clone())
        .execute(&mut *tx)
        .await?;
    let icon_id = rs.last_insert_id() as i64;

    tx.commit().await?;
    ICON_HASH_CACHE.insert(user_id, icon_hash).await;

    Ok((
        StatusCode::CREATED,
        axum::Json(PostIconResponse { id: icon_id }),
    ))
}

async fn get_me_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
) -> Result<axum::Json<User>, Error> {
    verify_user_session(&jar).await?;

    let cookie = jar.get(DEFAULT_SESSION_ID_KEY).ok_or(Error::SessionError)?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::SessionError)?;
    let user_id: i64 = sess.get(DEFAULT_USER_ID_KEY).ok_or(Error::SessionError)?;

    let user_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.id = ?")
        .bind(user_id)
        .fetch_optional(&pool)
        .await?
        .ok_or(Error::NotFound(
            "not found user that has the userid in session".into(),
        ))?;

    let user: User = user_model.into();

    Ok(axum::Json(user))
}

// ユーザ登録API
// POST /api/register
async fn register_handler(
    State(AppState {
        pool,
        powerdns_subdomain_address,
        ..
    }): State<AppState>,
    axum::Json(req): axum::Json<PostUserRequest>,
) -> Result<(StatusCode, axum::Json<User>), Error> {
    if req.name == "pipe" {
        return Err(Error::BadRequest("the username 'pipe' is reserved".into()));
    }

    const BCRYPT_DEFAULT_COST: u32 = 4;
    let hashed_password = bcrypt::hash(&req.password, BCRYPT_DEFAULT_COST)?;

    let mut tx = pool.begin().await?;

    let result = sqlx::query(
        "INSERT INTO users (name, display_name, description, password) VALUES(?, ?, ?, ?)",
    )
    .bind(&req.name)
    .bind(&req.display_name)
    .bind(&req.description)
    .bind(&hashed_password)
    .execute(&mut *tx)
    .await?;
    let user_id = result.last_insert_id() as i64;

    let result = sqlx::query("INSERT INTO themes (user_id, dark_mode) VALUES(?, ?)")
        .bind(user_id)
        .bind(req.theme.dark_mode)
        .execute(&mut *tx)
        .await?;
    let theme_id =  result.last_insert_id() as i64;

    let output = tokio::process::Command::new("pdnsutil")
        .arg("add-record")
        .arg("u.isucon.dev")
        .arg(&req.name)
        .arg("A")
        .arg("0")
        .arg(&*powerdns_subdomain_address)
        .output()
        .await?;
    if !output.status.success() {
        return Err(Error::InternalServerError(format!(
            "pdnsutil failed with stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    ICON_HASH_CACHE
        .insert(user_id, FALLBACK_IMAGE_HASH.clone())
        .await;

    tx.commit().await?;

    let user: User = User {
        id: user_id,
        name: req.name,
        display_name: Some(req.display_name),
        description: Some(req.description),
        theme: Theme {
            id: theme_id,
            dark_mode: req.theme.dark_mode,
        },
        icon_hash: FALLBACK_IMAGE_HASH.clone(),
    };

    Ok((StatusCode::CREATED, axum::Json(user)))
}

#[derive(Debug, serde::Serialize)]
struct Session {
    id: String,
    user_id: i64,
    expires: i64,
}

// ユーザログインAPI
// POST /api/login
async fn login_handler(
    State(AppState { pool, .. }): State<AppState>,
    mut jar: SignedCookieJar,
    axum::Json(req): axum::Json<LoginRequest>,
) -> Result<(SignedCookieJar, ()), Error> {
    let mut tx = pool.begin().await?;

    // usernameはUNIQUEなので、whereで一意に特定できる
    let user_model: UserModel = sqlx::query_as("SELECT * FROM users WHERE name = ?")
        .bind(req.username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Unauthorized("invalid username or password".into()))?;

    tx.commit().await?;

    let hashed_password = user_model.hashed_password.unwrap();
    if !bcrypt::verify(&req.password, &hashed_password)? {
        return Err(Error::Unauthorized("invalid username or password".into()));
    }

    let session_end_at = Utc::now() + chrono::Duration::hours(1);
    let session_id = Uuid::new_v4().to_string();
    let mut sess = async_session::Session::new();
    sess.insert(DEFAULT_SESSION_ID_KEY, session_id).unwrap();
    sess.insert(DEFAULT_USER_ID_KEY, user_model.id).unwrap();
    sess.insert(DEFAULT_USERNAME_KEY, user_model.name).unwrap();
    sess.insert(DEFUALT_SESSION_EXPIRES_KEY, session_end_at.timestamp())
        .unwrap();
    let cookie_store = CookieStore::new();
    if let Some(cookie_value) = cookie_store.store_session(sess).await? {
        let cookie =
            axum_extra::extract::cookie::Cookie::build(DEFAULT_SESSION_ID_KEY, cookie_value)
                .domain("u.isucon.dev")
                .max_age(time::Duration::minutes(1000))
                .path("/")
                .finish();
        jar = jar.add(cookie);
    }

    Ok((jar, ()))
}

// ユーザ詳細API
// GET /api/user/:username
async fn get_user_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((username,)): Path<(String,)>,
) -> Result<axum::Json<User>, Error> {
    verify_user_session(&jar).await?;


    let user_model: FilledUserModel = sqlx::query_as("SELECT u.id as id, u.name as name, u.display_name as display_name, u.description as description, t.id as theme_id, t.dark_mode as dark_mode, i.image_hash as icon_hash FROM users u JOIN themes t ON u.id = t.user_id LEFT OUTER JOIN icons i ON u.id = i.user_id  WHERE u.name = ?")
        .bind(username)
        .fetch_optional(&pool)
        .await?
        .ok_or(Error::NotFound(
            "not found user that has the given username".into(),
        ))?;
    let user: User = user_model.into();


    Ok(axum::Json(user))
}

async fn verify_user_session(jar: &SignedCookieJar) -> Result<(), Error> {
    let cookie = jar
        .get(DEFAULT_SESSION_ID_KEY)
        .ok_or(Error::Forbidden("".into()))?;
    let sess = CookieStore::new()
        .load_session(cookie.value().to_owned())
        .await?
        .ok_or(Error::Forbidden("".into()))?;
    let session_expires: i64 = sess
        .get(DEFUALT_SESSION_EXPIRES_KEY)
        .ok_or(Error::Forbidden("".into()))?;
    let now = Utc::now();
    if now.timestamp() > session_expires {
        return Err(Error::Unauthorized("session has expired".into()));
    }
    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct LivestreamStatistics {
    rank: i64,
    viewers_count: i64,
    total_reactions: i64,
    total_reports: i64,
    max_tip: i64,
}

#[derive(Debug)]
struct LivestreamRankingEntry {
    livestream_id: i64,
    score: i64,
}

#[derive(Debug, serde::Serialize)]
struct UserStatistics {
    rank: i64,
    viewers_count: i64,
    total_reactions: i64,
    total_livecomments: i64,
    total_tip: i64,
    favorite_emoji: String,
}

#[derive(Debug)]
struct UserRankingEntry {
    username: String,
    score: i64,
}

/// MySQL で COUNT()、SUM() 等を使って DECIMAL 型の値になったものを i64 に変換するための構造体。
#[derive(Debug)]
struct MysqlDecimal(i64);
impl sqlx::Decode<'_, sqlx::MySql> for MysqlDecimal {
    fn decode(
        value: sqlx::mysql::MySqlValueRef,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use sqlx::{Type as _, ValueRef as _};

        let type_info = value.type_info();
        if i64::compatible(&type_info) {
            i64::decode(value).map(Self)
        } else if u64::compatible(&type_info) {
            let n = u64::decode(value)?.try_into()?;
            Ok(Self(n))
        } else if sqlx::types::Decimal::compatible(&type_info) {
            use num_traits::ToPrimitive as _;
            let n = sqlx::types::Decimal::decode(value)?
                .to_i64()
                .expect("failed to convert DECIMAL type to i64");
            Ok(Self(n))
        } else if sqlx::types::BigDecimal::compatible(&type_info) {
            use num_traits::ToPrimitive as _;
            let n = sqlx::types::BigDecimal::decode(value)?
                .to_i64()
                .expect("failed to convert BigDECIMAL type to i64");
            Ok(Self(n))
        } else {
            todo!()
        }
    }
}
impl sqlx::Type<sqlx::MySql> for MysqlDecimal {
    fn type_info() -> sqlx::mysql::MySqlTypeInfo {
        i64::type_info()
    }

    fn compatible(ty: &sqlx::mysql::MySqlTypeInfo) -> bool {
        i64::compatible(ty) || u64::compatible(ty) || sqlx::types::Decimal::compatible(ty)
    }
}
impl From<MysqlDecimal> for i64 {
    fn from(value: MysqlDecimal) -> Self {
        value.0
    }
}

async fn get_user_statistics_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((username,)): Path<(String,)>,
) -> Result<axum::Json<UserStatistics>, Error> {
    verify_user_session(&jar).await?;

    // ユーザごとに、紐づく配信について、累計リアクション数、累計ライブコメント数、累計売上金額を算出
    // また、現在の合計視聴者数もだす

    let mut tx = pool.begin().await?;

    let user: UserModel = sqlx::query_as("SELECT * FROM users WHERE name = ? LIMIT 1")
        .bind(&username)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::BadRequest("".into()))?;

    // ランク算出
    let users: Vec<UserModel> = sqlx::query_as("SELECT * FROM users")
        .fetch_all(&mut *tx)
        .await?;

    let mut ranking = Vec::new();
    let mut score_map = HashMap::new();
    let query = r#"
        SELECT u.name, COUNT(*) FROM users u
        INNER JOIN livestreams l ON l.user_id = u.id
        INNER JOIN reactions r ON r.livestream_id = l.id
        GROUP BY u.name
        "#;
    let reactions: Vec<(String, i64)> = sqlx::query_as(query).fetch_all(&mut *tx).await?;

    for (username, score) in reactions.iter() {
        score_map.insert(username.to_string(), *score);
    }

    let query = r#"
        SELECT u.name, IFNULL(SUM(l2.tip), 0) FROM users u
        INNER JOIN livestreams l ON l.user_id = u.id
        INNER JOIN livecomments l2 ON l2.livestream_id = l.id
        GROUP BY u.name
        "#;
    let tips: Vec<(String, BigDecimal)> = sqlx::query_as(query).fetch_all(&mut *tx).await?;

    for (username, score) in tips.iter() {
        let score = score.to_i64().unwrap();
        if let Some(s) = score_map.get_mut(username) {
            *s += score;
        } else {
            score_map.insert(username.to_string(), score);
        }
    }
    for user in users {
        let score = score_map.get(&user.name).copied().unwrap_or(0);
        ranking.push(UserRankingEntry {
            username: user.name,
            score,
        });
    }

    ranking.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.username.cmp(&b.username))
    });

    let rpos = ranking
        .iter()
        .rposition(|entry| entry.username == username)
        .unwrap();
    let rank = (ranking.len() - rpos) as i64;

    // リアクション数
    let query = r"#
    SELECT COUNT(*) FROM users u
    INNER JOIN livestreams l ON l.user_id = u.id
    INNER JOIN reactions r ON r.livestream_id = l.id
    WHERE u.name = ?
    #";
    let MysqlDecimal(total_reactions) = sqlx::query_scalar(query)
        .bind(&username)
        .fetch_one(&mut *tx)
        .await?;

    // ライブコメント数、チップ合計
    let (total_livecomments, total_tip): (i64, BigDecimal)  =
        sqlx::query_as("SELECT COUNT(*), SUM(lc.tip) FROM livestreams ls INNER JOIN livecomments lc ON lc.livestream_id = ls.id WHERE ls.user_id = ?")
            .bind(user.id)
            .fetch_one(&mut *tx)
            .await?;

    // 合計視聴者数
    let MysqlDecimal(viewers_count) = sqlx::query_scalar(
        "SELECT COUNT(*) FROM livestreams l INNER JOIN livestream_viewers_history h ON h.livestream_id = l.id WHERE l.user_id = ?",
    ).bind(user.id)
        .fetch_one(&mut *tx)
        .await?;


    // お気に入り絵文字
    let query = r#"
    SELECT r.emoji_name
    FROM users u
    INNER JOIN livestreams l ON l.user_id = u.id
    INNER JOIN reactions r ON r.livestream_id = l.id
    WHERE u.name = ?
    GROUP BY emoji_name
    ORDER BY COUNT(*) DESC, emoji_name DESC
    LIMIT 1
    "#;
    let favorite_emoji: String = sqlx::query_scalar(query)
        .bind(&username)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_default();

    Ok(axum::Json(UserStatistics {
        rank,
        viewers_count,
        total_reactions,
        total_livecomments: total_livecomments,
        total_tip: total_tip.to_i64().unwrap(),
        favorite_emoji,
    }))
}

async fn get_livestream_statistics_handler(
    State(AppState { pool, .. }): State<AppState>,
    jar: SignedCookieJar,
    Path((livestream_id,)): Path<(i64,)>,
) -> Result<axum::Json<LivestreamStatistics>, Error> {
    verify_user_session(&jar).await?;

    let mut tx = pool.begin().await?;

    let _: LivestreamModel = sqlx::query_as("SELECT * FROM livestreams WHERE id = ?")
        .bind(livestream_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::BadRequest("".into()))?;

    let livestreams: Vec<LivestreamModel> = sqlx::query_as("SELECT * FROM livestreams")
        .fetch_all(&mut *tx)
        .await?;

    // ランク算出
    let mut ranking = Vec::new();

    let reactions_map: Vec<(i64, i64)> = sqlx::query_as("SELECT l.id, COUNT(*) FROM livestreams l INNER JOIN reactions r ON l.id = r.livestream_id GROUP BY l.id")
        .fetch_all(&mut *tx)
        .await?;
    let total_tip_map: Vec<(i64, BigDecimal)> = sqlx::query_as("SELECT l.id, IFNULL(SUM(l2.tip), 0) FROM livestreams l INNER JOIN livecomments l2 ON l.id = l2.livestream_id GROUP BY l.id")
        .fetch_all(&mut *tx)
        .await?;

    let reactions_map: HashMap<i64, i64> = reactions_map.into_iter().collect();
    let total_tip_map: HashMap<i64, i64> = total_tip_map.into_iter().map(|(x, y)| (x, y.to_i64().unwrap_or(0))).collect();

    for livestream in livestreams {

        let score = reactions_map.get(&livestream.id).copied().unwrap_or(0) + total_tip_map.get(&livestream.id).copied().unwrap_or(0);
        ranking.push(LivestreamRankingEntry {
            livestream_id: livestream.id,
            score,
        })
    }
    ranking.sort_by(|a, b| {
        a.score
            .cmp(&b.score)
            .then_with(|| a.livestream_id.cmp(&b.livestream_id))
    });

    let rpos = ranking
        .iter()
        .rposition(|entry| entry.livestream_id == livestream_id)
        .unwrap();
    let rank = (ranking.len() - rpos) as i64;

    // 視聴者数算出
    let MysqlDecimal(viewers_count) = sqlx::query_scalar("SELECT COUNT(*) FROM livestreams l INNER JOIN livestream_viewers_history h ON h.livestream_id = l.id WHERE l.id = ?")
        .bind(livestream_id)
        .fetch_one(&mut *tx)
        .await?;

    // 最大チップ額
    let MysqlDecimal(max_tip) = sqlx::query_scalar("SELECT IFNULL(MAX(tip), 0) FROM livestreams l INNER JOIN livecomments l2 ON l2.livestream_id = l.id WHERE l.id = ?")
        .bind(livestream_id)
        .fetch_one(&mut *tx)
        .await?;

    // リアクション数
    let MysqlDecimal(total_reactions) = sqlx::query_scalar("SELECT COUNT(*) FROM livestreams l INNER JOIN reactions r ON r.livestream_id = l.id WHERE l.id = ?")
        .bind(livestream_id)
        .fetch_one(&mut *tx)
        .await?;

    // スパム報告数
    let MysqlDecimal(total_reports) = sqlx::query_scalar("SELECT COUNT(*) FROM livestreams l INNER JOIN livecomment_reports r ON r.livestream_id = l.id WHERE l.id = ?")
        .bind(livestream_id)
        .fetch_one(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(axum::Json(LivestreamStatistics {
        rank,
        viewers_count,
        max_tip,
        total_reactions,
        total_reports,
    }))
}

#[derive(Debug, serde::Serialize)]
struct PaymentResult {
    total_tip: i64,
}

async fn get_payment_result(
    State(AppState { pool, .. }): State<AppState>,
) -> Result<axum::Json<PaymentResult>, Error> {
    let mut tx = pool.begin().await?;

    let MysqlDecimal(total_tip) =
        sqlx::query_scalar("SELECT IFNULL(SUM(tip), 0) FROM livecomments")
            .fetch_one(&mut *tx)
            .await?;

    tx.commit().await?;

    Ok(axum::Json(PaymentResult { total_tip }))
}
