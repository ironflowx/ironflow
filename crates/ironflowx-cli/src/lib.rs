//! The CLI: operations generated from the proto, plus `describe` and `version`
//! (architecture.md, "CLI"). An answer is JSON on stdout. A failure is one envelope on stderr and
//! its kind's exit code (D64, D65).

#![forbid(unsafe_code)]

use std::cell::OnceCell;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::error::ContextKind;
use clap::{Arg, ArgAction, ArgMatches, value_parser};
use ironflowx_core::{ErrorKind, Secret};
use ironflowx_proto::{Call, Client, OpError, wire};
use serde_core::de::{
    Deserialize as _, DeserializeSeed, Deserializer, Error as _, IgnoredAny, MapAccess, SeqAccess,
    Unexpected, Visitor,
};
use serde_json::{Value, json};

/// The most a call takes.
const DEADLINE: Duration = Duration::from_secs(30);

/// What every key starts with.
const KEY_PREFIX: &str = "ixk_";

/// How long a key's secret is, the part after its ID.
const SECRET_LEN: usize = 43;

const PROTOCOL: &str = "ironflowx.v1";

const DEFAULT_URL: &str = "http://127.0.0.1:9310";

/// A command the generator found in the proto.
struct Command {
    /// Its words, as `events emit`.
    path: &'static str,
    summary: &'static str,
    rpc: &'static str,
    input: &'static str,
    output: &'static str,
    example: &'static str,
    request: &'static [Field],
    answer: &'static [Field],
    /// Why wire JSON is no such request; `None`: it is one.
    refusal: fn(Value) -> Option<serde_json::Error>,
}

struct Field {
    name: &'static str,
    /// What it holds, or each item of its list.
    kind: Kind,
    list: bool,
    /// The JSON text of its default, which wire JSON leaves out; `None`: an absent one stays so.
    default: Option<&'static str>,
}

enum Kind {
    /// A string, or what wire JSON also reads from one: a number, a time.
    Text,
    Bool,
    /// A string that holds JSON text.
    JsonText,
    Message(&'static [Field]),
}

mod describe;

mod generated {
    use ironflowx_proto::{Call, Client, OpError, Operations as _, wire};

    use super::{Command, Field, Kind, undecoded};

    include!(concat!(env!("OUT_DIR"), "/cli.rs"));
}

/// Parses the process's arguments as one of `root`'s commands or of the CLI's own.
///
/// # Errors
///
/// What the process exits with: `--help` has printed its text, or a usage error its envelope; or
/// clap would print the arguments, so none was read.
pub fn parse(root: clap::Command) -> Result<ArgMatches, ExitCode> {
    // A clap that traces its parsing prints each argument before one can be hidden (TEN-3).
    // One that changes this text refuses every start, and the message names the wrong cause.
    if clap::Error::raw(clap::error::ErrorKind::Io, "").to_string() != "error: " {
        return Err(end(Err(invalid("clap is built with its debug feature"))));
    }
    let root = root
        .subcommand(
            clap::Command::new("version").about("Show the version, the commit and the protocol"),
        )
        .subcommand(describe::command());
    // Apart from the hand-written ones, so that clap refuses a name both have.
    let generated = generated::COMMANDS.iter();
    let generated = generated.fold(clap::Command::new("generated"), |all, command| {
        let words: Vec<_> = command.path.split(' ').collect();
        add(all, &words, command)
    });
    let root = root.subcommands(generated.get_subcommands().cloned());
    group(root).try_get_matches().map_err(|e| {
        if !e.use_stderr() {
            return end(e.print().map_err(|e| unprinted(&e)));
        }
        // Without what clap adds after it, the last of each: an argument can hold the same.
        let text = e.render().to_string();
        let footer = text.rsplit_once("\n\nFor more information");
        let text = footer.map_or(&*text, |(text, _)| text);
        let usage = e.get(ContextKind::Usage);
        let usage = usage.and_then(|_| text.rsplit_once("\n\nUsage: "));
        let message = without_keys(usage.map_or(text, |(text, _)| text));
        end(Err(OpError {
            hint: Some("--help says what a command takes".to_owned()),
            ..invalid(message.trim_start_matches("error: "))
        }))
    })
}

/// `text`, its whitespace as single spaces, without a word that may be a key.
fn without_keys<'a>(text: &'a str) -> String {
    let hide = |word: &'a str| {
        // Without clap's quotes, one on each side.
        let bare = word.strip_prefix('\'').unwrap_or(word);
        let bare = bare.strip_suffix('\'').unwrap_or(bare);
        if is_key(bare) { "<hidden>" } else { word }
    };
    let words: Vec<_> = text.split_whitespace().map(hide).collect();
    words.join(" ")
}

/// Whether `word` may be a key given where none goes, or the end of one (TEN-3).
fn is_key(word: &str) -> bool {
    word.contains(KEY_PREFIX) || word.len() >= SECRET_LEN
}

/// Runs the CLI's own command that [`parse`] found: the process's exit code.
pub fn run(matches: &ArgMatches) -> ExitCode {
    let (command, leaf) = chosen(matches);
    end(answer(&command, leaf).and_then(|text| {
        print(&text).map_err(|e| match &*command {
            "keys create" => OpError {
                hint: Some(
                    "the key is stored and its secret lost: list the keys, revoke it".into(),
                ),
                ..e
            },
            _ => e,
        })
    }))
}

/// The words of the command that was parsed, and its own arguments.
fn chosen(matches: &ArgMatches) -> (String, &ArgMatches) {
    let mut words = Vec::new();
    let mut leaf = matches;
    while let Some((word, inner)) = leaf.subcommand() {
        words.push(word);
        leaf = inner;
    }
    (words.join(" "), leaf)
}

/// A command that is nothing without one of its subcommands.
fn group(command: clap::Command) -> clap::Command {
    command
        .subcommand_required(true)
        // `--help` is the one answer that is not JSON.
        .disable_help_subcommand(true)
}

fn add(parent: clap::Command, words: &[&'static str], command: &Command) -> clap::Command {
    let word = words.last().expect("a command has a word");
    let leaf = clap::Command::new(*word)
        .about(command.summary)
        .args(flags(command).map(|f| flag(command, f)))
        .arg(
            Arg::new("json")
                .long("json")
                .value_name("JSON")
                .help("The whole request, in place of its flags: JSON, @<file> or -, stdin")
                .conflicts_with_all(flags(command).map(|f| f.name)),
        )
        .arg(Arg::new("url").value_name("URL").long("url").help(format!(
            "The engine to call; else IRONFLOWX_URL, else {DEFAULT_URL}"
        )))
        .arg(
            Arg::new("key-file")
                .long("key-file")
                .value_name("PATH")
                .help("The file that holds the key; else IRONFLOWX_KEY holds it")
                .value_parser(value_parser!(PathBuf)),
        );
    add_path(parent, words, leaf)
}

/// Adds a leaf command, sharing group names with other commands of this tree.
fn add_path(parent: clap::Command, words: &[&'static str], leaf: clap::Command) -> clap::Command {
    let [word, rest @ ..] = words else {
        unreachable!("a command has a word");
    };
    if rest.is_empty() {
        return parent.subcommand(leaf);
    }
    let parent = match parent.find_subcommand(word) {
        Some(_) => parent,
        None => parent.subcommand(group(clap::Command::new(*word))),
    };
    parent.mut_subcommand(word, |group| add_path(group, rest, leaf))
}

/// The fields of the request that have a flag.
fn flags(command: &Command) -> impl Iterator<Item = &'static Field> {
    let has_one = |f: &&Field| !matches!(f.kind, Kind::Message(_));
    command.request.iter().filter(has_one)
}

/// The flag of `field`: what it parses to is the field, or an item of its list, in wire JSON. A
/// `bool` field's flag takes no value: given, it sends `true` (foundation's architecture, "Proto").
fn flag(command: &Command, field: &'static Field) -> Arg {
    if is_switch(field) {
        return Arg::new(field.name)
            .long(&*field.name.replace('_', "-").leak())
            .action(ArgAction::SetTrue);
    }
    let refusal = command.refusal;
    let parse = move |text: &str| {
        let value = match (&field.kind, text.strip_prefix('@'), text.parse()) {
            (Kind::JsonText, Some(path), _) => {
                Value::String(std::fs::read_to_string(path).map_err(|e| e.to_string())?)
            }
            (Kind::Bool, _, Ok(on)) => Value::Bool(on),
            _ => Value::String(text.to_owned()),
        };
        // Alone, so that a refusal is of this flag.
        match refusal(json!({ field.name: set(field, vec![value.clone()]) })) {
            Some(e) => Err(e.to_string()),
            None => Ok(value),
        }
    };
    let help = match (&field.kind, field.list) {
        (Kind::JsonText, _) => "JSON text, or @<file>",
        (_, true) => "May be repeated",
        _ => "",
    };
    Arg::new(field.name)
        // Leaked, once a run: clap takes static text.
        .long(&*field.name.replace('_', "-").leak())
        .value_name(&*field.name.to_uppercase().leak())
        .help(help)
        .action(match field.list {
            true => ArgAction::Append,
            false => ArgAction::Set,
        })
        // A negative number is JSON text; a flag taken for a value is none.
        .allow_hyphen_values(matches!(field.kind, Kind::JsonText))
        .value_parser(parse)
}

/// Whether `field`'s flag is a switch: a single `bool`.
const fn is_switch(field: &Field) -> bool {
    matches!(field.kind, Kind::Bool) && !field.list
}

/// The field its flag's `values` set.
fn set(field: &Field, mut values: Vec<Value>) -> Value {
    match field.list {
        true => values.into(),
        false => values.remove(0),
    }
}

/// The request that the flags or `--json` give, in wire JSON.
fn request(command: &Command, matches: &ArgMatches) -> Result<String, OpError> {
    let Some(given) = matches.get_one::<String>("json") else {
        let given = flags(command).filter_map(|field| {
            if is_switch(field) {
                let on = matches.get_flag(field.name);
                return on.then(|| (field.name.to_owned(), Value::Bool(true)));
            }
            let values = matches.get_many(field.name)?.cloned().collect();
            Some((field.name.to_owned(), set(field, values)))
        });
        return Ok(Value::Object(given.collect()).to_string());
    };
    let text = match given.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path),
        None if given == "-" => io::read_to_string(io::stdin()),
        None => Ok(given.clone()),
    };
    // Without the path, as of --key-file.
    let text = text.map_err(|e| invalid(format!("cannot read what --json names: {e}")))?;
    check(&text, command.request)?;
    Ok(text)
}

/// Refuses what decoding a request ignores or takes for something else: a field that a message of
/// it does not have (D79), an array for a message, and JSON for its text.
fn check(text: &str, fields: &'static [Field]) -> Result<(), OpError> {
    // First, so that nothing below fails for its syntax.
    serde_json::from_str::<IgnoredAny>(text).map_err(|e| undecoded(&e))?;
    let refused = OnceCell::new();
    let request = Check {
        kind: &Kind::Message(fields),
        list: false,
        name: "",
        at: String::new(),
        refused: &refused,
    };
    let read = request.deserialize(&mut serde_json::Deserializer::from_str(text));
    match refused.into_inner() {
        Some(e) => Err(e),
        None => read.map_err(|e| undecoded(&e)),
    }
}

/// A value of a request, as its text is read. Not through `Value`, which reads each number and
/// fails on one out of range, whatever it is in.
struct Check<'a> {
    kind: &'a Kind,
    list: bool,
    /// The field it is of, and where that is.
    name: &'static str,
    at: String,
    /// Set before an error that stands for it.
    refused: &'a OnceCell<OpError>,
}

impl<'de> DeserializeSeed<'de> for &Check<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, json: D) -> Result<(), D::Error> {
        match self.kind {
            _ if self.list => json.deserialize_option(self),
            Kind::Message(_) => json.deserialize_option(self),
            // As bytes, which any string is, one that does not decode too.
            Kind::JsonText => json.deserialize_bytes(self).inspect_err(|_| {
                let refused = OpError {
                    hint: Some(format!("{} is JSON text: pass a string", self.name)),
                    ..invalid(format!("{} is not a string", self.at))
                };
                let _ = self.refused.set(refused);
            }),
            Kind::Text | Kind::Bool => IgnoredAny::deserialize(json).map(drop),
        }
    }
}

impl<'de> Visitor<'de> for &Check<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.list { "an array" } else { "an object" })
    }

    /// An absent one, or it does not decode.
    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    /// Asked for by what it must be: a build with `arbitrary_precision` offers a number as a map.
    fn visit_some<D: Deserializer<'de>>(self, json: D) -> Result<(), D::Error> {
        match self.list {
            true => json.deserialize_seq(self),
            false => json.deserialize_map(self),
        }
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<(), A::Error> {
        if !self.list {
            return Err(A::Error::invalid_type(Unexpected::Seq, &self));
        }
        for i in 0.. {
            let item = Check {
                list: false,
                at: format!("{}[{i}]", self.at),
                ..*self
            };
            if items.next_element_seed(&item)?.is_none() {
                break;
            }
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<(), A::Error> {
        let Kind::Message(fields) = self.kind else {
            return Err(A::Error::invalid_type(Unexpected::Map, &self));
        };
        let under = match &*self.at {
            "" => String::new(),
            at => format!("{at}."),
        };
        while let Some(name) = object.next_key::<String>()? {
            let Some(field) = fields.iter().find(|f| f.name == name) else {
                let name = match is_key(&name) {
                    true => "<hidden>",
                    false => &name,
                };
                let message = format!("the request has no field '{under}{name}'");
                let _ = self.refused.set(invalid(message));
                return Err(A::Error::custom("an unknown field"));
            };
            object.next_value_seed(&Check {
                kind: &field.kind,
                list: field.list,
                name: field.name,
                at: format!("{under}{name}"),
                refused: self.refused,
            })?;
        }
        Ok(())
    }
}

/// The text to print.
fn answer(command: &str, matches: &ArgMatches) -> Result<String, OpError> {
    let indent = io::stdout().is_terminal().then_some(0);
    if command == "version" {
        let version = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": env!("IRONFLOWX_COMMIT"),
            "protocol": PROTOCOL,
        });
        return render(&version, &Kind::Message(&[]), indent);
    }
    if command == "describe" {
        return render(&describe::answer(None), &Kind::Text, indent);
    }
    if let Some(path) = command.strip_prefix("describe ") {
        return render(&describe::answer(Some(path)), &Kind::Text, indent);
    }
    let known = generated::COMMANDS.iter().find(|c| c.path == command);
    let known = known.expect("a command it found");
    let request = request(known, matches)?;
    let client = client(matches)?.with_timeout(DEADLINE);
    let cx = credential(matches)?;
    // Its answer repeats the label, so it would not be printed, and the new key in it lost.
    if command == "keys create" {
        let decoded: Value = serde_json::from_str(&request).map_err(|e| undecoded(&e))?;
        if repeats_secret(&decoded.to_string(), &cx) {
            let message = "no key is created, since the request repeats the key the CLI was given";
            return Err(invalid(message));
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| internal(format!("no runtime: {e}")))?;
    let mut answer = runtime
        .block_on(generated::call(command, &client, &cx, &request))
        .map_err(|e| without_secret(e, &cx))?;
    complete(&mut answer, known.answer);
    let text = render(&answer, &Kind::Message(known.answer), indent)?;
    if repeats_secret(&text, &cx) {
        let message = "the answer is not printed, since it repeats the key the CLI was given";
        return Err(internal(message.to_owned()));
    }
    Ok(text)
}

fn client(matches: &ArgMatches) -> Result<Client, OpError> {
    let flag = matches.get_one::<String>("url").cloned();
    let (given, url) = url(flag, std::env::var_os("IRONFLOWX_URL"));
    Client::new(&url).map_err(|e| invalid(format!("{given} is {e}")))
}

/// The URL to call, and what gave it.
fn url(flag: Option<String>, variable: Option<OsString>) -> (&'static str, String) {
    match (flag, variable) {
        (Some(url), _) => ("--url", url),
        // Text that is not UTF-8 is no URL; nor is an empty one.
        (None, Some(url)) => ("IRONFLOWX_URL", url.into_string().unwrap_or_default()),
        (None, None) => ("the default", DEFAULT_URL.to_owned()),
    }
}

/// The key given, and no other: never `admin.key` (D61).
fn credential(matches: &ArgMatches) -> Result<Call, OpError> {
    let key = if let Some(path) = matches.get_one::<PathBuf>("key-file") {
        // Without the path, which may be a key given by mistake.
        std::fs::read(path)
            .map_err(|e| invalid(format!("cannot read the file --key-file names: {e}")))?
    } else if let Some(key) = std::env::var_os("IRONFLOWX_KEY") {
        key.into_encoded_bytes()
    } else {
        return Ok(Call::anonymous());
    };
    // Text that is not UTF-8 is no well-formed key; nor is what replaces it.
    let key = String::from_utf8_lossy(&key).trim().to_owned();
    Ok(Call::with_key(Secret::new(key)))
}

/// Whether `text` holds as many bytes in a row of the key in `cx` as a key's secret has (TEN-3).
/// Fewer are no one's secret, and may be in any answer; a repeat that was changed is not found.
fn repeats_secret(text: &str, cx: &Call) -> bool {
    let key = cx.key().map_or("", Secret::expose).as_bytes();
    // A part that is not text is of a key that is never sent as it is.
    key.windows(SECRET_LEN)
        .filter_map(|part| str::from_utf8(part).ok())
        .any(|part| text.contains(part))
}

/// `e`, whose text may come from no engine, without one that repeats the secret in `cx`.
fn without_secret(mut e: OpError, cx: &Call) -> OpError {
    for text in [Some(&mut e.message), e.hint.as_mut()]
        .into_iter()
        .flatten()
    {
        if repeats_secret(text, cx) {
            *text = "not printed, since it repeats the key the CLI was given".to_owned();
        }
    }
    e
}

/// Adds the fields that hold their defaults, which wire JSON leaves out, so an absent one is always
/// one with presence (D65).
fn complete(value: &mut Value, fields: &[Field]) {
    for field in fields {
        if let (Some(default), Some(object)) = (field.default, value.as_object_mut()) {
            let default = || serde_json::from_str(default).expect("the generator writes JSON");
            object.entry(field.name).or_insert_with(default);
        }
        let Kind::Message(fields) = field.kind else {
            continue;
        };
        match value.get_mut(field.name) {
            Some(Value::Array(items)) => items.iter_mut().for_each(|item| complete(item, fields)),
            Some(inner) => complete(inner, fields),
            None => {}
        }
    }
}

/// The JSON text of `value`, which holds `kind`s: on one line, or indented and this deep already.
/// JSON text is in it as the value it holds (D65).
fn render(value: &Value, kind: &Kind, indent: Option<usize>) -> Result<String, OpError> {
    let inner = indent.map(|depth| depth + 1);
    let (open, close, items) = match (value, kind) {
        (Value::String(held), Kind::JsonText) => {
            let no_json = |_| internal("the answer holds JSON text that is no JSON".to_owned());
            return compact(held).map_err(no_json);
        }
        (Value::Array(items), _) => {
            let items = items.iter().map(|item| render(item, kind, inner));
            ('[', ']', items.collect::<Result<Vec<_>, _>>()?)
        }
        (Value::Object(object), _) => {
            let fields = match kind {
                Kind::Message(fields) => fields,
                _ => &[][..],
            };
            let mut items = Vec::new();
            for (name, value) in object {
                let field = fields.iter().find(|f| f.name == name);
                let value = render(value, field.map_or(&Kind::Text, |f| &f.kind), inner)?;
                let colon = if indent.is_some() { ": " } else { ":" };
                items.push(format!("{}{colon}{value}", Value::from(&**name)));
            }
            ('{', '}', items)
        }
        _ => return Ok(value.to_string()),
    };
    let line = |depth: Option<usize>| match depth {
        Some(depth) if !items.is_empty() => format!("\n{}", "  ".repeat(depth)),
        _ => String::new(),
    };
    let between = format!(",{}", line(inner));
    let items = [line(inner), items.join(&between), line(indent)].concat();
    Ok(format!("{open}{items}{close}"))
}

/// JSON text without the whitespace between its tokens: its values, the order of its keys and its
/// numbers are as they were sent.
fn compact(json: &str) -> Result<String, serde_json::Error> {
    // What answered may be no engine. Not as `Value`, which reads no number out of range and, in a
    // build with `arbitrary_precision`, an object whose first key is serde_json's own as a number.
    serde_json::from_str::<IgnoredAny>(json)?;
    let (mut in_string, mut escaped) = (false, false);
    let kept = |c: &char| {
        let between = !in_string && c.is_ascii_whitespace();
        in_string ^= *c == '"' && !escaped;
        escaped = *c == '\\' && !escaped;
        !between
    };
    Ok(json.chars().filter(kept).collect())
}

fn print(text: &str) -> Result<(), OpError> {
    writeln!(io::stdout(), "{text}").map_err(|e| unprinted(&e))
}

/// The exit code of an answer printed, or of a failure, once its envelope is.
pub fn end(result: Result<(), OpError>) -> ExitCode {
    let Err(e) = result else {
        return ExitCode::SUCCESS;
    };
    let mut envelope = json!({
        "code": wire::ErrorInfo::from(&e).kind,
        "message": e.message,
        "retryable": e.kind.retryable(),
    });
    if let Some(hint) = e.hint {
        envelope["hint"] = hint.into();
    }
    // The exit code still says what happened.
    let _ = writeln!(io::stderr(), "{envelope}");
    ExitCode::from(e.kind.exit_code())
}

fn invalid(message: impl Into<String>) -> OpError {
    OpError {
        kind: ErrorKind::Invalid,
        message: message.into(),
        hint: None,
    }
}

fn internal(message: String) -> OpError {
    OpError {
        kind: ErrorKind::Internal,
        message,
        hint: None,
    }
}

/// Of a request that is not one.
fn undecoded(e: &serde_json::Error) -> OpError {
    invalid(without_keys(&format!("the request does not decode: {e}")))
}

fn unprinted(e: &io::Error) -> OpError {
    internal(format!("cannot write to stdout: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_found_under_its_words() {
        let command = |path| Command {
            path,
            summary: "",
            rpc: "",
            input: "",
            output: "",
            example: "",
            request: &[],
            answer: &[],
            refusal: |_| None,
        };
        let root = ["whoami", "keys list", "keys revoke", "a b c"]
            .into_iter()
            .fold(clap::Command::new("ironflowx"), |root, path| {
                let words: Vec<_> = path.split(' ').collect();
                add(root, &words, &command(path))
            });
        let root = group(root);
        root.clone().debug_assert();
        for (args, found) in [
            ("whoami", Some("whoami")),
            ("keys list --url x --key-file y", Some("keys list")),
            ("keys revoke --url x", Some("keys revoke")),
            ("a b c", Some("a b c")),
            ("keys", None),
            ("keys nope", None),
            ("keys help", None),
            ("a b", None),
            ("list", None),
            ("keys --url x list", None),
        ] {
            let line = format!("ironflowx {args}");
            let parsed = root.clone().try_get_matches_from(line.split(' ')).ok();
            let got = parsed.as_ref().map(chosen);
            assert_eq!(got.as_ref().map(|(c, _)| &**c), found, "{args}");
            let url = got.and_then(|(_, leaf)| leaf.get_one::<String>("url"));
            assert_eq!(
                url.is_some(),
                found.is_some() && args.contains("--url"),
                "{args}"
            );
        }
    }

    #[test]
    fn the_url_is_the_flags_or_else_the_variables_or_else_the_default() {
        let flag = || Some("f".to_owned());
        let variable = || Some(OsString::from("v"));
        assert_eq!(url(flag(), variable()), ("--url", "f".to_owned()));
        assert_eq!(url(flag(), None), ("--url", "f".to_owned()));
        assert_eq!(url(None, variable()), ("IRONFLOWX_URL", "v".to_owned()));
        let default = "http://127.0.0.1:9310".to_owned();
        assert_eq!(url(None, None), ("the default", default));
    }

    const fn field(name: &'static str, kind: Kind, default: Option<&'static str>) -> Field {
        Field {
            name,
            kind,
            list: false,
            default,
        }
    }

    const fn list(name: &'static str, kind: Kind) -> Field {
        Field {
            list: true,
            ..field(name, kind, Some("[]"))
        }
    }

    /// D65: a default is added wherever wire JSON leaves one out, and nothing else.
    #[test]
    fn an_answer_is_completed_at_any_depth() {
        const ITEM: &[Field] = &[
            field("ok", Kind::Bool, Some("false")),
            field("n", Kind::Text, Some("\"0\"")),
        ];
        const ANSWER: &[Field] = &[
            list("items", Kind::Message(ITEM)),
            field(
                "one",
                Kind::Message(&[field("inner", Kind::Message(ITEM), None)]),
                None,
            ),
            field("name", Kind::Text, Some("\"\"")),
        ];
        for (sent, printed) in [
            (json!({}), json!({"items": [], "name": ""})),
            (
                json!({"items": [{}, {"ok": true, "x": 1}], "name": "a"}),
                json!({"items": [{"ok": false, "n": "0"}, {"ok": true, "n": "0", "x": 1}], "name": "a"}),
            ),
            (
                json!({"one": {"inner": {"n": "7"}}}),
                json!({"items": [], "name": "", "one": {"inner": {"n": "7", "ok": false}}}),
            ),
            (
                json!({"one": {}}),
                json!({"items": [], "name": "", "one": {}}),
            ),
        ] {
            let mut value = sent.clone();
            complete(&mut value, ANSWER);
            assert_eq!(value, printed, "{sent}");
        }
    }

    /// D65: a field of JSON text is printed as what it holds, at any depth, and nothing else is.
    #[test]
    fn json_text_is_printed_as_the_value_it_holds() {
        const PART: &[Field] = &[field("data", Kind::JsonText, None)];
        const ANSWER: Kind = Kind::Message(&[
            field("data", Kind::JsonText, None),
            field("name", Kind::Text, None),
            list("texts", Kind::JsonText),
            list("parts", Kind::Message(PART)),
            field("part", Kind::Message(PART), None),
        ]);
        let answer = json!({
            "\"": {"data": "[ ]"},
            "data": " {\n\"b\": 1.50, \"a\" :[ ] } ",
            "name": "[ ]",
            "none": {},
            "part": {"data": "1 "},
            "parts": [{"data": " \"x y\""}, {}],
            "texts": ["[ 1 ]", "2"],
        });
        let line = concat!(
            r#"{"\"":{"data":"[ ]"},"data":{"b":1.50,"a":[]},"name":"[ ]","none":{},"#,
            r#""part":{"data":1},"parts":[{"data":"x y"},{}],"texts":[[1],2]}"#,
        );
        assert_eq!(render(&answer, &ANSWER, None).unwrap(), line);
        let indented = r#"{
  "\"": {
    "data": "[ ]"
  },
  "data": {"b":1.50,"a":[]},
  "name": "[ ]",
  "none": {},
  "part": {
    "data": 1
  },
  "parts": [
    {
      "data": "x y"
    },
    {}
  ],
  "texts": [
    [1],
    2
  ]
}"#;
        assert_eq!(render(&answer, &ANSWER, Some(0)).unwrap(), indented);
        // As JSON is printed that holds none.
        assert_eq!(
            render(&answer, &Kind::Text, Some(0)).unwrap(),
            format!("{answer:#}")
        );
        assert_eq!(
            render(&answer, &Kind::Text, None).unwrap(),
            answer.to_string()
        );
        let refused = render(&json!({"data": "1 2"}), &ANSWER, None).unwrap_err();
        let expected = internal("the answer holds JSON text that is no JSON".to_owned());
        assert_eq!(refused, expected);
    }

    #[test]
    fn only_whitespace_between_tokens_is_removed() {
        for (held, printed) in [
            ("null", "null"),
            (" \t\r\n-0\n", "-0"),
            ("1E+2", "1E+2"),
            (
                "123456789012345678901234567890.10",
                "123456789012345678901234567890.10",
            ),
            ("{ \"b\" : 1 , \"a\" : [ 1 , 2 ] }", "{\"b\":1,\"a\":[1,2]}"),
            (r#"[ " a  b " ]"#, r#"[" a  b "]"#),
            // What ends a string, and what does not.
            (r#"[ "\" [ " , 1 ]"#, r#"["\" [ ",1]"#),
            (r#"[ "\\" , " " ]"#, r#"["\\"," "]"#),
            (r#"[ "\\\" \\" , " " ]"#, r#"["\\\" \\"," "]"#),
            (r#"[ "\\n" , "\n \u0020" ]"#, r#"["\\n","\n \u0020"]"#),
            // What `Value` does not read, or reads as something else.
            ("[ 1e400 ]", "[1e400]"),
            (
                r#"{ "$serde_json::private::Number" : "x" }"#,
                r#"{"$serde_json::private::Number":"x"}"#,
            ),
            // Not between tokens.
            ("\"\u{a0}\u{2028} \u{feff}\"", "\"\u{a0}\u{2028} \u{feff}\""),
        ] {
            assert_eq!(compact(held).unwrap(), printed, "{held}");
        }
        let deepest = "[ ".repeat(127) + &"] ".repeat(127);
        let printed = "[".repeat(127) + &"]".repeat(127);
        assert_eq!(compact(&deepest).unwrap(), printed);
        for no_json in [
            "",
            " ",
            "1 2",
            "{",
            "nul l",
            "{},\"x\":1",
            "\u{c}1",
            "\u{a0}1",
            "\"\n\"",
            "'a'",
        ] {
            assert!(compact(no_json).is_err(), "{no_json:?}");
        }
    }

    /// D79, and what a message's fields may be given as.
    #[test]
    fn a_request_is_checked_at_any_depth() {
        const INNER: &[Field] = &[
            field("data", Kind::JsonText, None),
            field("n", Kind::Text, None),
        ];
        const REQUEST: &[Field] = &[
            field("data", Kind::JsonText, None),
            list("texts", Kind::JsonText),
            field("one", Kind::Message(INNER), None),
            list("items", Kind::Message(INNER)),
            list("names", Kind::Text),
        ];
        let check = |text: &str| check(text, REQUEST);
        let undecodable = |why: &str| invalid(format!("the request does not decode: {why}"));
        for good in [
            "{}",
            // Absent, or it does not decode.
            "null",
            r#"{"one":null,"items":null}"#,
            r#"{"items":[null]}"#,
            r#"{"data":"1","texts":["1","{"],"one":{"data":"x"},"items":[{},{"data":"","n":[]}]}"#,
            // No message of ours, so what it holds is the decoder's to say.
            r#"{"names":[{"x":1}],"one":{"n":1e400}}"#,
            // A string, if none that decodes.
            r#"{"data":"\ud800"}"#,
        ] {
            assert_eq!(check(good), Ok(()), "{good}");
        }
        for (bad, why) in [
            (
                "[]",
                "invalid type: sequence, expected an object at line 1 column 0",
            ),
            (
                r#""x""#,
                "invalid type: string \"x\", expected an object at line 1 column 3",
            ),
            (
                r#"{"one":["1"]}"#,
                "invalid type: sequence, expected an object at line 1 column 7",
            ),
            (
                r#"{"items":[{},["1"]]}"#,
                "invalid type: sequence, expected an object at line 1 column 13",
            ),
            (
                r#"{"items":{"x":1}}"#,
                "invalid type: map, expected an array at line 1 column 9",
            ),
            (
                r#"{"names":{"x":1}}"#,
                "invalid type: map, expected an array at line 1 column 9",
            ),
            (
                r#"{"items":"x"}"#,
                "invalid type: string \"x\", expected an array at line 1 column 12",
            ),
            (
                r#"{"data":"1}"#,
                "EOF while parsing a string at line 1 column 11",
            ),
            (r#"{"data":1,}"#, "key must be a string at line 1 column 11"),
            (r#"{"dat":1} 2"#, "trailing characters at line 1 column 11"),
        ] {
            assert_eq!(check(bad), Err(undecodable(why)), "{bad}");
        }
        let deep = format!(
            r#"{{"one":{{"m":{}{}}}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        // A build with `arbitrary_precision` reads a number without its value.
        let precise = serde_json::from_str::<Value>("1e400").is_ok();
        for (bad, default, column) in [
            ("1e400", "number out of range", 5),
            (
                r#"{"items":[1.5]}"#,
                "invalid type: floating point `1.5`, expected an object",
                13,
            ),
            (r#"{"one":1e400}"#, "number out of range", 12),
        ] {
            let why = match precise {
                true => "invalid type: number, expected an object",
                false => default,
            };
            let expected = undecodable(&format!("{why} at line 1 column {column}"));
            assert_eq!(check(bad), Err(expected), "{bad}");
        }
        let quoted = format!(r#"{{"'{}'":1}}"#, "n".repeat(SECRET_LEN - 2));
        let long = "n".repeat(SECRET_LEN - 1);
        let hidden = format!(r#"{{"one":{{"{long}n":1}}}}"#);
        let shown = format!(r#"{{"one":{{"{long}":1}}}}"#);
        for (bad, message) in [
            (r#"{"dat":"1"}"#, "the request has no field 'dat'"),
            (r#"{"Data":"1"}"#, "the request has no field 'Data'"),
            (r#"{"":1}"#, "the request has no field ''"),
            // As it is written, since `data` is one.
            (r#"{" data\n":"1"}"#, "the request has no field ' data\n'"),
            (r#"{"a ixk_ b":1}"#, "the request has no field '<hidden>'"),
            // Whatever it holds, if that is JSON.
            (r#"{"dat":1e400}"#, "the request has no field 'dat'"),
            (r#"{"dat":"\ud800"}"#, "the request has no field 'dat'"),
            (&deep, "the request has no field 'one.m'"),
            (r#"{"n":1}"#, "the request has no field 'n'"),
            (r#"{"one":{"m":1}}"#, "the request has no field 'one.m'"),
            (
                r#"{"items":[{},{"one":1}]}"#,
                "the request has no field 'items[1].one'",
            ),
            (r#"{"ixk_":1}"#, "the request has no field '<hidden>'"),
            (&quoted, "the request has no field '<hidden>'"),
            (&hidden, "the request has no field 'one.<hidden>'"),
            (&shown, &format!("the request has no field 'one.{long}'")),
        ] {
            assert_eq!(check(bad), Err(invalid(message)), "{bad}");
        }
        for (bad, at, name) in [
            (r#"{"data":{"id":1}}"#, "data", "data"),
            (r#"{"data":null}"#, "data", "data"),
            (r#"{"data":1}"#, "data", "data"),
            (r#"{"data":["1"]}"#, "data", "data"),
            (r#"{"data":1e400}"#, "data", "data"),
            (r#"{"data":1e400,"dat":1}"#, "data", "data"),
            (r#"{"texts":["1",2]}"#, "texts[1]", "texts"),
            (r#"{"texts":null,"one":{"data":true}}"#, "one.data", "data"),
            (
                r#"{"items":[{"data":"1"},{"data":[]}]}"#,
                "items[1].data",
                "data",
            ),
        ] {
            let expected = OpError {
                hint: Some(format!("{name} is JSON text: pass a string")),
                ..invalid(format!("{at} is not a string"))
            };
            assert_eq!(check(bad), Err(expected), "{bad}");
        }
    }

    #[test]
    fn a_flag_sets_its_field() {
        const REQUEST: &[Field] = &[
            field("a_name", Kind::Text, None),
            field("on", Kind::Bool, None),
            field("data", Kind::JsonText, None),
            list("names", Kind::Text),
            list("texts", Kind::JsonText),
            field("one", Kind::Message(&[]), None),
            list("items", Kind::Message(&[])),
        ];
        let command = Command {
            path: "do",
            summary: "",
            rpc: "",
            input: "",
            output: "",
            example: "",
            request: REQUEST,
            answer: &[],
            // As one that knows no such name, in whichever field.
            refusal: |v| match v.to_string().contains("refused") {
                true => serde_json::from_str::<Value>("").err(),
                false => None,
            },
        };
        let root = add(clap::Command::new("ironflowx"), &["do"], &command);
        root.clone().debug_assert();
        let parse = |args: &[&str]| {
            let line = ["ironflowx", "do"].into_iter().chain(args.iter().copied());
            root.clone().try_get_matches_from(line)
        };
        for (args, expected) in [
            (&[][..], json!({})),
            (&["--a-name", "x"], json!({"a_name": "x"})),
            (&["--a-name", ""], json!({"a_name": ""})),
            (&["--a-name", "@x"], json!({"a_name": "@x"})),
            (&["--a-name=-x"], json!({"a_name": "-x"})),
            (&["--a-name", "true"], json!({"a_name": "true"})),
            (&["--on"], json!({"on": true})),
            (&["--names", "x"], json!({"names": ["x"]})),
            (
                &["--names", "x", "--names", "y"],
                json!({"names": ["x", "y"]}),
            ),
            (&["--data", "-1e-5"], json!({"data": "-1e-5"})),
            (&["--data", "--on"], json!({"data": "--on"})),
            (&["--data", ""], json!({"data": ""})),
            (
                &["--texts", "1", "--texts", "-2"],
                json!({"texts": ["1", "-2"]}),
            ),
            (
                &["--on", "--a-name", "x", "--data", " [1] "],
                json!({"on": true, "a_name": "x", "data": " [1] "}),
            ),
        ] {
            let matches = parse(args).unwrap();
            let (_, leaf) = chosen(&matches);
            let given = request(&command, leaf).unwrap();
            let given: Value = serde_json::from_str(&given).unwrap();
            assert_eq!(given, expected, "{args:?}");
        }
        for (args, why) in [
            (&["--one", "x"][..], "unexpected argument '--one' found"),
            // A switch takes no value.
            (&["--on", "true"], "unexpected argument 'true' found"),
            (
                &["--on=false"],
                "unexpected value 'false' for '--on' found; no more were expected",
            ),
            (&["--items", "x"], "unexpected argument '--items' found"),
            (&["--a_name", "x"], "unexpected argument '--a_name' found"),
            (
                &["--a-name", "x", "--names", "refused"],
                "invalid value 'refused' for '--names <NAMES>': EOF while parsing a value",
            ),
            (
                &["--json", "{}", "--names", "x"],
                "the argument '--json <JSON>' cannot be used with '--names <NAMES>'",
            ),
            (
                &["--a-name", "x", "--a-name", "y"],
                "the argument '--a-name <A_NAME>' cannot be used multiple times",
            ),
        ] {
            let e = parse(args).unwrap_err().to_string();
            assert!(e.starts_with(&format!("error: {why}")), "{args:?}: {e}");
        }
    }
}
