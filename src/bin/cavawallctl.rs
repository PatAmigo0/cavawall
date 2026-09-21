//! Query and control a running cavawall over its control socket.

use cavawall::control::{request, Request, Response};
use clap::{Parser, Subcommand};
use std::process::exit;

#[derive(Parser)]
#[command(name = "cavawallctl", about = "Query and control a running cavawall")]
struct Cli {
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What the running instance is doing
    Status,
    /// Move to an output, or omit the name for automatic placement
    Move { output: Option<String> },
    /// Clear the surface and exit
    Stop,
    /// Re-read config and palette without restarting
    Reload,
}

/// Errors to stderr, data to stdout, non-zero when the answer is no
fn main() {
    let cli = Cli::parse();
    let req = match &cli.command {
        Command::Status => Request::Status,
        Command::Move { output } => Request::Move { output: output.clone() },
        Command::Stop => Request::Stop,
        Command::Reload => Request::Reload,
    };

    let response = match request(&req) {
        Ok(r) => r,
        Err(e) => {
            if cli.json {
                println!(r#"{{"ok":false,"error":"not running"}}"#);
            } else {
                eprintln!("cavawallctl: not running ({e})");
            }
            exit(1);
        }
    };

    report(&cli, &response);
}

fn report(cli: &Cli, response: &Response) {
    if cli.json {
        println!("{}", serde_json::to_string(response).unwrap_or_default());
    } else if let Some(error) = &response.error {
        eprintln!("cavawallctl: {error}");
    } else if let Some(data) = &response.data {
        for (k, v) in data.as_object().into_iter().flatten() {
            println!("{k:<15} {}", v.as_str().map_or_else(|| v.to_string(), str::to_owned));
        }
    }
    if !response.ok {
        exit(1);
    }
}
