//! The offline CLI catalog (D67).

use ironflowx_core::ErrorKind;
use ironflowx_proto::{OpError, Operation, required_scope, wire};
use serde_json::{Value, json};

use super::{Command, PROTOCOL, add_path, generated};

const EXIT_KINDS: &[ErrorKind] = &[
    ErrorKind::Internal,
    ErrorKind::Invalid,
    ErrorKind::Unauthenticated,
    ErrorKind::PermissionDenied,
    ErrorKind::NotFound,
    ErrorKind::Unknown,
    ErrorKind::ResourceExhausted,
    ErrorKind::Unavailable,
];

pub(super) fn command() -> clap::Command {
    let root = clap::Command::new("describe")
        .about("List operations and exit codes, or describe one operation")
        .disable_help_subcommand(true);
    generated::COMMANDS.iter().fold(root, |root, command| {
        let words: Vec<_> = command.path.split(' ').collect();
        let name = words.last().expect("a command has a word");
        let leaf = clap::Command::new(*name).about(command.summary);
        add_path(root, &words, leaf)
    })
}

pub(super) fn answer(path: Option<&str>) -> Value {
    if let Some(path) = path {
        let command = generated::COMMANDS
            .iter()
            .find(|command| command.path == path)
            .expect("a command clap found");
        let mut answer = entry(command);
        let read = |text| serde_json::from_str::<Value>(text).expect("the generator writes JSON");
        answer["input"] = read(command.input);
        answer["output"] = read(command.output);
        answer["example"] = read(command.example);
        return answer;
    }

    let mut exit_codes = vec![json!({"code": 0, "kind": "success", "retryable": false})];
    exit_codes.extend(EXIT_KINDS.iter().map(|&kind| {
        let error = OpError {
            kind,
            message: String::new(),
            hint: None,
        };
        json!({
            "code": kind.exit_code(),
            "kind": wire::ErrorInfo::from(&error).kind,
            "retryable": kind.retryable(),
        })
    }));
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "operations": generated::COMMANDS.iter().map(entry).collect::<Vec<_>>(),
        "exit_codes": exit_codes,
    })
}

fn entry(command: &Command) -> Value {
    let operation = Operation::ALL
        .iter()
        .copied()
        .find(|operation| operation.rpc() == command.rpc)
        .expect("a generated command has an operation");
    json!({
        "command": command.path,
        "rpc": command.rpc,
        "scope": required_scope(operation).map_or_else(|| "any".to_owned(), |s| s.to_string()),
        "summary": command.summary,
        "changes_state": operation.changes_state(),
    })
}
