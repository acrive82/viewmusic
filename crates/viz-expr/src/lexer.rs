//! Tokenizer for the ViewMusic formula language (contract §2).
//!
//! Produces a flat token stream the Pratt parser consumes. Identifiers are
//! atomic and may carry a single dotted suffix (`settings.bars`), matching the
//! contract's reserved-namespace rule. The lexer never allocates per token
//! beyond the small owned identifier `String` it must keep for diagnostics.

/// A lexical token with its byte offset in the source (for diagnostics).
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// The token classification.
    pub kind: TokenKind,
    /// Byte offset of the token's first character within the source string.
    pub position: usize,
}

/// Token classifications produced by [`lex`].
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// A JSON-style decimal numeric literal.
    Number(f64),
    /// An identifier, possibly dotted (`settings.bars`). Stored verbatim.
    Ident(String),
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `^`
    Caret,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    EqEq,
    /// `!=`
    Ne,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `!`
    Bang,
}

/// A tokenization failure with the offending text and its byte position.
#[derive(Clone, Debug, PartialEq)]
pub struct LexError {
    /// Offending character(s), verbatim (named in diagnostics).
    pub token: String,
    /// Byte offset of the error within the source.
    pub position: usize,
}

/// Tokenizes `src` into a flat [`Token`] stream.
///
/// Returns [`LexError`] on an unexpected character or a malformed number. Never
/// panics; never loops unboundedly (advances at least one byte per iteration).
pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let bytes = src.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'+' => push(&mut tokens, TokenKind::Plus, i, &mut i, 1),
            b'-' => push(&mut tokens, TokenKind::Minus, i, &mut i, 1),
            b'*' => push(&mut tokens, TokenKind::Star, i, &mut i, 1),
            b'/' => push(&mut tokens, TokenKind::Slash, i, &mut i, 1),
            b'%' => push(&mut tokens, TokenKind::Percent, i, &mut i, 1),
            b'^' => push(&mut tokens, TokenKind::Caret, i, &mut i, 1),
            b'(' => push(&mut tokens, TokenKind::LParen, i, &mut i, 1),
            b')' => push(&mut tokens, TokenKind::RParen, i, &mut i, 1),
            b',' => push(&mut tokens, TokenKind::Comma, i, &mut i, 1),
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(&mut tokens, TokenKind::Le, i, &mut i, 2);
                } else {
                    push(&mut tokens, TokenKind::Lt, i, &mut i, 1);
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(&mut tokens, TokenKind::Ge, i, &mut i, 2);
                } else {
                    push(&mut tokens, TokenKind::Gt, i, &mut i, 1);
                }
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(&mut tokens, TokenKind::EqEq, i, &mut i, 2);
                } else {
                    return Err(LexError {
                        token: "=".to_owned(),
                        position: i,
                    });
                }
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    push(&mut tokens, TokenKind::Ne, i, &mut i, 2);
                } else {
                    push(&mut tokens, TokenKind::Bang, i, &mut i, 1);
                }
            }
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    push(&mut tokens, TokenKind::AndAnd, i, &mut i, 2);
                } else {
                    return Err(LexError {
                        token: "&".to_owned(),
                        position: i,
                    });
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    push(&mut tokens, TokenKind::OrOr, i, &mut i, 2);
                } else {
                    return Err(LexError {
                        token: "|".to_owned(),
                        position: i,
                    });
                }
            }
            b'0'..=b'9' | b'.' => {
                let (tok, next) = lex_number(bytes, i)?;
                tokens.push(Token {
                    kind: tok,
                    position: i,
                });
                i = next;
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let (tok, next) = lex_ident(bytes, i);
                tokens.push(Token {
                    kind: tok,
                    position: i,
                });
                i = next;
            }
            _ => {
                // Emit the offending UTF-8 character verbatim for diagnostics.
                let ch_len = utf8_char_len(bytes, i);
                let token = String::from_utf8_lossy(&bytes[i..i + ch_len]).into_owned();
                return Err(LexError { token, position: i });
            }
        }
    }
    Ok(tokens)
}

/// Pushes a fixed-length operator token and advances the cursor.
fn push(tokens: &mut Vec<Token>, kind: TokenKind, position: usize, i: &mut usize, len: usize) {
    tokens.push(Token { kind, position });
    *i += len;
}

/// Lexes a JSON-style decimal number starting at `start`. Returns the token and
/// the index just past it. Accepts forms like `12`, `1.5`, `.5`, `1e3`, `2.5E-2`.
fn lex_number(bytes: &[u8], start: usize) -> Result<(TokenKind, usize), LexError> {
    let mut i = start;
    let mut seen_digit = false;
    // Integer part.
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        seen_digit = true;
        i += 1;
    }
    // Fractional part.
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            seen_digit = true;
            i += 1;
        }
    }
    if !seen_digit {
        return Err(LexError {
            token: ".".to_owned(),
            position: start,
        });
    }
    // Exponent part. Only consumed when a valid `e[+-]?digits` follows; otherwise
    // the `e` is left for the next token (so `1e` lexes as Number(1) then an
    // identifier `e`, which the parser then rejects as trailing input).
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let mut exp_digit = false;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            exp_digit = true;
            j += 1;
        }
        if exp_digit {
            i = j;
        }
    }
    let text = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
    match text.parse::<f64>() {
        Ok(v) => Ok((TokenKind::Number(v), i)),
        Err(_) => Err(LexError {
            token: text.to_owned(),
            position: start,
        }),
    }
}

/// Lexes an atomic identifier with an optional single dotted suffix
/// (`settings.bars`). Returns the token and the index just past it.
fn lex_ident(bytes: &[u8], start: usize) -> (TokenKind, usize) {
    let mut i = start;
    i = scan_ident_word(bytes, i);
    // Optional `.ident` suffix — atomic dotted identifier (contract §2.3).
    if i < bytes.len() && bytes[i] == b'.' {
        let after_dot = i + 1;
        if after_dot < bytes.len() && is_ident_start(bytes[after_dot]) {
            i = scan_ident_word(bytes, after_dot);
        }
    }
    let text = std::str::from_utf8(&bytes[start..i]).unwrap_or("");
    (TokenKind::Ident(text.to_owned()), i)
}

/// Advances past one `[a-zA-Z_][a-zA-Z0-9_]*` word. Assumes `bytes[start]` is a
/// valid identifier-start byte.
fn scan_ident_word(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() && is_ident_continue(bytes[i]) {
        i += 1;
    }
    i
}

/// True for an identifier's first byte: `[a-zA-Z_]`.
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

/// True for an identifier continuation byte: `[a-zA-Z0-9_]`.
fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Length in bytes of the UTF-8 character beginning at `bytes[i]`.
fn utf8_char_len(bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    let len = if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    };
    len.min(bytes.len() - i)
}
