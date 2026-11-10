//! `lg`: the littleguy command.
//!
//! - `lg serve` runs the daemon. It owns the database and the assistant.
//! - `lg chat <text>` sends one message to the daemon and prints the answer.
//! - `lg pair <code>`, `lg pairs` and `lg unpair <sender>` manage who may
//!   talk to the assistant on Discord and Slack.
//! - `lg connect <integration>`, `lg connections` and `lg disconnect` manage
//!   the OAuth accounts that San Mateo keeps tokens for.
//!
//! With `DISCORD_BOT_TOKEN` set, `lg serve` also answers Discord direct
//! messages. With `SLACK_BOT_TOKEN` and `SLACK_APP_TOKEN` set, it answers
//! Slack direct messages over Socket Mode. Each token comes from the
//! environment only. Each
//! `[[camel.route]]` of the config feeds the assistant too.
//!
//! `lg serve --model claude`, `openai` or `openrouter` talks to a hosted
//! model. Each reads its key from the environment only, so the key never
//! shows in a process list.

mod approvals;
mod brief;
mod camel;
mod chats;
mod client;
mod daemon;
mod guard;
pub mod mandate_bridge;
mod memory;
mod oauth;
mod pairs;
mod paths;
mod protocol;
mod sandbox;
mod tools;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use autumn_harvest_agent::AgentModel;
use clap::{Parser, Subcommand, ValueEnum};
use littleguy_channels::{DiscordRest, SlackApi, discord, slack};
use littleguy_core::{Config, OwnerName};
use littleguy_models::{ClaudeModel, OfflineModel, OpenAiModel, Provider};

use crate::paths::Env;
use crate::protocol::Request;

/// An always-on personal agent on autumn-harvest.
#[derive(Debug, Parser)]
#[command(name = "lg", version, about)]
struct Cli {
    /// The daemon socket. Defaults to `<data home>/littleguy/lg.sock`.
    #[arg(long, global = true, env = "LG_SOCKET")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon.
    Serve(ServeArgs),
    /// Send one message and print the answer.
    Chat(ChatArgs),
    /// Let the stranger who got `code` talk to the assistant.
    Pair {
        /// The code from the pairing message, such as `ABCD-1234`.
        code: String,
    },
    /// List the senders that may talk to the assistant.
    Pairs,
    /// Take a paired sender off its channel.
    Unpair {
        /// The sender id, such as `discord:123456789012345678`.
        sender: String,
    },
    /// Print a link that connects an account of an OAuth integration.
    Connect {
        /// The integration key from the config, such as `google`.
        integration: String,
    },
    /// List the OAuth connections.
    Connections,
    /// Delete one OAuth connection.
    Disconnect {
        /// The integration key.
        integration: String,
        /// The connection id, from `lg connections`.
        connection: String,
    },
    /// List the calls that wait for an approval.
    Approvals,
    /// Run the call that waits as request `id`.
    Approve {
        /// The request number, from `lg approvals`.
        id: i64,
    },
    /// Refuse the call that waits as request `id`.
    Deny {
        /// The request number, from `lg approvals`.
        id: i64,
        /// Why, for the assistant.
        #[arg(long)]
        reason: Option<String>,
    },
}

/// The models that `lg serve` can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelChoice {
    /// The offline echo model. It needs no key and no network.
    Offline,
    /// Claude over the Messages API. It needs `ANTHROPIC_API_KEY`.
    Claude,
    /// OpenAI, or any server that speaks its API. It needs `OPENAI_API_KEY`
    /// and `--model-name`.
    #[value(name = "openai")]
    OpenAi,
    /// OpenRouter. It needs `OPENROUTER_API_KEY` and `--model-name`.
    #[value(name = "openrouter")]
    OpenRouter,
}

/// The variable that holds the Claude API key.
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The variable that holds the Discord bot token.
const DISCORD_TOKEN_VAR: &str = "DISCORD_BOT_TOKEN";

/// The variable that holds the Slack bot token (`xoxb-`).
const SLACK_BOT_VAR: &str = "SLACK_BOT_TOKEN";

/// The variable that holds the Slack app-level token (`xapp-`), for Socket
/// Mode.
const SLACK_APP_VAR: &str = "SLACK_APP_TOKEN";

/// The directory of the raw Claude turns, next to the database.
const TURN_DIR: &str = "claude-turns";

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// The config file. Defaults to `<config home>/littleguy/lg.toml`.
    #[arg(long, env = "LG_CONFIG")]
    config: Option<PathBuf>,
    /// The database. Defaults to `<data home>/littleguy/lg.db`.
    #[arg(long, env = "LG_DB")]
    db: Option<PathBuf>,
    /// The model of the assistant.
    #[arg(long, value_enum, default_value = "offline")]
    model: ModelChoice,
    /// The model to ask. Claude defaults to `claude-opus-5-5`. OpenAI and
    /// OpenRouter have no default. OpenRouter names take a vendor prefix,
    /// such as `<vendor>/<model>`.
    #[arg(long, env = "LG_MODEL_NAME", alias = "claude-model")]
    model_name: Option<String>,
    /// The base URL of an OpenAI-compatible server, such as a local one.
    /// Defaults to the provider's own.
    #[arg(long, env = "LG_BASE_URL")]
    base_url: Option<String>,
    /// The Messages API endpoint. For tests.
    #[arg(long, env = "LG_CLAUDE_ENDPOINT", hide = true)]
    claude_endpoint: Option<String>,
    /// The Discord REST base URL. For tests.
    #[arg(long, env = "LG_DISCORD_API", hide = true)]
    discord_api: Option<String>,
    /// The Discord gateway URL. For tests.
    #[arg(long, env = "LG_DISCORD_GATEWAY", hide = true)]
    discord_gateway: Option<String>,
    /// The Slack Web API base URL. For tests.
    #[arg(long, env = "LG_SLACK_API", hide = true)]
    slack_api: Option<String>,
    /// Append one line per offline model call to this file. For tests.
    #[arg(long, env = "LG_OFFLINE_CALL_LOG", hide = true)]
    offline_call_log: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct ChatArgs {
    /// The message.
    text: String,
    /// The message id. A retry with the same id delivers the message once.
    #[arg(long)]
    message_id: Option<String>,
}

/// Reads the config. Without `--config` and with no default file, the owner
/// is `$USER` and every other key takes its default.
fn load_config(explicit: Option<&Path>, env: &Env) -> Result<Config> {
    if let Some(path) = explicit {
        return Ok(Config::load(path)?);
    }
    let path = env.config()?;
    if path.exists() {
        return Ok(Config::load(&path)?);
    }
    let user = std::env::var("USER").context("set USER, or write a config with `owner`")?;
    Ok(Config::for_owner(OwnerName::new(user)?))
}

/// The model that `args` names. `key` reads one environment variable.
/// The raw Claude turns live next to `db`, so a restart replays each
/// thinking block.
fn build_model(
    args: &ServeArgs,
    db: &Path,
    key: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn AgentModel>> {
    let key_of = |var: &str, model: &str| {
        key(var)
            .filter(|key| !key.trim().is_empty())
            .with_context(|| format!("set {var} to run --model {model}"))
    };
    Ok(match args.model {
        ModelChoice::Offline => Arc::new(
            args.offline_call_log
                .clone()
                .map_or_else(OfflineModel::new, OfflineModel::with_call_log),
        ),
        ModelChoice::Claude => {
            let turns = db.parent().unwrap_or_else(|| Path::new(".")).join(TURN_DIR);
            let mut model = ClaudeModel::new(key_of(API_KEY_VAR, "claude")?).turn_dir(turns);
            if let Some(name) = &args.model_name {
                model = model.model(name);
            }
            if let Some(endpoint) = &args.claude_endpoint {
                model = model.endpoint(endpoint);
            }
            Arc::new(model)
        }
        ModelChoice::OpenAi | ModelChoice::OpenRouter => {
            let (provider, flag) = if args.model == ModelChoice::OpenAi {
                (Provider::OpenAi, "openai")
            } else {
                (Provider::OpenRouter, "openrouter")
            };
            let api_key = key_of(provider.key_var(), flag)?;
            let name = args
                .model_name
                .as_deref()
                .with_context(|| format!("--model {flag} needs --model-name"))?;
            let mut model = OpenAiModel::new(provider, api_key, name);
            if let Some(base_url) = &args.base_url {
                model = model.base_url(base_url);
            }
            Arc::new(model)
        }
    })
}

/// The Discord link, when `key` holds a bot token.
fn discord_link(
    args: &ServeArgs,
    key: impl Fn(&str) -> Option<String>,
) -> Option<daemon::DiscordLink> {
    let token = key(DISCORD_TOKEN_VAR).filter(|token| !token.trim().is_empty())?;
    let base = args
        .discord_api
        .clone()
        .unwrap_or_else(|| String::from(discord::API_BASE));
    Some(daemon::DiscordLink {
        rest: DiscordRest::new(base, token.clone()),
        token,
        gateway_url: args
            .discord_gateway
            .clone()
            .unwrap_or_else(|| String::from(discord::GATEWAY)),
    })
}

/// The Slack client, when `key` holds both Slack tokens. One token alone
/// is a mistake, so the log says which is missing.
fn slack_link(args: &ServeArgs, key: impl Fn(&str) -> Option<String>) -> Option<SlackApi> {
    let token = |var| key(var).filter(|token: &String| !token.trim().is_empty());
    match (token(SLACK_BOT_VAR), token(SLACK_APP_VAR)) {
        (Some(bot), Some(app)) => {
            let base = args
                .slack_api
                .clone()
                .unwrap_or_else(|| String::from(slack::API_BASE));
            Some(SlackApi::new(base, bot, app))
        }
        (None, None) => None,
        (Some(_), None) => {
            tracing::warn!("{SLACK_BOT_VAR} is set without {SLACK_APP_VAR}: Slack stays off");
            None
        }
        (None, Some(_)) => {
            tracing::warn!("{SLACK_APP_VAR} is set without {SLACK_BOT_VAR}: Slack stays off");
            None
        }
    }
}

async fn serve(socket: PathBuf, args: ServeArgs, env: &Env) -> Result<()> {
    let config = load_config(args.config.as_deref(), env)?;
    let db = match args.db.clone() {
        Some(db) => db,
        None => env.db()?,
    };
    let model = build_model(&args, &db, |var| std::env::var(var).ok())?;
    let links = daemon::Links {
        discord: discord_link(&args, |var| std::env::var(var).ok()),
        slack: slack_link(&args, |var| std::env::var(var).ok()),
    };
    let uid = rustix::process::getuid().as_raw();
    // San Mateo starts first, because the Google tools send through it.
    let connections = match &config.san_mateo {
        Some(section) => {
            let key = std::env::var(oauth::KEY_VAR).with_context(|| {
                format!(
                    "set {}, 32 random bytes in base64, to keep OAuth connections",
                    oauth::KEY_VAR
                )
            })?;
            let started = oauth::Connections::start(
                section,
                config.owner.as_str(),
                &db,
                &key,
                |var| std::env::var(var).ok(),
                oauth::Hooks::default(),
            )
            .await?;
            Some(started)
        }
        None => None,
    };
    let sandbox = config.sandbox.as_ref().map(sandbox::runtime).transpose()?;
    let mcp = tools::mcp_tools(&config.mcp).await;
    let google = connections
        .as_ref()
        .zip(config.google.as_ref())
        .map(|(started, google)| {
            Arc::new(started.google(&google.integration))
                as Arc<dyn littleguy_tools::google::GoogleApi>
        });
    let extras = daemon::Extras {
        sandbox,
        mcp,
        google,
        slack: links.slack.is_some(),
    };
    let discord = links.discord.is_some();
    let mut daemon = daemon::Daemon::open_with(&db, &config, model, uid, discord, extras)?;
    if let Some(started) = connections {
        daemon = daemon.with_connections(started);
    }
    let listener = daemon::bind(&socket).await?;
    let stop = async {
        // A failed signal handler leaves only a kill to stop the daemon.
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    let camel = if config.camel.is_empty() {
        None
    } else {
        Some(camel::Camel::start(daemon.camel_mapper(&config), &db).await?)
    };
    daemon::run(daemon, listener, links, camel, stop).await?;
    // The socket file goes with the daemon. A crash leaves it, and the next
    // start replaces it.
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Sends one request and prints the answer.
async fn ask(socket: &Path, request: &Request) -> Result<bool> {
    let response = client::send(socket, request).await?;
    let (text, ok) = client::render(&response);
    println!("{text}");
    Ok(ok)
}

async fn run(cli: Cli) -> Result<bool> {
    let env = Env::from_process();
    let socket = match cli.socket {
        Some(socket) => socket,
        None => env.socket()?,
    };
    match cli.command {
        Command::Serve(args) => Box::pin(serve(socket, args, &env)).await.map(|()| true),
        Command::Chat(args) => {
            let request = Request::Chat {
                message_id: args.message_id.unwrap_or_else(client::fresh_message_id),
                text: args.text,
            };
            ask(&socket, &request).await
        }
        Command::Pair { code } => ask(&socket, &Request::Pair { code }).await,
        Command::Pairs => ask(&socket, &Request::Pairs).await,
        Command::Unpair { sender } => ask(&socket, &Request::Unpair { sender }).await,
        Command::Connect { integration } => ask(&socket, &Request::Connect { integration }).await,
        Command::Connections => ask(&socket, &Request::Connections).await,
        Command::Disconnect {
            integration,
            connection,
        } => {
            let request = Request::Disconnect {
                integration,
                connection,
            };
            ask(&socket, &request).await
        }
        Command::Approvals => ask(&socket, &Request::Approvals).await,
        Command::Approve { id } => ask(&socket, &Request::Approve { id }).await,
        Command::Deny { id, reason } => ask(&socket, &Request::Deny { id, reason }).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LG_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    match Box::pin(run(Cli::parse())).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve_args(words: &[&str]) -> ServeArgs {
        let cli = Cli::try_parse_from(words).unwrap();
        match cli.command {
            Command::Serve(args) => args,
            _ => panic!("not serve"),
        }
    }

    #[test]
    fn the_claude_model_needs_a_key() {
        let args = serve_args(&["lg", "serve", "--model", "claude"]);
        let db = Path::new("/d/lg.db");
        let error = build_model(&args, db, |_| None).err().unwrap();
        assert!(error.to_string().contains(API_KEY_VAR));
        assert!(build_model(&args, db, |_| Some(String::from("  "))).is_err());
    }

    #[test]
    fn the_claude_model_takes_its_name_and_endpoint() {
        let args = serve_args(&[
            "lg",
            "serve",
            "--model",
            "claude",
            "--model-name",
            "claude-sonnet-5-5",
            "--claude-endpoint",
            "http://127.0.0.1:1/v1/messages",
        ]);
        let model = build_model(&args, Path::new("/d/lg.db"), |_| Some(String::from("k"))).unwrap();
        assert_eq!(model.model_id(), Some("claude-sonnet-5-5"));
        let args = serve_args(&["lg", "serve", "--model", "claude"]);
        let model = build_model(&args, Path::new("lg.db"), |_| Some(String::from("k"))).unwrap();
        assert_eq!(
            model.model_id(),
            Some("claude-opus-5-5+effort=high+fallbacks=default")
        );
    }

    #[test]
    fn the_old_claude_model_flag_still_works() {
        let args = serve_args(&["lg", "serve", "--claude-model", "x"]);
        assert_eq!(args.model_name.as_deref(), Some("x"));
    }

    #[test]
    fn openai_and_openrouter_read_their_own_key_and_need_a_name() {
        let only =
            |wanted: &'static str| move |var: &str| (var == wanted).then(|| String::from("k"));
        let db = Path::new("/d/lg.db");
        let args = serve_args(&["lg", "serve", "--model", "openai", "--model-name", "gpt-x"]);
        let model = build_model(&args, db, only("OPENAI_API_KEY")).unwrap();
        assert_eq!(model.model_id(), Some("openai:gpt-x"));
        let error = build_model(&args, db, only("OPENROUTER_API_KEY"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("OPENAI_API_KEY"));

        let args = serve_args(&[
            "lg",
            "serve",
            "--model",
            "openrouter",
            "--model-name",
            "a/b",
            "--base-url",
            "http://127.0.0.1:1/v1",
        ]);
        let model = build_model(&args, db, only("OPENROUTER_API_KEY")).unwrap();
        assert_eq!(model.model_id(), Some("openrouter:a/b"));

        let nameless = serve_args(&["lg", "serve", "--model", "openrouter"]);
        let error = build_model(&nameless, db, only("OPENROUTER_API_KEY"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("--model-name"));
    }

    #[test]
    fn slack_runs_only_with_both_tokens() {
        let args = serve_args(&["lg", "serve"]);
        assert!(slack_link(&args, |_| None).is_none());
        let only = |name: &'static str| move |var: &str| (var == name).then(|| String::from("t"));
        assert!(slack_link(&args, only(SLACK_BOT_VAR)).is_none());
        assert!(slack_link(&args, only(SLACK_APP_VAR)).is_none());
        assert!(slack_link(&args, |_| Some(String::from(" "))).is_none());
        let api = slack_link(&args, |_| Some(String::from("t"))).unwrap();
        assert!(format!("{api:?}").contains(slack::API_BASE));
        let args = serve_args(&["lg", "serve", "--slack-api", "http://127.0.0.1:1/api"]);
        let api = slack_link(&args, |_| Some(String::from("t"))).unwrap();
        assert!(format!("{api:?}").contains("127.0.0.1:1"));
    }

    #[test]
    fn discord_runs_only_with_a_token() {
        let args = serve_args(&["lg", "serve"]);
        assert!(discord_link(&args, |_| None).is_none());
        assert!(discord_link(&args, |_| Some(String::from(" "))).is_none());
        let link = discord_link(&args, |var| {
            (var == DISCORD_TOKEN_VAR).then(|| String::from("secret-token"))
        })
        .unwrap();
        assert_eq!(link.gateway_url, discord::GATEWAY);
        assert!(!format!("{link:?}").contains("secret-token"));
        let args = serve_args(&[
            "lg",
            "serve",
            "--discord-api",
            "http://127.0.0.1:1/api",
            "--discord-gateway",
            "ws://127.0.0.1:1",
        ]);
        let link = discord_link(&args, |_| Some(String::from("t"))).unwrap();
        assert_eq!(link.gateway_url, "ws://127.0.0.1:1");
    }

    #[test]
    fn the_cli_parses_the_pairing_commands() {
        let cli = Cli::try_parse_from(["lg", "pair", "ABCD-1234"]).unwrap();
        assert!(matches!(cli.command, Command::Pair { ref code } if code == "ABCD-1234"));
        let cli = Cli::try_parse_from(["lg", "pairs"]).unwrap();
        assert!(matches!(cli.command, Command::Pairs));
        let cli = Cli::try_parse_from(["lg", "connect", "google"]).unwrap();
        assert!(
            matches!(cli.command, Command::Connect { ref integration } if integration == "google")
        );
        let cli = Cli::try_parse_from(["lg", "connections"]).unwrap();
        assert!(matches!(cli.command, Command::Connections));
        let cli = Cli::try_parse_from(["lg", "disconnect", "google", "c-1"]).unwrap();
        assert!(
            matches!(cli.command, Command::Disconnect { ref connection, .. } if connection == "c-1")
        );
        let cli = Cli::try_parse_from(["lg", "unpair", "discord:1"]).unwrap();
        assert!(matches!(cli.command, Command::Unpair { ref sender } if sender == "discord:1"));
    }

    #[test]
    fn the_offline_model_is_the_default() {
        let args = serve_args(&["lg", "serve"]);
        let model = build_model(&args, Path::new("/d/lg.db"), |_| None).unwrap();
        assert_eq!(model.model_id(), Some("littleguy-offline-1"));
    }

    #[test]
    fn an_explicit_config_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.toml");
        std::fs::write(&path, "owner = \"explicit\"").unwrap();
        let config = load_config(Some(&path), &Env::default()).unwrap();
        assert_eq!(config.owner.as_str(), "explicit");
    }

    #[test]
    fn the_default_config_file_is_read_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("littleguy").join("lg.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "owner = \"default\"\nwindow = 3").unwrap();
        let env = Env {
            config_home: Some(dir.path().to_path_buf()),
            ..Env::default()
        };
        let config = load_config(None, &env).unwrap();
        assert_eq!(config.owner.as_str(), "default");
        assert_eq!(config.window, 3);
    }

    #[test]
    fn a_bad_explicit_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "owner = 3").unwrap();
        assert!(load_config(Some(&path), &Env::default()).is_err());
    }

    #[test]
    fn the_cli_parses_serve_and_chat() {
        let cli = Cli::try_parse_from(["lg", "--socket", "/s", "chat", "hi", "--message-id", "m"])
            .unwrap();
        assert_eq!(cli.socket, Some(PathBuf::from("/s")));
        assert!(matches!(cli.command, Command::Chat(ChatArgs { ref text, .. }) if text == "hi"));
        let cli = Cli::try_parse_from(["lg", "serve", "--db", "/d"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Serve(ServeArgs {
                model: ModelChoice::Offline,
                ..
            })
        ));
    }
}
