//! cosh-acp: protocol bridge between cosh-shell and ACP agents (ADR-011).
//!
//! One binary, two forms selected by argv:
//! - `bridge`: JSONL ⇄ ACP translation with agent process custody.
//! - `mcp-shell`: stdio MCP server proxying the shell Evidence Service (S2).
//!
//! stdout is reserved for protocol traffic in both forms; all logging goes
//! to stderr.

#![forbid(unsafe_code)]

mod bridge;
mod protocol;
mod session;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cosh-acp", version, about = "ACP bridge for cosh-shell")]
struct Cli {
    #[command(subcommand)]
    form: Form,
}

#[derive(Subcommand)]
enum Form {
    /// Run the JSONL ⇄ ACP bridge (stdin/stdout protocol stream).
    Bridge,
    /// Run the `cosh-shell` MCP server proxying the Evidence Service.
    McpShell,
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("COSH_ACP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> std::process::ExitCode {
    init_logging();
    let cli = Cli::parse();
    let code = match cli.form {
        Form::Bridge => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime");
            runtime.block_on(bridge::run())
        }
        Form::McpShell => {
            // Implemented in S2 (ADR-011 migration plan).
            tracing::error!("mcp-shell is not implemented yet");
            2
        }
    };
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
