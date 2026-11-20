//! Variable expansion in Dockerfile words, the way Docker's shell lexer does it: `$V`,
//! `${V}`, `${V:-word}`, `${V-word}`, `${V:+word}`, `${V+word}`, `${V:?msg}`, single and
//! double quotes, and the escape character.

use crate::{Error, ErrorKind, Result};

/// Expand one word: variables from `lookup`, quotes removed, escapes applied. A name that
/// `lookup` does not know expands to nothing.
pub fn expand(word: &str, escape: char, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut lexer = Lexer {
        chars: &chars,
        pos: 0,
        escape,
        lookup,
    };
    lexer.word(None)
}

/// The names a word refers to, sorted: `$A ${B:-$C}` → A, B, C.
pub fn names_in(word: &str, escape: char) -> Vec<String> {
    let seen = std::cell::RefCell::new(Vec::new());
    let lookup = |name: &str| {
        seen.borrow_mut().push(name.to_string());
        None
    };
    let _ = expand(word, escape, &lookup);
    let mut names = seen.into_inner();
    names.sort();
    names.dedup();
    names
}

struct Lexer<'a> {
    chars: &'a [char],
    pos: usize,
    escape: char,
    lookup: &'a dyn Fn(&str) -> Option<String>,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Read until the end, or until `stop` (unquoted, unescaped) for the word inside
    /// `${V:-word}`.
    fn word(&mut self, stop: Option<char>) -> Result<String> {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            if Some(c) == stop {
                return Ok(out);
            }
            self.pos += 1;
            match c {
                '\'' => self.single_quoted(&mut out)?,
                '"' => self.double_quoted(&mut out)?,
                '$' => out.push_str(&self.dollar()?),
                c if c == self.escape => match self.next() {
                    Some(n) => out.push(n),
                    // An escape at the very end stays.
                    None => out.push(c),
                },
                c => out.push(c),
            }
        }
        match stop {
            Some(_) => Err(bad("a ${...} is not closed")),
            None => Ok(out),
        }
    }

    fn single_quoted(&mut self, out: &mut String) -> Result<()> {
        loop {
            match self.next() {
                Some('\'') => return Ok(()),
                Some(c) => out.push(c),
                None => return Err(bad("a single quote is not closed")),
            }
        }
    }

    fn double_quoted(&mut self, out: &mut String) -> Result<()> {
        loop {
            match self.next() {
                Some('"') => return Ok(()),
                Some('$') => out.push_str(&self.dollar()?),
                Some(c) if c == self.escape => {
                    match self.peek() {
                        Some(n) if n == '"' || n == '$' || n == self.escape => {
                            self.pos += 1;
                            out.push(n);
                        }
                        // Other escapes are kept as written.
                        _ => out.push(c),
                    }
                }
                Some(c) => out.push(c),
                None => return Err(bad("a double quote is not closed")),
            }
        }
    }

    /// After a `$`.
    fn dollar(&mut self) -> Result<String> {
        if self.peek() != Some('{') {
            let name = self.name();
            if name.is_empty() {
                return Ok("$".to_string());
            }
            return Ok(self.value(&name).unwrap_or_default());
        }
        self.pos += 1;
        let name = self.name();
        if name.is_empty() {
            return Err(bad("a ${} has no variable name"));
        }
        match self.next() {
            Some('}') => Ok(self.value(&name).unwrap_or_default()),
            Some(':') => {
                let op = self.next();
                let word = self.word(Some('}'))?;
                self.pos += 1;
                let value = self.value(&name).filter(|v| !v.is_empty());
                operator(op, &name, value, word)
            }
            Some(op @ ('-' | '+' | '?')) => {
                let word = self.word(Some('}'))?;
                self.pos += 1;
                let value = self.value(&name);
                operator(Some(op), &name, value, word)
            }
            _ => Err(bad(&format!("cannot read ${{{name}...}}"))),
        }
    }

    fn name(&mut self) -> String {
        let mut name = String::new();
        // A positional parameter is one digit; special ones (`$?`) expand to nothing here.
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                self.pos += 1;
                return c.to_string();
            }
            Some(c) if "*#@!?-$".contains(c) => return name,
            _ => {}
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        name
    }

    fn value(&self, name: &str) -> Option<String> {
        (self.lookup)(name)
    }
}

/// `value` is None when the variable is unset (or empty, for the `:` forms).
fn operator(op: Option<char>, name: &str, value: Option<String>, word: String) -> Result<String> {
    match op {
        Some('-') => Ok(value.unwrap_or(word)),
        Some('+') => Ok(if value.is_some() { word } else { String::new() }),
        Some('?') => value.ok_or_else(|| {
            let why = if word.is_empty() {
                format!("{name} is not set")
            } else {
                format!("{name}: {word}")
            };
            Error::new(
                ErrorKind::InvalidInput,
                "cannot build the image",
                why,
                format!("give it a value, for example with --build-arg {name}=..."),
            )
        }),
        _ => Err(bad(&format!("unsupported modifier in ${{{name}:...}}"))),
    }
}

fn bad(why: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot read the Dockerfile",
        why.to_string(),
        "fix the variable syntax",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(word: &str) -> String {
        let lookup = |name: &str| match name {
            "A" => Some("apple".to_string()),
            "EMPTY" => Some(String::new()),
            "SP" => Some("a b".to_string()),
            _ => None,
        };
        expand(word, '\\', &lookup).unwrap()
    }

    #[test]
    fn plain_and_braced_variables() {
        assert_eq!(env("$A"), "apple");
        assert_eq!(env("${A}s"), "apples");
        assert_eq!(env("x$A-y"), "xapple-y");
        assert_eq!(env("$NOPE"), "");
        assert_eq!(env("cost: $5"), "cost: ");
        assert_eq!(env("a$"), "a$");
        assert_eq!(env("$ A"), "$ A");
    }

    #[test]
    fn modifiers() {
        assert_eq!(env("${NOPE:-dflt}"), "dflt");
        assert_eq!(env("${EMPTY:-dflt}"), "dflt");
        assert_eq!(env("${EMPTY-dflt}"), "");
        assert_eq!(env("${NOPE-dflt}"), "dflt");
        assert_eq!(env("${A:-dflt}"), "apple");
        assert_eq!(env("${A:+set}"), "set");
        assert_eq!(env("${EMPTY:+set}"), "");
        assert_eq!(env("${EMPTY+set}"), "set");
        assert_eq!(env("${NOPE:+set}"), "");
        assert_eq!(env("${NOPE:-$A}"), "apple");
        assert_eq!(env("${NOPE:-${A}x}"), "applex");
        let lookup = |_: &str| None;
        let e = expand("${X:?needed}", '\\', &lookup).unwrap_err();
        assert!(e.why.contains("needed"), "{e}");
    }

    #[test]
    fn quotes_and_escapes() {
        assert_eq!(env("'$A'"), "$A");
        assert_eq!(env("\"$A b\""), "apple b");
        assert_eq!(env("\"\\$A\""), "$A");
        assert_eq!(env("\"a\\nb\""), "a\\nb");
        assert_eq!(env("\\$A"), "$A");
        assert_eq!(env("a\\ b"), "a b");
        assert_eq!(env("\"$SP\""), "a b");
        assert_eq!(env("it\"'\"s"), "it's");
        let lookup = |_: &str| Some("v".to_string());
        assert_eq!(expand("`$A", '`', &lookup).unwrap(), "$A");
        assert_eq!(expand("C:\\dir", '`', &lookup).unwrap(), "C:\\dir");
    }

    #[test]
    fn errors() {
        let lookup = |_: &str| None;
        assert!(expand("'open", '\\', &lookup).is_err());
        assert!(expand("\"open", '\\', &lookup).is_err());
        assert!(expand("${open", '\\', &lookup).is_err());
        assert!(expand("${}", '\\', &lookup).is_err());
    }

    #[test]
    fn names_are_collected() {
        assert_eq!(names_in("$A ${B:-$C} '$D'", '\\'), ["A", "B", "C"]);
    }
}
