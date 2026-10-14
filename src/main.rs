use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::Result;
use lazygrafana::config::{Config, GraphicsMode};

/// A lazygit-style terminal UI for operating Grafana.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Config file (default: ~/.config/lazygrafana/config.yml)
    #[arg(short, long, env = "LAZYGRAFANA_CONFIG")]
    config: Option<PathBuf>,

    /// Context to start with
    #[arg(short = 'x', long)]
    context: Option<String>,

    /// Graphics mode: auto, kitty, sixel, iterm, unicode
    #[arg(short, long)]
    graphics: Option<GraphicsMode>,

    /// Initial time range, e.g. now-24h or "now-2d to now-1d"
    #[arg(short, long)]
    time: Option<String>,

    /// Open this dashboard (UID) on start
    #[arg(short, long)]
    dashboard: Option<String>,

    /// Write debug logs to the state directory
    #[arg(long)]
    debug: bool,

    /// List configured contexts and exit
    #[arg(long)]
    list_contexts: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Manage tokens stored in the OS keyring
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(clap::Subcommand, Debug)]
enum AuthAction {
    /// Store a service account token for a context (read from stdin, not echoed)
    Set {
        /// Context name; its `keyring:` entry (or the context name) is used as the account
        context: String,
    },
    /// Check that a context's token resolves and the instance accepts it
    Check {
        /// Context name
        context: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref())?;
    if let Some(g) = cli.graphics {
        config.graphics = g;
    }
    if let Some(t) = cli.time {
        config.default_time = t;
    }
    if let Some(Command::Auth { action }) = cli.command {
        return auth(&config, action).await;
    }
    if cli.list_contexts {
        for name in config.context_names() {
            let c = &config.contexts[&name];
            println!("{name}\t{}{}", c.url, if c.readonly { "\t(read-only)" } else { "" });
        }
        return Ok(());
    }
    let _guard = init_logging(cli.debug);
    lazygrafana::app::run(config, cli.context, cli.dashboard).await
}

async fn auth(config: &Config, action: AuthAction) -> Result<()> {
    use color_eyre::eyre::bail;
    match action {
        AuthAction::Set { context } => {
            let Some(ctx) = config.contexts.get(&context) else { bail!("unknown context '{context}'") };
            let entry = ctx.keyring.clone().unwrap_or_else(|| context.clone());
            let (service, account) = lazygrafana::config::keyring_entry(&entry);
            let token = read_secret(&format!("Token for '{context}' (service account token, input hidden): "))?;
            if token.trim().is_empty() {
                bail!("empty token, nothing stored");
            }
            lazygrafana::config::keyring_set(&service, &account, token.trim())?;
            println!("stored in keyring as service={service} account={account}");
            if ctx.keyring.is_none() {
                println!("add this to the context in your config:\n  keyring: {entry}");
            }
            Ok(())
        }
        AuthAction::Check { context } => {
            let ctx = config.resolve_context(&context)?;
            if let Some(e) = &ctx.token_error {
                bail!("token: {e}");
            }
            let client = lazygrafana::grafana::Client::new(&ctx)?;
            let health = client.health().await?;
            let user = client.user().await?;
            println!(
                "ok: Grafana {} at {} — signed in as {} (org {})",
                health.version,
                ctx.url,
                if user.login.is_empty() { user.name } else { user.login },
                user.org_id
            );
            Ok(())
        }
    }
}

/// Reads a line from the terminal without echo (falls back to plain stdin when piped).
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        return Ok(line);
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut out = String::new();
    let result = loop {
        match crossterm::event::read() {
            Ok(crossterm::event::Event::Key(k)) if k.kind != crossterm::event::KeyEventKind::Release => match k.code {
                crossterm::event::KeyCode::Enter => break Ok(()),
                crossterm::event::KeyCode::Char('c') if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                    break Err(color_eyre::eyre::eyre!("cancelled"));
                }
                crossterm::event::KeyCode::Char(c) => out.push(c),
                crossterm::event::KeyCode::Backspace => {
                    out.pop();
                }
                _ => {}
            },
            Ok(crossterm::event::Event::Paste(s)) => out.push_str(&s),
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    eprintln!();
    result.map(|_| out)
}

fn init_logging(debug: bool) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = lazygrafana::config::state_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let file = tracing_appender::rolling::never(&dir, "lazygrafana.log");
    // The log may contain URLs and error bodies; keep it private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("lazygrafana.log");
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(&path);
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = if debug { "lazygrafana=debug,warn" } else { "lazygrafana=info,warn" };
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("LAZYGRAFANA_LOG").unwrap_or_else(|_| filter.into()))
        .init();
    Some(guard)
}
