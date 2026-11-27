//! Incremental JSON tokens. Retained strings are bounded prefixes; their entire
//! spelling is still consumed/validated. IgnoredAny's permissive string rules
//! are deliberately separate from typed/key string rules.
use super::super::{Error, protocol};
#[derive(Debug)]
pub(super) enum Token {
    Object,
    Array,
    ObjectEnd,
    ArrayEnd,
    Colon,
    Comma,
    String(String),
    Number(Vec<u8>, bool),
    Null,
    Bool,
}
#[derive(Default)]
pub(super) struct Lexer {
    state: State,
}
#[derive(Default)]
enum State {
    #[default]
    Idle,
    String(Text),
    Number {
        bytes: Vec<u8>,
        stage: u8,
        long: bool,
    },
    Literal {
        word: &'static [u8],
        at: usize,
    },
}
struct Text {
    out: String,
    strict: bool,
    escape: u8,
    code: u32,
    high: u32,
    utf: [u8; 4],
    used: usize,
    need: usize,
}
impl Text {
    fn new(strict: bool) -> Self {
        Self {
            out: String::new(),
            strict,
            escape: 0,
            code: 0,
            high: 0,
            utf: [0; 4],
            used: 0,
            need: 0,
        }
    }
    fn add(&mut self, c: char) {
        if self.strict && self.out.len() <= 4096 {
            self.out.push(c);
        }
    }
    fn byte(&mut self, b: u8) -> Result<bool, Error> {
        if self.escape == 1 {
            self.escape = 0;
            match b {
                b'"' => self.add('"'),
                b'\\' => self.add('\\'),
                b'/' => self.add('/'),
                b'b' => self.add('\u{8}'),
                b'f' => self.add('\u{c}'),
                b'n' => self.add('\n'),
                b'r' => self.add('\r'),
                b't' => self.add('\t'),
                b'u' => {
                    self.escape = 2;
                    self.code = 0;
                }
                _ => return protocol(),
            }
            return Ok(false);
        }
        if (2..=5).contains(&self.escape) {
            let n = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return protocol(),
            };
            self.code = self.code * 16 + u32::from(n);
            self.escape += 1;
            if self.escape == 6 {
                self.escape = 0;
                if !self.strict {
                    return Ok(false);
                }
                if self.high != 0 {
                    if !(0xdc00..=0xdfff).contains(&self.code) {
                        return protocol();
                    }
                    let c = 0x10000 + ((self.high - 0xd800) << 10) + (self.code - 0xdc00);
                    self.high = 0;
                    self.add(char::from_u32(c).ok_or(Error::Protocol)?);
                } else if (0xd800..=0xdbff).contains(&self.code) {
                    self.high = self.code;
                    self.escape = 7;
                } else {
                    self.add(char::from_u32(self.code).ok_or(Error::Protocol)?);
                }
            }
            return Ok(false);
        }
        if self.escape == 7 {
            if b != b'\\' {
                return protocol();
            }
            self.escape = 8;
            return Ok(false);
        }
        if self.escape == 8 {
            if b != b'u' {
                return protocol();
            }
            self.escape = 2;
            self.code = 0;
            return Ok(false);
        }
        if self.need != 0 {
            if b & 0xc0 != 0x80 {
                return protocol();
            }
            self.utf[self.used] = b;
            self.used += 1;
            if self.used == self.need {
                let text =
                    std::str::from_utf8(&self.utf[..self.used]).map_err(|_| Error::Protocol)?;
                let c = text.chars().next().ok_or(Error::Protocol)?;
                self.add(c);
                self.need = 0;
                self.used = 0;
            }
            return Ok(false);
        }
        match b {
            b'"' => return Ok(true),
            b'\\' => self.escape = 1,
            0..=31 => return protocol(),
            0x20..=0x7f => self.add(b as char),
            _ => {
                if self.strict {
                    self.need = match b {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return protocol(),
                    };
                    self.utf[0] = b;
                    self.used = 1;
                }
            }
        }
        Ok(false)
    }
}
impl Lexer {
    pub(super) fn idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }
    /// Returns whether this byte was consumed (numbers end before delimiters).
    pub(super) fn byte(&mut self, b: u8, strict: bool) -> Result<(bool, Option<Token>), Error> {
        match &mut self.state {
            State::Idle => {
                let token = match b {
                    b' ' | b'\r' | b'\n' | b'\t' => return Ok((true, None)),
                    b'{' => Token::Object,
                    b'[' => Token::Array,
                    b'}' => Token::ObjectEnd,
                    b']' => Token::ArrayEnd,
                    b':' => Token::Colon,
                    b',' => Token::Comma,
                    b'"' => {
                        self.state = State::String(Text::new(strict));
                        return Ok((true, None));
                    }
                    b'n' | b't' | b'f' => {
                        self.state = State::Literal {
                            word: match b {
                                b'n' => b"null",
                                b't' => b"true",
                                _ => b"false",
                            },
                            at: 1,
                        };
                        return Ok((true, None));
                    }
                    b'-' | b'0'..=b'9' => {
                        self.state = State::Number {
                            bytes: vec![b],
                            stage: match b {
                                b'-' => 0,
                                b'0' => 1,
                                _ => 2,
                            },
                            long: false,
                        };
                        return Ok((true, None));
                    }
                    _ => return protocol(),
                };
                Ok((true, Some(token)))
            }
            State::String(text) => {
                if text.byte(b)? {
                    let State::String(text) = std::mem::take(&mut self.state) else {
                        unreachable!()
                    };
                    Ok((true, Some(Token::String(text.out))))
                } else {
                    Ok((true, None))
                }
            }
            State::Literal { word, at } => {
                if word.get(*at) != Some(&b) {
                    return protocol();
                }
                *at += 1;
                if *at == word.len() {
                    let token = if *word == b"null" {
                        Token::Null
                    } else {
                        Token::Bool
                    };
                    self.state = State::Idle;
                    Ok((true, Some(token)))
                } else {
                    Ok((true, None))
                }
            }
            State::Number { bytes, stage, long } => {
                let next = match (*stage, b) {
                    (0, b'0') => Some(1),
                    (0, b'1'..=b'9') => Some(2),
                    (2, b'0'..=b'9') => Some(2),
                    (1 | 2, b'.') => Some(3),
                    (1 | 2 | 4, b'e' | b'E') => Some(5),
                    (3 | 4, b'0'..=b'9') => Some(4),
                    (5, b'+' | b'-') => Some(6),
                    (5..=7, b'0'..=b'9') => Some(7),
                    _ => None,
                };
                if let Some(next) = next {
                    *stage = next;
                    if bytes.len() < 64 {
                        bytes.push(b);
                    } else {
                        *long = true;
                    }
                    Ok((true, None))
                } else {
                    if !matches!(*stage, 1 | 2 | 4 | 7) {
                        return protocol();
                    }
                    let State::Number { bytes, long, .. } = std::mem::take(&mut self.state) else {
                        unreachable!()
                    };
                    Ok((false, Some(Token::Number(bytes, long))))
                }
            }
        }
    }
    pub(super) fn finish(&mut self) -> Result<Option<Token>, Error> {
        match &self.state {
            State::Idle => Ok(None),
            State::Number {
                stage: 1 | 2 | 4 | 7,
                ..
            } => {
                let State::Number { bytes, long, .. } = std::mem::take(&mut self.state) else {
                    unreachable!()
                };
                Ok(Some(Token::Number(bytes, long)))
            }
            _ => protocol(),
        }
    }
}
// Four bits per ignored container, rather than a semantic frame/JSON Value.
// Maximum input remains64MiB; even adversarial ignored nesting cannot retain
// one allocation per node or a copy of the document.
#[derive(Default)]
pub(super) struct Skip {
    states: Vec<u8>,
    len: usize,
}
impl Skip {
    fn push(&mut self, state: u8) {
        if self.len % 2 == 0 {
            self.states.push(state);
        } else {
            *self.states.last_mut().unwrap() |= state << 4;
        }
        self.len += 1;
    }
    fn top(&self) -> Option<u8> {
        (self.len != 0).then(|| {
            let v = *self.states.last().unwrap();
            if self.len % 2 == 0 { v >> 4 } else { v & 15 }
        })
    }
    fn set(&mut self, s: u8) {
        let v = self.states.last_mut().unwrap();
        if self.len % 2 == 0 {
            *v = (*v & 15) | (s << 4);
        } else {
            *v = (*v & 0xf0) | s;
        }
    }
    fn pop(&mut self) {
        if self.len % 2 == 1 {
            self.states.pop();
        } else {
            *self.states.last_mut().unwrap() &= 15;
        }
        self.len -= 1;
    }
    pub(super) fn token(&mut self, t: &Token) -> Result<bool, Error> {
        if let Some(s) = self.top() {
            match t {
                Token::Colon if s == 2 => {
                    self.set(3);
                    return Ok(false);
                }
                Token::Comma if s == 4 || s == 7 => {
                    self.set(if s == 4 { 1 } else { 6 });
                    return Ok(false);
                }
                Token::ObjectEnd if s == 0 || s == 4 => {
                    self.pop();
                    return Ok(self.len == 0);
                }
                Token::ArrayEnd if s == 5 || s == 7 => {
                    self.pop();
                    return Ok(self.len == 0);
                }
                Token::String(_) if s == 0 || s == 1 => {
                    self.set(2);
                    return Ok(false);
                }
                _ => {
                    if !matches!(s, 3 | 5 | 6) {
                        return protocol();
                    }
                    self.set(if s == 3 { 4 } else { 7 });
                }
            }
        }
        match t {
            Token::Object => self.push(0),
            Token::Array => self.push(5),
            Token::String(_) | Token::Number(..) | Token::Null | Token::Bool => {
                return Ok(self.len == 0);
            }
            _ => return protocol(),
        };
        Ok(false)
    }
}
