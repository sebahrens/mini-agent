//! Conservative classifier for read-only shell scripts that standard mode may
//! run without a per-command prompt.
//!
//! The whole script is lexed (quotes honoured, no expansion performed) and it
//! qualifies only when EVERY command is on a small read-only allowlist with
//! read-only flags and no construct can write, expand or nest another command.
//! Anything unrecognised returns `false`, which leaves the normal `ask` policy.
//! Explicit deny rules are evaluated before this classifier and are unaffected.

use std::path::{Component, Path};

/// Whether `script` consists solely of allowlisted read-only commands.
/// `allow_git` is false when the session runs unsandboxed after a fallback:
/// git can launch repository-configured helpers (`diff.external`, textconv,
/// `core.fsmonitor`), which is why the built-in `git status` allow is withheld
/// in that case as well.
pub(crate) fn is_read_only_script(script: &str, working_dir: &str, allow_git: bool) -> bool {
    let Some(segments) = lex(script) else {
        return false;
    };
    if segments.is_empty() {
        return false;
    }
    segments
        .iter()
        .all(|words| command_is_read_only(words, working_dir, allow_git))
}

/// Split into pipeline/list segments of unquoted words. `None` on any syntax
/// that could write, expand, nest or background a command.
fn lex(script: &str) -> Option<Vec<Vec<String>>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = script.chars().peekable();

    macro_rules! end_word {
        () => {
            if in_word {
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
        };
    }
    macro_rules! end_segment {
        () => {
            end_word!();
            if !words.is_empty() {
                segments.push(std::mem::take(&mut words));
            }
        };
    }

    while let Some(c) = chars.next() {
        if c.is_control() && !matches!(c, '\n' | '\t') {
            return None;
        }
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
                        other => word.push(other),
                    }
                }
            }
            ' ' | '\t' => {
                end_word!();
            }
            '\n' | ';' => {
                end_segment!();
            }
            '|' => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                end_segment!();
            }
            '&' => {
                if chars.next()? != '&' {
                    return None;
                }
                end_segment!();
            }
            '>' | '<' | '`' | '$' | '\\' | '(' | ')' | '{' | '}' | '#' | '@' => return None,
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if !words.is_empty() {
        segments.push(words);
    }
    Some(segments)
}

fn path_ok(value: &str, working_dir: &str) -> bool {
    if value.starts_with('~') {
        return false;
    }
    let path = Path::new(value);
    if path.components().any(|part| part == Component::ParentDir) {
        return false;
    }
    !path.is_absolute() || path.starts_with(working_dir)
}

/// Path-confinement for an argument; the value of `--opt=value` is checked.
fn arg_ok(arg: &str, working_dir: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        return match long.split_once('=') {
            Some((_, value)) => path_ok(value, working_dir),
            None => true,
        };
    }
    if arg.starts_with('-') {
        return true;
    }
    path_ok(arg, working_dir)
}

fn short_cluster_has(arg: &str, letters: &[char]) -> bool {
    arg.starts_with('-')
        && !arg.starts_with("--")
        && arg.chars().skip(1).any(|c| letters.contains(&c))
}

fn command_is_read_only(words: &[String], working_dir: &str, allow_git: bool) -> bool {
    let Some((command, args)) = words.split_first() else {
        return false;
    };
    // A sed address such as `/start/,/end/p` is a script, not a path.
    if !args
        .iter()
        .all(|arg| (command == "sed" && is_sed_script(arg)) || arg_ok(arg, working_dir))
    {
        return false;
    }
    match command.as_str() {
        "ls" | "cat" | "head" | "wc" | "grep" => true,
        "tail" => {
            !args
                .iter()
                .any(|a| short_cluster_has(a, &['f', 'F']) || a.starts_with("--follow"))
                && !args.iter().any(|a| a.starts_with("--retry"))
        }
        "rg" => !args.iter().any(|a| {
            a.starts_with("--pre")
                || a.starts_with("--hostname-bin")
                || a == "--search-zip"
                || short_cluster_has(a, &['z'])
        }),
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
            ) || a.starts_with("-fprint")
                || a == "-fls"
        }),
        "sed" => sed_is_read_only(args),
        "git" => allow_git && git_is_read_only(args),
        _ => false,
    }
}

/// `sed -n [-E|-r] ADDR[,ADDR]p [files]` only: no in-place edit, script file
/// or write/execute command is expressible.
fn sed_is_read_only(args: &[String]) -> bool {
    let mut quiet = false;
    let mut script: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "-E" | "-r" | "--regexp-extended" => {}
            flag if flag.starts_with('-') && flag != "-" => return false,
            other if script.is_none() => script = Some(other),
            _ => {}
        }
    }
    quiet && script.is_some_and(is_sed_script)
}

fn is_sed_script(script: &str) -> bool {
    static SCRIPT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^(?:\d+|\$|/[^/\;{}]*/)(?:,(?:\d+|\$|/[^/\;{}]*/))?p$")
            .expect("trusted sed pattern must compile")
    });
    SCRIPT.is_match(script)
}

/// `git <subcommand>` directly (no `-c`, `-C`, `--git-dir` or other global
/// option, which could define aliases or redirect the repository).
fn git_is_read_only(args: &[String]) -> bool {
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    if !matches!(
        sub.as_str(),
        "log" | "show" | "diff" | "status" | "blame" | "rev-parse" | "ls-files"
    ) {
        return false;
    }
    !rest.iter().any(|a| {
        a == "-c"
            || a == "-O"
            || a.starts_with("--output")
            || a.starts_with("--ext-diff")
            || a.starts_with("--textconv")
            || a.starts_with("--exec-path")
            || a.starts_with("--git-dir")
            || a.starts_with("--work-tree")
            || a.starts_with("--open-files-in-pager")
            || a.starts_with("--upload-pack")
    })
}

#[cfg(test)]
mod tests {
    use super::is_read_only_script as ro;

    const WD: &str = "/work/repo";

    fn yes(script: &str) {
        assert!(ro(script, WD, true), "should auto-allow: {script}");
    }
    fn no(script: &str) {
        assert!(!ro(script, WD, true), "should ask: {script}");
    }

    #[test]
    fn plain_read_only_analysis_is_allowed() {
        for script in [
            "ls",
            "ls -la src",
            "cat Cargo.toml",
            "head -n 20 src/main.rs",
            "tail -n 5 log.txt",
            "wc -l src/main.rs",
            "grep -rn 'fn main' src",
            "rg --files-with-matches foo src",
            "find . -name '*.rs' -type f",
            "sed -n 1,20p src/main.rs",
            "sed -n '/start/,/end/p' src/lib.rs",
            "git log --oneline -n 5",
            "git show HEAD~1",
            "git diff HEAD~2..HEAD -- src",
            "git status --short",
            "git blame src/main.rs",
            "git rev-parse HEAD",
            "git ls-files src",
            "ls src && cat Cargo.toml | head -n 3",
            "cat /work/repo/Cargo.toml",
            "grep 'a->b' src | wc -l\nls",
        ] {
            yes(script);
        }
    }

    #[test]
    fn writers_redirections_and_expansions_ask() {
        for script in [
            "sed -i s/a/b/ f",
            "sed -n -i 1p f",
            "sed -n 1w out f",
            "sed -n '1e id' f",
            "sed -f script.sed f",
            "sed 1p f",
            "find . -delete",
            "find . -exec rm {} ;",
            "find . -execdir rm x ;",
            "find . -ok rm x ;",
            "find . -fprint out",
            "find . -fprintf out x",
            "echo hi > out",
            "cat a >> out",
            "ls 2> err",
            "ls &> err",
            "cat f > /dev/null",
            "cat < f",
            "ls $(rm -rf x)",
            "ls `rm x`",
            "cat \"$(rm x)\"",
            "cat $HOME/f",
            "ls; rm -rf x",
            "ls && mv a b",
            "cat f | tee out",
            "ls | xargs rm",
            "ls &",
            "FOO=1 ls",
            "(ls)",
            "git status @args",
            "{ ls; }",
            "ls # trailing",
            "git -c alias.x=!rm log",
            "git -C /tmp log",
            "git --git-dir=/x log",
            "git commit -m x",
            "git push",
            "git diff --output=out",
            "git diff --ext-diff",
            "git log --upload-pack=x",
            "rg --pre 'sh -c x' foo",
            "rg --pre=cmd foo",
            "rg -z foo",
            "tail -f log",
            "tail -F log",
            "rm file",
            "mv a b",
            "cargo test",
            "xargs ls",
            "tee out",
            "ls 'unterminated",
            "",
            ";",
        ] {
            no(script);
        }
    }

    #[test]
    fn paths_must_stay_in_the_workspace() {
        for script in [
            "cat /etc/passwd",
            "cat ~/.ssh/id_rsa",
            "cat ../secret",
            "ls /work/repo/../other",
            "grep -f /etc/shadow x",
            "grep --file=/etc/shadow x",
            "cat /work/repository/x",
        ] {
            no(script);
        }
        yes("grep --file=/work/repo/pats x");
    }

    #[test]
    fn git_withheld_when_unsandboxed() {
        assert!(!ro("git log", WD, false));
        assert!(ro("ls", WD, false));
    }

    fn checker(
        mode: crate::permission::SecurityMode,
        sandbox: crate::permission::SandboxResolution,
    ) -> crate::permission::checker::PermissionChecker {
        crate::permission::checker::PermissionChecker::new_for_sandbox(
            &crate::permission::PermissionConfigs::default(),
            mode,
            Some(std::env::temp_dir()),
            None,
            sandbox,
        )
        .unwrap()
    }

    #[test]
    fn checker_auto_allows_only_in_standard_mode_without_a_matching_rule() {
        use crate::permission::checker::CheckResult;
        use crate::permission::{SandboxResolution::*, SecurityMode};
        let mut standard = checker(SecurityMode::Standard, Enforced);
        assert_eq!(standard.check("bash", "ls -la"), CheckResult::Allowed);
        assert_eq!(standard.check("bash", "git log -n 3"), CheckResult::Allowed);
        assert_eq!(standard.check("bash", "ls; rm x"), CheckResult::Ask);
        assert_eq!(standard.check("bash", "find . -delete"), CheckResult::Ask);
        assert_eq!(standard.check("bash", "cat a > b"), CheckResult::Ask);
        let mut degraded = checker(SecurityMode::Standard, DegradedUnavailable);
        assert_eq!(degraded.check("bash", "ls"), CheckResult::Allowed);
        assert_eq!(degraded.check("bash", "git log"), CheckResult::Ask);
        let mut restrictive = checker(SecurityMode::Restrictive, Enforced);
        assert_eq!(restrictive.check("bash", "ls"), CheckResult::Ask);
    }
}
