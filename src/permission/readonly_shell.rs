//! Conservative classifier for read-only shell scripts that standard mode may
//! run without a per-command prompt.
//!
//! The whole script is lexed (quotes honoured, no expansion performed) and it
//! qualifies only when EVERY command is on a small read-only allowlist with
//! read-only flags and no construct can write, expand or nest another command.
//! Anything unrecognised returns `false`, which leaves the normal `ask` policy.
//! Explicit deny rules are evaluated before this classifier and are unaffected.

use std::path::{Component, Path};

/// What the checker must still verify before auto-allowing a classified script.
#[derive(Debug, Default)]
pub(crate) struct ReadOnlyScript {
    /// Every non-flag argument (and `--opt=value` value): resolved by the
    /// checker, which requires it to stay inside the workspace and pass the
    /// read policy, so a symlinked argument cannot reach outside.
    pub(crate) paths: Vec<String>,
    /// Some command traverses directories; auto-allow only under an active sandbox.
    pub(crate) recursive: bool,
    /// A `git log|show|diff|blame` can print blob contents of every tracked
    /// file in history, not only the arguments: with any read deny rule
    /// present the checker asks instead of auto-allowing (mini-agent-qc4cf).
    pub(crate) reads_history: bool,
}

/// Classify `script`; `None` unless it consists solely of allowlisted
/// read-only commands. `allow_git` is false when the session runs unsandboxed
/// after a fallback: git can launch repository-configured helpers
/// (`diff.external`, textconv, `core.fsmonitor`).
pub(crate) fn classify(script: &str, working_dir: &str, allow_git: bool) -> Option<ReadOnlyScript> {
    let segments = lex(script)?;
    if segments.is_empty() {
        return None;
    }
    let mut report = ReadOnlyScript::default();
    for words in &segments {
        command_is_read_only(words, working_dir, allow_git, &mut report).then_some(())?;
    }
    Some(report)
}

#[cfg(test)]
pub(crate) fn is_read_only_script(script: &str, working_dir: &str, allow_git: bool) -> bool {
    classify(script, working_dir, allow_git).is_some()
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
            // An unquoted glob is expanded by the shell to paths the checker
            // never sees (`cat .e*` -> `.env`, `cat lin*/x` through a
            // symlink); quoted globs stay literal and are handled above.
            '*' | '?' | '[' => return None,
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

/// GNU/BSD `getopt_long` accepts any unambiguous prefix of a long option, so a
/// blocked option must also match its abbreviations (`--der` for
/// `--dereference-recursive`). `arg` matches when its name (before any `=`)
/// is a non-empty prefix of `full`; ambiguous prefixes are over-blocked on
/// purpose (the command would fail anyway).
fn long_opt_is(arg: &str, full: &str) -> bool {
    let Some(long) = arg.strip_prefix("--") else {
        return false;
    };
    let name = long.split_once('=').map_or(long, |(name, _)| name);
    !name.is_empty() && full.starts_with(name)
}

/// Values attached to a short option inside a cluster (`-f/etc/x`, `-nf.env`).
/// Only options whose value is a file path are returned; every other
/// value-taking option swallows the rest of the cluster without yielding it.
fn attached_short_path_values<'a>(
    args: &'a [String],
    takes_value: &str,
    file: char,
) -> Vec<&'a str> {
    let mut values = Vec::new();
    for arg in args {
        if !arg.starts_with('-') || arg.starts_with("--") {
            continue;
        }
        for (index, c) in arg.char_indices().skip(1) {
            if takes_value.contains(c) {
                let rest = &arg[index + c.len_utf8()..];
                if c == file && !rest.is_empty() {
                    values.push(rest);
                }
                break;
            }
        }
    }
    values
}

fn short_cluster_has(arg: &str, letters: &[char]) -> bool {
    arg.starts_with('-')
        && !arg.starts_with("--")
        && arg.chars().skip(1).any(|c| letters.contains(&c))
}

/// GNU grep parses the `-d`/`--directories` ACTION with argmatch, which accepts
/// unambiguous abbreviations (`rec`, `recu`, ...). Treat any action other than
/// the exact non-traversing `read`/`skip` as a recursive walk (fail closed).
fn grep_directories_action_may_recurse(args: &[String]) -> bool {
    let non_traversing = |v: &str| v == "read" || v == "skip";
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        if a == "--" {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            if !name.is_empty() && "directories".starts_with(name) {
                let v = match value {
                    Some(v) => v,
                    None => {
                        let v = args.get(i).map(String::as_str).unwrap_or("");
                        i += 1;
                        v
                    }
                };
                if !non_traversing(v) {
                    return true;
                }
            }
        } else if let Some(cluster) = a.strip_prefix('-')
            && let Some(pos) = cluster.find('d')
        {
            let rest = &cluster[pos + 1..];
            let v = if rest.is_empty() {
                let v = args.get(i).map(String::as_str).unwrap_or("");
                i += 1;
                v
            } else {
                rest
            };
            if !non_traversing(v) {
                return true;
            }
        }
    }
    false
}

/// Whether the grep arguments select directory recursion by any spelling:
/// `-r`, `--recursive` and abbreviations, any non-`read`/`skip` `--directories`
/// action (`-d recurse`, `-drecurse`, `--dir=rec`), or a bare `recurse` word.
/// Fail closed: the union of every spelling we know to traverse.
fn grep_selects_recursion(args: &[String]) -> bool {
    args.iter().any(|a| {
        short_cluster_has(a, &['r'])
            || long_opt_is(a, "recursive")
            || a == "recurse"
            || (a.starts_with('-') && a.ends_with("recurse"))
    }) || grep_directories_action_may_recurse(args)
}

fn command_is_read_only(
    words: &[String],
    working_dir: &str,
    allow_git: bool,
    report: &mut ReadOnlyScript,
) -> bool {
    let Some((command, args)) = words.split_first() else {
        return false;
    };
    // A sed address such as `/start/,/end/p` is a script, not a path; only the
    // one operand `sed_is_read_only` selects as the script is exempt, so a
    // file operand like `/etc/p` is still confined and screened.
    let sed_script = (command == "sed").then(|| sed_script_index(args)).flatten();
    // After a bare `--` every word is an operand, even one that starts with
    // `-` (a legal file name), so it is confined and screened like any path.
    let end_of_options = args.iter().position(|a| a == "--");
    let is_operand = |index: usize| end_of_options.is_some_and(|at| index > at);
    if !args.iter().enumerate().all(|(index, arg)| {
        sed_script == Some(index)
            || if is_operand(index) {
                path_ok(arg, working_dir)
            } else {
                arg_ok(arg, working_dir)
            }
    }) {
        return false;
    }
    for (index, arg) in args.iter().enumerate() {
        if sed_script == Some(index) {
            continue;
        }
        if is_operand(index) {
            report.paths.push(arg.clone());
        } else if let Some(long) = arg.strip_prefix("--") {
            if let Some((_, value)) = long.split_once('=') {
                report.paths.push(value.to_string());
            }
        } else if !arg.starts_with('-') {
            report.paths.push(arg.clone());
        }
    }
    // `-f/path` / `-nf.env`: the file operand is attached to the short option.
    let attached = match command.as_str() {
        "grep" => attached_short_path_values(args, "efmABCdD", 'f'),
        "rg" => attached_short_path_values(args, "efgmrtTABCjMEd", 'f'),
        _ => Vec::new(),
    };
    for value in attached {
        if !path_ok(value, working_dir) {
            return false;
        }
        report.paths.push(value.to_string());
    }
    match command.as_str() {
        "ls" => {
            report.recursive |= args
                .iter()
                .any(|a| short_cluster_has(a, &['R']) || long_opt_is(a, "recursive"));
            !args.iter().any(|a| {
                short_cluster_has(a, &['L', 'H']) || long_opt_is(a, "dereference-command-line")
            }) && !args.iter().any(|a| a.starts_with("--deref"))
        }
        "cat" | "head" | "wc" => true,
        "grep" => {
            if args
                .iter()
                .any(|a| short_cluster_has(a, &['R']) || long_opt_is(a, "dereference-recursive"))
            {
                return false;
            }
            report.recursive |= grep_selects_recursion(args);
            true
        }
        "tail" => !args.iter().any(|a| {
            short_cluster_has(a, &['f', 'F']) || long_opt_is(a, "follow") || long_opt_is(a, "retry")
        }),
        "rg" => {
            report.recursive = true;
            !args.iter().any(|a| {
                a.starts_with("--pre")
                    || a.starts_with("--hostname-bin")
                    || a == "--search-zip"
                    || a == "--follow"
                    || short_cluster_has(a, &['z', 'L'])
            })
        }
        "find" => {
            report.recursive = true;
            !args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-L" | "-H" | "-follow"
                ) || a.starts_with("-fprint")
                    || a == "-fls"
            })
        }
        "sed" => sed_is_read_only(args),
        "git" => {
            if !(allow_git && git_is_read_only(args)) {
                return false;
            }
            if matches!(
                args.first().map(String::as_str),
                Some("log" | "show" | "diff" | "blame")
            ) {
                report.reads_history = true;
            }
            // `REV:path` names a blob; screen both sides as paths so the
            // path part is matched against read deny rules.
            for arg in args.iter().filter(|a| !a.starts_with('-')) {
                if let Some((rev, path)) = arg.split_once(':') {
                    for part in [rev, path] {
                        if !part.is_empty() {
                            report.paths.push(part.to_string());
                        }
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// `sed -n [-E|-r] ADDR[,ADDR]p [files]` only: no in-place edit, script file
/// or write/execute command is expressible.
fn sed_is_read_only(args: &[String]) -> bool {
    let mut quiet = false;
    for arg in args {
        match arg.as_str() {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "-E" | "-r" | "--regexp-extended" => {}
            // GNU getopt permutes, so a flag after the script counts too.
            flag if flag.starts_with('-') && flag != "-" => return false,
            _ => {}
        }
    }
    quiet && sed_script_index(args).is_some_and(|index| is_sed_script(&args[index]))
}

/// Index of the first operand that is not one of the accepted sed flags: the
/// script. Every later operand is a file.
fn sed_script_index(args: &[String]) -> Option<usize> {
    args.iter().position(|arg| {
        !matches!(
            arg.as_str(),
            "-n" | "--quiet" | "--silent" | "-E" | "-r" | "--regexp-extended"
        ) && (!arg.starts_with('-') || arg == "-")
    })
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
    // `-O<orderfile>` (attached, or inside a short cluster such as `-pO<file>`)
    // and `--orderfile[=<file>]` read an arbitrary file operand that is never
    // screened as a path; git accepts unambiguous abbreviations of long
    // options, so `--orderf=` and `--outp=` count too. Refuse them all.
    let long_name_is_prefix_of = |arg: &str, full: &str| {
        arg.strip_prefix("--").is_some_and(|rest| {
            let name = rest.split('=').next().unwrap_or_default();
            !name.is_empty() && full.starts_with(name)
        })
    };
    !rest.iter().any(|a| {
        a == "-c"
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('O'))
            || long_name_is_prefix_of(a, "orderfile")
            || long_name_is_prefix_of(a, "output")
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
            "git diff -O/etc/shadow",
            "git diff -O../../x",
            "git diff -pO/etc/shadow",
            "git log -O/etc/passwd",
            "git diff --orderfile=/etc/shadow",
            "git diff --orderf=/etc/shadow",
            "git diff --outp=out",
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
    fn link_following_flags_ask() {
        for script in [
            "grep -R foo .",
            "grep --dereference-recursive foo .",
            "rg -L foo",
            "rg --follow foo",
            "find -L . -name x",
            "find -H . -name x",
            "find . -follow",
            "ls -L",
            "ls -H",
            "ls --dereference",
        ] {
            no(script);
        }
    }

    #[test]
    fn abbreviated_and_attached_options_cannot_evade_the_screen() {
        for script in [
            "grep --der -n . link",
            "grep --dereference-r foo .",
            "tail --fol app.log",
            "tail --foll app.log",
            "tail --ret app.log",
            "tail --retr app.log",
            "ls --deref",
            "ls --dereference-command-line",
            "grep -f/etc/x foo",
            "grep -nf/etc/x foo",
            "grep -f../x foo",
            "rg -f/etc/x foo",
        ] {
            no(script);
        }
        yes("grep -fpats foo src");
        let report = |s: &str| super::classify(s, WD, true).unwrap();
        for script in [
            "grep --rec -n foo src",
            "grep --recu foo src",
            "grep --directories=recurse -n foo src",
            "grep -drecurse foo src",
            "grep -d recurse foo src",
            "grep -d rec -n SECRET .",
            "grep -drec foo src",
            "grep --directories=rec foo src",
            "grep --dir=recu foo src",
            "grep --directories rec foo src",
            "grep -nd rec foo src",
            "grep -ndrec foo src",
            "ls -R .",
            "ls --recursive .",
            "ls --rec .",
        ] {
            assert!(report(script).recursive, "should be recursive: {script}");
        }
        assert!(!report("ls -la src").recursive);
        for script in [
            "grep -d read foo src",
            "grep -d skip foo src",
            "grep --directories=skip foo src",
        ] {
            assert!(!report(script).recursive, "not recursive: {script}");
        }
        assert!(
            report("grep -f.env foo src")
                .paths
                .contains(&".env".to_string())
        );
        assert!(
            report("grep -nf.env foo src")
                .paths
                .contains(&".env".to_string())
        );
    }

    #[test]
    fn dash_leading_operands_after_double_dash_are_confined_and_screened() {
        // `-k` may be a symlink to a secret: it must reach the path checker.
        for script in ["cat -- -k", "head -- -k", "wc -c -- -k"] {
            let report = super::classify(script, WD, true).unwrap();
            assert!(
                report.paths.contains(&"-k".to_string()),
                "operand screened: {script}"
            );
        }
        // Confinement applies to dash-leading operands too.
        for script in [
            "cat -- -d/../../../etc/passwd",
            "cat -- --x/../../y",
            "cat -n -- ok -d/../x",
        ] {
            no(script);
        }
        yes("cat -n -- -k");
        // Before `--`, flags stay flags.
        let report = super::classify("cat -n a -- b", WD, true).unwrap();
        assert_eq!(report.paths, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn sed_file_operands_that_look_like_scripts_are_still_screened() {
        for script in [
            "sed -n 1p /etc/p",
            "sed -n 1p /tmp/p",
            "sed -n 1p /root/p",
            "sed -n 1,2p /home/p",
            "sed -n 1p src/a /etc/p",
            "sed -n 1p f -i",
        ] {
            no(script);
        }
        let report = super::classify("sed -n 1p /work/repo/p", WD, true).unwrap();
        assert_eq!(report.paths, vec!["/work/repo/p".to_string()]);
        yes("sed -n '/start/,/end/p' src/lib.rs");
    }

    #[test]
    fn recursion_is_reported() {
        let report = |s: &str| super::classify(s, WD, true).unwrap().recursive;
        assert!(report("grep -rn foo src"));
        assert!(report("rg foo"));
        assert!(report("find . -name x"));
        assert!(!report("grep foo src/main.rs"));
        assert!(!report("cat a | head"));
        for script in [
            "grep --directories=recurse -n KEY .",
            "grep --directories recurse KEY .",
            "grep -drecurse KEY .",
            "grep -d recurse KEY .",
            "grep -nd recurse KEY .",
            "grep -ndrecurse KEY .",
            "grep --dir=rec KEY .",
            "grep --rec KEY .",
            "grep --recur KEY .",
            "grep --recursive KEY .",
        ] {
            assert!(report(script), "{script}");
        }
        assert!(!report("grep --directories=skip KEY file"));
        assert!(!report("grep -d skip KEY file"));
    }

    #[test]
    fn abbreviated_dereference_recursive_asks() {
        for script in [
            "grep --dereference-rec foo .",
            "grep --dereference-recur foo .",
            "grep --der foo .",
        ] {
            no(script);
        }
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

    #[cfg(unix)]
    #[test]
    fn checker_refuses_symlinks_leaving_the_workspace_and_uncontained_recursion() {
        use crate::permission::checker::CheckResult;
        use crate::permission::{PermissionConfigs, SandboxResolution::*, SecurityMode};
        let fixture = crate::tests::OwnedTestDirectory::new("readonly-shell-links");
        let root = fixture.path().canonicalize().unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(work.join("src")).unwrap();
        std::fs::write(root.join("secret"), b"s").unwrap();
        std::fs::write(work.join("src/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink(root.join("secret"), work.join("link")).unwrap();
        std::os::unix::fs::symlink(work.join("src/a.txt"), work.join("inner")).unwrap();
        let make = |sandbox| {
            crate::permission::checker::PermissionChecker::new_for_sandbox(
                &PermissionConfigs::default(),
                SecurityMode::Standard,
                Some(work.clone()),
                None,
                sandbox,
            )
            .unwrap()
        };
        let mut enforced = make(Enforced);
        assert_eq!(
            enforced.check("bash", "cat src/a.txt"),
            CheckResult::Allowed
        );
        assert_eq!(enforced.check("bash", "cat inner"), CheckResult::Allowed);
        assert_eq!(enforced.check("bash", "cat link"), CheckResult::Ask);
        assert_eq!(enforced.check("bash", "head -n 1 link"), CheckResult::Ask);
        assert_eq!(
            enforced.check("bash", "grep -rn a src"),
            CheckResult::Allowed
        );
        let mut disabled = make(Disabled);
        assert_eq!(
            disabled.check("bash", "cat src/a.txt"),
            CheckResult::Allowed
        );
        assert_eq!(disabled.check("bash", "cat link"), CheckResult::Ask);
        assert_eq!(disabled.check("bash", "grep -rn a src"), CheckResult::Ask);
        assert_eq!(disabled.check("bash", "find . -name x"), CheckResult::Ask);
        assert_eq!(
            disabled.check("bash", "grep -n a src/a.txt"),
            CheckResult::Allowed
        );
    }

    #[test]
    fn unquoted_globs_ask_but_quoted_stay_literal() {
        for script in [
            "cat .e*",
            "cat .en?",
            "head .e[n]v",
            "cat lin*/x",
            "ls src/*",
            "grep a* f",
            "cat \"ok\" .e*",
            "ls \"a\"*",
        ] {
            no(script);
        }
        yes("find . -name '*.rs'");
        yes("grep 'a*' f");
        yes("grep \"a?[b]\" f");
    }

    #[cfg(unix)]
    #[test]
    fn glob_and_read_deny_rules_block_shell_auto_allow() {
        use crate::permission::checker::CheckResult;
        use crate::permission::{
            Action, PermissionConfigs, SandboxResolution::*, SecurityMode, ToolPerm,
        };
        let fixture = crate::tests::OwnedTestDirectory::new("readonly-shell-deny");
        let work = fixture.path().canonicalize().unwrap();
        std::fs::write(work.join(".env"), b"s").unwrap();
        std::fs::write(work.join("Cargo.toml"), b"c").unwrap();
        let make = |deny: bool| {
            let mut configs = PermissionConfigs::default();
            if deny {
                configs.glob.read = Some(ToolPerm::Granular(
                    [("**/.env".to_string(), Action::Deny)].into(),
                ));
            }
            crate::permission::checker::PermissionChecker::new_for_sandbox(
                &configs,
                SecurityMode::Standard,
                Some(work.clone()),
                None,
                Enforced,
            )
            .unwrap()
        };
        let mut denied = make(true);
        assert_eq!(denied.check("bash", "cat .e*"), CheckResult::Ask);
        assert_eq!(denied.check("bash", "grep -rn x ."), CheckResult::Ask);
        assert_eq!(denied.check("bash", "rg x"), CheckResult::Ask);
        assert_eq!(
            denied.check("bash", "find . -name '*.pem'"),
            CheckResult::Ask
        );
        assert_eq!(denied.check("bash", "cat Cargo.toml"), CheckResult::Allowed);
        // History readers print denied blobs the argument screen cannot see.
        for script in [
            "git show HEAD:.env",
            "git show :.env",
            "git log -p",
            "git show",
            "git show HEAD",
            "git diff",
            "git diff HEAD~1",
            "git blame .env",
            "git blame Cargo.toml",
            "git log -p | head",
        ] {
            assert_eq!(denied.check("bash", script), CheckResult::Ask, "{script}");
        }
        assert_eq!(denied.check("bash", "git status"), CheckResult::Allowed);
        let mut open = make(false);
        assert_eq!(open.check("bash", "git log -p"), CheckResult::Allowed);
        assert_eq!(
            open.check("bash", "git show HEAD:Cargo.toml"),
            CheckResult::Allowed
        );
        assert_eq!(open.check("bash", "grep -rn x ."), CheckResult::Allowed);
        assert_eq!(open.check("bash", "cat .e*"), CheckResult::Ask);
    }
}
