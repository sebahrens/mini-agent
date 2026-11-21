//! Explicit argv-prefix grants for a deliberately small, portable shell subset.
//! No shell interpretation occurs here. Unsupported syntax retains normal Ask
//! or exact-script policy; it never authorizes only the start of a compound script.

#[derive(Clone)]
pub(crate) struct ShellPrefix {
    argv: Vec<String>,
    specificity: usize,
}

pub(crate) fn literal_argv(script: &str) -> Option<Vec<&str>> {
    if !script.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b' ' | b'\t' | b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'='
            )
    }) {
        return None;
    }
    let argv: Vec<_> = script.split_ascii_whitespace().collect();
    let executable = *argv.first()?;
    if !matches!(executable.as_bytes()[0], b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'.' | b'/')
        || executable.contains('=')
        || matches!(
            executable.to_ascii_lowercase().as_str(),
            "." | "case"
                | "coproc"
                | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "select"
                | "then"
                | "time"
                | "until"
                | "while"
                | "begin"
                | "process"
                | "end"
                | "dynamicparam"
                | "param"
                | "trap"
                | "switch"
                | "foreach"
                | "try"
                | "catch"
                | "finally"
                | "filter"
                | "workflow"
                | "class"
                | "enum"
                | "using"
                | "configuration"
                | "exit"
                | "return"
                | "break"
                | "continue"
                | "throw"
        )
    {
        return None;
    }
    Some(argv)
}

/// Programs whose next word can itself be a command or code, so a two-word
/// prefix would authorize an unbounded range of other programs.
const NEVER_OFFERED: [&str; 16] = [
    "sh", "bash", "zsh", "dash", "fish", "ksh", "env", "sudo", "doas", "xargs", "eval", "exec",
    "nohup", "timeout", "su", "find",
];

/// The prefix the prompt may offer as a reusable session grant for `script`:
/// its first two words, only when the whole script is a literal argv of at
/// least two words (so `ls; rm x` and quoted or expanded scripts get none) and
/// the program is not a launcher for other commands.
pub(crate) fn offered_prefix(script: &str) -> Option<String> {
    let argv = literal_argv(script)?;
    if argv.len() < 2 {
        return None;
    }
    let program = argv[0].rsplit('/').next().unwrap_or(argv[0]);
    if NEVER_OFFERED.contains(&program.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some(format!("{} {}", argv[0], argv[1]))
}

impl ShellPrefix {
    pub(crate) fn compatibility_fields(&self) -> (&[String], usize) {
        (&self.argv, self.specificity)
    }

    pub(crate) fn parse(script: &str) -> anyhow::Result<Self> {
        let argv = literal_argv(script).ok_or_else(|| anyhow::anyhow!(
            "prefix rules require a complete simple ASCII command: literal words separated by spaces/tabs, without quoting, expansion, operators, redirection, leading assignments or control syntax; example: git status"
        ))?;
        Ok(Self {
            specificity: argv.iter().map(|word| word.len()).sum::<usize>() + argv.len() - 1,
            argv: argv.into_iter().map(str::to_owned).collect(),
        })
    }

    pub(crate) fn matches(&self, argv: &[&str]) -> bool {
        argv.len() >= self.argv.len()
            && self
                .argv
                .iter()
                .zip(argv)
                .all(|(expected, actual)| expected.as_str() == *actual)
    }
    pub(crate) fn specificity(&self) -> usize {
        self.specificity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offered_prefix_needs_a_literal_two_word_command_that_is_not_a_launcher() {
        assert_eq!(
            offered_prefix("cargo test foo").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            offered_prefix("npm run lint --fix").as_deref(),
            Some("npm run")
        );
        for script in [
            "ls",
            "ls; rm x",
            "cargo test && rm x",
            "cargo test $(x)",
            "cargo test 'a b'",
            "bash -c ls",
            "/bin/sh script.sh",
            "sudo rm x",
            "find . -delete",
            "FOO=1 cargo test",
        ] {
            assert_eq!(offered_prefix(script), None, "{script}");
        }
    }

    #[test]
    fn portable_prefix_requires_the_whole_script_to_be_literal_argv() {
        let prefix = ShellPrefix::parse("git status").unwrap();
        for command in [
            "git status",
            "git\tstatus --short",
            "git status --porcelain=v1",
        ] {
            assert!(prefix.matches(&literal_argv(command).unwrap()));
        }
        for command in [
            "git status; rm -rf /",
            "git status && rm -rf /",
            "git status | cat",
            "git status &",
            "git status\nrm -rf /",
            "git status > out",
            "git status < in",
            "git status $(id)",
            "git status `id`",
            "git status '$HOME'",
            "git status \"--short\"",
            "git status @args",
            "git status --% %HOME%",
            "Write-Output a,b",
            "1+1",
            "-not 1",
            "git status *.rs",
            "git status ?",
            "git status [a]",
            "git status {a,b}",
            "git status \\;",
            "git status #comment",
            "git\u{a0}status",
            "git status\r",
            "git status\u{b}",
            "KEY=x git status",
            "time git status",
            ". script",
            "IF git status",
            "",
        ] {
            assert!(literal_argv(command).is_none(), "{command:?}");
        }
        assert!(!prefix.matches(&literal_argv("git status-extra").unwrap()));
        assert!(!prefix.matches(&literal_argv("gitx status").unwrap()));
    }
}
