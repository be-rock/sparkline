//! A small, error-tolerant Spark SQL lexer.
//!
//! It never fails: incomplete input (an unterminated string, comment or
//! backtick identifier) produces a token flagged `unterminated`. That is what
//! the highlighter, multi-line validator and completer need, since they all
//! run on half-typed SQL.

use crate::keywords::is_keyword;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    /// `-- ...` or `/* ... */` (bracketed comments nest in Spark).
    Comment,
    Keyword,
    Ident,
    /// `` `quoted identifier` ``
    QuotedIdent,
    /// `'...'`, `"..."`, or raw `r'...'`.
    String,
    Number,
    /// `${var}` / `$var` substitution.
    Variable,
    Operator,
    Punct,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offsets into the source.
    pub start: usize,
    pub end: usize,
    pub unterminated: bool,
}

impl Token {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.start..self.end]
    }

    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::Comment)
    }

    /// Is this the punctuation/operator `p`?
    pub fn is(&self, src: &str, p: &str) -> bool {
        matches!(self.kind, TokenKind::Punct | TokenKind::Operator) && self.text(src) == p
    }

    /// Is this the keyword `kw` (case-insensitive)?
    pub fn is_kw(&self, src: &str, kw: &str) -> bool {
        self.kind == TokenKind::Keyword && self.text(src).eq_ignore_ascii_case(kw)
    }
}

pub fn tokenize(src: &str) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let start = i;
        let c = bytes[i];
        let mut unterminated = false;

        let kind = if c.is_ascii_whitespace() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            TokenKind::Whitespace
        } else if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            TokenKind::Comment
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            unterminated = depth > 0;
            TokenKind::Comment
        } else if (c == b'r' || c == b'R') && matches!(bytes.get(i + 1), Some(b'\'' | b'"')) {
            let quote = bytes[i + 1];
            i += 2;
            match bytes[i..].iter().position(|&b| b == quote) {
                Some(p) => i += p + 1,
                None => {
                    i = bytes.len();
                    unterminated = true;
                }
            }
            TokenKind::String
        } else if c == b'\'' || c == b'"' {
            i += 1;
            unterminated = true;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b if b == c => {
                        i += 1;
                        unterminated = false;
                        break;
                    }
                    _ => i += 1,
                }
            }
            i = i.min(bytes.len());
            TokenKind::String
        } else if c == b'`' {
            i += 1;
            unterminated = true;
            while i < bytes.len() {
                if bytes[i] == b'`' {
                    // `` inside a quoted identifier is an escaped backtick.
                    if bytes.get(i + 1) == Some(&b'`') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    unterminated = false;
                    break;
                }
                i += 1;
            }
            TokenKind::QuotedIdent
        } else if c == b'$' {
            i += 1;
            if bytes.get(i) == Some(&b'{') {
                match bytes[i..].iter().position(|&b| b == b'}') {
                    Some(p) => i += p + 1,
                    None => {
                        i = bytes.len();
                        unterminated = true;
                    }
                }
            } else {
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
            }
            TokenKind::Variable
        } else if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            i = lex_number(bytes, i);
            TokenKind::Number
        } else if is_ident_start(src, i) {
            while i < bytes.len() && is_ident_continue(src, i) {
                i += char_len(src, i);
            }
            if is_keyword(&src[start..i]) {
                TokenKind::Keyword
            } else {
                TokenKind::Ident
            }
        } else if let Some(len) = operator_len(&bytes[i..]) {
            i += len;
            TokenKind::Operator
        } else if matches!(c, b'(' | b')' | b',' | b';' | b'.' | b'[' | b']' | b'{' | b'}') {
            i += 1;
            TokenKind::Punct
        } else {
            i += char_len(src, i);
            TokenKind::Unknown
        };

        tokens.push(Token { kind, start, end: i, unterminated });
    }
    tokens
}

fn lex_number(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        let mut j = i + 1;
        if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
            j += 1;
        }
        if j < bytes.len() && bytes[j].is_ascii_digit() {
            i = j;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    // Type suffixes: 1L, 1S, 1Y, 1.0D, 1.0F, 1.0BD
    if i + 1 < bytes.len() && bytes[i..i + 2].eq_ignore_ascii_case(b"bd") {
        if !bytes.get(i + 2).is_some_and(|b| is_ident_byte(*b)) {
            return i + 2;
        }
    }
    if i < bytes.len()
        && matches!(bytes[i].to_ascii_uppercase(), b'L' | b'S' | b'Y' | b'D' | b'F')
        && !bytes.get(i + 1).is_some_and(|b| is_ident_byte(*b))
    {
        i += 1;
    }
    i
}

fn operator_len(rest: &[u8]) -> Option<usize> {
    const OPS: &[&[u8]] = &[
        b"<=>", b"<=", b">=", b"<>", b"!=", b"==", b"||", b"&&", b"<<", b">>>", b">>", b"->",
        b"::", b"=", b"<", b">", b"+", b"-", b"*", b"/", b"%", b"&", b"|", b"^", b"~", b"!",
        b":", b"?",
    ];
    OPS.iter().find(|op| rest.starts_with(op)).map(|op| op.len())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_ident_start(src: &str, i: usize) -> bool {
    src[i..].chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
}

fn is_ident_continue(src: &str, i: usize) -> bool {
    src[i..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn char_len(src: &str, i: usize) -> usize {
    src[i..].chars().next().map_or(1, char::len_utf8)
}

/// Splits a script into statements on `;` (ignoring `;` inside strings and
/// comments). Returns trimmed statement text, skipping empty or comment-only
/// statements.
pub fn split_statements(src: &str) -> Vec<&str> {
    let tokens = tokenize(src);
    let mut out = Vec::new();
    let mut seg_start = 0;
    let mut has_code = false;
    for t in &tokens {
        if t.is(src, ";") {
            if has_code {
                out.push(src[seg_start..t.start].trim());
            }
            seg_start = t.end;
            has_code = false;
        } else if !t.is_trivia() {
            has_code = true;
        }
    }
    if has_code {
        out.push(src[seg_start..].trim());
    }
    out
}

/// True when the input is ready to run: nothing is left open and the last
/// significant token is `;`.
pub fn is_complete_statement(src: &str) -> bool {
    let tokens = tokenize(src);
    if tokens.iter().any(|t| t.unterminated) {
        return false;
    }
    tokens
        .iter()
        .rev()
        .find(|t| !t.is_trivia())
        .is_some_and(|t| t.is(src, ";"))
}

/// The first keyword of a statement, uppercased (e.g. `"SELECT"`), skipping
/// leading comments and parentheses.
pub fn leading_keyword(stmt: &str) -> Option<String> {
    tokenize(stmt)
        .iter()
        .find(|t| !t.is_trivia() && !t.is(stmt, "("))
        .filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident))
        .map(|t| t.text(stmt).to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str) -> Vec<(TokenKind, &str)> {
        tokenize(src)
            .into_iter()
            .filter(|t| t.kind != Whitespace)
            .map(|t| (t.kind, t.text(src)))
            .collect()
    }

    #[test]
    fn basic_select() {
        assert_eq!(
            kinds("SELECT a, count(*) FROM db.t WHERE x >= 1.5e3 -- hi"),
            vec![
                (Keyword, "SELECT"),
                (Ident, "a"),
                (Punct, ","),
                (Ident, "count"),
                (Punct, "("),
                (Operator, "*"),
                (Punct, ")"),
                (Keyword, "FROM"),
                (Ident, "db"),
                (Punct, "."),
                (Ident, "t"),
                (Keyword, "WHERE"),
                (Ident, "x"),
                (Operator, ">="),
                (Number, "1.5e3"),
                (Comment, "-- hi"),
            ]
        );
    }

    #[test]
    fn strings_and_quoted_idents() {
        assert_eq!(
            kinds(r#"'it\'s' "dq" r'raw\' `we``ird`"#),
            vec![(String, r"'it\'s'"), (String, r#""dq""#), (String, r"r'raw\'"), (QuotedIdent, "`we``ird`")]
        );
    }

    #[test]
    fn unterminated_tokens() {
        for src in ["SELECT 'abc", "SELECT `abc", "/* open", "SELECT ${x"] {
            assert!(tokenize(src).iter().any(|t| t.unterminated), "{src}");
        }
        assert!(!tokenize("/* a /* nested */ b */").iter().any(|t| t.unterminated));
    }

    #[test]
    fn number_suffixes() {
        assert_eq!(kinds("10L 1.0BD 2Y 3.5D"), vec![(Number, "10L"), (Number, "1.0BD"), (Number, "2Y"), (Number, "3.5D")]);
        // An identifier starting with digits-then-letters isn't a suffix.
        assert_eq!(kinds("1Lx").first(), Some(&(Number, "1")));
    }

    #[test]
    fn variables() {
        assert_eq!(kinds("${env:x} $y"), vec![(Variable, "${env:x}"), (Variable, "$y")]);
    }

    #[test]
    fn unicode_identifiers() {
        assert_eq!(kinds("SELECT café"), vec![(Keyword, "SELECT"), (Ident, "café")]);
    }

    #[test]
    fn statement_splitting() {
        let src = "select 1; -- c\n select ';' ; /* only comment */ ; select 2";
        assert_eq!(split_statements(src), vec!["select 1", "-- c\n select ';'", "select 2"]);
    }

    #[test]
    fn completeness() {
        assert!(is_complete_statement("select 1;"));
        assert!(is_complete_statement("select 1; -- trailing comment"));
        assert!(!is_complete_statement("select 1"));
        assert!(!is_complete_statement("select ';"));
        assert!(!is_complete_statement("select 1 /* ; "));
    }

    #[test]
    fn leading_kw() {
        assert_eq!(leading_keyword("  -- c\n (select 1)").as_deref(), Some("SELECT"));
        assert_eq!(leading_keyword("use db").as_deref(), Some("USE"));
    }
}
