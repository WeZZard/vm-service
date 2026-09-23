//! An `argparse.HelpFormatter` port.
//!
//! `vmctl` is a Rust port of a Python CLI built with `argparse`. Parsing
//! stays in `clap`, but the reference `--help` output is `argparse`'s, and the
//! two renderers lay the same metadata out differently. This module renders
//! the help text from a model derived from `clap`'s command metadata, copying
//! CPython 3.14's `HelpFormatter` algorithm (usage wrapping, section headings,
//! two-column action layout, `textwrap` filling) so the output matches byte
//! for byte.
//!
//! The model is built from `clap::Command` only, so it cannot drift from the
//! parser: `model_from_command` walks `get_subcommands()` recursively and
//! reads each argument's option strings, value names, choices, arity and help.

use clap::builder::ValueRange;

use crate::textwrap::TextWrapper;

/// How many values an argparse action consumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nargs {
    /// A single value (`nargs` unset).
    None,
    /// No value (`nargs=0`); only optionals.
    Zero,
    /// `nargs='*'`.
    ZeroOrMore,
    /// `nargs='+'`.
    OneOrMore,
    /// `nargs=N` for an integer `N`.
    Exact(usize),
    /// The `_SubParsersAction` (`nargs='A...'`).
    Parser,
}

/// One argparse action (an optional or a positional).
#[derive(Clone, Debug)]
pub struct Action {
    pub option_strings: Vec<String>,
    pub dest: String,
    pub help: Option<String>,
    /// An explicitly-set metavar / value name.
    pub metavar: Option<String>,
    pub choices: Option<Vec<String>>,
    pub nargs: Nargs,
    pub required: bool,
    /// Declaration order across optionals and positionals, used to list
    /// missing required arguments the way `argparse` does.
    pub order: usize,
    /// Deprecated hidden aliases (`--line`, `--pack`). `argparse` declares
    /// them as separate suppressed actions, so they take part in prefix
    /// matching but never appear in help.
    pub aliases: Vec<String>,
    /// `_SubParsersAction._ChoicesPseudoAction`s.
    pub subactions: Vec<Action>,
}

/// One parser: its program path, description, two sections and children.
#[derive(Clone, Debug)]
pub struct ParserModel {
    pub name: String,
    pub prog: String,
    pub about: Option<String>,
    pub description: Option<String>,
    pub optionals: Vec<Action>,
    pub positionals: Vec<Action>,
    pub subcommands: Vec<ParserModel>,
}

/// Build the model for `command` and its descendants.
pub fn model_from_command(command: &clap::Command) -> ParserModel {
    build_parser(command, command.get_name().to_string(), true)
}

fn build_parser(command: &clap::Command, prog: String, is_root: bool) -> ParserModel {
    let name = command.get_name().to_string();
    let about = command.get_about().map(|about| about.to_string());
    // Only the top-level parser passes `description=`; subparsers pass `help=`
    // to `add_parser`, which becomes the parent's pseudo-action rather than the
    // child's description.
    let description = if is_root { about.clone() } else { None };

    let subcommands: Vec<ParserModel> = command
        .get_subcommands()
        .map(|sub| build_parser(sub, format!("{prog} {}", sub.get_name()), false))
        .collect();

    // argparse adds its help action before any user argument.
    let mut optionals: Vec<Action> = vec![help_action()];
    optionals.extend(
        command
            .get_arguments()
            .enumerate()
            .filter(|(_, argument)| !argument.is_positional())
            .map(|(index, argument)| optional_action(argument, index + 1)),
    );

    let mut positionals: Vec<Action> = command
        .get_arguments()
        .enumerate()
        .filter(|(_, argument)| argument.is_positional())
        .map(|(index, argument)| positional_action(argument, index + 1))
        .collect();

    if !subcommands.is_empty() {
        let subactions: Vec<Action> = subcommands
            .iter()
            .map(|sub| Action {
                option_strings: Vec::new(),
                dest: sub.name.clone(),
                help: sub.about.clone(),
                metavar: Some(sub.name.clone()),
                choices: None,
                nargs: Nargs::Zero,
                required: false,
                order: 0,
                aliases: Vec::new(),
                subactions: Vec::new(),
            })
            .collect();
        let choices: Vec<String> = subcommands.iter().map(|sub| sub.name.clone()).collect();
        positionals.push(Action {
            option_strings: Vec::new(),
            dest: "cmd".to_string(),
            help: None,
            metavar: None,
            choices: Some(choices),
            nargs: Nargs::Parser,
            required: true,
            order: usize::MAX,
            aliases: Vec::new(),
            subactions,
        });
    }

    ParserModel {
        name,
        prog,
        about,
        description,
        optionals,
        positionals,
        subcommands,
    }
}

fn help_action() -> Action {
    Action {
        option_strings: vec!["-h".to_string(), "--help".to_string()],
        dest: "help".to_string(),
        help: Some("show this help message and exit".to_string()),
        metavar: None,
        choices: None,
        nargs: Nargs::Zero,
        required: false,
        order: 0,
        aliases: Vec::new(),
        subactions: Vec::new(),
    }
}

fn optional_action(argument: &clap::Arg, order: usize) -> Action {
    let dest = argument.get_id().to_string();
    let takes_value = argument.get_action().takes_values();
    let nargs = if takes_value {
        nargs_from_clap(argument.get_num_args())
    } else {
        Nargs::Zero
    };
    let choices = if takes_value {
        let possible: Vec<String> = argument
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_string())
            .collect();
        if possible.is_empty() {
            None
        } else {
            Some(possible)
        }
    } else {
        None
    };
    let value_name = argument
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string());
    let metavar = if takes_value { value_name } else { None };

    let mut option_strings = Vec::new();
    if let Some(short) = argument.get_short() {
        option_strings.push(format!("-{short}"));
    }
    if let Some(long) = argument.get_long() {
        option_strings.push(format!("--{long}"));
    }
    let aliases: Vec<String> = argument
        .get_all_aliases()
        .map(|aliases| {
            aliases
                .into_iter()
                .map(|alias| format!("--{alias}"))
                .collect()
        })
        .unwrap_or_default();

    Action {
        option_strings,
        dest,
        help: argument.get_help().map(|help| help.to_string()),
        metavar,
        choices,
        nargs,
        required: argument.is_required_set(),
        order,
        aliases,
        subactions: Vec::new(),
    }
}

fn positional_action(argument: &clap::Arg, order: usize) -> Action {
    let dest = argument.get_id().to_string();
    // `clap`'s derive always stores a value name (`ScreamingSnake` of the
    // field). argparse renders a positional with its `dest` as written unless a
    // metavar was given explicitly, so treat the mechanical value name as
    // absent.
    let value_name = argument
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string());
    let metavar = match value_name {
        Some(name) if name != dest.to_uppercase() => Some(name),
        _ => None,
    };
    let choices = {
        let possible: Vec<String> = argument
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_string())
            .collect();
        if possible.is_empty() {
            None
        } else {
            Some(possible)
        }
    };

    Action {
        option_strings: Vec::new(),
        dest,
        help: argument.get_help().map(|help| help.to_string()),
        metavar,
        choices,
        nargs: nargs_from_clap(argument.get_num_args()),
        required: true,
        order,
        aliases: Vec::new(),
        subactions: Vec::new(),
    }
}

fn nargs_from_clap(range: Option<ValueRange>) -> Nargs {
    match range {
        None => Nargs::None,
        Some(range) => {
            let min = range.min_values();
            let max = range.max_values();
            if min == 0 && max == usize::MAX {
                Nargs::ZeroOrMore
            } else if min == 1 && max == usize::MAX {
                Nargs::OneOrMore
            } else if min == 0 && max == 0 {
                Nargs::Zero
            } else if min == 1 && max == 1 {
                Nargs::None
            } else {
                Nargs::Exact(min)
            }
        }
    }
}

// =========================
// argparse.HelpFormatter
// =========================

/// The `argparse` section of CPython's `_colorize` theme.
///
/// `prog_extra` is omitted: it only colours an explicit `usage=` string, which
/// the vmctl parsers never set.
#[derive(Clone, Copy)]
struct Theme {
    usage: &'static str,
    prog: &'static str,
    heading: &'static str,
    summary_long_option: &'static str,
    summary_short_option: &'static str,
    summary_label: &'static str,
    summary_action: &'static str,
    long_option: &'static str,
    short_option: &'static str,
    label: &'static str,
    action: &'static str,
    reset: &'static str,
}

impl Theme {
    /// `_colorize.Argparse`'s default colours.
    const COLORED: Theme = Theme {
        usage: "\x1b[1;34m",
        prog: "\x1b[1;35m",
        heading: "\x1b[1;34m",
        summary_long_option: "\x1b[36m",
        summary_short_option: "\x1b[32m",
        summary_label: "\x1b[33m",
        summary_action: "\x1b[32m",
        long_option: "\x1b[1;36m",
        short_option: "\x1b[1;32m",
        label: "\x1b[1;33m",
        action: "\x1b[1;32m",
        reset: "\x1b[0m",
    };

    /// `_colorize.Theme.no_colors()`: every code becomes the empty string.
    const PLAIN: Theme = Theme {
        usage: "",
        prog: "",
        heading: "",
        summary_long_option: "",
        summary_short_option: "",
        summary_label: "",
        summary_action: "",
        long_option: "",
        short_option: "",
        label: "",
        action: "",
        reset: "",
    };
}

/// The environment inputs to `_colorize.can_colorize`.
///
/// Kept explicit so the decision is testable without touching process globals
/// or requiring a tty.
#[derive(Clone, Debug, Default)]
pub struct ColorizeEnvironment {
    pub python_colors: Option<String>,
    pub no_color: Option<String>,
    pub force_color: Option<String>,
    pub term: Option<String>,
    pub stdout_is_tty: bool,
}

/// The pure `_colorize.can_colorize` decision for explicit inputs.
///
/// Order and semantics mirror CPython 3.14.7: `PYTHON_COLORS` wins outright
/// when exactly `"0"` or `"1"`; an *empty* `NO_COLOR` / `FORCE_COLOR` does not
/// count as set; `TERM=dumb` disables; otherwise colour follows `isatty`.
pub fn can_colorize_in(environment: &ColorizeEnvironment) -> bool {
    if environment.python_colors.as_deref() == Some("0") {
        return false;
    }
    if environment.python_colors.as_deref() == Some("1") {
        return true;
    }
    if environment
        .no_color
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return false;
    }
    if environment
        .force_color
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return true;
    }
    if environment.term.as_deref() == Some("dumb") {
        return false;
    }
    environment.stdout_is_tty
}

/// Whether CPython's `_colorize.can_colorize` would colour this process's
/// stdout.
pub fn can_colorize() -> bool {
    can_colorize_in(&ColorizeEnvironment {
        python_colors: environment_string("PYTHON_COLORS"),
        no_color: environment_string("NO_COLOR"),
        force_color: environment_string("FORCE_COLOR"),
        term: environment_string("TERM"),
        stdout_is_tty: stdout_is_tty(),
    })
}

fn environment_string(key: &str) -> Option<String> {
    std::env::var_os(key).map(|value| value.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn stdout_is_tty() -> bool {
    // `can_colorize` ends with `os.isatty(sys.stdout.fileno())`.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdout_is_tty() -> bool {
    false
}

/// Every ANSI code the theme can insert, for `_colorize.decolor`.
const THEME_CODES: [&str; 10] = [
    "\x1b[1;34m", // usage / heading
    "\x1b[1;35m", // prog
    "\x1b[35m",   // prog_extra (reserved)
    "\x1b[36m",   // summary_long_option
    "\x1b[32m",   // summary_short_option / summary_action
    "\x1b[33m",   // summary_label
    "\x1b[1;36m", // long_option
    "\x1b[1;32m", // short_option / action
    "\x1b[1;33m", // label
    "\x1b[0m",    // reset
];

/// `_colorize.decolor`: drop the theme's ANSI codes so widths measure the
/// visible text, exactly as Python does.
fn decolor(text: &str) -> String {
    let mut out = text.to_string();
    for code in THEME_CODES {
        if out.contains(code) {
            out = out.replace(code, "");
        }
    }
    out
}

/// Visible code-point length of a possibly-coloured string.
fn decolor_len(text: &str) -> usize {
    decolor(text).chars().count()
}

/// Render `parser` the way `argparse` would at the given terminal width,
/// colouring exactly when CPython's `can_colorize` says so.
pub fn format_parser_help(parser: &ParserModel, columns: usize) -> String {
    format_parser_help_colored(parser, columns, can_colorize())
}

/// `format_parser_help` with the colour decision supplied, so tests can force
/// either rendering without a tty.
pub fn format_parser_help_colored(parser: &ParserModel, columns: usize, color: bool) -> String {
    // argparse builds its formatter with `width=None`, then `HelpFormatter`
    // subtracts two columns from `shutil.get_terminal_size().columns`.
    let width = columns as i64 - 2;
    let theme = if color { Theme::COLORED } else { Theme::PLAIN };
    let mut formatter = HelpFormatter::new(parser.prog.clone(), width, theme);

    // `_action_max_length` is accumulated over every action before any of them
    // is rendered, so measure first.
    for action in parser.optionals.iter().chain(parser.positionals.iter()) {
        formatter.action_max_length = formatter
            .action_max_length
            .max(formatter.max_invocation_length(action, 2));
    }

    let mut parts = Vec::new();
    parts.push(formatter.format_usage(parser));
    if let Some(description) = &parser.description {
        parts.push(formatter.format_text(description));
    }
    parts.push(formatter.format_section("positional arguments", &parser.positionals));
    parts.push(formatter.format_section("options", &parser.optionals));

    let joined = parts.concat();
    let collapsed = collapse_long_breaks(&joined);
    format!("{}\n", collapsed.trim_matches('\n'))
}

/// The `usage: ...` block an `argparse` error prints.
///
/// `format_usage` ends with two newlines for help; the error message follows
/// immediately, so only one is kept. Colour follows the same `can_colorize`
/// gate as help.
pub fn usage_block(parser: &ParserModel) -> String {
    usage_block_colored(parser, effective_columns(), can_colorize())
}

pub fn usage_block_colored(parser: &ParserModel, columns: usize, color: bool) -> String {
    let width = columns as i64 - 2;
    let theme = if color { Theme::COLORED } else { Theme::PLAIN };
    let formatter = HelpFormatter::new(parser.prog.clone(), width, theme);
    let usage = formatter.format_usage(parser);
    format!("{}\n", usage.trim_end_matches('\n'))
}

struct HelpFormatter {
    prog: String,
    /// `HelpFormatter._width` (already reduced by two columns).
    width: i64,
    /// `HelpFormatter._max_help_position`.
    max_help_position: i64,
    current_indent: i64,
    action_max_length: i64,
    theme: Theme,
}

impl HelpFormatter {
    fn new(prog: String, width: i64, theme: Theme) -> Self {
        // `__init__`: min(24, max(width - 20, indent_increment * 2)).
        let max_help_position = 24.min((width - 20).max(4));
        Self {
            prog,
            width,
            max_help_position,
            current_indent: 0,
            action_max_length: 0,
            theme,
        }
    }

    /// `_metavar_formatter`. `clap` choices win over an auto-generated value
    /// name; explicit metavars are otherwise honoured.
    fn metavar(&self, action: &Action, default: &str) -> String {
        if let Some(choices) = &action.choices {
            format!("{{{}}}", choices.join(","))
        } else if let Some(metavar) = &action.metavar {
            metavar.clone()
        } else {
            default.to_string()
        }
    }

    /// `_format_args`.
    fn format_args(&self, action: &Action, default: &str) -> String {
        let metavar = self.metavar(action, default);
        match action.nargs {
            Nargs::None => metavar,
            Nargs::Zero => String::new(),
            Nargs::ZeroOrMore => format!("[{metavar} ...]"),
            Nargs::OneOrMore => format!("{metavar} [{metavar} ...]"),
            Nargs::Exact(count) => vec![metavar; count].join(" "),
            Nargs::Parser => format!("{metavar} ..."),
        }
    }

    /// `_format_action_invocation`.
    fn invocation(&self, action: &Action) -> String {
        let t = self.theme;
        if action.option_strings.is_empty() {
            let metavar = self.metavar(action, &action.dest);
            format!("{}{metavar}{}", t.action, t.reset)
        } else if action.nargs == Nargs::Zero {
            action
                .option_strings
                .iter()
                .map(|option| self.color_option(option))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            let default = action.dest.to_uppercase();
            let args = self.format_args(action, &default);
            let options = action
                .option_strings
                .iter()
                .map(|option| self.color_option(option))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{options} {}{args}{}", t.label, t.reset)
        }
    }

    /// `_format_action_invocation.color_option_strings`: a long option is
    /// anything longer than two characters.
    fn color_option(&self, option: &str) -> String {
        let t = self.theme;
        if option.chars().count() > 2 {
            format!("{}{option}{}", t.long_option, t.reset)
        } else {
            format!("{}{option}{}", t.short_option, t.reset)
        }
    }

    /// The usage fragment for one action (`_get_actions_usage_parts`).
    fn usage_part(&self, action: &Action) -> String {
        let t = self.theme;
        if action.option_strings.is_empty() {
            let default = action.dest.clone();
            let part = self.format_args(action, &default);
            format!("{}{part}{}", t.summary_action, t.reset)
        } else {
            let option_string = &action.option_strings[0];
            let option_color = if option_string.chars().count() > 2 {
                t.summary_long_option
            } else {
                t.summary_short_option
            };
            let mut part = if action.nargs == Nargs::Zero {
                format!("{option_color}{option_string}{}", t.reset)
            } else {
                let default = action.dest.to_uppercase();
                let args = self.format_args(action, &default);
                format!(
                    "{option_color}{option_string} {}{args}{}",
                    t.summary_label, t.reset
                )
            };
            if !action.required {
                part = format!("[{part}]");
            }
            part
        }
    }

    /// `_format_usage`.
    fn format_usage(&self, parser: &ParserModel) -> String {
        let t = self.theme;
        let prefix = "usage: ";
        let prog = &self.prog;
        let opt_parts: Vec<String> = parser
            .optionals
            .iter()
            .map(|a| self.usage_part(a))
            .collect();
        let pos_parts: Vec<String> = parser
            .positionals
            .iter()
            .map(|a| self.usage_part(a))
            .collect();
        let parts: Vec<String> = opt_parts.iter().chain(pos_parts.iter()).cloned().collect();

        let mut usage = std::iter::once(prog.clone())
            .chain(parts.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let text_width = self.width - self.current_indent;

        if prefix.chars().count() as i64 + decolor_len(&usage) as i64 > text_width {
            let prog_len = decolor_len(prog) as i64;
            let prefix_len = prefix.chars().count() as i64;
            if (prefix_len + prog_len) as f64 <= 0.75 * text_width as f64 {
                let indent = " ".repeat((prefix_len + prog_len + 1) as usize);
                let lines = if !opt_parts.is_empty() {
                    let mut with_prog = vec![prog.clone()];
                    with_prog.extend(opt_parts.iter().cloned());
                    let mut lines = get_lines(&with_prog, &indent, text_width, Some(prefix));
                    lines.extend(get_lines(&pos_parts, &indent, text_width, None));
                    lines
                } else if !pos_parts.is_empty() {
                    let mut with_prog = vec![prog.clone()];
                    with_prog.extend(pos_parts.iter().cloned());
                    get_lines(&with_prog, &indent, text_width, Some(prefix))
                } else {
                    vec![prog.clone()]
                };
                usage = lines.join("\n");
            } else {
                let indent = " ".repeat(prefix_len as usize);
                let mut lines = get_lines(&parts, &indent, text_width, None);
                if lines.len() > 1 {
                    lines = Vec::new();
                    lines.extend(get_lines(&opt_parts, &indent, text_width, None));
                    lines.extend(get_lines(&pos_parts, &indent, text_width, None));
                }
                lines.insert(0, prog.clone());
                usage = lines.join("\n");
            }
        }

        // Python strips the plain `prog` off the front, colours it on its own,
        // and colours the `usage: ` label separately.
        let rest = usage.strip_prefix(prog.as_str()).unwrap_or(usage.as_str());
        format!(
            "{}{}{}{}{}{}{}\n\n",
            t.usage, prefix, t.reset, t.prog, prog, t.reset, rest
        )
    }

    /// `_format_text` for the description / epilog.
    fn format_text(&self, text: &str) -> String {
        let collapsed = collapse_whitespace(text);
        let text_width = (self.width - self.current_indent).max(11);
        let indent = " ".repeat(self.current_indent.max(0) as usize);
        let wrapper = TextWrapper::with_indents(text_width as usize, &indent, &indent);
        format!("{}\n\n", wrapper.fill(&collapsed))
    }

    /// The longest action invocation reachable from `action`, with `indent`
    /// added for the action and two more per sub-action level.
    fn max_invocation_length(&self, action: &Action, indent: i64) -> i64 {
        let mut longest = decolor_len(&self.invocation(action)) as i64 + indent;
        for subaction in &action.subactions {
            longest = longest.max(self.max_invocation_length(subaction, indent + 2));
        }
        longest
    }

    /// `_Section.format_help`.
    fn format_section(&mut self, heading: &str, actions: &[Action]) -> String {
        let heading_indent = self.current_indent;
        self.current_indent += 2;
        let mut item_help = String::new();
        for action in actions {
            item_help.push_str(&self.render_action(action));
        }
        self.current_indent -= 2;

        if item_help.is_empty() {
            return String::new();
        }
        let t = self.theme;
        format!(
            "\n{}{}{}:{}\n{}{}",
            " ".repeat(heading_indent.max(0) as usize),
            t.heading,
            heading,
            t.reset,
            item_help,
            "\n"
        )
    }

    /// `_format_action`.
    fn render_action(&mut self, action: &Action) -> String {
        let help_position = (self.action_max_length + 2).min(self.max_help_position);
        let help_width = (self.width - help_position).max(11);
        let action_width = help_position - self.current_indent - 2;
        let action_header = self.invocation(action);
        // `_format_action` measures the invocation after decolouring it.
        let action_header_plain_len = decolor_len(&action_header) as i64;
        let mut out = String::new();

        let indent_first = match &action.help {
            None => {
                out.push_str(&" ".repeat(self.current_indent.max(0) as usize));
                out.push_str(&action_header);
                out.push('\n');
                0
            }
            Some(_) if action_header_plain_len <= action_width => {
                out.push_str(&" ".repeat(self.current_indent.max(0) as usize));
                out.push_str(&action_header);
                let padding = (action_width - action_header_plain_len).max(0) as usize;
                out.push_str(&" ".repeat(padding));
                out.push_str("  ");
                0
            }
            Some(_) => {
                out.push_str(&" ".repeat(self.current_indent.max(0) as usize));
                out.push_str(&action_header);
                out.push('\n');
                help_position
            }
        };

        if let Some(help) = &action.help {
            if !help.trim().is_empty() {
                let help_text = expand_help(help);
                if !help_text.is_empty() {
                    let help_lines = split_lines(&help_text, help_width);
                    if let Some(first) = help_lines.first() {
                        out.push_str(&" ".repeat(indent_first.max(0) as usize));
                        out.push_str(first);
                        out.push('\n');
                    }
                    for line in help_lines.iter().skip(1) {
                        out.push_str(&" ".repeat(help_position.max(0) as usize));
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            } else if !action_header.ends_with('\n') {
                out.push('\n');
            }
        }

        if !action.subactions.is_empty() {
            self.current_indent += 2;
            for subaction in &action.subactions {
                out.push_str(&self.render_action(subaction));
            }
            self.current_indent -= 2;
        }

        out
    }
}

/// `argparse`'s local `get_lines` helper inside `_format_usage`.
fn get_lines(parts: &[String], indent: &str, text_width: i64, prefix: Option<&str>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line: Vec<String> = Vec::new();
    let indent_length = indent.chars().count() as i64;
    let mut line_len = match prefix {
        Some(prefix) => prefix.chars().count() as i64 - 1,
        None => indent_length - 1,
    };
    for part in parts {
        let part_len = decolor_len(part) as i64;
        if line_len + 1 + part_len > text_width && !line.is_empty() {
            lines.push(format!("{indent}{}", line.join(" ")));
            line.clear();
            line_len = indent_length - 1;
        }
        line.push(part.clone());
        line_len += part_len + 1;
    }
    if !line.is_empty() {
        lines.push(format!("{indent}{}", line.join(" ")));
    }
    if prefix.is_some() {
        if let Some(first) = lines.first_mut() {
            let characters: Vec<char> = first.chars().collect();
            if characters.len() as i64 >= indent_length {
                *first = characters[indent_length as usize..].iter().collect();
            }
        }
    }
    lines
}

/// `_split_lines`: collapse whitespace, then `textwrap.wrap`.
fn split_lines(text: &str, width: i64) -> Vec<String> {
    let collapsed = collapse_whitespace(text);
    TextWrapper::new(width.max(1) as usize).wrap(&collapsed)
}

/// `_expand_help`. `argparse` interpolates `%(name)s` placeholders only when
/// the help string contains `%`; none of vmctl's do, so this is an identity.
fn expand_help(help: &str) -> String {
    help.to_string()
}

/// `re.compile(r'\s+', re.ASCII)` followed by `.strip()`.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_whitespace = false;
    for character in text.chars() {
        if is_ascii_whitespace(character) {
            if !in_whitespace {
                out.push(' ');
                in_whitespace = true;
            }
        } else {
            out.push(character);
            in_whitespace = false;
        }
    }
    out.trim().to_string()
}

fn is_ascii_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// `_long_break_matcher = re.compile(r'\n\n\n+')` collapsing to `\n\n`.
fn collapse_long_breaks(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\n' {
            let start = index;
            while index < characters.len() && characters[index] == '\n' {
                index += 1;
            }
            let count = index - start;
            if count >= 3 {
                out.push_str("\n\n");
            } else {
                for _ in 0..count {
                    out.push('\n');
                }
            }
        } else {
            out.push(characters[index]);
            index += 1;
        }
    }
    out
}

// =========================
// Help interception
// =========================

/// If `args` requests help for some parser, render it; otherwise `None`.
///
/// This mirrors where `argparse` fires its help action: `-h`/`--help` at the
/// current parser level prints that parser's help before required-argument
/// validation, an unknown long option does not stop the scan, a positional
/// that is not a subcommand at the top level is an error (so no help), an
/// option that is missing its value is an error, and `--` ends the scan.
pub fn intercept_help(root: &ParserModel, args: &[String]) -> Option<String> {
    let columns = effective_columns();
    let mut current = root;
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];

        if token == "--" {
            return None;
        }
        if token == "-h" || token == "--help" {
            return Some(format_parser_help(current, columns));
        }

        if token.starts_with('-') && token.len() > 1 {
            if token.contains('=') {
                index += 1;
                continue;
            }
            if let Some(action) = current
                .optionals
                .iter()
                .find(|action| action.option_strings.iter().any(|string| string == token))
            {
                if action.nargs != Nargs::Zero {
                    let next = args.get(index + 1)?;
                    if next.starts_with('-') && !is_negative_number(next) {
                        return None;
                    }
                    index += 2;
                    continue;
                }
            }
            index += 1;
            continue;
        }

        if !current.subcommands.is_empty() {
            current = current
                .subcommands
                .iter()
                .find(|subcommand| subcommand.name == *token)?;
            index += 1;
            continue;
        }
        index += 1;
    }
    None
}

/// Unicode `Nd` (decimal digit) ranges in code-point order.
///
/// Python's `int()` accepts every Unicode decimal digit, not just ASCII, and
/// `shutil.get_terminal_size` parses `COLUMNS` with `int()`.
const UNICODE_DECIMAL_DIGITS: [(u32, u32); 71] = [
    (0x0030, 0x0039),
    (0x0660, 0x0669),
    (0x06F0, 0x06F9),
    (0x07C0, 0x07C9),
    (0x0966, 0x096F),
    (0x09E6, 0x09EF),
    (0x0A66, 0x0A6F),
    (0x0AE6, 0x0AEF),
    (0x0B66, 0x0B6F),
    (0x0BE6, 0x0BEF),
    (0x0C66, 0x0C6F),
    (0x0CE6, 0x0CEF),
    (0x0D66, 0x0D6F),
    (0x0DE6, 0x0DEF),
    (0x0E50, 0x0E59),
    (0x0ED0, 0x0ED9),
    (0x0F20, 0x0F29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x17E0, 0x17E9),
    (0x1810, 0x1819),
    (0x1946, 0x194F),
    (0x19D0, 0x19D9),
    (0x1A80, 0x1A89),
    (0x1A90, 0x1A99),
    (0x1B50, 0x1B59),
    (0x1BB0, 0x1BB9),
    (0x1C40, 0x1C49),
    (0x1C50, 0x1C59),
    (0xA620, 0xA629),
    (0xA8D0, 0xA8D9),
    (0xA900, 0xA909),
    (0xA9D0, 0xA9D9),
    (0xA9F0, 0xA9F9),
    (0xAA50, 0xAA59),
    (0xABF0, 0xABF9),
    (0xFF10, 0xFF19),
    (0x104A0, 0x104A9),
    (0x10D30, 0x10D39),
    (0x10D40, 0x10D49),
    (0x11066, 0x1106F),
    (0x110F0, 0x110F9),
    (0x11136, 0x1113F),
    (0x111D0, 0x111D9),
    (0x112F0, 0x112F9),
    (0x11450, 0x11459),
    (0x114D0, 0x114D9),
    (0x11650, 0x11659),
    (0x116C0, 0x116C9),
    (0x116D0, 0x116E3),
    (0x11730, 0x11739),
    (0x118E0, 0x118E9),
    (0x11950, 0x11959),
    (0x11BF0, 0x11BF9),
    (0x11C50, 0x11C59),
    (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9),
    (0x11F50, 0x11F59),
    (0x16130, 0x16139),
    (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9),
    (0x16B50, 0x16B59),
    (0x16D70, 0x16D79),
    (0x1CCF0, 0x1CCF9),
    (0x1D7CE, 0x1D7FF),
    (0x1E140, 0x1E149),
    (0x1E2F0, 0x1E2F9),
    (0x1E4F0, 0x1E4F9),
    (0x1E5F1, 0x1E5FA),
    (0x1E950, 0x1E959),
    (0x1FBF0, 0x1FBF9),
];

/// Python's `str.isspace()` for the characters `int()` strips.
fn python_space(character: char) -> bool {
    matches!(character,
        '\t'..='\r' | ' '
            | '\u{1c}'..='\u{1f}'
            | '\u{85}' | '\u{a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}'
            | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// The digit value of a Unicode decimal digit.
fn decimal_digit_value(character: char) -> Option<u32> {
    let code = character as u32;
    if code <= 0x39 {
        return (code >= 0x30).then(|| code - 0x30);
    }
    match UNICODE_DECIMAL_DIGITS.binary_search_by(|(start, end)| {
        if code < *start {
            std::cmp::Ordering::Greater
        } else if code > *end {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(index) => Some(code - UNICODE_DECIMAL_DIGITS[index].0),
        Err(_) => None,
    }
}

/// `int(text)` for the base-10 subset Python accepts.
///
/// `shutil.get_terminal_size` reads `COLUMNS` with `int()`, so this accepts
/// surrounding whitespace, a leading sign, single underscores between digits,
/// and non-ASCII decimal digits. The value saturates instead of overflowing
/// because it is only ever used as a terminal width.
pub fn python_int(text: &str) -> Option<i128> {
    let trimmed = text.trim_matches(python_space);
    let mut characters = trimmed.chars().peekable();
    let negative = match characters.peek() {
        Some('+') => {
            characters.next();
            false
        }
        Some('-') => {
            characters.next();
            true
        }
        _ => false,
    };
    let mut seen_digit = false;
    let mut digit_before = false;
    let mut value: i128 = 0;
    for character in characters {
        if character == '_' {
            // PEP 515 allows a single underscore between digits only.
            if !digit_before {
                return None;
            }
            digit_before = false;
            continue;
        }
        let digit = decimal_digit_value(character)?;
        seen_digit = true;
        digit_before = true;
        value = value.saturating_mul(10).saturating_add(i128::from(digit));
    }
    if !seen_digit || !digit_before {
        return None;
    }
    Some(if negative { -value } else { value })
}

/// The terminal width `argparse` would see: a positive `COLUMNS`, else the
/// tty width, else 80.
///
/// `shutil.get_terminal_size` falls back to the terminal and then to 80 when
/// `COLUMNS` is missing, unparseable, or not positive.
pub fn effective_columns() -> usize {
    if let Some(value) = std::env::var_os("COLUMNS") {
        if let Some(text) = value.to_str() {
            if let Some(columns) = python_int(text) {
                if columns > 0 {
                    return usize::try_from(columns).unwrap_or(usize::MAX);
                }
            }
        }
    }
    terminal_columns().unwrap_or(80)
}

#[cfg(unix)]
fn terminal_columns() -> Option<usize> {
    // `shutil.get_terminal_size` queries `sys.__stdout__.fileno()`.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            return Some(size.ws_col as usize);
        }
    }
    None
}

#[cfg(not(unix))]
fn terminal_columns() -> Option<usize> {
    None
}

/// `argparse`'s `_negative_number_matcher = r'^-\d+$|^-\d*\.\d+$'`.
fn is_negative_number(token: &str) -> bool {
    let rest = match token.strip_prefix('-') {
        Some(rest) if !rest.is_empty() => rest,
        _ => return false,
    };
    if rest.chars().all(|character| character.is_ascii_digit()) {
        return true;
    }
    if let Some((integer, fraction)) = rest.split_once('.') {
        return integer.chars().all(|c| c.is_ascii_digit())
            && !fraction.is_empty()
            && fraction.chars().all(|c| c.is_ascii_digit());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        can_colorize_in, decolor, format_parser_help_colored, model_from_command, python_int,
        ColorizeEnvironment,
    };
    use clap::CommandFactory;

    fn environment(
        python_colors: Option<&str>,
        no_color: Option<&str>,
        force_color: Option<&str>,
        term: Option<&str>,
        stdout_is_tty: bool,
    ) -> ColorizeEnvironment {
        ColorizeEnvironment {
            python_colors: python_colors.map(str::to_string),
            no_color: no_color.map(str::to_string),
            force_color: force_color.map(str::to_string),
            term: term.map(str::to_string),
            stdout_is_tty,
        }
    }

    /// Every expectation is CPython 3.14.7's `_colorize.can_colorize`.
    #[test]
    fn can_colorize_gating_matches_python() {
        // `PYTHON_COLORS` wins outright, even over `NO_COLOR`, the terminal
        // name and a non-tty stdout.
        assert!(!can_colorize_in(&environment(
            Some("0"),
            None,
            Some("1"),
            None,
            true
        )));
        assert!(can_colorize_in(&environment(
            Some("1"),
            Some("1"),
            None,
            Some("dumb"),
            false
        )));
        // `NO_COLOR` outranks `FORCE_COLOR` and `TERM`.
        assert!(!can_colorize_in(&environment(
            None,
            Some("1"),
            Some("1"),
            None,
            true
        )));
        // Empty `NO_COLOR` / `FORCE_COLOR` do not count as set.
        assert!(can_colorize_in(&environment(
            None,
            Some(""),
            None,
            None,
            true
        )));
        assert!(!can_colorize_in(&environment(
            None,
            None,
            Some(""),
            None,
            false
        )));
        // `FORCE_COLOR` turns a pipe on.
        assert!(can_colorize_in(&environment(
            None,
            None,
            Some("1"),
            None,
            false
        )));
        // `TERM=dumb` is off even on a tty.
        assert!(!can_colorize_in(&environment(
            None,
            None,
            None,
            Some("dumb"),
            true
        )));
        // Otherwise colour follows `isatty`.
        assert!(can_colorize_in(&environment(None, None, None, None, true)));
        assert!(!can_colorize_in(&environment(
            None, None, None, None, false
        )));
    }

    /// Colour must be a pure decoration of the plain rendering, with the ANSI
    /// codes at CPython 3.14.7's insertion points.
    #[test]
    fn colored_help_decorates_plain_help() {
        let command = <crate::Cli as CommandFactory>::command();
        let model = model_from_command(&command);
        let plain = format_parser_help_colored(&model, 80, false);
        let colored = format_parser_help_colored(&model, 80, true);

        assert_eq!(decolor(&colored), plain, "colour moved visible text");
        assert_ne!(colored, plain);
        assert_eq!(
            colored.lines().next().unwrap(),
            "\x1b[1;34musage: \x1b[0m\x1b[1;35mvmctl\x1b[0m [\x1b[32m-h\x1b[0m] [\x1b[36m--environment \x1b[33mENVIRONMENT\x1b[0m]"
        );
        assert!(colored.contains("\x1b[1;34mpositional arguments:\x1b[0m"));
        assert!(colored.contains("\x1b[1;36m--environment\x1b[0m \x1b[1;33mENVIRONMENT\x1b[0m"));
    }

    /// Every expectation is CPython 3.14.7's `int(text, 10)`.
    #[test]
    fn python_int_matches_python() {
        let cases: [(&str, Option<i128>); 20] = [
            ("80", Some(80)),
            ("0", Some(0)),
            ("+80", Some(80)),
            ("-5", Some(-5)),
            (" 80 ", Some(80)),
            ("\t80\n", Some(80)),
            // PEP 515 underscores are allowed only between digits.
            ("1_0", Some(10)),
            ("1_000", Some(1000)),
            ("1__0", None),
            ("_10", None),
            ("10_", None),
            ("_", None),
            // Non-ASCII decimal digits.
            ("\u{668}\u{660}", Some(80)),
            ("\u{ff18}\u{ff10}", Some(80)),
            ("\u{96c}\u{96d}", Some(67)),
            // Radix prefixes, floats and empty input are rejected.
            ("0x50", None),
            ("8.0", None),
            ("1e2", None),
            ("", None),
            ("   ", None),
        ];
        for (text, expected) in cases {
            assert_eq!(python_int(text), expected, "int({text:?})");
        }
    }

    #[test]
    fn python_int_saturates_instead_of_overflowing() {
        let huge = "9".repeat(400);
        assert_eq!(python_int(&huge), Some(i128::MAX));
        assert_eq!(python_int(&format!("-{huge}")), Some(i128::MIN + 1));
    }
}
