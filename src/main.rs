mod agent;
mod app;
mod client;
mod commands;
mod config;
mod failures;
mod grep;
mod host;
mod hostfs;
mod input;
mod list;
mod patch;
mod read;
mod repl;
mod root;
mod session;
mod token_count;
mod tool_call_id;
mod tool_reasoning;
mod transcript;
mod tui;
mod worker;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

use session::{NOT_RUN, Session};

// Only worker protocol configuration sets a limit; the TUI remains unlimited.
#[global_allocator]
static ALLOC: monty_alloc::LimitedAllocator = monty_alloc::LimitedAllocator;

#[derive(Subcommand)]
enum Mode {
    #[command(hide = true)]
    Subprocess,
}

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    mode: Option<Mode>,
    /// Continue the named session, appending to it. Without a name, list the saved
    /// sessions and exit.
    #[arg(long, value_name = "NAME")]
    resume: Option<Option<String>>,
    /// Replace `[chat] prompt` with literal text for this invocation.
    #[arg(long, value_name = "TEXT")]
    system_prompt: Option<String>,
}

/// Dispatch workers before creating the application runtime or reading configuration.
fn main() -> anyhow::Result<ExitCode> {
    let args = Args::parse();
    if matches!(args.mode, Some(Mode::Subprocess)) {
        return Ok(worker::run());
    }
    run_app(args)?;
    Ok(ExitCode::SUCCESS)
}

/// Load the user's session and start the interactive application.
#[tokio::main]
async fn run_app(args: Args) -> anyhow::Result<()> {
    let dir = config::harness_dir()?;
    // Listing needs no config, so it works even while the config is broken.
    if let Some(None) = args.resume {
        for name in session::list(&dir)? {
            println!("{name}");
        }
        return Ok(());
    }
    // Config and session errors are reported before the terminal UI starts.
    let mut settings = config::load(&dir)?;
    // Replaces `[chat] prompt` for this invocation: the root's system message, and the
    // subagents' too unless `[rlm] subagent_prompt` is set. Requests and transcript
    // rendering both resolve it through `Settings::system_prompt`.
    if let Some(prompt) = args.system_prompt {
        settings.chat_prompt = Some(prompt);
    }
    let session = match args.resume {
        Some(Some(name)) => {
            let (session, repaired) = Session::resume(&dir, &name)?;
            // Printed to the normal screen before the TUI's alternate screen, so it's
            // there after exit.
            if repaired > 0 {
                println!(
                    "{}: added {NOT_RUN} for {repaired} tool calls without results",
                    session.path().display()
                );
            }
            session
        }
        _ => Session::new(&dir)?,
    };
    app::run(settings, session).await
}
