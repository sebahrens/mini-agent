use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;

#[derive(Debug, Default)]
struct SourceProvenance {
    bindings: BTreeMap<String, Option<Vec<String>>>,
    qualified_bindings: BTreeMap<Vec<String>, Option<Vec<String>>>,
    local_names: BTreeSet<String>,
    module_path: Vec<String>,
    opaque_modules: BTreeSet<Vec<String>>,
}

impl SourceProvenance {
    fn absolute_target(&self, target: Vec<String>) -> Vec<String> {
        let mut index = 0;
        let mut base = match target.first().map(String::as_str) {
            Some("crate") => {
                index = 1;
                Vec::new()
            }
            Some("self") => {
                index = 1;
                self.module_path.clone()
            }
            Some("super") => self.module_path.clone(),
            _ => return target,
        };
        while target.get(index).is_some_and(|segment| segment == "super") {
            base.pop();
            index += 1;
        }
        base.extend(target.into_iter().skip(index));
        base
    }

    fn record_binding(&mut self, name: String, target: Vec<String>) {
        let target = self.absolute_target(target);
        self.bindings
            .entry(name.clone())
            .and_modify(|existing| {
                if existing.as_ref() != Some(&target) {
                    *existing = None;
                }
            })
            .or_insert_with(|| Some(target.clone()));

        let mut qualified_name = self.module_path.clone();
        qualified_name.push(name);
        self.qualified_bindings
            .entry(qualified_name)
            .and_modify(|existing| {
                if existing.as_ref() != Some(&target) {
                    *existing = None;
                }
            })
            .or_insert_with(|| Some(target));
    }

    fn record_unknown_binding(&mut self, name: String) {
        self.bindings.insert(name.clone(), None);
        let mut qualified_name = self.module_path.clone();
        qualified_name.push(name);
        self.qualified_bindings.insert(qualified_name, None);
    }

    fn record_opaque_module(&mut self) {
        if !self.module_path.is_empty() {
            self.opaque_modules.insert(self.module_path.clone());
        }
    }

    fn path_is_opaque(&self, path: &[String]) -> bool {
        self.opaque_modules
            .iter()
            .any(|module| path.starts_with(module))
    }

    fn matching_binding(&self, path: &[String]) -> Option<(usize, &Option<Vec<String>>)> {
        (1..=path.len())
            .rev()
            .find_map(|length| {
                self.qualified_bindings
                    .get(&path[..length])
                    .map(|binding| (length, binding))
            })
            .or_else(|| {
                path.first()
                    .and_then(|first| self.bindings.get(first))
                    .map(|binding| (1, binding))
            })
    }

    fn resolve_path(&self, path: &[String]) -> Option<Vec<String>> {
        let mut resolved = path.to_vec();
        let mut seen = BTreeSet::new();
        while seen.insert(resolved.clone()) {
            let Some((binding_length, binding)) = self.matching_binding(&resolved) else {
                return Some(resolved);
            };
            let mut next = binding.clone()?;
            next.extend(resolved.iter().skip(binding_length).cloned());
            resolved = next;
        }
        None
    }

    fn is_proven_local_path(&self, path: &[String]) -> bool {
        if self.matching_binding(path).is_some() {
            return self.resolve_path(path).is_some_and(|target| {
                !self.path_is_opaque(&target)
                    && target.first().is_some_and(|root| {
                        self.local_names.contains(root)
                            || matches!(root.as_str(), "crate" | "self" | "super")
                    })
            });
        }
        let Some(first) = path.first() else {
            return false;
        };
        if self.path_is_opaque(path) {
            return false;
        }
        if let Some(binding) = self.bindings.get(first) {
            return binding
                .as_ref()
                .and_then(|target| target.first())
                .is_some_and(|root| {
                    self.local_names.contains(root)
                        || matches!(root.as_str(), "crate" | "self" | "super")
                });
        }
        self.local_names.contains(first) || matches!(first.as_str(), "crate" | "self" | "super")
    }
}

fn collect_use_bindings(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    provenance: &mut SourceProvenance,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_use_bindings(&path.tree, prefix, provenance);
            prefix.pop();
        }
        syn::UseTree::Name(name) if name.ident == "self" => {
            if let Some(binding) = prefix.last().cloned() {
                provenance.record_binding(binding, prefix.clone());
            }
        }
        syn::UseTree::Name(name) => {
            let mut target = prefix.clone();
            target.push(name.ident.to_string());
            provenance.record_binding(name.ident.to_string(), target);
        }
        syn::UseTree::Rename(rename) => {
            let mut target = prefix.clone();
            target.push(rename.ident.to_string());
            provenance.record_binding(rename.rename.to_string(), target);
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_bindings(item, prefix, provenance);
            }
        }
        syn::UseTree::Glob(_) => provenance.record_opaque_module(),
    }
}

impl<'ast> Visit<'ast> for SourceProvenance {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        collect_use_bindings(&item.tree, &mut Vec::new(), self);
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        let binding = item
            .rename
            .as_ref()
            .map_or_else(|| item.ident.to_string(), |(_, rename)| rename.to_string());
        self.record_binding(binding, vec![item.ident.to_string()]);
    }

    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.local_names.insert(item.ident.to_string());
        self.module_path.push(item.ident.to_string());
        if item.content.is_none() {
            self.record_opaque_module();
        }
        syn::visit::visit_item_mod(self, item);
        self.module_path.pop();
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.local_names.insert(item.ident.to_string());
        syn::visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.local_names.insert(item.ident.to_string());
        syn::visit::visit_item_enum(self, item);
    }

    fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
        self.local_names.insert(item.ident.to_string());
        syn::visit::visit_item_union(self, item);
    }

    fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
        if let syn::Type::Path(path) = item.ty.as_ref() {
            if path.qself.is_none() {
                self.record_binding(
                    item.ident.to_string(),
                    path.path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect(),
                );
            } else {
                self.record_unknown_binding(item.ident.to_string());
            }
        } else {
            self.record_unknown_binding(item.ident.to_string());
        }
        syn::visit::visit_item_type(self, item);
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        self.local_names.insert(item.ident.to_string());
        syn::visit::visit_item_trait(self, item);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalCall {
    line: usize,
    name: String,
    guarded: bool,
    macro_contained: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RawTerminalAudit {
    function: String,
    fingerprint: String,
    guard_dominates: bool,
}

#[derive(Debug)]
struct FunctionGuardScope {
    name: String,
    start_line: usize,
    end_line: usize,
    guard_dominates: bool,
}

fn is_creation_guard_expression(expression: &syn::Expr) -> bool {
    let expression = match expression {
        syn::Expr::Try(expression) => expression.expr.as_ref(),
        syn::Expr::Group(expression) => expression.expr.as_ref(),
        syn::Expr::Paren(expression) => expression.expr.as_ref(),
        expression => expression,
    };
    matches!(
        expression,
        syn::Expr::Call(call)
            if matches!(
                call.func.as_ref(),
                syn::Expr::Path(path)
                    if path.path.segments.last().is_some_and(|segment| segment.ident == "creation_guard")
            )
    )
}

fn block_holds_creation_guard(block: &syn::Block) -> bool {
    let Some(syn::Stmt::Local(local)) = block.stmts.first() else {
        return false;
    };
    let syn::Pat::Ident(binding) = &local.pat else {
        return false;
    };
    if binding.ident != "_guard"
        || !local
            .init
            .as_ref()
            .is_some_and(|init| is_creation_guard_expression(&init.expr))
    {
        return false;
    }

    #[derive(Default)]
    struct GuardUseCounter(usize);

    impl<'ast> Visit<'ast> for GuardUseCounter {
        fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
            if expression.qself.is_none() && expression.path.is_ident("_guard") {
                self.0 += 1;
            }
            syn::visit::visit_expr_path(self, expression);
        }
    }

    let mut uses = GuardUseCounter::default();
    uses.visit_block(block);
    if uses.0 != 0 {
        return false;
    }

    #[derive(Default)]
    struct SuspensionOrDeferralDetector(bool);

    impl<'ast> Visit<'ast> for SuspensionOrDeferralDetector {
        fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {
            self.0 = true;
        }

        fn visit_expr_await(&mut self, _: &'ast syn::ExprAwait) {
            self.0 = true;
        }

        fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {
            self.0 = true;
        }
    }

    let mut detector = SuspensionOrDeferralDetector::default();
    detector.visit_block(block);
    !detector.0
}

#[derive(Default)]
struct FunctionGuardCollector {
    scopes: Vec<FunctionGuardScope>,
}

impl FunctionGuardCollector {
    fn record(
        &mut self,
        name: String,
        block: &syn::Block,
        span: proc_macro2::Span,
        is_async: bool,
    ) {
        self.scopes.push(FunctionGuardScope {
            name,
            start_line: span.start().line,
            end_line: span.end().line,
            guard_dominates: !is_async && block_holds_creation_guard(block),
        });
    }
}

impl<'ast> Visit<'ast> for FunctionGuardCollector {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.record(
            item.sig.ident.to_string(),
            &item.block,
            item.span(),
            item.sig.asyncness.is_some(),
        );
        syn::visit::visit_item_fn(self, item);
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        self.record(
            item.sig.ident.to_string(),
            &item.block,
            item.span(),
            item.sig.asyncness.is_some(),
        );
        syn::visit::visit_impl_item_fn(self, item);
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if let Some(block) = &item.default {
            self.record(
                item.sig.ident.to_string(),
                block,
                item.span(),
                item.sig.asyncness.is_some(),
            );
        }
        syn::visit::visit_trait_item_fn(self, item);
    }
}

fn process_creation_raw_terminal_audit(source: &str) -> Result<Vec<RawTerminalAudit>, String> {
    let file =
        syn::parse_file(source).map_err(|error| format!("Rust source did not parse: {error}"))?;
    let mut functions = FunctionGuardCollector::default();
    functions.visit_file(&file);
    let lines: Vec<_> = source.lines().collect();
    let mut audits = Vec::new();

    for call in terminal_calls(source)?
        .into_iter()
        .filter(|call| !call.guarded)
    {
        let owner = functions
            .scopes
            .iter()
            .filter(|scope| scope.start_line <= call.line && call.line <= scope.end_line)
            .min_by_key(|scope| scope.end_line - scope.start_line);
        let fingerprint = lines
            .get(call.line.saturating_sub(1))
            .map_or("<missing source line>", |line| line.trim())
            .to_string();
        audits.push(RawTerminalAudit {
            function: owner.map_or("<no function>".to_string(), |scope| scope.name.clone()),
            fingerprint,
            guard_dominates: !call.macro_contained
                && owner.is_some_and(|scope| scope.guard_dominates),
        });
    }
    audits.sort();
    Ok(audits)
}

fn validate_process_creation_raw_inventory(
    source: &str,
    expected: &BTreeMap<String, usize>,
) -> Result<(), String> {
    let audits = process_creation_raw_terminal_audit(source)?;
    let mut observed = BTreeMap::<String, usize>::new();
    for audit in &audits {
        *observed
            .entry(format!("{}|{}", audit.function, audit.fingerprint))
            .or_default() += 1;
    }
    let mut errors = Vec::new();
    if &observed != expected {
        errors.push(format!(
            "raw terminal inventory drifted; observed {observed:#?}, expected {expected:#?}"
        ));
    }
    let unguarded: Vec<_> = audits
        .iter()
        .filter(|audit| !audit.guard_dominates)
        .collect();
    if !unguarded.is_empty() {
        errors.push(format!(
            "raw terminals are not dominated by a retained creation guard: {unguarded:#?}"
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn terminal_calls(source: &str) -> Result<Vec<TerminalCall>, String> {
    fn is_terminal(name: &str) -> bool {
        matches!(
            name,
            "spawn" | "output" | "status" | "spawn_guarded" | "output_guarded" | "status_guarded"
        )
    }

    fn is_method_or_ufcs(tokens: &[TokenTree], index: usize) -> bool {
        let punct = |token: Option<&TokenTree>, expected| matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == expected);
        if punct(index.checked_sub(1).and_then(|i| tokens.get(i)), '.') {
            return true;
        }
        is_ufcs(tokens, index)
    }

    fn is_ufcs(tokens: &[TokenTree], index: usize) -> bool {
        let punct = |token: Option<&TokenTree>, expected| matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == expected);
        punct(index.checked_sub(1).and_then(|i| tokens.get(i)), ':')
            && punct(index.checked_sub(2).and_then(|i| tokens.get(i)), ':')
    }

    fn simple_qualifier(tokens: &[TokenTree], index: usize) -> Option<Vec<String>> {
        let mut cursor = index.checked_sub(3)?;
        let TokenTree::Ident(ident) = tokens.get(cursor)? else {
            return None;
        };
        let mut reversed = vec![ident.to_string()];
        while cursor >= 3 {
            let is_colon = |token: &TokenTree| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':');
            if !is_colon(&tokens[cursor - 1]) || !is_colon(&tokens[cursor - 2]) {
                break;
            }
            let TokenTree::Ident(ident) = &tokens[cursor - 3] else {
                break;
            };
            reversed.push(ident.to_string());
            cursor -= 3;
        }
        reversed.reverse();
        Some(reversed)
    }

    fn is_proven_non_process_spawn(
        tokens: &[TokenTree],
        index: usize,
        name: &str,
        provenance: &SourceProvenance,
    ) -> bool {
        if name != "spawn" {
            return false;
        }
        let Some(path) = simple_qualifier(tokens, index) else {
            // Qualified-angle UFCS is deliberately never exempted.
            return false;
        };
        if provenance.is_proven_local_path(&path) {
            // A local associated `spawn` is not itself an OS terminal. If its implementation
            // creates a process, that raw/method terminal is inspected at the implementation.
            return true;
        }
        let Some(resolved) = provenance.resolve_path(&path) else {
            return false;
        };
        resolved.starts_with(&["std".into(), "thread".into()])
            || (resolved.first().is_some_and(|segment| segment == "tokio")
                && resolved.get(1).is_none_or(|segment| segment != "process"))
    }

    fn scan(
        stream: TokenStream,
        provenance: &SourceProvenance,
        calls: &mut Vec<TerminalCall>,
        inside_macro: bool,
    ) {
        let tokens: Vec<_> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                let macro_arguments = index
                    .checked_sub(1)
                    .and_then(|previous| tokens.get(previous));
                let group_is_macro = inside_macro
                    || matches!(macro_arguments, Some(TokenTree::Punct(punct)) if punct.as_char() == '!');
                scan(group.stream(), provenance, calls, group_is_macro);
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let name = ident.to_string();
            if !is_terminal(&name) || !is_method_or_ufcs(&tokens, index) {
                continue;
            }
            if is_proven_non_process_spawn(&tokens, index, &name, provenance) {
                continue;
            }
            let immediate_call = matches!(
                tokens.get(index + 1),
                Some(TokenTree::Group(arguments)) if arguments.delimiter() == Delimiter::Parenthesis
            );
            if !immediate_call && !is_ufcs(&tokens, index) {
                // Rust cannot take a bound method from `value.method`; without call syntax this is
                // a field/accessor such as `output.status`, not terminal authority.
                continue;
            }
            if !immediate_call && name.ends_with("_guarded") {
                // Taking a guarded helper as a function item preserves the helper's own lock.
                continue;
            }
            calls.push(TerminalCall {
                line: ident.span().start().line,
                guarded: name.ends_with("_guarded"),
                name,
                macro_contained: inside_macro,
            });
        }
    }

    let file =
        syn::parse_file(source).map_err(|error| format!("Rust source did not parse: {error}"))?;
    let mut provenance = SourceProvenance::default();
    provenance.visit_file(&file);
    let stream = TokenStream::from_str(source)
        .map_err(|error| format!("Rust source did not tokenize: {error}"))?;
    let mut calls = Vec::new();
    scan(stream, &provenance, &mut calls, false);
    calls.sort_by_key(|call| call.line);
    Ok(calls)
}

/// `(path, trimmed source fingerprint, occurrence count, trust class)`.
///
/// Counts make adding an identical launch expression visible without coupling
/// the inventory to source line numbers.
const UNIFORM_SITES: &[(&str, &str, usize, &str)] = &[
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/docs.rs",
        "let status = std::process::Command::new(\"less\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/docs.rs", ".status()?;", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/extras/acp/mod.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    (
        "src/extras/export.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/export.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        ".output()",
        12,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        ".output();",
        5,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "Command::new(\"git\")",
        2,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let branch_output = Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let output = Command::new(\"git\")",
        14,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "let output = Command::new(\"git\").args(args).output().ok()?;",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "let mut cmd = Command::new(program);",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/js/engine.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "if is_followable_redirect(response.status()) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/admission.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/embed.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/proposal.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/verify.rs",
        "handle.spawn(&program, &args).map_err(|reason| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".output()",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let output = std::process::Command::new(SW_VERS)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/loop/mod.rs",
        ".status()",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/extras/loop/mod.rs",
        "let status = std::process::Command::new(\"bash\")",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    ("src/extras/lsp/client.rs", ".spawn()", 1, "TC-LSP-SERVICE"),
    (
        "src/extras/lsp/client.rs",
        "child: tokio::process::Child,",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "let mut child = tokio::process::Command::new(cfg.command.as_str())",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/mod.rs",
        "let spawned = LspClient::spawn(name, cfg, &self.inner.root, self.inner.diags.clone(), {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/lsp/client.rs",
        "stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,",
        1,
        "TC-LSP-SERVICE",
    ),
    ("src/extras/mcp/client.rs", ".spawn()", 1, "TC-MCP-STDIO"),
    (
        "src/extras/mcp/client.rs",
        "mut stderr: tokio::process::ChildStderr,",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/client.rs",
        "use tokio::process::Command;",
        1,
        "TC-MCP-STDIO",
    ),
    ("src/sandbox.rs", ".status();", 2, "TC-LIFECYCLE-HELPER"),
    (
        "src/sandbox.rs",
        "let _ = std::process::Command::new(\"kill\")",
        2,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-MODEL-ACTION",
    ),
    ("src/sandbox/worker.rs", ".spawn()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker.rs",
        "let mut command = Command::new(executable);",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/worker/linux.rs", ".status()", 1, "TEST-ONLY"),
    ("src/sandbox/worker/linux.rs", ".spawn()?;", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/linux.rs",
        "if std::process::Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "if std::thread::Builder::new().spawn(|| {}).is_ok() {",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn()?;",
        4,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut command = Command::new(bwrap);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    ("src/session/mod.rs", ".output()", 1, "TC-INTERNAL-GIT"),
    (
        "src/session/mod.rs",
        "let out = std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    ("src/startup.rs", ".output()?;", 1, "TC-EXPLICIT-USER-SHELL"),
    (
        "src/startup.rs",
        "let output = std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
    (
        "src/ui/app.rs",
        "if std::process::Command::new(\"lazygit\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let _ = std::process::Command::new(\"lazygit\").status();",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
    ("src/ui/input/mod.rs", ".status();", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/ui/input/mod.rs",
        "let _ = std::process::Command::new(\"sh\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/ui/renderer.rs", ".spawn()", 2, "TC-SUPPORT-UTILITY"),
    (
        "src/ui/renderer.rs",
        "let Ok(mut child) = std::process::Command::new(cmd)",
        2,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/memory.rs",
        ".status()?;",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/memory.rs",
        "let status = std::process::Command::new(shell)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/slash/session.rs",
        "match std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    ("src/ui/slash/session.rs", ".output()", 1, "TC-INTERNAL-GIT"),
];

/// Sites whose identical terminal expression inherits different classes from
/// the surrounding constructor, in source order.
const MIXED_SITES: &[(&str, &str, &[&str])] = &[(
    "src/ui/app.rs",
    ".output()",
    &["TC-EXPLICIT-USER-SHELL", "TC-SUPPORT-UTILITY"],
)];

const ALLOWED_CURRENT_CLASSES: &[&str] = &[
    "NON-PROCESS",
    "TEST-ONLY",
    "TC-EXPLICIT-USER-SHELL",
    "TC-INTERNAL-GIT",
    "TC-INTERNAL-VERIFICATION",
    "TC-LIFECYCLE-HELPER",
    "TC-LOOP-VALIDATION",
    "TC-LSP-SERVICE",
    "TC-MCP-STDIO",
    "TC-MODEL-ACTION",
    "TC-BROKER-JS-WORKER",
    "TC-PROJECT-AUTOMATION",
    "TC-SUPPORT-UTILITY",
];

/// Exact ownership for every lexical disposition and every site in a source
/// file that contains more than one production trust class.
const EXACT_UNIFORM_SITE_CLASSES: &[(&str, &str, usize, &str)] = &[
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/acp/mod.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    (
        "src/extras/export.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/export.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/git_worktree/mod.rs",
        "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/engine.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "if is_followable_redirect(response.status()) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/admission.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/embed.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/proposal.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/verify.rs",
        "handle.spawn(&program, &args).map_err(|reason| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/lsp/mod.rs",
        "let spawned = LspClient::spawn(name, cfg, &self.inner.root, self.inner.diags.clone(), {",
        1,
        "NON-PROCESS",
    ),
    ("src/sandbox.rs", ".status();", 2, "TC-LIFECYCLE-HELPER"),
    (
        "src/sandbox.rs",
        "let _ = std::process::Command::new(\"kill\")",
        2,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-MODEL-ACTION",
    ),
    ("src/sandbox/worker.rs", ".spawn()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker.rs",
        "let mut command = Command::new(executable);",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/worker/linux.rs", ".status()", 1, "TEST-ONLY"),
    ("src/sandbox/worker/linux.rs", ".spawn()?;", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/linux.rs",
        "if std::process::Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "if std::thread::Builder::new().spawn(|| {}).is_ok() {",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = Command::new(WORKER_PATH)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut child = command.spawn()?;",
        4,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/linux.rs",
        "let mut command = Command::new(bwrap);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/ui/app.rs",
        "if std::process::Command::new(\"lazygit\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let _ = std::process::Command::new(\"lazygit\").status();",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "std::process::Command::new(\"bash\")",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
];

const EXACT_MIXED_SITE_CLASSES: &[(&str, &str, &[&str])] = &[(
    "src/ui/app.rs",
    ".output()",
    &["TC-EXPLICIT-USER-SHELL", "TC-SUPPORT-UTILITY"],
)];

/// Files whose non-disposition launch expressions all have one owner.
const SINGLE_CLASS_FAMILIES: &[(&str, &str)] = &[
    ("src/docs.rs", "TC-SUPPORT-UTILITY"),
    ("src/extras/git_worktree/mod.rs", "TC-INTERNAL-GIT"),
    ("src/extras/hooks/subprocess.rs", "TC-PROJECT-AUTOMATION"),
    ("src/extras/loop/mod.rs", "TC-INTERNAL-VERIFICATION"),
    ("src/extras/lsp/client.rs", "TC-LSP-SERVICE"),
    ("src/extras/mcp/client.rs", "TC-MCP-STDIO"),
    ("src/sandbox/worker/macos.rs", "TC-SUPPORT-UTILITY"),
    ("src/session/mod.rs", "TC-INTERNAL-GIT"),
    ("src/startup.rs", "TC-EXPLICIT-USER-SHELL"),
    ("src/ui/input/mod.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/renderer.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/slash/memory.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/slash/session.rs", "TC-INTERNAL-GIT"),
];

fn checked_inventory() -> BTreeMap<(String, String, usize), &'static str> {
    let mut expected = BTreeMap::new();
    for &(path, source, count, classification) in UNIFORM_SITES {
        for occurrence in 1..=count {
            assert!(
                expected
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate checked inventory entry for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    for &(path, source, classifications) in MIXED_SITES {
        for (index, &classification) in classifications.iter().enumerate() {
            let occurrence = index + 1;
            assert!(
                expected
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate checked inventory entry for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    expected
}

fn checked_exact_site_classes() -> BTreeMap<(String, String, usize), &'static str> {
    let mut exact = BTreeMap::new();
    for &(path, source, count, classification) in EXACT_UNIFORM_SITE_CLASSES {
        for occurrence in 1..=count {
            assert!(
                exact
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate exact ownership rule for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    for &(path, source, classifications) in EXACT_MIXED_SITE_CLASSES {
        for (index, &classification) in classifications.iter().enumerate() {
            let occurrence = index + 1;
            assert!(
                exact
                    .insert(
                        (path.to_string(), source.to_string(), occurrence),
                        classification,
                    )
                    .is_none(),
                "duplicate exact ownership rule for {path} occurrence {occurrence}: {source}"
            );
        }
    }
    exact
}

fn validate_class_assignments(
    inventory: &BTreeMap<(String, String, usize), &'static str>,
) -> Result<(), String> {
    let exact = checked_exact_site_classes();
    let mut families = BTreeMap::new();
    for &(path, classification) in SINGLE_CLASS_FAMILIES {
        if families.insert(path, classification).is_some() {
            return Err(format!("duplicate source-family ownership rule for {path}"));
        }
    }

    for ((path, source, occurrence), classification) in inventory {
        if !ALLOWED_CURRENT_CLASSES.contains(classification) {
            return Err(format!(
                "class {classification} is not allowed for current launch inventory"
            ));
        }
        let site = (path.clone(), source.clone(), *occurrence);
        let owner = exact
            .get(&site)
            .copied()
            .or_else(|| families.get(path.as_str()).copied())
            .ok_or_else(|| {
                format!("site has no exact or single-class ownership rule: {path} occurrence {occurrence}: {source}")
            })?;
        if owner != *classification {
            return Err(format!(
                "class {classification} cannot own {path} occurrence {occurrence}: {source}; expected {owner}"
            ));
        }
    }

    for ((path, source, occurrence), _) in &exact {
        if !inventory.contains_key(&(path.clone(), source.clone(), *occurrence)) {
            return Err(format!(
                "stale exact ownership rule for {path} occurrence {occurrence}: {source}"
            ));
        }
    }
    for path in families.keys() {
        if !inventory
            .keys()
            .any(|(inventory_path, source, occurrence)| {
                inventory_path == path
                    && !exact.contains_key(&(inventory_path.clone(), source.clone(), *occurrence))
            })
        {
            return Err(format!("stale single-class ownership rule for {path}"));
        }
    }
    Ok(())
}

fn validate_current_class_assignments() -> Result<(), String> {
    validate_class_assignments(&checked_inventory())
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).expect("source directory must be readable") {
            let path = entry.expect("source entry must be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

fn is_inventory_line(line: &str) -> bool {
    line.contains("Command::new")
        || line.contains("tokio::process")
        || line.contains(".spawn(")
        || line.contains(".spawn_guarded(")
        || line.contains(".output(")
        || line.contains(".output_guarded(")
        || line.contains(".status(")
        || line.contains(".status_guarded(")
}

fn normalized_inventory_line(line: &str) -> String {
    line.replace(".spawn_guarded(", ".spawn(")
        .replace(".output_guarded(", ".output(")
        .replace(".status_guarded(", ".status(")
}

fn creation_boundary_violations(
    relative: &str,
    contents: &str,
    expected: &BTreeMap<(String, String, usize), &'static str>,
) -> Result<Vec<String>, String> {
    let mut seen = BTreeMap::<(String, String), usize>::new();
    let mut classes_by_line = BTreeMap::<usize, Vec<&'static str>>::new();

    for (line_index, line) in contents.lines().enumerate() {
        let line = line.trim();
        let normalized = normalized_inventory_line(line);
        let explicitly_inventoried = expected
            .keys()
            .any(|(path, source, _)| path == relative && source == &normalized);
        if !is_inventory_line(line) && !explicitly_inventoried {
            continue;
        }
        let fingerprint = (relative.to_string(), normalized);
        let occurrence = seen.entry(fingerprint.clone()).or_default();
        *occurrence += 1;
        if let Some(&classification) = expected.get(&(fingerprint.0, fingerprint.1, *occurrence)) {
            classes_by_line
                .entry(line_index + 1)
                .or_default()
                .push(classification);
        }
    }

    let mut violations = Vec::new();
    for call in terminal_calls(contents)? {
        if call.guarded {
            continue;
        }
        match classes_by_line.get(&call.line) {
            Some(classes) if classes.iter().all(|class| !class.starts_with("TC-")) => {}
            Some(classes) => violations.push(format!(
                "{relative}:{} unguarded {} terminal classified as {}",
                call.line,
                call.name,
                classes.join("/")
            )),
            None => violations.push(format!(
                "{relative}:{} unrecognized unguarded {} terminal",
                call.line, call.name
            )),
        }
    }
    Ok(violations)
}

#[test]
fn production_subprocess_sites_have_a_trust_classification() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_root = root.join("src");
    let expected = checked_inventory();
    let mut seen = BTreeMap::<(String, String), usize>::new();
    let mut observed = BTreeSet::<(String, String, usize)>::new();

    for source in rust_sources(&source_root) {
        let relative = source
            .strip_prefix(root)
            .expect("source must be below manifest root")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("src/tests/") || relative.starts_with("src/extras/js/tests/") {
            continue;
        }
        if relative == "src/process_creation.rs" {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        for line in contents.lines().map(str::trim) {
            let normalized = normalized_inventory_line(line);
            let explicitly_inventoried = expected
                .keys()
                .any(|(path, source, _)| path == &relative && source == &normalized);
            if !is_inventory_line(line) && !explicitly_inventoried {
                continue;
            }
            let fingerprint = (relative.clone(), normalized);
            let occurrence = seen.entry(fingerprint.clone()).or_default();
            *occurrence += 1;
            observed.insert((fingerprint.0, fingerprint.1, *occurrence));
        }
    }

    let unclassified: Vec<_> = observed
        .iter()
        .filter(|site| !expected.contains_key(*site))
        .collect();
    let stale: Vec<_> = expected
        .keys()
        .filter(|site| !observed.contains(*site))
        .collect();
    assert!(
        unclassified.is_empty() && stale.is_empty(),
        "subprocess inventory drifted; classify every new production Command::new/tokio::process/.spawn/.output/.status result in docs/specs/subprocess-trust.md and update the checked inventory.\nUnclassified or changed: {unclassified:#?}\nStale: {stale:#?}"
    );

    let specification = std::fs::read_to_string(root.join("docs/specs/subprocess-trust.md"))
        .expect("the normative subprocess trust specification must exist");
    for classification in expected.values() {
        if classification.starts_with("TC-") {
            let table_prefix = format!("| `{classification}`");
            assert!(
                specification
                    .lines()
                    .any(|line| line.starts_with(&table_prefix)),
                "inventory classification {classification} has no normative contract-table row"
            );
        } else {
            assert!(
                specification.contains(&format!("`{classification}`")),
                "inventory disposition {classification} has no specification entry"
            );
        }
    }
}

#[test]
fn windows_capable_production_process_terminals_use_creation_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_root = root.join("src");
    let expected = checked_inventory();
    let mut unguarded = Vec::new();

    for source in rust_sources(&source_root) {
        let relative = source
            .strip_prefix(root)
            .expect("source must be below manifest root")
            .to_string_lossy()
            .replace('\\', "/");
        if relative.starts_with("src/tests/")
            || relative.starts_with("src/extras/js/tests/")
            || relative == "src/process_creation.rs"
            || matches!(
                relative.as_str(),
                "src/sandbox/worker/linux.rs" | "src/sandbox/worker/macos.rs"
            )
        {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        unguarded.extend(
            creation_boundary_violations(&relative, &contents, &expected)
                .unwrap_or_else(|error| panic!("could not inspect {relative}: {error}")),
        );
    }

    assert!(
        unguarded.is_empty(),
        "Windows-capable production process terminals bypass the crate creation boundary: {unguarded:#?}"
    );
}

#[test]
fn process_creation_raw_terminals_are_exact_and_guard_dominated() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/process_creation.rs"))
        .expect("process creation source must be readable");
    let expected = BTreeMap::from([
        (
            "output_guarded|std::process::Command::output(self)".to_string(),
            1,
        ),
        ("spawn_guarded|TokioCommand::spawn(self)".to_string(), 1),
        (
            "spawn_guarded|rmcp::transport::child_process::TokioChildProcessBuilder::spawn(self)"
                .to_string(),
            1,
        ),
        (
            "spawn_guarded|std::process::Command::spawn(self)".to_string(),
            1,
        ),
    ]);

    validate_process_creation_raw_inventory(&source, &expected)
        .expect("every raw terminal must be enumerated behind the retained crate guard");
}

#[test]
fn process_creation_raw_inventory_rejects_new_unguarded_helpers() {
    let fixture = r#"
fn bypass(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    std::process::Command::spawn(command)
}
"#;
    let expected = BTreeMap::from([(
        "bypass|std::process::Command::spawn(command)".to_string(),
        1,
    )]);
    let error = validate_process_creation_raw_inventory(fixture, &expected)
        .expect_err("an enumerated raw helper without the crate guard must fail");

    assert!(error.contains("not dominated by a retained creation guard"));
}

#[test]
fn process_creation_raw_inventory_rejects_a_guard_dropped_before_spawn() {
    let fixture = r#"
fn bypass(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    let _guard = creation_guard()?;
    drop(_guard);
    std::process::Command::spawn(command)
}
"#;
    let expected = BTreeMap::from([(
        "bypass|std::process::Command::spawn(command)".to_string(),
        1,
    )]);
    let error = validate_process_creation_raw_inventory(fixture, &expected)
        .expect_err("a raw helper must retain the crate guard through process creation");

    assert!(error.contains("not dominated by a retained creation guard"));
}

#[test]
fn process_creation_raw_inventory_rejects_guards_across_suspension_and_deferral() {
    let fixtures = [
        (
            r#"
async fn bypass(command: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    let _guard = creation_guard()?;
    yield_now().await;
    tokio::process::Command::spawn(command)
}
"#,
            "bypass|tokio::process::Command::spawn(command)",
        ),
        (
            r#"
fn bypass(command: &mut std::process::Command) {
    let _guard = creation_guard().unwrap();
    let deferred = || std::process::Command::spawn(command);
}
"#,
            "bypass|let deferred = || std::process::Command::spawn(command);",
        ),
    ];

    for (fixture, fingerprint) in fixtures {
        let expected = BTreeMap::from([(fingerprint.to_string(), 1)]);
        let error = validate_process_creation_raw_inventory(fixture, &expected).expect_err(
            "a creation guard must not authorize terminals across suspension or deferral",
        );
        assert!(
            error.contains("not dominated by a retained creation guard"),
            "unexpected validation error: {error}"
        );
    }
}

#[test]
fn process_creation_raw_inventory_rejects_macro_deferred_terminals() {
    let fixture = r#"
fn bypass(command: &mut std::process::Command) -> std::io::Result<()> {
    let _guard = creation_guard()?;
    defer!(std::process::Command::spawn(command));
    Ok(())
}
"#;
    let expected = BTreeMap::from([(
        "bypass|defer!(std::process::Command::spawn(command));".to_string(),
        1,
    )]);
    let error = validate_process_creation_raw_inventory(fixture, &expected)
        .expect_err("a raw terminal hidden in a macro may not inherit lexical guard dominance");

    assert!(error.contains("not dominated by a retained creation guard"));
}

#[test]
fn current_subprocess_inventory_accepts_exact_broker_and_rejects_cross_family_classes() {
    validate_current_class_assignments().expect("current subprocess classes must be allowed");
}

#[test]
fn subprocess_inventory_rejects_site_specific_relabels_in_mixed_files() {
    let cases = [
        (
            "src/agent/tools/bash.rs",
            ".status()",
            1,
            "TC-LIFECYCLE-HELPER",
            "an exact TEST-ONLY fingerprint",
        ),
        (
            "src/extras/git_worktree/mod.rs",
            "// freezes the TUI during worktree merges. Migrate to tokio::process::Command",
            1,
            "TC-INTERNAL-GIT",
            "an exact NON-PROCESS fingerprint in a production family",
        ),
        (
            "src/extras/git_worktree/mod.rs",
            ".output()",
            1,
            "NON-PROCESS",
            "a production launch in a family with a NON-PROCESS comment",
        ),
        (
            "src/sandbox.rs",
            "let mut cmd = Command::new(\"zerobox\");",
            1,
            "TC-LIFECYCLE-HELPER",
            "a model action in the mixed sandbox family",
        ),
        (
            "src/sandbox.rs",
            "let _ = std::process::Command::new(\"kill\")",
            1,
            "TC-MODEL-ACTION",
            "a lifecycle helper in the mixed sandbox family",
        ),
        (
            "src/sandbox/worker/linux.rs",
            "let Ok(mut child) = command.spawn() else {",
            1,
            "TC-MODEL-ACTION",
            "the broker-only Linux preflight launch",
        ),
        (
            "src/ui/app.rs",
            ".output()",
            1,
            "TC-SUPPORT-UTILITY",
            "the explicit shell output occurrence in the mixed UI family",
        ),
        (
            "src/ui/app.rs",
            "if std::process::Command::new(\"lazygit\")",
            1,
            "TC-EXPLICIT-USER-SHELL",
            "the lazygit utility in the mixed UI family",
        ),
    ];

    for (path, source, occurrence, replacement, description) in cases {
        let mut relabeled = checked_inventory();
        let key = (path.to_string(), source.to_string(), occurrence);
        assert!(
            relabeled.contains_key(&key),
            "missing test fixture for {description}"
        );
        relabeled.insert(key, replacement);
        assert!(
            validate_class_assignments(&relabeled).is_err(),
            "ownership validation accepted relabeling {description} as {replacement}"
        );
    }
}

#[test]
fn token_terminal_discovery_rejects_multiline_creation_lock_bypasses() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    let _ = command.spawn
        ();
    let _ = command.output
        ();
    let _ = command.status
        ();
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "output", "status"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_std_and_tokio_ufcs_bypasses() {
    let calls = terminal_calls(
        r#"
fn launch(
    std_command: &mut std::process::Command,
    tokio_command: &mut tokio::process::Command,
) {
    let _ = std::process::Command::spawn(&mut *std_command);
    let _ = std::process::Command::output(&mut *std_command);
    let _ = std::process::Command::status(&mut *std_command);
    let _ = tokio::process::Command::spawn(&mut *tokio_command);
    let _ = ProcessCommand::spawn(&mut *std_command);
    let _ = Cmd::output(&mut *std_command);
    let _ = <std::process::Command>::spawn(&mut *std_command);
    let _ = <tokio::process::Command>::output(&mut *tokio_command);
    let _ = <Cmd>::spawn(&mut *std_command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        [
            "spawn", "output", "status", "spawn", "spawn", "output", "spawn", "output", "spawn"
        ]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_does_not_trust_process_alias_spelling() {
    let calls = terminal_calls(
        r#"
use std::process::Command as tokio;
use tokio::process::Command as thread;

fn launch(std_command: &mut tokio, tokio_command: &mut thread) {
    let _ = tokio::spawn(&mut *std_command);
    let _ = thread::output(&mut *tokio_command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "output"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_resolves_process_type_aliases_and_module_reexports() {
    let calls = terminal_calls(
        r#"
type Cmd = std::process::Command;
type IndirectCmd = Cmd;

mod local {
    pub use std::process::Command;
}

mod indirect {
    pub use crate::local::Command;
}

mod nested {
    pub mod twice {
        pub use super::super::local::Command;
    }
}

mod globbed {
    pub use std::process::*;
}

fn launch(command: &mut std::process::Command) {
    let _ = Cmd::spawn(command);
    let _ = IndirectCmd::spawn(command);
    let _ = local::Command::spawn(command);
    let _ = indirect::Command::spawn(command);
    let _ = nested::twice::Command::spawn(command);
    let _ = globbed::Command::spawn(command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "spawn", "spawn", "spawn", "spawn", "spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_fails_closed_after_opaque_module_imports() {
    let calls = terminal_calls(
        r#"
mod globbed {
    pub use std::process::*;
}
mod external;

use globbed::Command;

fn launch(command: &mut std::process::Command) {
    let _ = Command::spawn(command);
    let _ = external::Command::spawn(command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_process_function_item_indirection() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    let terminal = std::process::Command::spawn;
    let _ = terminal(command);
    invoke!(std::process::Command::output, command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn", "output"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_distinguishes_guarded_calls_and_ignores_text() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    // command.spawn();
    let text = ".output()";
    let task = tokio::spawn(async {});
    let thread = std::thread::spawn(|| {});
    let _ = command.spawn_guarded
        ();
    let _ = StdCommandCreationExt::output_guarded(command);
    let _ = command.status_guarded();
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call.guarded));
}

#[test]
fn boundary_validation_fails_closed_for_unclassified_multiline_and_ufcs_terminals() {
    let fixture = r#"
fn launch(
    std_command: &mut std::process::Command,
    tokio_command: &mut tokio::process::Command,
) {
    let _ = std_command.spawn
        ();
    let _ = std::process::Command::output(&mut *std_command);
    let _ = tokio::process::Command::spawn(&mut *tokio_command);
    let _ = ProcessCommand::status(&mut *std_command);
}
"#;
    let violations = creation_boundary_violations("src/fixture.rs", fixture, &BTreeMap::new())
        .expect("fixture must be inspectable");

    assert_eq!(violations.len(), 4);
    assert!(
        violations
            .iter()
            .all(|violation| violation.contains("unrecognized unguarded")),
        "every terminal missing from the inventory must fail closed: {violations:#?}"
    );
}
