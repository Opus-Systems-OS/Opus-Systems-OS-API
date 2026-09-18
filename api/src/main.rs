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

    if let Some(Command::Keys { action }) = cli.command {
        return keys_command(&db, action);
    }

    let control_plane = opus_api::upstream::control_plane::ControlPlane::new(
        &cfg.control_plane_url,
        &cfg.control_plane_token,
    )?;
    let app = opus_api::app(v1::AppState { db, control_plane });

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| error::Error::Config(format!("bind {addr}: {e}")))?;
    tracing::info!(%addr, control_plane = %cfg.control_plane_url, db = %cfg.database_path.display(), "opus-api listening");
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
