//! `ironflowx`: the settings of `serve`, its log and its exit code, and the CLI.

#[cfg(feature = "__fault-points")]
mod crash_hook;
mod settings;

use std::io;
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command};
use ironflowx_core::ErrorKind;
use ironflowx_proto::OpError;
use tracing_subscriber::{Layer as _, filter, prelude::*};

fn main() -> ExitCode {
    #[cfg(feature = "__fault-points")]
    crash_hook::install();

    let root = Command::new("ironflowx")
        .subcommand(settings::command())
        .subcommand(
            Command::new("recover-key")
                .about("Recover an admin key from a local data directory")
                .arg(settings::data_dir_arg("The existing data directory"))
                .arg(
                    Arg::new("stdout")
                        .long("stdout")
                        .action(ArgAction::SetTrue)
                        .help(
                            "Print the recovered key and its key ID instead of writing admin.key",
                        ),
                ),
        )
        .subcommand(
            Command::new("seed")
                .about("Make a seed for an engine")
                .subcommand_required(true)
                .subcommand(Command::new("generate").about("Print a new seed and its key ID")),
        );
    let matches = match ironflowx_cli::parse(root) {
        Ok(matches) => matches,
        Err(code) => return code,
    };
    match matches.subcommand() {
        Some(("serve", serve)) => ironflowx_cli::end(run_serve(serve)),
        Some(("recover-key", args)) => ironflowx_cli::end(
            settings::data_dir(args)
                .and_then(|dir| ironflowx_bin::recover_key(&dir, args.get_flag("stdout"))),
        ),
        Some(("seed", _)) => ironflowx_cli::end(ironflowx_bin::generate_seed()),
        _ => ironflowx_cli::run(&matches),
    }
}

fn run_serve(args: &ArgMatches) -> Result<(), OpError> {
    let settings = settings::Settings::read(args)?;
    // Dependencies can trace request paths, headers and bodies. Only host and door events go out.
    let targets = filter::Targets::new()
        .with_target("ironflowx_bin", settings.log_level)
        .with_target("ironflowx_doors", settings.log_level);
    if settings.log_format == "json" {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(io::stderr)
                    .with_filter(targets),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(io::stderr)
                    .with_filter(targets),
            )
            .init();
    }
    settings::warn_unread_variables();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| internal(format!("cannot start the host runtime: {e}")))?;
    let result = runtime.block_on(ironflowx_bin::serve_with_seed(
        &settings.data_dir,
        settings.listen,
        settings.seed,
    ));
    // Engine::close has joined the writer. A dropped call can still have a blocking query.
    runtime.shutdown_background();
    result.map_err(|refused| {
        let error = refused.envelope();
        tracing::error!(target: "ironflowx_bin", "{}", error.message);
        error
    })
}

fn internal(message: String) -> OpError {
    OpError {
        kind: ErrorKind::Internal,
        message,
        hint: None,
    }
}
