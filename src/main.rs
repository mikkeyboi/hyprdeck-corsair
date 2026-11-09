//! hyprdeck-corsair: Hyprdeck plugin + opt-in live-mode companion for Corsair keyboards.
//! See docs/DESIGN.md.

mod bragi;
mod config;
mod daemon;
mod devices;
mod effects;
mod ipc;
mod layouts;
mod plugin;
mod rescue;
mod storage;

use std::io::{self, Read, Write};
use std::process::ExitCode;

const MAX_REQUEST: u64 = 1024 * 1024;

const HELP: &str = "hyprdeck-corsair — Corsair K65 PLUS Wireless / K100 RGB for Hyprdeck

Usage:
  hyprdeck-corsair --request               one Hyprdeck request on stdin, envelope on stdout
  hyprdeck-corsair --input-stream <id>     key-tester NDJSON stream (spawned by Hyprdeck)
  hyprdeck-corsair state                   print the plugin state (pretty JSON)
  hyprdeck-corsair action <name> [json]    run one plugin action (args JSON object)
  hyprdeck-corsair daemon                  run the live-mode companion (systemd --user)
  hyprdeck-corsair rescue [--all|<id>]     close handles and restore hardware mode
  hyprdeck-corsair --help | --version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--request"] => {
            let mut input = Vec::new();
            if let Err(error) = io::stdin().take(MAX_REQUEST + 1).read_to_end(&mut input) {
                eprintln!("read request: {error}");
                return ExitCode::FAILURE;
            }
            let envelope = if input.len() as u64 > MAX_REQUEST {
                serde_json::json!({"api_version": 1, "error": "request exceeds 1 MiB", "state": null})
            } else {
                plugin::run_request(&input)
            };
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{envelope}");
            ExitCode::SUCCESS
        }
        ["--input-stream", id] => report(plugin::run_input_stream(id)),
        ["state"] => print_cli(plugin::run_cli("state", None, None)),
        ["action", name] => print_cli(plugin::run_cli("action", Some(name), None)),
        ["action", name, json] => print_cli(plugin::run_cli("action", Some(name), Some(json))),
        ["daemon"] => report(daemon::run()),
        ["rescue"] | ["rescue", "--all"] => report_lines(rescue::run(None)),
        ["rescue", id] => report_lines(rescue::run(Some(id))),
        ["--version"] => {
            println!("hyprdeck-corsair {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        ["--help"] | ["-h"] | [] => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{HELP}");
            ExitCode::from(2)
        }
    }
}

fn print_cli(envelope: serde_json::Value) -> ExitCode {
    let failed = !envelope["error"].is_null();
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).unwrap_or_default()
    );
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn report(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hyprdeck-corsair: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn report_lines(result: anyhow::Result<Vec<String>>) -> ExitCode {
    match result {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("hyprdeck-corsair: {error:#}");
            ExitCode::FAILURE
        }
    }
}
