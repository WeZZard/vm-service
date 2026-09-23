//! `argparse`-compatible diagnostics for rejected command lines.
//!
//! `clap` still performs the parse. When it rejects an input this module
//! reconstructs the message CPython's `argparse` would print for the same
//! command line, including which parser's usage line is shown, so `vmctl`'s
//! failures are byte-identical to the Python client's.
//!
//! The wording and the choices are measured against CPython 3.14.7 by
//! `python3 /tmp/error-corpus.py`, which compares exit code, stdout and stderr
//! for every failure shape the client can produce.

use crate::argparse_help::{self, Action, Nargs, ParserModel};

/// Print the `argparse` diagnostic for `argv` and exit 2.
///
/// `clap_error` is used only when the command line is valid by `argparse`'s
/// rules but `clap` rejected it. That keeps a future change in `clap`'s
/// acceptance visible instead of silently rewriting it.
pub fn exit_with(root: &ParserModel, argv: &[String], clap_error: &clap::Error) -> ! {
    match diagnose(root, argv) {
        Some((context, message)) => {
            eprint!("{}", argparse_help::usage_block(context));
            eprintln!("{}: error: {message}", context.prog);
            std::process::exit(2);
        }
        None => clap_error.exit(),
    }
}

/// The parser whose usage line `argparse` would show, and the message text.
fn diagnose<'a>(root: &'a ParserModel, argv: &[String]) -> Option<(&'a ParserModel, String)> {
    let mut top_extras: Vec<String> = Vec::new();
    let mut index = 0;
    let mut target: Option<&'a ParserModel> = None;

    // The top level: global options, then exactly one subcommand.
    while index < argv.len() {
        let token = &argv[index];
        if token == "--" {
            top_extras.extend(argv[index + 1..].iter().cloned());
            index = argv.len();
            break;
        }
        if is_option(token) {
            match match_option(&root.optionals, token, argv, index) {
                Ok(Some(matched)) => index += matched.consumed,
                Ok(None) => {
                    top_extras.push(token.clone());
                    index += 1;
                }
                Err(message) => return Some((root, message)),
            }
            continue;
        }
        match root.subcommands.iter().find(|sub| sub.name == *token) {
            Some(sub) => {
                target = Some(sub);
                index += 1;
                break;
            }
            None => {
                return Some((
                    root,
                    format!(
                        "argument cmd: invalid choice: '{}' (choose from {})",
                        token,
                        quoted_list(&cmd_choices(root))
                    ),
                ));
            }
        }
    }

    // Without a subcommand, `argparse` reports the required pseudo-positional
    // before it reports any leftover token.
    let Some(target) = target else {
        return Some((
            root,
            "the following arguments are required: cmd".to_string(),
        ));
    };

    let mut seen: Vec<String> = Vec::new();
    let mut sub_extras: Vec<String> = Vec::new();
    let mut positional_index = 0;
    let mut literal = false;

    while index < argv.len() {
        let token = &argv[index];
        if !literal && token == "--" {
            literal = true;
            index += 1;
            continue;
        }
        if !literal && is_option(token) {
            match match_option(&target.optionals, token, argv, index) {
                Ok(Some(matched)) => {
                    push_unique(&mut seen, &matched.action.dest);
                    index += matched.consumed;
                }
                Ok(None) => {
                    sub_extras.push(token.clone());
                    index += 1;
                }
                Err(message) => return Some((target, message)),
            }
            continue;
        }
        match next_positional(target, &mut positional_index) {
            Some(action) => {
                push_unique(&mut seen, &action.dest);
                positional_index += 1;
                if matches!(action.nargs, Nargs::ZeroOrMore | Nargs::OneOrMore) {
                    index = argv.len();
                } else if let Nargs::Exact(count) = action.nargs {
                    index += count.max(1);
                } else {
                    index += 1;
                }
            }
            None => {
                sub_extras.push(token.clone());
                index += 1;
            }
        }
    }

    // A missing required argument is reported before leftover tokens.
    let mut missing: Vec<&Action> = target
        .optionals
        .iter()
        .chain(target.positionals.iter())
        .filter(|action| is_required(action) && !seen.contains(&action.dest))
        .collect();
    missing.sort_by_key(|action| action.order);
    if !missing.is_empty() {
        let names = missing
            .iter()
            .map(|action| display_name(action))
            .collect::<Vec<_>>()
            .join(", ");
        return Some((
            target,
            format!("the following arguments are required: {names}"),
        ));
    }

    if !top_extras.is_empty() || !sub_extras.is_empty() {
        top_extras.extend(sub_extras);
        return Some((
            root,
            format!("unrecognized arguments: {}", top_extras.join(" ")),
        ));
    }

    None
}

/// One resolved optional and the tokens it consumed.
struct Matched<'a> {
    action: &'a Action,
    consumed: usize,
}

/// Resolve one token against `optionals`, as `argparse`'s parser would.
///
/// Returns `Ok(None)` for a token no option matches, which `argparse` collects
/// as an unrecognized argument, and `Err` for a message it reports directly.
fn match_option<'a>(
    optionals: &'a [Action],
    token: &str,
    argv: &[String],
    index: usize,
) -> Result<Option<Matched<'a>>, String> {
    let (name, inline) = match token.split_once('=') {
        Some((name, value)) => (name, Some(value.to_string())),
        None => (token, None),
    };

    let action = match optionals
        .iter()
        .find(|action| action.option_strings.iter().any(|string| string == name))
    {
        Some(action) => Some(action),
        // Only long options are abbreviated; a short option is either exact or
        // unrecognized.
        None if name.starts_with("--") => match prefix_matches(optionals, name) {
            Prefix::One(action) => Some(action),
            Prefix::Many(option_strings) => {
                return Err(format!(
                    "ambiguous option: {name} could match {}",
                    option_strings.join(", ")
                ));
            }
            Prefix::None => None,
        },
        None => None,
    };

    let Some(action) = action else {
        return Ok(None);
    };

    if action.nargs == Nargs::Zero {
        // `--flag=value` has no message in this client's surface; treat it as
        // unrecognized rather than inventing wording.
        if inline.is_some() {
            return Ok(None);
        }
        return Ok(Some(Matched {
            action,
            consumed: 1,
        }));
    }

    let (value, consumed) = match inline {
        Some(value) => (value, 1),
        None => match argv.get(index + 1) {
            Some(next) if !is_option(next) => (next.clone(), 2),
            _ => {
                return Err(format!(
                    "argument {}: expected one argument",
                    canonical(action)
                ));
            }
        },
    };
    validate(action, &value)?;
    Ok(Some(Matched { action, consumed }))
}

enum Prefix<'a> {
    None,
    One(&'a Action),
    Many(Vec<String>),
}

/// `argparse`'s `_get_option_tuples` for long-option prefixes.
fn prefix_matches<'a>(optionals: &'a [Action], name: &str) -> Prefix<'a> {
    let mut actions: Vec<&'a Action> = Vec::new();
    let mut option_strings: Vec<String> = Vec::new();
    for action in optionals {
        for string in action.option_strings.iter().chain(action.aliases.iter()) {
            if string.starts_with(name) {
                if !actions.iter().any(|other| std::ptr::eq(*other, action)) {
                    actions.push(action);
                }
                option_strings.push(string.clone());
            }
        }
    }
    match actions.len() {
        0 => Prefix::None,
        1 => Prefix::One(actions[0]),
        _ => Prefix::Many(option_strings),
    }
}

/// `argparse`'s value validation for one option, in message order.
fn validate(action: &Action, value: &str) -> Result<(), String> {
    let name = canonical(action);
    if let Some(choices) = &action.choices {
        if !choices.iter().any(|choice| choice == value) {
            return Err(format!(
                "argument {name}: invalid choice: '{value}' (choose from {})",
                quoted_list(choices)
            ));
        }
        return Ok(());
    }
    match value_kind(action) {
        ValueKind::Int if argparse_help::python_int(value).is_none() => {
            Err(format!("argument {name}: invalid int value: '{value}'"))
        }
        ValueKind::Float if python_float(value).is_none() => {
            Err(format!("argument {name}: invalid float value: '{value}'"))
        }
        _ => Ok(()),
    }
}

/// The `type=` `bin/vmctl` declares in Python for an option.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Text,
    Int,
    Float,
}

/// The value parsers `bin/vmctl` declares with `type=int` / `type=float`.
///
/// Every other value in this client is a string. `crates/vmctl/src/main.rs`
/// holds the same list in its `Cli` field types, and the error corpus checks
/// each one against the Python client.
fn value_kind(action: &Action) -> ValueKind {
    for option in &action.option_strings {
        match option.as_str() {
            "--cpu" | "--memory-mb" | "--disk-gb" | "--timeout" | "--timeout-s" => {
                return ValueKind::Int;
            }
            "--ttl-hours" => return ValueKind::Float,
            _ => {}
        }
    }
    ValueKind::Text
}

/// CPython's `float()` for the strings `argparse`'s `type=float` sees.
///
/// Being stricter than Python is safe on this path: a value this rejects but
/// Python accepts can only reach the message path when `clap` rejected it too.
fn python_float(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let lowered = rest.to_ascii_lowercase();
    let magnitude = match lowered.as_str() {
        "inf" | "infinity" => f64::INFINITY,
        "nan" => f64::NAN,
        _ => parse_float_body(rest)?,
    };
    Some(if negative { -magnitude } else { magnitude })
}

/// Parse a decimal float body, allowing Python's digit-separating underscores.
fn parse_float_body(body: &str) -> Option<f64> {
    let characters: Vec<char> = body.chars().collect();
    let mut cleaned = String::with_capacity(body.len());
    for (index, character) in characters.iter().enumerate() {
        match character {
            '_' => {
                let before = index > 0 && characters[index - 1].is_ascii_digit();
                let after = characters
                    .get(index + 1)
                    .is_some_and(|character| character.is_ascii_digit());
                if !(before && after) {
                    return None;
                }
                continue;
            }
            // Only the sign of an exponent may follow the leading sign.
            '+' | '-' => {
                let after_exponent = index > 0 && matches!(characters[index - 1], 'e' | 'E');
                if !after_exponent {
                    return None;
                }
            }
            character if character.is_ascii_digit() || matches!(character, '.' | 'e' | 'E') => {}
            _ => return None,
        }
        cleaned.push(*character);
    }
    cleaned.parse::<f64>().ok()
}

/// The next positional slot, skipping the subcommand pseudo-action.
fn next_positional<'a>(parser: &'a ParserModel, index: &mut usize) -> Option<&'a Action> {
    while *index < parser.positionals.len() {
        let action = &parser.positionals[*index];
        if matches!(action.nargs, Nargs::Zero | Nargs::Parser) {
            *index += 1;
            continue;
        }
        return Some(action);
    }
    None
}

/// `argparse` requires an action when `required` is set, except a positional
/// with `nargs='*'`; `positional_action` marks every positional required.
fn is_required(action: &Action) -> bool {
    if !action.required {
        return false;
    }
    if action.option_strings.is_empty() {
        return matches!(
            action.nargs,
            Nargs::None | Nargs::OneOrMore | Nargs::Exact(_)
        );
    }
    true
}

/// `argparse`'s `_get_action_name`: the long option string, or the `dest`.
fn canonical(action: &Action) -> String {
    action
        .option_strings
        .iter()
        .find(|string| string.starts_with("--"))
        .or_else(|| action.option_strings.first())
        .cloned()
        .unwrap_or_else(|| action.dest.clone())
}

/// How a required argument is named in a "required" message.
fn display_name(action: &Action) -> String {
    if action.option_strings.is_empty() {
        action.dest.clone()
    } else {
        canonical(action)
    }
}

fn cmd_choices(root: &ParserModel) -> Vec<String> {
    root.positionals
        .iter()
        .find(|action| action.dest == "cmd")
        .and_then(|action| action.choices.clone())
        .unwrap_or_default()
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `argparse`'s `_parse_optional`: a token is an option unless it looks like a
/// negative number, which this client has no options resembling.
fn is_option(token: &str) -> bool {
    token.len() > 1 && token.starts_with('-') && !looks_like_negative_number(token)
}

fn looks_like_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    if rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    match rest.split_once('.') {
        Some((integer, fraction)) => {
            !fraction.is_empty()
                && fraction.bytes().all(|byte| byte.is_ascii_digit())
                && (integer.is_empty() || integer.bytes().all(|byte| byte.is_ascii_digit()))
        }
        None => false,
    }
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn root() -> ParserModel {
        argparse_help::model_from_command(&<crate::Cli as CommandFactory>::command())
    }

    fn message(argv: &[&str]) -> String {
        let argv: Vec<String> = argv.iter().map(|token| token.to_string()).collect();
        let root = root();
        diagnose(&root, &argv).expect("a diagnostic").1
    }

    fn context<'a>(root: &'a ParserModel, argv: &[&str]) -> &'a ParserModel {
        let argv: Vec<String> = argv.iter().map(|token| token.to_string()).collect();
        diagnose(root, &argv).expect("a diagnostic").0
    }

    #[test]
    fn missing_subcommand_and_invalid_subcommand() {
        assert_eq!(message(&[]), "the following arguments are required: cmd");
        assert_eq!(
            message(&["--nope"]),
            "the following arguments are required: cmd"
        );
        assert!(message(&["bogus"]).starts_with(
            "argument cmd: invalid choice: 'bogus' (choose from 'environment', 'acquire'"
        ));
    }

    #[test]
    fn value_errors_use_argparse_wording() {
        assert_eq!(
            message(&["acquire", "--purpose"]),
            "argument --purpose: expected one argument"
        );
        assert_eq!(
            message(&["acquire", "--purpose", "p", "--network", "bogus"]),
            "argument --network: invalid choice: 'bogus' (choose from 'nat', 'control-only')"
        );
        assert_eq!(
            message(&["acquire", "--purpose", "p", "--cpu", "1.5"]),
            "argument --cpu: invalid int value: '1.5'"
        );
        assert_eq!(
            message(&["acquire", "--purpose", "p", "--ttl-hours", "x"]),
            "argument --ttl-hours: invalid float value: 'x'"
        );
    }

    #[test]
    fn required_arguments_are_listed_in_declaration_order() {
        assert_eq!(
            message(&["console-open"]),
            "the following arguments are required: vm, --lease-id, --console-id, --attempt-id"
        );
        assert_eq!(
            message(&["console-open", "vm", "--lease-id", "L"]),
            "the following arguments are required: --console-id, --attempt-id"
        );
        // `argv` is `nargs='*'` and is not required.
        assert_eq!(
            message(&["exec"]),
            "the following arguments are required: vm"
        );
    }

    #[test]
    fn usage_context_is_the_subparser_for_subparser_errors() {
        let root = root();
        assert_eq!(context(&root, &["list", "extra"]).name, "vmctl");
        assert_eq!(
            context(&root, &["acquire", "--purpose", "p", "--cpu", "x"]).name,
            "acquire"
        );
    }

    #[test]
    fn extras_are_collected_and_use_the_top_level_usage() {
        let root = root();
        assert_eq!(message(&["list", "a", "b"]), "unrecognized arguments: a b");
        assert_eq!(
            message(&["list", "--nope", "--other"]),
            "unrecognized arguments: --nope --other"
        );
        assert_eq!(
            message(&["acquire", "--purpose", "p", "extra", "more"]),
            "unrecognized arguments: extra more"
        );
        assert_eq!(context(&root, &["list", "a", "b"]).name, "vmctl");
    }

    #[test]
    fn unambiguous_prefixes_resolve_and_ambiguous_ones_are_reported() {
        assert_eq!(
            message(&["acquire", "--purp", "p", "--network", "bogus"]),
            "argument --network: invalid choice: 'bogus' (choose from 'nat', 'control-only')"
        );
        assert_eq!(
            message(&["acquire", "--p", "x"]),
            "ambiguous option: --p could match --purpose, --pack, --profile"
        );
    }

    #[test]
    fn python_float_matches_cpython_for_the_relevant_forms() {
        for text in [
            "1", "1.5", ".5", "1.", "1e5", "1E-5", "1_0.5", "inf", "-inf", "nan", " +3 ",
        ] {
            assert!(python_float(text).is_some(), "{text} should parse");
        }
        for text in ["", "x", "1.0e3x", "1__0", "1_e5", "1-2", "--1"] {
            assert!(python_float(text).is_none(), "{text} should not parse");
        }
    }
}
