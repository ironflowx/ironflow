//! The flags, their environment twins and defaults for `serve`.

use std::env;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Arg, ArgMatches, Command, value_parser};
use ironflowx_core::{ErrorKind, Secret};
use ironflowx_operations::key_id;
use ironflowx_proto::OpError;
use tracing::level_filters::LevelFilter;

struct Setting {
    flag: &'static str,
    variable: &'static str,
    default: Option<&'static str>,
}

impl Setting {
    fn arg(&self, value: &'static str, help: &'static str) -> Arg {
        let suffix = match self.default {
            Some(default) => format!(" [env: {}; default: {default}]", self.variable),
            None => format!(" [env: {}; or supply IRONFLOWX_SEED]", self.variable),
        };
        Arg::new(self.flag)
            .long(self.flag)
            .value_name(value)
            .help(format!("{help}{suffix}"))
    }

    fn read<T: Clone + Send + Sync + 'static>(
        &self,
        args: &ArgMatches,
        parse: impl FnOnce(OsString) -> Option<T>,
    ) -> Result<Option<T>, OpError> {
        if let Some(value) = args.get_one::<T>(self.flag) {
            return Ok(Some(value.clone()));
        }
        env::var_os(self.variable)
            .or_else(|| self.default.map(OsString::from))
            .map(|value| parse(value).ok_or_else(|| invalid(format!("invalid {}", self.variable))))
            .transpose()
    }
}

const DATA_DIR: Setting = Setting {
    flag: "data-dir",
    variable: "IRONFLOWX_DATA_DIR",
    default: Some("./.ironflowx"),
};
const LISTEN: Setting = Setting {
    flag: "listen",
    variable: "IRONFLOWX_LISTEN",
    default: Some("127.0.0.1:9310"),
};
const SEED_FILE: Setting = Setting {
    flag: "seed-file",
    variable: "IRONFLOWX_SEED_FILE",
    default: None,
};
const LOG_LEVEL: Setting = Setting {
    flag: "log-level",
    variable: "IRONFLOWX_LOG_LEVEL",
    default: Some("info"),
};
const LOG_FORMAT: Setting = Setting {
    flag: "log-format",
    variable: "IRONFLOWX_LOG_FORMAT",
    default: Some("json"),
};

const SEED: &str = "IRONFLOWX_SEED";
const LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
const FORMATS: [&str; 2] = ["json", "text"];

pub fn data_dir_arg(help: &'static str) -> Arg {
    DATA_DIR
        .arg("PATH", help)
        .value_parser(value_parser!(PathBuf))
}

pub fn data_dir(args: &ArgMatches) -> Result<PathBuf, OpError> {
    let dir = DATA_DIR.read(args, path)?.expect("has a default");
    if dir.as_os_str().is_empty() {
        return Err(invalid("--data-dir must not be empty"));
    }
    Ok(dir)
}

pub fn command() -> Command {
    Command::new("serve")
        .about("Open the engine in a data directory and serve it")
        .arg(data_dir_arg("The data directory, created if missing"))
        .arg(
            LISTEN
                .arg(
                    "ADDRESS",
                    "The IP address and port to listen on; port 0 picks one",
                )
                .value_parser(value_parser!(SocketAddr)),
        )
        .arg(
            SEED_FILE
                .arg("PATH", "Read the seed from a file")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            LOG_LEVEL
                .arg("LEVEL", "The maximum log verbosity")
                .value_parser(LEVELS),
        )
        .arg(
            LOG_FORMAT
                .arg("FORMAT", "The log format")
                .value_parser(FORMATS),
        )
}

pub struct Settings {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub seed: Option<Secret>,
    pub log_level: LevelFilter,
    pub log_format: String,
}

impl Settings {
    pub fn read(args: &ArgMatches) -> Result<Self, OpError> {
        let data_dir = data_dir(args)?;
        let listen = LISTEN
            .read(args, |value| value.to_str()?.parse().ok())?
            .expect("has a default");
        let log_level = LOG_LEVEL
            .read(args, |value| choice(value, &LEVELS))?
            .expect("has a default")
            .parse()
            .expect("a supported level");
        let log_format = LOG_FORMAT
            .read(args, |value| choice(value, &FORMATS))?
            .expect("has a default");
        let seed_file = SEED_FILE.read(args, path)?;
        let malformed = || invalid(ironflowx_operations::EngineError::MalformedSeed.to_string());
        let seed = match (seed_file, env::var_os(SEED)) {
            (Some(_), Some(_)) => return Err(invalid("supply only one seed source")),
            (Some(path), None) => {
                let input = std::fs::read_to_string(path)
                    .map_err(|_| invalid("cannot read the seed file as UTF-8"))?;
                Some(Secret::new(input))
            }
            (None, Some(value)) => Some(Secret::new(value.into_string().map_err(|_| malformed())?)),
            (None, None) => None,
        };
        let seed = seed
            .map(|input| {
                let trimmed = Secret::new(input.expose().trim().to_owned());
                key_id(&trimmed).ok_or_else(malformed)?;
                Ok(trimmed)
            })
            .transpose()?;
        Ok(Self {
            data_dir,
            listen,
            seed,
            log_level,
            log_format,
        })
    }
}

fn path(value: OsString) -> Option<PathBuf> {
    (!value.is_empty()).then(|| value.into())
}

fn choice(value: OsString, choices: &[&str]) -> Option<String> {
    let value = value.into_string().ok()?;
    choices.contains(&value.as_str()).then_some(value)
}

pub fn warn_unread_variables() {
    let known = [
        DATA_DIR.variable,
        LISTEN.variable,
        SEED_FILE.variable,
        SEED,
        LOG_LEVEL.variable,
        LOG_FORMAT.variable,
        "IRONFLOWX_URL",
        "IRONFLOWX_KEY",
    ];
    for (name, _) in env::vars_os() {
        const PREFIX: &[u8] = b"IRONFLOWX_";
        let Some(suffix) = name.as_encoded_bytes().strip_prefix(PREFIX) else {
            continue;
        };
        let name = name.to_str();
        if name.is_some_and(|name| known.contains(&name)) {
            continue;
        }
        // A variable name can itself contain a pasted key or arbitrary bytes.
        let label = if !suffix.windows(4).any(|part| part == b"ixk_")
            && suffix.len() < 43
            && suffix
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            name.expect("ASCII name")
        } else {
            "IRONFLOWX_<hidden>"
        };
        tracing::warn!(target: "ironflowx_bin::settings", variable = label, "{label} is not a setting");
    }
}

fn invalid(message: impl Into<String>) -> OpError {
    OpError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        hint: None,
    }
}
