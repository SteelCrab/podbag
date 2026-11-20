//! `.dockerignore`: which context files a build leaves out. Patterns work like Docker's
//! (Go's filepath.Match, plus `**` for any number of directories, and `!` for exceptions);
//! the last pattern that matches a path, or one of its parents, decides.

#[derive(Debug, Clone, Default)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

#[derive(Debug, Clone)]
struct Pattern {
    exclusion: bool,
    tokens: Vec<Token>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Char(char),
    /// `?`: one character, not `/`.
    One,
    /// `*`: any characters but `/`.
    Star,
    /// `**/` or `**` inside a pattern: any number of whole directories, or none.
    Dirs,
    /// `**` at the end: anything.
    All,
    /// `[a-z]`, `[^a-z]`.
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Ignore {
    /// Read `.dockerignore` text: one pattern per line, `#` comments.
    pub fn parse(text: &str) -> Ignore {
        let mut patterns = Vec::new();
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut p = line.trim();
            if p.is_empty() {
                continue;
            }
            let exclusion = p.starts_with('!');
            if exclusion {
                p = p[1..].trim();
            }
            let cleaned = clean(p);
            let cleaned = match cleaned.strip_prefix('/') {
                Some(rest) if !rest.is_empty() => rest.to_string(),
                _ => cleaned,
            };
            patterns.push(Pattern {
                exclusion,
                tokens: compile(&cleaned),
            });
        }
        Ignore { patterns }
    }

    /// True when the context path (relative, `/`-separated) is left out.
    pub fn excludes(&self, path: &str) -> bool {
        let path = clean(path);
        let parents: Vec<&str> = match path.rfind('/') {
            Some(i) => path[..i].split('/').collect(),
            None => Vec::new(),
        };
        let mut matched = false;
        for p in &self.patterns {
            if p.exclusion != matched {
                continue;
            }
            let mut hit = matches(&p.tokens, &path.chars().collect::<Vec<_>>());
            if !hit {
                for i in 0..parents.len() {
                    let parent: Vec<char> = parents[..=i].join("/").chars().collect();
                    if matches(&p.tokens, &parent) {
                        hit = true;
                        break;
                    }
                }
            }
            if hit {
                matched = !p.exclusion;
            }
        }
        matched
    }

    /// True when some `!` pattern may bring back a file below a left-out directory.
    pub fn has_exceptions(&self) -> bool {
        self.patterns.iter().any(|p| p.exclusion)
    }
}

/// Go's filepath.Match for one path (`*` and `?` stop at `/`), as `COPY` sources use it.
pub fn glob(pattern: &str, path: &str) -> bool {
    matches(&compile(pattern), &path.chars().collect::<Vec<_>>())
}

/// True when a `COPY` source has wildcards.
pub fn has_wildcards(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// Go's filepath.Clean for `/`-separated paths.
pub fn clean(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

fn compile(pattern: &str) -> Vec<Token> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' if chars.get(i) == Some(&'*') => {
                i += 1;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                }
                tokens.push(if i >= chars.len() {
                    Token::All
                } else {
                    Token::Dirs
                });
            }
            '*' => tokens.push(Token::Star),
            '?' => tokens.push(Token::One),
            '\\' if i < chars.len() => {
                tokens.push(Token::Char(chars[i]));
                i += 1;
            }
            '[' => {
                let negated = matches!(chars.get(i), Some('^'));
                if negated {
                    i += 1;
                }
                let mut ranges = Vec::new();
                while i < chars.len() && chars[i] != ']' {
                    let lo = chars[i];
                    if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|c| *c != ']')
                    {
                        ranges.push((lo, chars[i + 2]));
                        i += 3;
                    } else {
                        ranges.push((lo, lo));
                        i += 1;
                    }
                }
                i += 1;
                tokens.push(Token::Class { negated, ranges });
            }
            c => tokens.push(Token::Char(c)),
        }
    }
    tokens
}

fn matches(tokens: &[Token], text: &[char]) -> bool {
    let Some((first, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match first {
        Token::Char(c) => text.first() == Some(c) && matches(rest, &text[1..]),
        Token::One => text.first().is_some_and(|c| *c != '/') && matches(rest, &text[1..]),
        Token::Class { negated, ranges } => {
            text.first().is_some_and(|c| {
                *c != '/' && ranges.iter().any(|(lo, hi)| lo <= c && c <= hi) != *negated
            }) && matches(rest, &text[1..])
        }
        Token::Star => {
            let mut n = 0;
            loop {
                if matches(rest, &text[n..]) {
                    return true;
                }
                if n >= text.len() || text[n] == '/' {
                    return false;
                }
                n += 1;
            }
        }
        // `(.*/)?`: nothing, or anything that ends with `/`.
        Token::Dirs => {
            if matches(rest, text) {
                return true;
            }
            (0..text.len()).any(|i| text[i] == '/' && matches(rest, &text[i + 1..]))
        }
        Token::All => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(pattern: &str, path: &str) -> bool {
        Ignore::parse(pattern).excludes(path)
    }

    /// Docker's own cases (moby/patternmatcher, TestMatches).
    #[test]
    fn dockers_cases() {
        let cases: &[(&str, &str, bool)] = &[
            ("**", "file", true),
            ("**", "/", true),
            ("**/", "file", true),
            ("**/", "/", true),
            ("**", "dir/file", true),
            ("**/", "dir/file", true),
            ("**/**", "dir/file", true),
            ("dir/**", "dir/file", true),
            ("dir/**", "dir/dir2/file", true),
            ("**/dir", "dir", true),
            ("**/dir", "dir/file", true),
            ("**/dir2/*", "dir/dir2/file", true),
            ("**/dir2/**", "dir/dir2/dir3/file", true),
            ("**file", "file", true),
            ("**file", "dir/file", true),
            ("**/file", "dir/file", true),
            ("**file", "dir/dir/file", true),
            ("**/file", "dir/dir/file", true),
            ("**/file*", "dir/dir/file", true),
            ("**/file*", "dir/dir/file.txt", true),
            ("**/file*txt", "dir/dir/file.txt", true),
            ("**/file*.txt", "dir/dir/file.txt", true),
            ("**/file*.txt*", "dir/dir/file.txt", true),
            ("**/**/*.txt", "dir/dir/file.txt", true),
            ("**/**/*.txt2", "dir/dir/file.txt", false),
            ("**/*.txt", "file.txt", true),
            ("**/**/*.txt", "file.txt", true),
            ("a**/*.txt", "a/file.txt", true),
            ("a**/*.txt", "a/dir/file.txt", true),
            ("a**/*.txt", "a/dir/dir/file.txt", true),
            ("a/*.txt", "a/dir/file.txt", false),
            ("a/*.txt", "a/file.txt", true),
            ("a/*.txt**", "a/file.txt", true),
            ("a[b-d]e", "ae", false),
            ("a[b-d]e", "ace", true),
            ("a[b-d]e", "aae", false),
            ("a[^b-d]e", "aze", true),
            (".*", ".foo", true),
            (".*", "foo", false),
            ("abc.def", "abcdef", false),
            ("abc.def", "abc.def", true),
            ("abc.def", "abcZdef", false),
            ("abc?def", "abcZdef", true),
            ("abc?def", "abcdef", false),
            ("a\\\\", "a\\", true),
            ("**/foo/bar", "foo/bar", true),
            ("**/foo/bar", "dir/foo/bar", true),
            ("**/foo/bar", "dir/dir2/foo/bar", true),
            ("abc/**", "abc", false),
            ("abc/**", "abc/def", true),
            ("abc/**", "abc/def/ghi", true),
            ("**/.foo", ".foo", true),
            ("**/.foo", "bar.foo", false),
            ("a(b)c/def", "a(b)c/def", true),
            ("a(b)c/def", "a(b)c/xyz", false),
            ("a.|)$(}+{bc", "a.|)$(}+{bc", true),
            (
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                true,
            ),
            (
                "dist/*.whl",
                "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
                true,
            ),
        ];
        for (pattern, path, want) in cases {
            assert_eq!(one(pattern, path), *want, "{pattern:?} against {path:?}");
        }
    }

    /// Docker's exclusion cases (TestMatchesWithMalformedPatterns aside).
    #[test]
    fn exceptions_and_parents() {
        let ig = Ignore::parse("*.go\n!keep.go\n# comment\n\n/docs\nbuild/**\n!build/out\n");
        assert!(ig.excludes("main.go"));
        assert!(!ig.excludes("keep.go"));
        assert!(ig.excludes("docs/a.md"));
        assert!(ig.excludes("docs"));
        assert!(ig.excludes("build/tmp/x"));
        assert!(!ig.excludes("build/out"));
        assert!(!ig.excludes("readme"));
        assert!(!ig.excludes("dir/main.go"), "* does not cross /");
        assert!(ig.has_exceptions());
        // A parent that matches leaves its children out.
        let ig = Ignore::parse("node_modules");
        assert!(ig.excludes("node_modules/a/b.js"));
        assert!(!ig.excludes("src/node_modules"));
        // Cleaned patterns.
        assert!(Ignore::parse("./a/../b/").excludes("b/c"));
        assert!(!Ignore::parse("# all\n").has_exceptions());
    }

    #[test]
    fn cleans_like_go() {
        assert_eq!(clean("a/./b//c/"), "a/b/c");
        assert_eq!(clean("a/../../b"), "../b");
        assert_eq!(clean("/../a"), "/a");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("./"), ".");
    }
}
