mod cli;
mod config;
#[cfg(feature = "gui")]
mod gui;
mod layout;
mod markup;
mod model;
mod output;
mod pages;
mod pdf;
mod progress;
mod text;
mod translate;
mod typeset;
mod workflow;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, InspectArgs};
use crate::layout::LayoutOptions;

fn main() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(feature = "gui")]
    if matches!(cli.command, Command::Gui(_)) {
        return finish(run(cli));
    }

    // PDF parsers and recursive Form XObject graphs need a larger stack. Joining
    // also turns worker panics into a normal error exit.
    let worker = std::thread::Builder::new()
        .name("pdf-translate".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run(cli));

    match worker {
        Ok(handle) => match handle.join() {
            Ok(result) => finish(result),
            Err(_) => {
                eprintln!("error: internal panic; the input PDF may be malformed");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            eprintln!("error: failed to start worker thread: {err}");
            ExitCode::FAILURE
        }
    }
}

fn finish(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    init_logging();
    let config = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Inspect(args) => inspect(args),
        Command::Translate(args) => {
            let report = workflow::translate(args, &config)?;
            tracing::debug!(
                output = %report.output.display(),
                translated = report.translation.translated,
                cached = report.translation.cache_hits,
                typeset = report.typeset.paragraphs,
                "translation completed"
            );
            Ok(())
        }
        #[cfg(feature = "gui")]
        Command::Gui(args) => gui::run(config, args.input, cli.config.clone()),
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .try_init();
}

fn inspect(args: InspectArgs) -> Result<()> {
    let output = args
        .output
        .as_deref()
        .map(|path| output::destinations(&args.input, path, None))
        .transpose()?;
    let (_, document) = workflow::load_layout(&args.input, &args.pages, LayoutOptions::default())?;

    let json = if args.compact {
        serde_json::to_vec(&document)?
    } else {
        serde_json::to_vec_pretty(&document)?
    };

    if let Some((output, _)) = output {
        output::write_bytes(&output, &json)?;
        eprintln!(
            "wrote {} ({} pages, {} paragraphs) to {}",
            args.input.display(),
            document.stats.pages,
            document.stats.paragraphs,
            output.display()
        );
    } else {
        println!("{}", String::from_utf8_lossy(&json));
        eprintln!(
            "{}: {} pages, {} lines, {} paragraphs ({} protected)",
            args.input.display(),
            document.stats.pages,
            document.stats.lines,
            document.stats.paragraphs,
            document.stats.protected
        );
    }

    Ok(())
}
