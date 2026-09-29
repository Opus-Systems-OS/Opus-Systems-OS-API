//! Opus Systems OS API. `serve` (default) runs the HTTP service; `keys`
//! manages client keys on the host — the only way to mint a `keys:admin` key.

use clap::{Parser, Subcommand};
use opus_api::auth::keys::Scope;
use opus_api::config::Config;
use opus_api::{db, error, v1};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "opus-api", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service (default).
    Serve,
    /// Manage client keys directly in the database (no server needed).
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Connect a briefing source that needs consent (Google, WHOOP).
    Oauth {
        #[command(subcommand)]
        action: OauthAction,
    },
}

#[derive(Subcommand)]
enum OauthAction {
    /// Print a consent URL (valid 15 minutes, once). Open it, approve, and
    /// the provider sends the browser back to this API, which keeps the
    /// tokens.
    Start {
        /// `google` (Gmail, Calendar, YouTube) or `whoop`.
        provider: String,
    },
    /// Which providers are connected, and when their tokens last changed.
    Status,
}

#[derive(Subcommand)]
enum KeysAction {
    /// Mint a key and print it once. This is the only path that may grant keys:admin.
    Create {
        /// Who holds it: "mac", "quest-3", "jarvis-ios".
        #[arg(long)]
        name: String,
        /// Comma-separated: fleet:read,sessions:read,sessions:write,usage:read,inference,keys:admin
        #[arg(long)]
        scopes: String,
    },
    /// List keys (never shows secrets).
    List,
    /// Revoke a key by its public id.
    Revoke {
        #[arg(long)]
        id: String,
    },
    /// Add scopes to an existing key (never keys:admin — only `create` mints that).
    Grant {
        #[arg(long)]
        id: String,
        /// Comma-separated, e.g. sources:read
        #[arg(long)]
        scopes: String,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .with_target(false)
        .init();

    if let Err(e) = run().await {
        tracing::error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

async fn run() -> error::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env()?;
    let db = db::Db::open(&cfg.database_path)?;

    match cli.command {
        Some(Command::Keys { action }) => return keys_command(&db, action),
        Some(Command::Oauth { action }) => return oauth_command(&db, &cfg, action),
        _ => {}
    }

    let control_plane = opus_api::upstream::control_plane::ControlPlane::new(
        &cfg.control_plane_url,
        &cfg.control_plane_token,
    )?;
    let limiter = std::sync::Arc::new(opus_api::auth::rate_limit::RateLimiter::new(
        cfg.rate_limit_per_minute,
    ));
    let voice = match cfg.voice.clone() {
        Some(v) => {
            tracing::info!(voice_id = %v.voice_id, model = v.model.as_deref().unwrap_or("provider default"), "voice configured (Fish Audio)");
            Some(opus_api::upstream::fish_audio::FishAudio::new(v)?)
        }
        None => {
            tracing::info!("no FISH_AUDIO_API_KEY — /v1/voice/* disabled");
            None
        }
    };
    let ops = if cfg.ops.any() {
        let ops = opus_api::upstream::ops::Ops::new(cfg.ops.clone())?;
        tracing::info!(services = ?ops.configured(), "ops configured");
        Some(ops)
    } else {
        tracing::info!("no service tokens — /v1/ops disabled");
        None
    };
    let sources = opus_api::upstream::sources::Sources::new(cfg.sources.clone(), db.clone())?;
    tracing::info!(sources = ?sources.configured(), "briefing sources configured");
    let app = opus_api::app(
        v1::AppState {
            db,
            control_plane,
            limiter,
            voice: std::sync::Arc::new(voice),
            ops: std::sync::Arc::new(ops),
            sources: std::sync::Arc::new(Some(sources)),
            pairings: Default::default(),
        },
        &cfg.allowed_origins,
    );

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| error::Error::Config(format!("bind {addr}: {e}")))?;
    tracing::info!(
        %addr,
        control_plane = %cfg.control_plane_url,
        db = %cfg.database_path.display(),
        rate_limit_per_minute = cfg.rate_limit_per_minute,
        cors_origins = cfg.allowed_origins.len(),
        "opus-api listening"
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| error::Error::Config(format!("serve: {e}")))?;
    Ok(())
}

fn keys_command(db: &db::Db, action: KeysAction) -> error::Result<()> {
    match action {
        KeysAction::Create { name, scopes } => {
            let scopes = Scope::parse_list(&scopes).map_err(error::Error::InvalidRequest)?;
            let created = v1::keys::create_key(db, &name, &scopes)?;
            println!("id:      {}", created.id);
            println!("name:    {}", created.name);
            println!("scopes:  {}", Scope::join(&created.scopes));
            println!("key:     {}", created.key);
            println!("(shown once — it is stored only as a hash)");
        }
        KeysAction::List => {
            for k in db.keys()? {
                println!(
                    "{}  {:<16} {:<60} created {}  last used {}  {}",
                    k.id,
                    k.name,
                    Scope::join(&k.scopes),
                    k.created_at,
                    k.last_used_at.as_deref().unwrap_or("never"),
                    if k.revoked_at.is_some() {
                        "REVOKED"
                    } else {
                        ""
                    }
                );
            }
        }
        KeysAction::Grant { id, scopes } => {
            let add = Scope::parse_list(&scopes).map_err(error::Error::InvalidRequest)?;
            if add.contains(&Scope::KeysAdmin) {
                return Err(error::Error::InvalidRequest(
                    "keys:admin is only granted by `keys create`".into(),
                ));
            }
            match db.grant_scopes(&id, &add)? {
                Some(now) => println!("{id}  scopes: {}", Scope::join(&now)),
                None => return Err(error::Error::NotFound),
            }
        }
        KeysAction::Revoke { id } => {
            if db.revoke_key(&id)? {
                println!("revoked {id}");
            } else {
                return Err(error::Error::NotFound);
            }
        }
    }
    Ok(())
}

fn oauth_command(db: &db::Db, cfg: &Config, action: OauthAction) -> error::Result<()> {
    use opus_api::upstream::sources::oauth::{self, Provider};
    match action {
        OauthAction::Start { provider } => {
            let p = Provider::parse(&provider).ok_or_else(|| {
                error::Error::InvalidRequest("provider must be `google` or `whoop`".into())
            })?;
            let url = oauth::start(db, &cfg.sources, p)?;
            println!("Open this within 15 minutes and approve:\n\n{url}\n");
            println!(
                "The redirect URI registered with {} must be exactly:\n  {}",
                p.name(),
                oauth::redirect_uri(&cfg.sources, p)
            );
        }
        OauthAction::Status => {
            for p in [Provider::Google, Provider::Whoop] {
                let configured = p.client(&cfg.sources).is_some();
                let token = db.oauth_token(p.as_str())?;
                println!(
                    "{:<7} client {:<14} {}",
                    p.as_str(),
                    if configured {
                        "configured"
                    } else {
                        "NOT configured"
                    },
                    match token {
                        Some(t) => format!("connected (tokens updated {})", t.updated_at),
                        None => "not connected".into(),
                    }
                );
            }
        }
    }
    Ok(())
}
