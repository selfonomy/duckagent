use crate::sandbox::config::{PermissionAction, ResolvedSandbox, ShellPermissionRules};
use crate::sandbox::permissions::PermissionMatch;
use anyhow::{Context, Result, bail};
use brush_parser::ast::{self, CommandPrefixOrSuffixItem, IoRedirect, SeparatorOperator};
use brush_parser::{Parser, ParserOptions, SourceInfo};
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPermissionEvaluation {
    pub action: PermissionMatch,
    pub normalized_command: String,
    pub reason: ShellPermissionReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPermissionReason {
    ExactRule,
    SimpleRule,
    ShellSyntax,
    DynamicCommand,
    ParseFailed,
    DefaultAllow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedShell {
    normalized: String,
    simple_commands: Vec<ParsedSimpleCommand>,
    syntax_requires_ask: bool,
    dynamic_command: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSimpleCommand {
    argv: Option<Vec<String>>,
    redirection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedRule {
    Exact {
        normalized: String,
        action: PermissionAction,
    },
    Simple {
        argv: Vec<String>,
        action: PermissionAction,
    },
}

pub fn validate_shell_permission_rules(rules: &ShellPermissionRules) -> Result<()> {
    for pattern in rules.rules.keys() {
        parse_rule(pattern)
            .with_context(|| format!("invalid sandbox permissions.shell rule `{pattern}`"))?;
    }
    Ok(())
}

pub fn normalized_shell_command(command: &str) -> String {
    parse_shell(command)
        .map(|parsed| parsed.normalized)
        .unwrap_or_else(|_| command.trim().to_string())
}

pub fn evaluate_shell_permission(
    sandbox: &ResolvedSandbox,
    command: &str,
) -> ShellPermissionEvaluation {
    let parsed = match parse_shell(command) {
        Ok(parsed) => parsed,
        Err(_) => {
            return ShellPermissionEvaluation {
                action: PermissionMatch::Ask,
                normalized_command: command.trim().to_string(),
                reason: ShellPermissionReason::ParseFailed,
            };
        }
    };

    let rules = compiled_rules(&sandbox.preset.permissions.shell);
    let exact_action = exact_action(&rules, &parsed.normalized);

    if matches!(exact_action, Some(PermissionAction::Deny)) {
        return ShellPermissionEvaluation {
            action: PermissionMatch::Deny,
            normalized_command: parsed.normalized,
            reason: ShellPermissionReason::ExactRule,
        };
    }

    let simple_action = strongest_simple_action(&rules, &parsed.simple_commands);
    if matches!(simple_action, Some(PermissionAction::Deny)) {
        return ShellPermissionEvaluation {
            action: PermissionMatch::Deny,
            normalized_command: parsed.normalized,
            reason: ShellPermissionReason::SimpleRule,
        };
    }

    match exact_action {
        Some(PermissionAction::Allow) => ShellPermissionEvaluation {
            action: PermissionMatch::Allow,
            normalized_command: parsed.normalized,
            reason: ShellPermissionReason::ExactRule,
        },
        Some(PermissionAction::Ask) => ShellPermissionEvaluation {
            action: PermissionMatch::Ask,
            normalized_command: parsed.normalized,
            reason: ShellPermissionReason::ExactRule,
        },
        Some(PermissionAction::Deny) => unreachable!("handled above"),
        None => {
            if matches!(simple_action, Some(PermissionAction::Ask)) {
                return ShellPermissionEvaluation {
                    action: PermissionMatch::Ask,
                    normalized_command: parsed.normalized,
                    reason: ShellPermissionReason::SimpleRule,
                };
            }
            if parsed.dynamic_command {
                return ShellPermissionEvaluation {
                    action: PermissionMatch::Ask,
                    normalized_command: parsed.normalized,
                    reason: ShellPermissionReason::DynamicCommand,
                };
            }
            if parsed.syntax_requires_ask {
                return ShellPermissionEvaluation {
                    action: PermissionMatch::Ask,
                    normalized_command: parsed.normalized,
                    reason: ShellPermissionReason::ShellSyntax,
                };
            }
            if matches!(simple_action, Some(PermissionAction::Allow)) {
                return ShellPermissionEvaluation {
                    action: PermissionMatch::Allow,
                    normalized_command: parsed.normalized,
                    reason: ShellPermissionReason::SimpleRule,
                };
            }
            ShellPermissionEvaluation {
                action: PermissionMatch::Allow,
                normalized_command: parsed.normalized,
                reason: ShellPermissionReason::DefaultAllow,
            }
        }
    }
}

fn compiled_rules(rules: &ShellPermissionRules) -> Vec<ParsedRule> {
    rules
        .rules
        .iter()
        .filter_map(|(pattern, action)| parse_rule(pattern).ok().map(|rule| (rule, *action)))
        .map(|(mut rule, action)| {
            match &mut rule {
                ParsedRule::Exact { action: a, .. } | ParsedRule::Simple { action: a, .. } => {
                    *a = action;
                }
            }
            rule
        })
        .collect()
}

fn parse_rule(pattern: &str) -> Result<ParsedRule> {
    let parsed = parse_shell(pattern)?;
    if parsed.simple_commands.len() == 1
        && !parsed.syntax_requires_ask
        && !parsed.dynamic_command
        && parsed.normalized == parsed.normalized.trim()
    {
        let argv = parsed.simple_commands[0]
            .argv
            .clone()
            .context("shell simple rule must have a static command name")?;
        if argv.is_empty() {
            bail!("shell simple rule must have a non-empty argv");
        }
        return Ok(ParsedRule::Simple {
            argv,
            action: PermissionAction::Allow,
        });
    }

    Ok(ParsedRule::Exact {
        normalized: parsed.normalized,
        action: PermissionAction::Allow,
    })
}

fn exact_action(rules: &[ParsedRule], normalized: &str) -> Option<PermissionAction> {
    let mut best = None;
    for rule in rules {
        let ParsedRule::Exact {
            normalized: pattern,
            action,
        } = rule
        else {
            continue;
        };
        if pattern == normalized {
            best = Some(merge_action(best, *action));
        }
    }
    best
}

fn strongest_simple_action(
    rules: &[ParsedRule],
    commands: &[ParsedSimpleCommand],
) -> Option<PermissionAction> {
    let mut best: Option<(usize, PermissionAction)> = None;
    for command in commands {
        let Some(argv) = command.argv.as_ref() else {
            continue;
        };
        for rule in rules {
            let ParsedRule::Simple {
                argv: pattern,
                action,
            } = rule
            else {
                continue;
            };
            if pattern.len() <= argv.len() && argv_prefix_matches(pattern, argv) {
                match best {
                    Some((best_len, _)) if best_len > pattern.len() => {}
                    Some((best_len, best_action)) if best_len == pattern.len() => {
                        best = Some((best_len, merge_action(Some(best_action), *action)));
                    }
                    _ => best = Some((pattern.len(), *action)),
                }
            }
        }
    }
    best.map(|(_, action)| action)
}

fn argv_prefix_matches(pattern: &[String], argv: &[String]) -> bool {
    pattern
        .iter()
        .zip(argv.iter())
        .all(|(left, right)| left == right)
}

fn merge_action(current: Option<PermissionAction>, next: PermissionAction) -> PermissionAction {
    match (current, next) {
        (Some(PermissionAction::Deny), _) | (_, PermissionAction::Deny) => PermissionAction::Deny,
        (Some(PermissionAction::Ask), _) | (_, PermissionAction::Ask) => PermissionAction::Ask,
        _ => PermissionAction::Allow,
    }
}

fn parse_shell(command: &str) -> Result<ParsedShell> {
    let options = ParserOptions::default();
    let source_info = SourceInfo::default();
    let cursor = Cursor::new(command.as_bytes());
    let mut parser = Parser::new(cursor, &options, &source_info);
    let program = parser.parse_program().context("failed to parse shell")?;
    let mut state = BuildState::default();
    let mut pieces = Vec::new();

    for complete in &program.complete_commands {
        let rendered = render_compound_list(complete, &mut state)?;
        if !rendered.is_empty() {
            pieces.push(rendered);
        }
    }

    let normalized = pieces.join("; ").trim().to_string();
    if normalized.is_empty() {
        bail!("shell command is empty");
    }
    Ok(ParsedShell {
        normalized,
        simple_commands: state.simple_commands,
        syntax_requires_ask: state.syntax_requires_ask,
        dynamic_command: state.dynamic_command,
    })
}

#[derive(Default)]
struct BuildState {
    simple_commands: Vec<ParsedSimpleCommand>,
    syntax_requires_ask: bool,
    dynamic_command: bool,
}

fn render_compound_list(list: &ast::CompoundList, state: &mut BuildState) -> Result<String> {
    let mut pieces = Vec::new();
    for (index, item) in list.0.iter().enumerate() {
        let rendered = render_and_or_list(&item.0, state)?;
        if rendered.is_empty() {
            continue;
        }
        let suffix = match item.1 {
            SeparatorOperator::Async => {
                state.syntax_requires_ask = true;
                " &"
            }
            SeparatorOperator::Sequence if index + 1 < list.0.len() => " ;",
            SeparatorOperator::Sequence => "",
        };
        pieces.push(format!("{rendered}{suffix}"));
    }
    Ok(pieces.join(" "))
}

fn render_and_or_list(list: &ast::AndOrList, state: &mut BuildState) -> Result<String> {
    let mut rendered = render_pipeline(&list.first, state)?;
    for and_or in &list.additional {
        let (op, pipeline) = match and_or {
            ast::AndOr::And(pipeline) => ("&&", pipeline),
            ast::AndOr::Or(pipeline) => ("||", pipeline),
        };
        let next = render_pipeline(pipeline, state)?;
        rendered = format!("{rendered} {op} {next}");
    }
    Ok(rendered)
}

fn render_pipeline(pipeline: &ast::Pipeline, state: &mut BuildState) -> Result<String> {
    let mut pieces = Vec::new();
    for command in &pipeline.seq {
        pieces.push(render_command(command, state)?);
    }
    Ok(pieces.join(" | "))
}

fn render_command(command: &ast::Command, state: &mut BuildState) -> Result<String> {
    match command {
        ast::Command::Simple(simple) => render_simple_command(simple, state),
        ast::Command::Compound(compound, redirects) => {
            state.dynamic_command = true;
            if let Some(redirects) = redirects
                && !redirects.0.is_empty()
            {
                state.syntax_requires_ask = true;
            }
            Ok(compound.to_string())
        }
        ast::Command::Function(function) => {
            state.dynamic_command = true;
            Ok(function.to_string())
        }
        ast::Command::ExtendedTest(test) => {
            state.dynamic_command = true;
            Ok(format!("[[ {test} ]]"))
        }
    }
}

fn render_simple_command(simple: &ast::SimpleCommand, state: &mut BuildState) -> Result<String> {
    let mut rendered_parts = Vec::new();
    let mut argv = Vec::new();
    let mut redirection = false;

    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            match render_prefix_or_suffix_item(item)? {
                RenderedItem::Argument(arg) => rendered_parts.push(arg),
                RenderedItem::Assignment(assign) => rendered_parts.push(assign),
                RenderedItem::Redirection(redir) => {
                    redirection = true;
                    rendered_parts.push(redir);
                }
                RenderedItem::Dynamic(text) => {
                    state.dynamic_command = true;
                    rendered_parts.push(text);
                }
            }
        }
    }

    if let Some(word) = &simple.word_or_name {
        match static_word_value(word) {
            Some(value) => {
                let command = normalize_executable_name(&value);
                argv.push(command.clone());
                rendered_parts.push(shell_join(&[command]));
            }
            None => {
                state.dynamic_command = true;
                rendered_parts.push(word.value.clone());
            }
        }
    }

    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match render_prefix_or_suffix_item(item)? {
                RenderedItem::Argument(word) => {
                    if let Some(value) = static_word_from_rendered(&word) {
                        argv.push(value);
                    }
                    rendered_parts.push(word);
                }
                RenderedItem::Assignment(assign) => {
                    if let Some(value) = assignment_arg_from_rendered(&assign) {
                        argv.push(value);
                    }
                    rendered_parts.push(assign);
                }
                RenderedItem::Redirection(redir) => {
                    redirection = true;
                    rendered_parts.push(redir);
                }
                RenderedItem::Dynamic(text) => {
                    state.dynamic_command = true;
                    rendered_parts.push(text);
                }
            }
        }
    }

    if redirection {
        state.syntax_requires_ask = true;
    }
    state.simple_commands.push(ParsedSimpleCommand {
        argv: (!argv.is_empty()).then_some(argv),
        redirection,
    });
    Ok(rendered_parts.join(" "))
}

enum RenderedItem {
    Argument(String),
    Assignment(String),
    Redirection(String),
    Dynamic(String),
}

fn render_prefix_or_suffix_item(item: &CommandPrefixOrSuffixItem) -> Result<RenderedItem> {
    match item {
        CommandPrefixOrSuffixItem::Word(word) => {
            if let Some(value) = static_word_value(word) {
                Ok(RenderedItem::Argument(shell_join(&[value])))
            } else {
                Ok(RenderedItem::Dynamic(word.value.clone()))
            }
        }
        CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
            if let Some(value) = static_word_value(word) {
                Ok(RenderedItem::Assignment(shell_join(&[value])))
            } else {
                Ok(RenderedItem::Dynamic(word.value.clone()))
            }
        }
        CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            Ok(RenderedItem::Redirection(render_redirect(redirect)))
        }
        CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell) => {
            Ok(RenderedItem::Dynamic(format!("{kind}({subshell})")))
        }
    }
}

fn render_redirect(redirect: &IoRedirect) -> String {
    redirect.to_string()
}

fn static_word_value(word: &ast::Word) -> Option<String> {
    let options = ParserOptions::default();
    let pieces = brush_parser::word::parse(&word.value, &options).ok()?;
    if word_pieces_are_static(&pieces) {
        Some(brush_parser::unquote_str(&word.value))
    } else {
        None
    }
}

fn word_pieces_are_static(pieces: &[brush_parser::word::WordPieceWithSource]) -> bool {
    pieces
        .iter()
        .all(|piece| word_piece_is_static(&piece.piece))
}

fn word_piece_is_static(piece: &brush_parser::word::WordPiece) -> bool {
    use brush_parser::word::WordPiece;
    match piece {
        WordPiece::Text(_)
        | WordPiece::SingleQuotedText(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::TildePrefix(_)
        | WordPiece::EscapeSequence(_) => true,
        WordPiece::DoubleQuotedSequence(items) | WordPiece::GettextDoubleQuotedSequence(items) => {
            word_pieces_are_static(items)
        }
        WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => false,
    }
}

fn static_word_from_rendered(rendered: &str) -> Option<String> {
    shlex::split(rendered).and_then(|mut values| (values.len() == 1).then(|| values.remove(0)))
}

fn assignment_arg_from_rendered(rendered: &str) -> Option<String> {
    static_word_from_rendered(rendered)
}

fn normalize_executable_name(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(raw)
        .to_string()
}

fn shell_join(values: &[String]) -> String {
    shlex::try_join(values.iter().map(String::as_str)).unwrap_or_else(|_| values.join(" "))
}

pub fn permission_action_for_pattern(
    rules: &BTreeMap<String, PermissionAction>,
    value: &str,
    default: Option<PermissionAction>,
) -> Option<PermissionAction> {
    let mut best: Option<(usize, PermissionAction)> = None;
    for (pattern, action) in rules {
        if crate::sandbox::matcher::glob_matches(pattern, value) || pattern == value {
            let specificity = pattern_specificity(pattern);
            match best {
                Some((best_specificity, _)) if best_specificity > specificity => {}
                Some((best_specificity, best_action)) if best_specificity == specificity => {
                    best = Some((specificity, merge_action(Some(best_action), *action)));
                }
                _ => best = Some((specificity, *action)),
            }
        }
    }
    best.map(|(_, action)| action).or(default)
}

fn pattern_specificity(pattern: &str) -> usize {
    pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']' | '{' | '}' | ','))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::config::{SandboxConfig, SandboxPresetConfig};

    fn sandbox_with_shell_rules(rules: &[(&str, PermissionAction)]) -> ResolvedSandbox {
        let mut config = SandboxConfig::default();
        config.preset = "test".to_string();
        let mut preset = SandboxPresetConfig {
            extends: Some("workspace".to_string()),
            ..Default::default()
        };
        preset.permissions = Some(crate::sandbox::config::PresetPermissions {
            shell: ShellPermissionRules {
                rules: rules
                    .iter()
                    .map(|(key, action)| ((*key).to_string(), *action))
                    .collect(),
            },
            ..Default::default()
        });
        config.presets.insert("test".to_string(), preset);
        config.resolve(Some("test")).unwrap()
    }

    fn action(rules: &[(&str, PermissionAction)], command: &str) -> PermissionMatch {
        evaluate_shell_permission(&sandbox_with_shell_rules(rules), command).action
    }

    #[test]
    fn simple_rules_match_normalized_argv_prefix() {
        assert_eq!(
            action(
                &[("git status", PermissionAction::Allow)],
                "git  \"status\""
            ),
            PermissionMatch::Allow
        );
        assert_eq!(
            action(&[("rm -rf", PermissionAction::Deny)], "/bin/rm -rf tmp"),
            PermissionMatch::Deny
        );
        assert_eq!(
            action(&[("rm -rf", PermissionAction::Deny)], "FOO=1 rm -rf tmp"),
            PermissionMatch::Deny
        );
    }

    #[test]
    fn longest_simple_match_wins_before_tie_break() {
        assert_eq!(
            action(
                &[
                    ("rm", PermissionAction::Ask),
                    ("rm -rf", PermissionAction::Deny)
                ],
                "rm -rf tmp"
            ),
            PermissionMatch::Deny
        );
    }

    #[test]
    fn multi_segment_commands_merge_actions() {
        assert_eq!(
            action(
                &[
                    ("git status", PermissionAction::Allow),
                    ("rm -rf", PermissionAction::Deny)
                ],
                "git status && rm -rf tmp"
            ),
            PermissionMatch::Deny
        );
        assert_eq!(
            action(
                &[
                    ("git status", PermissionAction::Allow),
                    ("cargo test", PermissionAction::Ask)
                ],
                "git status && cargo test"
            ),
            PermissionMatch::Ask
        );
    }

    #[test]
    fn static_pipeline_uses_each_simple_command_permission() {
        assert_eq!(action(&[], "ls -la . | head -40"), PermissionMatch::Allow);
        assert_eq!(action(&[], "git status | cat"), PermissionMatch::Allow);
        assert_eq!(
            action(&[("cat", PermissionAction::Ask)], "git status | cat"),
            PermissionMatch::Ask
        );
        assert_eq!(
            action(
                &[("rm -rf", PermissionAction::Deny)],
                "git status | rm -rf tmp"
            ),
            PermissionMatch::Deny
        );
    }

    #[test]
    fn redirect_and_background_require_ask_without_exact_allow() {
        assert_eq!(action(&[], "echo hi > out.txt"), PermissionMatch::Ask);
        assert_eq!(action(&[], "cat < input.txt"), PermissionMatch::Ask);
        assert_eq!(action(&[], "echo hi |& cat"), PermissionMatch::Ask);
        assert_eq!(action(&[], "sleep 10 &"), PermissionMatch::Ask);
    }

    #[test]
    fn exact_compound_allow_can_override_shell_syntax_ask() {
        assert_eq!(
            action(
                &[("echo ok > out.txt", PermissionAction::Allow)],
                "echo ok > out.txt"
            ),
            PermissionMatch::Allow
        );
    }

    #[test]
    fn exact_compound_allow_cannot_bypass_inner_deny() {
        assert_eq!(
            action(
                &[
                    ("rm -rf", PermissionAction::Deny),
                    ("echo ok && rm -rf tmp", PermissionAction::Allow)
                ],
                "echo ok && rm -rf tmp"
            ),
            PermissionMatch::Deny
        );
    }

    #[test]
    fn unmatched_command_defaults_to_allow() {
        assert_eq!(action(&[], "echo ok"), PermissionMatch::Allow);
    }

    #[test]
    fn inline_execution_is_only_matched_as_outer_argv() {
        assert_eq!(
            action(
                &[("bash -c", PermissionAction::Ask)],
                "bash -c \"rm -rf .\""
            ),
            PermissionMatch::Ask
        );
        assert_eq!(
            action(&[("node -e", PermissionAction::Ask)], "node -e \"1 + 1\""),
            PermissionMatch::Ask
        );
    }

    #[test]
    fn dynamic_command_name_requires_ask() {
        assert_eq!(action(&[], "$CMD -rf tmp"), PermissionMatch::Ask);
    }
}
