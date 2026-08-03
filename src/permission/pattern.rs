use regex::Regex;

#[derive(Debug, Clone)]
pub struct Pattern {
    regex: Regex,
    pub original: String,
}

impl Pattern {
    pub fn new(pattern: &str) -> Self {
        let original = pattern.to_string();
        let expanded = crate::fs::expand_tilde(pattern);
        Pattern {
            regex: Regex::new(&glob_to_regex(&expanded))
                .expect("glob conversion must always produce a valid regular expression"),
            original,
        }
    }

    pub fn new_regex(pattern: &str) -> Result<Self, regex::Error> {
        let expanded = crate::fs::expand_tilde(pattern);
        Ok(Pattern {
            regex: Regex::new(&expanded)?,
            original: pattern.to_string(),
        })
    }

    pub fn matches(&self, input: &str) -> bool {
        self.regex.is_match(input)
    }
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() * 2);
    re.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push('.'),
            '.' => re.push_str("\\."),
            '\\' => re.push_str("\\\\"),
            '(' | ')' | '[' | ']' | '{' | '}' | '+' | '^' | '$' | '|' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}
