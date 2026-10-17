mod clients;
mod commands;
mod config;
mod credentials;
mod daemon;
mod doctor;
mod identity;
mod index;
mod logging;
mod oauth;
mod paths;
mod project;
mod protocol;
mod registry;
mod secrets;
mod session;
mod shim;
mod trust;
mod upstream;
mod vault;

use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand};

use crate::paths::Paths;

/// mise for MCP servers: one entry in every agent, the right servers and accounts per directory.
#[derive(Parser)]
#[command(name = "plugbay", version, about)]
struct Cli {
    /// Log level for plugbay itself (`debug`, `trace`; dependencies stay at `warn`), or a full
    /// filter like `debug,rmcp=debug` for everything. Defaults to `warn` for commands and `info`
    /// for `connect` and `daemon`. Falls back to `PLUGBAY_LOG`.
    #[arg(long, global = true, env = "PLUGBAY_LOG")]
    log_level: Option<String>,

    /// Act as if started in this directory instead of the current one.
    #[arg(
        short = 'C',
        long = "cd",
        alias = "project",
        global = true,
        value_name = "DIR"
    )]
    cd: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Stdio MCP server for agents: forwards to the plugbay daemon, starting it if needed.
    Connect,
    /// Run the daemon in the foreground (normally started by `connect`).
    Daemon {
        /// Exit after this many seconds without any connected agent.
        #[arg(long, default_value_t = 600)]
        idle_timeout: u64,
    },
    /// Ask a running daemon to exit (agents start a new one on their next connection).
    Stop,
    /// List the servers resolved for a directory.
    Ls {
        /// Every directory with a plugbay config that plugbay has seen, not just this one.
        #[arg(long, conflicts_with = "available")]
        all: bool,
        /// Every server you can pin: built in, and the catalog in your global config.
        #[arg(long)]
        available: bool,
    },
    /// Show which config file set each field of a server.
    Why {
        /// Server name as pinned, e.g. `linear`.
        server: String,
    },
    /// Pin servers for this directory: `plugbay use linear:initech sentry`.
    Use(commands::PinOptions),
    /// Remove pins, or switch off servers this directory inherits.
    Unuse(commands::PinOptions),
    /// Add a server definition to the catalog in the global config, pinned nowhere yet.
    Add(commands::AddOptions),
    /// Remove a server definition from the catalog.
    Remove {
        /// Catalog entry to remove; `plugbay unuse` stops using a server in one directory.
        name: String,
    },
    /// Every server account in use, whether it is logged in, and where it is used.
    Accounts,
    /// Log in with OAuth (`plugbay login linear:initech`) or store an API token (`--token`).
    /// Without a server, logs in to every account that still needs it.
    Login {
        /// `server` or `server:account`; found in this directory, else in any directory plugbay has seen.
        target: Option<String>,
        #[command(flatten)]
        token: commands::TokenOptions,
    },
    /// Forget the stored OAuth login or token for a server account.
    Logout {
        /// `server` or `server:account`, e.g. `linear:initech`.
        target: String,
    },
    /// Trust the executable parts (stdio commands, secret refs) of the config files for a directory.
    Trust {
        /// Remove trust instead of granting it.
        #[arg(long)]
        revoke: bool,
    },
    /// Register plugbay as an MCP server in Claude Code and Codex.
    Setup {
        /// Only set up these clients (`claude`, `codex`). Defaults to every installed client.
        #[arg(long = "client")]
        clients: Vec<String>,
        /// Print what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move every MCP server from Claude Code and Codex into plugbay: definitions, pins, secrets.
    Migrate(clients::MigrateOptions),
    /// Copy one project's native MCP servers into plugbay config (`migrate` does every project).
    #[command(hide = true)]
    Import(clients::ImportOptions),
    /// Check the installation, daemon, config and client setup.
    Doctor,
    /// Print shell completions: `plugbay completion zsh > ~/.zfunc/_plugbay`.
    Completion { shell: clap_complete::Shell },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(cli, paths))
}

async fn run(cli: Cli, paths: Paths) -> anyhow::Result<()> {
    let log_level = cli.log_level.as_deref();
    match cli.command {
        Command::Daemon { .. } => logging::init_daemon(&paths, log_level.unwrap_or("info"))?,
        Command::Connect => logging::init_stderr(log_level.unwrap_or("info")),
        _ => logging::init_stderr(log_level.unwrap_or("warn")),
    }

    let cd = cli.cd;
    match cli.command {
        Command::Connect => shim::run(&paths, cd).await,
        Command::Daemon { idle_timeout } => {
            daemon::run(&paths, Duration::from_secs(idle_timeout)).await
        }
        Command::Stop => commands::stop(&paths).await,
        Command::Ls { all: true, .. } => commands::ls_all(&paths, cd).await,
        Command::Ls { available, .. } => commands::ls(&paths, cd, available).await,
        Command::Why { server } => commands::why(&paths, &server, cd).await,
        Command::Use(options) => commands::use_servers(&paths, cd, options).await,
        Command::Unuse(options) => commands::unuse_servers(&paths, cd, options).await,
        Command::Add(options) => commands::add(&paths, options).await,
        Command::Remove { name } => commands::remove(&paths, &name).await,
        Command::Accounts => commands::accounts(&paths, cd).await,
        Command::Login { target, token } => {
            commands::login(&paths, target.as_deref(), cd, token).await
        }
        Command::Logout { target } => commands::logout(&paths, &target, cd).await,
        Command::Trust { revoke } => commands::trust(&paths, cd, revoke).await,
        Command::Setup { clients, dry_run } => clients::setup(&paths, &clients, dry_run).await,
        Command::Migrate(options) => clients::migrate(&paths, options).await,
        Command::Import(options) => clients::import(&paths, options).await,
        Command::Doctor => doctor::run(&paths, cd).await,
        Command::Completion { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "plugbay",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}
