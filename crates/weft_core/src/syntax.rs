//! Shell command syntax highlighting — tokenizes a command line into colored
//! spans for the editor input box and the block-view command line.
//!
//! MVP: a hand-written character scanner (no dependency). It covers the common
//! shell constructs (commands, flags, paths, quoted strings, variables,
//! operators) well enough to color a line; it is NOT a full shell parser.

/// What kind of token a piece of a command line is. Drives its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The command name (first word, or the word after a `|` / `&&` / `;`).
    Command,
    /// A flag: `-x` / `--flag` (incl. `--flag=value`, colored as one span).
    Flag,
    /// A filesystem path: any word containing `/`.
    Path,
    /// A quoted string (`"…"` / `'…'`), including the quotes.
    String,
    /// A shell operator: `|` `>` `<` `>>` `&&` `||` `;` `&`.
    Operator,
    /// A variable reference: `$VAR` / `${VAR}`.
    Variable,
    /// A run of whitespace (kept so the whole line is covered, spacing preserved).
    Whitespace,
    /// Anything else (arguments, values).
    Default,
}

/// A token: a slice of the line + its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

impl Token {
    fn new(text: String, kind: TokenKind) -> Self {
        Self { text, kind }
    }
}

fn is_operator_char(c: char) -> bool {
    matches!(c, '|' | '>' | '<' | ';' | '&')
}

/// A "word" char: anything that is not whitespace, an operator, a quote, or a
/// `$` (which starts its own variable token). `=` is a word char so
/// `--flag=value` stays one span (MVP simplification).
fn is_word_char(c: char) -> bool {
    !c.is_whitespace() && !is_operator_char(c) && c != '"' && c != '\'' && c != '$'
}

fn classify_word(word: &str, at_command_position: bool) -> TokenKind {
    if at_command_position {
        TokenKind::Command
    } else if word.starts_with('-') {
        TokenKind::Flag
    } else if word.contains('/') {
        TokenKind::Path
    } else {
        TokenKind::Default
    }
}

/// Tokenize a command line into covering spans (every char belongs to exactly
/// one token). `at_command_position` tracks whether the next word is a command
/// (true at the start and after an operator).
pub fn tokenize(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut at_command_position = true;

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            let start = i;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            tokens.push(Token::new(chars[start..i].iter().collect(), TokenKind::Whitespace));
            continue;
        }

        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1; // opening quote
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2; // escaped char
                    continue;
                }
                if chars[i] == quote {
                    i += 1; // closing quote
                    break;
                }
                i += 1;
            }
            tokens.push(Token::new(chars[start..i].iter().collect(), TokenKind::String));
            at_command_position = false;
            continue;
        }

        if is_operator_char(c) {
            let start = i;
            while i < chars.len() && is_operator_char(chars[i]) {
                i += 1;
            }
            tokens.push(Token::new(chars[start..i].iter().collect(), TokenKind::Operator));
            at_command_position = true; // next word is a command
            continue;
        }

        if c == '$' {
            let start = i;
            i += 1; // consume '$'
            if i < chars.len() && chars[i] == '{' {
                // ${VAR}
                while i < chars.len() && chars[i] != '}' {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // consume '}'
                }
            } else {
                while i < chars.len() && is_word_char(chars[i]) {
                    i += 1;
                }
            }
            tokens.push(Token::new(chars[start..i].iter().collect(), TokenKind::Variable));
            at_command_position = false;
            continue;
        }

        // A plain word.
        let start = i;
        while i < chars.len() && is_word_char(chars[i]) {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let kind = classify_word(&word, at_command_position);
        tokens.push(Token::new(word, kind));
        at_command_position = false;
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<TokenKind> {
        tokenize(s).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn tokenize_single_command() {
        assert_eq!(
            tokenize("ls"),
            vec![Token::new("ls".into(), TokenKind::Command)]
        );
    }

    #[test]
    fn command_with_flag() {
        assert_eq!(
            kinds("ls -la"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Flag,
            ]
        );
    }

    #[test]
    fn command_with_path_arg() {
        assert_eq!(
            kinds("ls /tmp"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Path,
            ]
        );
    }

    #[test]
    fn variable_token() {
        assert_eq!(
            kinds("echo $HOME"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Variable,
            ]
        );
    }

    #[test]
    fn braced_variable() {
        assert_eq!(
            kinds("echo ${HOME}"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Variable,
            ]
        );
    }

    #[test]
    fn flag_with_value_is_one_span() {
        // MVP: `--out=rv` is a single Flag span (`=` is a word char).
        assert_eq!(
            kinds("cmd --out=rv"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Flag,
            ]
        );
    }

    #[test]
    fn tokenize_covers_the_whole_line() {
        // Every char must belong to exactly one token (concatenated text == input).
        for line in ["ls -la /tmp", "echo \"a b\"", "a | b && c", "cat $F > out"] {
            let joined: String = tokenize(line).into_iter().map(|t| t.text).collect();
            assert_eq!(joined, line, "tokens must cover the line exactly: {line}");
        }
    }

    // ── edge cases (Task 2) ────────────────────────────────────────────────

    #[test]
    fn quoted_string_preserves_spaces() {
        let toks = tokenize("echo \"a b c\"");
        let s = toks
            .iter()
            .find(|t| t.kind == TokenKind::String)
            .expect("a String token");
        assert_eq!(s.text, "\"a b c\"");
        // Only ONE Whitespace token (the separator between echo and the string);
        // the two spaces inside the quotes must NOT become separate Whitespace tokens.
        let ws = toks
            .iter()
            .filter(|t| t.kind == TokenKind::Whitespace)
            .count();
        assert_eq!(ws, 1, "internal spaces must stay inside the String token");
    }

    #[test]
    fn escaped_quote_inside_string() {
        // one String token containing the escaped quote
        let toks = tokenize("echo \"a\\\"b\"");
        let strings: Vec<&Token> = toks.iter().filter(|t| t.kind == TokenKind::String).collect();
        assert_eq!(strings.len(), 1);
        assert_eq!(strings[0].text, "\"a\\\"b\"");
    }

    #[test]
    fn pipe_resets_command_position() {
        assert_eq!(
            kinds("a | b"),
            vec![
                TokenKind::Command,   // a
                TokenKind::Whitespace,
                TokenKind::Operator,  // |
                TokenKind::Whitespace,
                TokenKind::Command,   // b (command after pipe)
            ]
        );
    }

    #[test]
    fn path_forms_as_arguments() {
        assert_eq!(
            kinds("cmd /abs/y rel/p ~/z"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Path, // /abs/y
                TokenKind::Whitespace,
                TokenKind::Path, // rel/p
                TokenKind::Whitespace,
                TokenKind::Path, // ~/z
            ]
        );
    }

    #[test]
    fn first_word_path_like_is_command() {
        // ./run is path-like but it's the command position -> Command
        assert_eq!(
            kinds("./run --x"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Flag,
            ]
        );
    }

    #[test]
    fn multiple_operators_split() {
        assert_eq!(
            kinds("a && b > c"),
            vec![
                TokenKind::Command,
                TokenKind::Whitespace,
                TokenKind::Operator, // &&
                TokenKind::Whitespace,
                TokenKind::Command, // b
                TokenKind::Whitespace,
                TokenKind::Operator, // >
                TokenKind::Whitespace,
                TokenKind::Command, // c
            ]
        );
    }

    #[test]
    fn single_quoted_string() {
        let toks = tokenize("echo 'a | b'");
        let s = toks.iter().find(|t| t.kind == TokenKind::String).unwrap();
        assert_eq!(s.text, "'a | b'");
        // the pipe inside single quotes is NOT an operator
        assert!(!toks.iter().any(|t| t.kind == TokenKind::Operator));
    }

    #[test]
    fn unclosed_quote_consumes_to_end() {
        // graceful: an unterminated quote reads to end-of-line as one String
        let toks = tokenize("echo \"oops");
        assert!(toks.iter().any(|t| t.kind == TokenKind::String && t.text == "\"oops"));
    }
}
