use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use sha2::{Digest, Sha256};
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
        let rewrite_limit = self
            .bindings
            .len()
            .saturating_add(self.qualified_bindings.len())
            .saturating_add(1);
        for _ in 0..rewrite_limit {
            if !seen.insert(resolved.clone()) {
                return None;
            }
            let Some((binding_length, binding)) = self.matching_binding(&resolved) else {
                return Some(resolved);
            };
            let mut next = binding.clone()?;
            if next.starts_with(&resolved[..binding_length]) {
                return None;
            }
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
    macro_context: Option<MacroContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MacroContext {
    digest: String,
    occurrence: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RawTerminalAudit {
    function: String,
    fingerprint: String,
    guard_dominates: bool,
    macro_context: Option<MacroContext>,
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
            guard_dominates: call.macro_context.is_none()
                && owner.is_some_and(|scope| scope.guard_dominates),
            macro_context: call.macro_context,
        });
    }
    audits.sort();
    Ok(audits)
}

fn validate_process_creation_raw_inventory(
    source: &str,
    expected: &BTreeMap<String, usize>,
) -> Result<(), String> {
    validate_process_creation_raw_inventory_with_non_process(source, expected, &BTreeSet::new())
}

fn validate_process_creation_raw_inventory_with_non_process(
    source: &str,
    expected: &BTreeMap<String, usize>,
    exact_non_process: &BTreeSet<(String, usize)>,
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
        .filter(|audit| {
            !audit.guard_dominates
                && !audit.macro_context.as_ref().is_some_and(|context| {
                    exact_non_process.contains(&(context.digest.clone(), context.occurrence))
                })
        })
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
    fn normalized_ident(ident: &proc_macro2::Ident) -> String {
        let spelling = ident.to_string();
        spelling.strip_prefix("r#").unwrap_or(&spelling).to_string()
    }

    fn is_terminal(name: &str) -> bool {
        matches!(
            name,
            "spawn" | "output" | "status" | "spawn_guarded" | "output_guarded" | "status_guarded"
        )
    }

    fn macro_digest(
        tokens: &[TokenTree],
        index: usize,
        group: &proc_macro2::Group,
        macro_rules_body: bool,
        parent_context: Option<&MacroContext>,
    ) -> String {
        fn invocation_path(tokens: &[TokenTree], bang_index: usize) -> String {
            let Some(mut cursor) = bang_index.checked_sub(1) else {
                return "<missing>".to_string();
            };
            let Some(TokenTree::Ident(ident)) = tokens.get(cursor) else {
                return "<non-ident>".to_string();
            };
            let mut reversed = vec![normalized_ident(ident)];
            while cursor >= 3 {
                let colon = |token: &TokenTree| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':');
                if !colon(&tokens[cursor - 1]) || !colon(&tokens[cursor - 2]) {
                    break;
                }
                let TokenTree::Ident(ident) = &tokens[cursor - 3] else {
                    break;
                };
                reversed.push(normalized_ident(ident));
                cursor -= 3;
            }
            reversed.reverse();
            reversed.join("::")
        }

        let delimiter = match group.delimiter() {
            Delimiter::Parenthesis => "paren",
            Delimiter::Brace => "brace",
            Delimiter::Bracket => "bracket",
            Delimiter::None => "none",
        };
        let prefix = if macro_rules_body {
            let name = index
                .checked_sub(1)
                .and_then(|i| tokens.get(i))
                .and_then(|token| match token {
                    TokenTree::Ident(ident) => Some(normalized_ident(ident)),
                    _ => None,
                })
                .unwrap_or_else(|| "<missing>".to_string());
            format!("macro_rules!{name}")
        } else {
            let bang_index = index.saturating_sub(1);
            format!("{}!", invocation_path(tokens, bang_index))
        };
        let canonical = format!("{prefix}{delimiter}({})", group.stream());
        let Some(parent_context) = parent_context else {
            return format!("{:x}", Sha256::digest(canonical.as_bytes()));
        };

        fn hash_frame(hasher: &mut Sha256, value: &[u8]) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }

        let mut hasher = Sha256::new();
        hasher.update(b"mini-agent:macro-context-chain:v1");
        hash_frame(&mut hasher, parent_context.digest.as_bytes());
        hash_frame(&mut hasher, canonical.as_bytes());
        format!("{:x}", hasher.finalize())
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
        let mut reversed = vec![normalized_ident(ident)];
        while cursor >= 3 {
            let is_colon = |token: &TokenTree| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':');
            if !is_colon(&tokens[cursor - 1]) || !is_colon(&tokens[cursor - 2]) {
                break;
            }
            let TokenTree::Ident(ident) = &tokens[cursor - 3] else {
                break;
            };
            reversed.push(normalized_ident(ident));
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
        macro_context: Option<MacroContext>,
        macro_occurrences: &mut BTreeMap<String, usize>,
    ) {
        fn is_macro_rules_body(tokens: &[TokenTree], index: usize) -> bool {
            matches!(index.checked_sub(3).and_then(|i| tokens.get(i)), Some(TokenTree::Ident(ident)) if normalized_ident(ident) == "macro_rules")
                && matches!(index.checked_sub(2).and_then(|i| tokens.get(i)), Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
                && matches!(
                    index.checked_sub(1).and_then(|i| tokens.get(i)),
                    Some(TokenTree::Ident(_))
                )
        }

        let tokens: Vec<_> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let TokenTree::Group(group) = token {
                let macro_arguments = index
                    .checked_sub(1)
                    .and_then(|previous| tokens.get(previous));
                let macro_rules_body = is_macro_rules_body(&tokens, index);
                let begins_macro = matches!(macro_arguments, Some(TokenTree::Punct(punct)) if punct.as_char() == '!')
                    || macro_rules_body;
                let group_context = if begins_macro {
                    let digest = macro_digest(
                        &tokens,
                        index,
                        group,
                        macro_rules_body,
                        macro_context.as_ref(),
                    );
                    let occurrence = macro_occurrences.entry(digest.clone()).or_default();
                    *occurrence += 1;
                    Some(MacroContext {
                        digest,
                        occurrence: *occurrence,
                    })
                } else {
                    macro_context.clone()
                };
                scan(
                    group.stream(),
                    provenance,
                    calls,
                    group_context,
                    macro_occurrences,
                );
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let name = normalized_ident(ident);
            let qualified = is_method_or_ufcs(&tokens, index);
            let macro_method_ident = macro_context.is_some() && !qualified;
            if !is_terminal(&name) || (!qualified && !macro_method_ident) {
                continue;
            }
            if qualified && is_proven_non_process_spawn(&tokens, index, &name, provenance) {
                continue;
            }
            let immediate_call = matches!(
                tokens.get(index + 1),
                Some(TokenTree::Group(arguments)) if arguments.delimiter() == Delimiter::Parenthesis
            );
            if !macro_method_ident && !immediate_call && !is_ufcs(&tokens, index) {
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
                macro_context: macro_context.clone(),
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
    scan(stream, &provenance, &mut calls, None, &mut BTreeMap::new());
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

/// Exact macro-token fingerprints whose terminal spelling is data rather than
/// a process method. Any unlisted `spawn`, `output`, or `status` identifier in
/// macro-controlled tokens remains process authority and fails closed.
const MACRO_IDENTIFIER_NON_PROCESS_SITES: &[(&str, &str, usize, &str)] = &[
    (
        "src/agent/runner.rs",
        "assert!(output.contains(\"cacheStatus\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/runner.rs",
        "assert!(output.contains(\"unknown stream item variant\"));",
        1,
        "NON-PROCESS",
    ),
    ("src/agent/runner.rs", "output,", 1, "NON-PROCESS"),
    ("src/agent/runner.rs", "output.len(),", 1, "NON-PROCESS"),
    (
        "src/agent/runner.rs",
        "println!(\"{}\", output);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "assert!(output.stderr.is_empty());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "assert!(output.stdout.is_empty());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "assert_eq!(output, \"stdout\\nstderr\\nExit code: 7\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "assert_eq!(output.stderr.len(), limits.stderr_bytes);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "assert_eq!(output.stdout.len(), limits.stdout_bytes);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "output.status,",
        3,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "output.stdout.len() + output.stderr.len(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/bash.rs",
        "tracing::warn!(\"tool bash stopped before completion: {:?}\", output.status);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(!output.contains(\"0 more\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(!output.contains(\"[truncated\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(!output.contains(\"swapped_marker.txt\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.contains(\"authorized_marker.txt\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.contains(\"truncated after 100 entries\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.contains(\"unknown number of additional entries\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.contains(marker));",
        2,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.starts_with(\"100 files found:\\n\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert_eq!(output, \"No files found matching the pattern.\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(!output.contains(\"0 more matches\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(!output.contains(\"[truncated after\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(!output.contains(\"swapped_binding_marker\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(output.contains(\"authorized_binding_marker\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(output.contains(\"unknown number of additional matches\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(output.contains(marker));",
        2,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(output.starts_with(\"2 results (searched 1 files):\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert_eq!(output, \"No matches found.\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "TextContent::new(output.to_string()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/export.rs",
        "anyhow::bail!(\"GitHub API returned {}: {}\", status, text.trim());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/audit.rs",
        "let _ = write!(output, \"{byte:02x}\");",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/js/host.rs", "output.status,", 2, "NON-PROCESS"),
    ("src/extras/js/host.rs", "status,", 1, "NON-PROCESS"),
    ("src/extras/js/host.rs", "status: 200,", 7, "NON-PROCESS"),
    ("src/extras/js/host.rs", "status: 204,", 1, "NON-PROCESS"),
    ("src/extras/js/host.rs", "status: 304,", 1, "NON-PROCESS"),
    (
        "src/extras/js/skills/capability.rs",
        "\"status\": status,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/store.rs",
        "Some((ref status, ref report_id, ref reason))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/store.rs",
        "if status == \"rejected\"",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/js/skills/store.rs", "status,", 3, "NON-PROCESS"),
    (
        "src/extras/js/skills/telemetry.rs",
        "if !matches!(status, LifecycleStatus::Canary | LifecycleStatus::Active) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  capability: {}\", skill.capability.tier);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  rank: {}\", skill.rank);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  route: {:?}\", route.route_kind);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  route_fingerprint: {}\", route.route_fingerprint);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  route_policy: {}\", route.policy_version);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"  score: {:.6}\", skill.score());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"- id: {}\", skill.id);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"<available_js_skills>\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"BEGIN_{resource_delimiter} path={path}\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"END_{delimiter}\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/turn.rs",
        "let _ = writeln!(output, \"END_{resource_delimiter}\");",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/js/skills/turn.rs", "output,", 5, "NON-PROCESS"),
    (
        "src/extras/js/supervisor.rs",
        "output = future => Ok(output),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/loop/mod.rs",
        "status.success(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Err(\"boom\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Err(\"second failed\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"completed first\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"first\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"late success\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"must be cancelled\".into()),",
        3,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"must not start\".into()),",
        5,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"result one\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"result two\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"result zero\".into()),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/subagents/task_tool.rs",
        "output: Ok(\"x\".repeat(2_000)),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/print.rs",
        "writeln!(output).expect(\"writing configuration output to a String cannot fail\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/print.rs",
        "writeln!(output, \"  {k:<width$}  {v}\")",
        1,
        "NON-PROCESS",
    ),
    (
        "src/print.rs",
        "writeln!(output, \"{}:\", title).expect(\"writing configuration output to a String cannot fail\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "!output.exit_status.is_some_and(|status| status.success()),",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "String::from_utf8_lossy(&output.stderr)",
        4,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "assert_eq!(output.status, CommandStatus::Failed);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "assert_eq!(output.stdout, b\"LINUX_SANDBOX_POLICY_PASS\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "assert_eq!(output.stdout, b\"MACOS_SEATBELT_POLICY_PASS\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "output.exit_status.is_some_and(|status| status.success()),",
        2,
        "NON-PROCESS",
    ),
    ("src/sandbox.rs", "output.status,", 2, "NON-PROCESS"),
    ("src/sandbox.rs", "output.status", 1, "NON-PROCESS"),
    (
        "src/sandbox.rs",
        "output.stdout.is_empty(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "status = child.wait() => CommandTermination::Exited(status),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        "crate::extras::truncate::truncate_cjk(output, 500, \"…\")",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/renderer.rs",
        "if matches!(child.wait(), Ok(status) if status.success()) {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/renderer.rs",
        "if wrote && matches!(child.wait(), Ok(status) if status.success()) {",
        1,
        "NON-PROCESS",
    ),
    ("src/ui/slash/features.rs", "status,", 1, "NON-PROCESS"),
];

/// SHA-256 of the canonical macro path, delimiter, and complete token body.
/// Occurrence counts prevent an identical invocation from borrowing an earlier
/// approval in the same source file.
const MACRO_NON_PROCESS_CONTEXTS: &[(&str, &[(&str, usize)])] = &[
    (
        "src/agent/runner.rs",
        &[
            (
                "6ff3c28d14d5d84917571e5f05c45de5d7d220dc38a220a6ca1d5eb0a671c3c6",
                1,
            ),
            (
                "7227db9bd08d4ea9bf40f6912fc1ed1fba5335091e6094ab523147919980e3a0",
                1,
            ),
            (
                "dc83f63ab70ec0837d5f9481e0048bb1f3bd3e305541b75c396811449c20dec2",
                1,
            ),
            (
                "e0e7704453a0e0b64d6def7a3eadf474c4e3d31d4fdaf826469fb63b863de145",
                1,
            ),
            (
                "e527deefc2f7d807e5fc10cdcfe09d67add441967e55ba56b0fc482fad9fc029",
                1,
            ),
        ],
    ),
    (
        "src/agent/tools/bash.rs",
        &[
            (
                "0ccaa30485938b3de9aaf3f1d5a975abc6865706f053276f99e96167e6caf3b4",
                1,
            ),
            (
                "1e5465670ad8339ad9f89476c1fbf99ba7c3fb5d7fadfcd6a39bc27a42871125",
                1,
            ),
            (
                "400a4b791d5f7cfad3ec39bc23ff111e0ace3959151b56782c2e7c0a1c948dfc",
                1,
            ),
            (
                "529cac7f49cba5304b809d39239f7e8af1d802c3084999ca7dfa7b1b4f04afbf",
                1,
            ),
            (
                "609285a59b245c86a704e74642a4c97409c0ee1fab1f24f8a07254b85da2574e",
                1,
            ),
            (
                "8dcaaf6d07b83ad709a7199a7201132a0c360d593ff9a558479df3024c159ddb",
                1,
            ),
            (
                "8e52692f3756645c6c3e867310a60af884333a0f3afcd31015076ffe53568c40",
                1,
            ),
            (
                "9977ccef98daa5097ca1b00f78e82acf13d24f3e5cbf2c5d7d70178425bc2a5a",
                1,
            ),
            (
                "9de9c88424c7265d3a8f49680c0cd1e71221cba50e8424e83b0443c602913c26",
                1,
            ),
            (
                "d2fa760055d244ce2b5d20ac50011cf220f8422486ebdc30652bf0f26eaa3267",
                1,
            ),
        ],
    ),
    (
        "src/agent/tools/find_files.rs",
        &[
            (
                "16f89557f90f902c7ec7f039802baffa7ba301966a60d1f22556f76f1168e753",
                1,
            ),
            (
                "1aaf1f7e6b05c745716ae647a5f4aa4d353150d40dbad6b0b6f5cc780c7965aa",
                1,
            ),
            (
                "58f47cd2363b3368e446ba9792e51612647a0fef38b93bb3c5c7afaca7470ad9",
                1,
            ),
            (
                "679b54c98870903405ecb367bd3aff2cc97d611b12b94602a90d997fc7687164",
                1,
            ),
            (
                "85ad7d54eff77124f8e86fc91c764e6e23135fc4fd85b8bf4a839ab87fc10c5d",
                1,
            ),
            (
                "9fe0cd9972b1eaad791d9993e0cf88d4aab964d69e2d9dd245f77a7406cdb18a",
                2,
            ),
            (
                "b2b779d390dbfb18cee3d21a262087067dd4e350dd933368d47d6a8edc8e8fa9",
                1,
            ),
            (
                "f65a92530c141e6eee59299ded45cf929b15f52b09398fb35272fb40757371e6",
                1,
            ),
            (
                "f74b6824f1903234970cbeb3726daa8c858d4b29cd0c043c268b977972cbad7f",
                1,
            ),
        ],
    ),
    (
        "src/agent/tools/grep.rs",
        &[
            (
                "1e4161769f79aa679b9473aa8f980f4267e170e5edda47b3fa8b23ff0c7a80c7",
                1,
            ),
            (
                "38004f86e59697c1b5e725ed1e033615399cc3470593d9cb01e4f4a454bacf2a",
                1,
            ),
            (
                "476d3ade66214347b93f4d1b283d838e1c8f9aebf9106c98b74b160537def4f3",
                1,
            ),
            (
                "6e32bfd3be6e7c7cc64d642f88fe48402340254cbdc361099be169f1262a75f7",
                1,
            ),
            (
                "713d8cc85fa695b3b29459def4ab6cecc3b3cf780097c41ca33c04810343a665",
                1,
            ),
            (
                "9fe0cd9972b1eaad791d9993e0cf88d4aab964d69e2d9dd245f77a7406cdb18a",
                2,
            ),
            (
                "a300a469769f43c771776366f63090873725863cc86afcf64bb1207b72a97f3a",
                1,
            ),
            (
                "a82c556d9be4e7284c52a3eff6aee4125ad25265eca7d767de2d9272d3ee9d0a",
                1,
            ),
        ],
    ),
    (
        "src/extras/acp/mod.rs",
        &[(
            "b139bdf25cd51e4504776171ad7b9574a8dc5537f599246d967c23a049d80961",
            1,
        )],
    ),
    (
        "src/extras/export.rs",
        &[(
            "f467ee1a288daf5012c070fe01d5c1b2eb4184803a20665bda2fd2d7dace28bd",
            1,
        )],
    ),
    (
        "src/extras/js/audit.rs",
        &[(
            "17ab185b28a4e2ccf0963a9814ad8c597591efac4aeda33e3a426ab74057a93b",
            1,
        )],
    ),
    (
        "src/extras/js/host.rs",
        &[
            (
                "0c11ba5f5dfb13c024a5c6503549b552284161bea32e4a2a6fd90cba84fabfad",
                1,
            ),
            (
                "125bc6c48f884518e485a05df14aba2329f380333574782c5ef254e3a73ed76d",
                1,
            ),
            (
                "1d6b4727fd361a687a362c496c373c471e9558f497ac1eccf161a99dc2636ff2",
                1,
            ),
            (
                "2651f1f9dcbcbea50132c9f397a71fe8d4e0e53ad1e57aa902a7a37e677a6219",
                1,
            ),
            (
                "76d813b7df3c301d824c6807c711a94268cd55ce6c9c2c1ce2ab25301cefb605",
                2,
            ),
            (
                "7d219cfac89a4cfd69fa84419469c6936bfcba376fe5ae6f73cf392db3d4edad",
                1,
            ),
            (
                "8f9fe1bb1a1cf6cf053d07aee5f1c5fd1f03777b040191f06387d2d6614dfea6",
                1,
            ),
            (
                "d114a8dff406decaf329f3ace3cc01fb159a6eaf7a362a8de67bc6b4c07fb168",
                1,
            ),
            (
                "e019001c2105b07e7ade5795b3bcd57f11658295f9d16937545bf7c029f693bb",
                1,
            ),
            (
                "fd7aad175a48f9ec579a9afafcc82c7ee11c3490921f21da7b40eb33b6ae0c47",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/skills/capability.rs",
        &[(
            "f47fa5d2f6a6c605c4428c1104ee2bb7b3cadd863311fe89e0d5f6cfc6c973cc",
            1,
        )],
    ),
    (
        "src/extras/js/skills/store.rs",
        &[
            (
                "d0d098d44cc9d2bf1ed33699ab1bde4ce238b4c9687ddbf2c45622c2b83e9ac0",
                1,
            ),
            (
                "f3839eff42a062f930fc3bce8a6516ad36feb746c264515b964427c0d957d3a0",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        &[(
            "8b85d9a26a71cdcae5855b9be3fa0a80266f1f90aedea1bc774f3c00ae8f8c7b",
            1,
        )],
    ),
    (
        "src/extras/js/skills/turn.rs",
        &[
            (
                "03543ce780408ea6f211d13654c6300810d083afa3d480ebd62f424a80d464c7",
                1,
            ),
            (
                "1945aecd88d664569d0863c639b256c1854f141c52a9b6fdc41f65d0b5e61412",
                1,
            ),
            (
                "2f872b4fd8428e1daf3a22589f141d9523f1e6aa119e041fdf175a69217dde2a",
                1,
            ),
            (
                "3f17dd102213634b0e6842ee149191c1c4b7924eb4d4d711bf68df8604bc4be3",
                1,
            ),
            (
                "4e56fbfcc3a2035534694b3f6f351720780d5890f183bd7aeb5546af34502e5b",
                1,
            ),
            (
                "667ba34a1dd74c035eb254b6855d2d668f9c8aa23efa205b3bc4e5809a668357",
                1,
            ),
            (
                "7278b77d2a9482b0e978d3476f71f2741b29b753196b1a224e25fcd01857fc3f",
                1,
            ),
            (
                "737cb5d5157acda87d5ce8087f61006d494d111e9b1bfc1a1933f6af5cb9f513",
                1,
            ),
            (
                "931ebf613b64748252207cd341f00a3edb634ced3b6119970a5876654c2fcfbb",
                1,
            ),
            (
                "a385de4d3efea15a8f9ed4903fb5febbff19cd9a1abd6d5beb2979bef42202dc",
                1,
            ),
            (
                "bb5c0d91a5103b20c9d888f7f743355b0c519e49edf689f0780094cc2a4616b9",
                1,
            ),
            (
                "c6def2be8d35ef4e7478b1726ee91a79638feeb55e8f25a38ced339a532fdee1",
                1,
            ),
            (
                "d24eda52d7decd0a2b0e49ee3f43928afa5465cf29da727d8d7cb4b1523c165d",
                1,
            ),
            (
                "eb2976f0342d80e42ab65fb8f4793518e49117141946a1501382133f432a2b42",
                1,
            ),
            (
                "fa053209a909a78ab8af355cec3c34bff7465e6fa19d1b1e9670a40f25b37cc7",
                1,
            ),
            (
                "fee53c345ad62cb8ad60ade580efa4406835f890e332b7bd9f0149a591384b07",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/supervisor.rs",
        &[(
            "290164c18c5020ca450782d25b7327721079ee85a0b16a94d9139acb53ece31a",
            1,
        )],
    ),
    (
        "src/extras/js/tool.rs",
        &[(
            "001aab5f18bd14a8a8f6f8bd63ec0fcfa7795714d2516531f11dc0bde14db5c8",
            1,
        )],
    ),
    (
        "src/extras/loop/mod.rs",
        &[(
            "53e1cc6b4c8058fec64aa055ba35ee966079cc044145dbae5a2e0f68b3f1f2e8",
            1,
        )],
    ),
    (
        "src/extras/subagents/task_tool.rs",
        &[
            (
                "3ec1e10cbc5a53f21a5c370a83139973270d27190164be3caf9775f5896384f6",
                1,
            ),
            (
                "69e8776d5e549d11d0e5cd4a5813fb99dc265b9523e6a46a2c171cf1bcf19c69",
                1,
            ),
            (
                "7756992ddc099546cce7cd35330b8f77343483eeb7307f2dd396a6f91b912772",
                1,
            ),
            (
                "994cad6940046f9ffc567e8eeee577b7deb4961a807b4c67ea69304f9321a238",
                1,
            ),
            (
                "d4cb7f5c0d79bcc97cd340555b7e18597c8f5b72149eee4a11fa55c3c5cd83b5",
                1,
            ),
        ],
    ),
    (
        "src/print.rs",
        &[
            (
                "7e29ef9d82008876ce58c29268a4936e42d935910f92243b01b4230f14aa405e",
                1,
            ),
            (
                "88f2e641e3b76065c266fa5c99b46ee938c6a42eb267ad0feab765b970f4cfc3",
                1,
            ),
            (
                "c850050f68dbbfef5c69f81fa7b9f94d25c8961431a5830dcf9bdcfd0dba91a0",
                1,
            ),
        ],
    ),
    (
        "src/sandbox.rs",
        &[
            (
                "1d3d757ac6197721fb5b3e8b4c709a5247c6b6a8c7f5f0b1374105a97da70815",
                1,
            ),
            (
                "200af6a39037d86c9fca42b3540109fc6a560dc582c98925666018af6469f170",
                2,
            ),
            (
                "21041daffca44529b2847fcf39b9deddf5343de5ab82852928c162ba0027841d",
                1,
            ),
            (
                "28137b226deab753bfc4a441481a9e791ddc23a273ef92cf7c995009d69c3b45",
                1,
            ),
            (
                "2d819cb2c8461982eac1eea5e6f3c077d018c2c7062d5771e444394315c9af8d",
                1,
            ),
            (
                "41bc3246e2124977e3a18ec39cfdeaa9db56c06f569511060981708f49fd3f28",
                1,
            ),
            (
                "48a3910a9bb9f8ed54f2aca265e5df830a258a500b4d005bc665acf27b353dc5",
                1,
            ),
            (
                "7feddac47daa2cb91ecd4b0b9fe5652ccf44272c76270f3a7dbf974416db085b",
                1,
            ),
            (
                "d9a63377271c68b6512b18b5d6022ab89f619007bd0f13726f4f0a4fe876c060",
                1,
            ),
            (
                "fc571cf25cd5606f55ffe751da75c17960e5d3a6daf4ad8b4ce011a4bf0137f6",
                1,
            ),
            (
                "ff8a44e780531e17a32cd5d7d766ddeb3dad33d64444ec081ce37856bd4a22ca",
                1,
            ),
        ],
    ),
    (
        "src/ui/app.rs",
        &[(
            "f21429181c665d0c014b342a4aee17c46d4e582e37f076f719fa350f6e12eff8",
            1,
        )],
    ),
    (
        "src/ui/renderer.rs",
        &[(
            "8483db8b18509f9e207b030ec52fb8caea8071f567fe1214e7e5bcc052a7fb2d",
            2,
        )],
    ),
    (
        "src/ui/slash/features.rs",
        &[(
            "ee7f122ababd728284c8c0825360968589289954958bf3e0a321d3e675c1c99e",
            1,
        )],
    ),
];

fn checked_macro_non_process_contexts() -> BTreeSet<(String, String, usize)> {
    let mut contexts = BTreeSet::new();
    for &(path, digests) in MACRO_NON_PROCESS_CONTEXTS {
        for &(digest, count) in digests {
            for occurrence in 1..=count {
                assert!(
                    contexts.insert((path.to_string(), digest.to_string(), occurrence)),
                    "duplicate macro non-process context for {path} occurrence {occurrence}: {digest}"
                );
            }
        }
    }
    contexts
}

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
    for sites in [UNIFORM_SITES, MACRO_IDENTIFIER_NON_PROCESS_SITES] {
        for &(path, source, count, classification) in sites {
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
    for sites in [
        EXACT_UNIFORM_SITE_CLASSES,
        MACRO_IDENTIFIER_NON_PROCESS_SITES,
    ] {
        for &(path, source, count, classification) in sites {
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
    macro_non_process: &BTreeSet<(String, String, usize)>,
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
            Some(classes) if classes.iter().all(|class| !class.starts_with("TC-")) => {
                if let Some(context) = &call.macro_context {
                    let identity = (
                        relative.to_string(),
                        context.digest.clone(),
                        context.occurrence,
                    );
                    if !macro_non_process.contains(&identity) {
                        violations.push(format!(
                            "{relative}:{} unrecognized macro context {}#{} for {} terminal",
                            call.line, context.digest, context.occurrence, call.name
                        ));
                    }
                }
            }
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
    let macro_non_process = checked_macro_non_process_contexts();
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
            creation_boundary_violations(&relative, &contents, &expected, &macro_non_process)
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
        (
            "guarded_output_preserves_explicit_stdio_across_builder_reuse|assert!(output.status.success());"
                .to_string(),
            1,
        ),
        (
            "guarded_output_preserves_explicit_stdio_across_builder_reuse|output.stderr.is_empty(),"
                .to_string(),
            1,
        ),
        (
            "guarded_output_preserves_explicit_stdio_across_builder_reuse|output.stdout.is_empty(),"
                .to_string(),
            1,
        ),
    ]);
    let exact_non_process = BTreeSet::from([
        (
            "1d6ccddea3e4bee7c11ea8d4d12dafd2dc82cb6015f16af0cacbcfbb4fbb2b2a".to_string(),
            1,
        ),
        (
            "6ba0397559c8869254538160d7b7b62ea1e5a3ff91aded328d0a58a6cae12040".to_string(),
            1,
        ),
        (
            "e45715c6647ca997a244579dd7c1ab396c0d5f3b9c155266cfc18f35411e30fc".to_string(),
            1,
        ),
    ]);

    validate_process_creation_raw_inventory_with_non_process(
        &source,
        &expected,
        &exact_non_process,
    )
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
fn process_creation_raw_inventory_rejects_locally_defined_deferred_macros() {
    let fixture = r#"
fn bypass(command: &mut std::process::Command) -> std::io::Result<()> {
    let _guard = creation_guard()?;
    macro_rules! deferred { ($command:expr) => { || std::process::Command::spawn($command) }; }
    let _deferred = deferred!(command);
    Ok(())
}
"#;
    let expected = BTreeMap::from([(
        "bypass|macro_rules! deferred { ($command:expr) => { || std::process::Command::spawn($command) }; }".to_string(),
        1,
    )]);
    let error = validate_process_creation_raw_inventory(fixture, &expected)
        .expect_err("a raw terminal generated by a local macro may not inherit guard dominance");

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
fn path_provenance_fails_closed_for_self_prefix_namespace_collisions() {
    let file = syn::parse_file(
        r#"
use smallvec::{SmallVec, smallvec};

fn render(values: &SmallVec<[String; 4]>) {}
"#,
    )
    .expect("fixture must parse");
    let mut provenance = SourceProvenance::default();
    provenance.visit_file(&file);

    assert_eq!(
        provenance.resolve_path(&["SmallVec".to_string()]),
        None,
        "a macro/type namespace collision must not grow an alias path without bound"
    );
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
fn token_terminal_discovery_rejects_macro_method_ident_indirection() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    terminal!(command, spawn);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_inferred_macro_method_ident_indirection() {
    let calls = terminal_calls(
        r#"
use std::process::Command;

fn launch() {
    let mut command = Command::new("program");
    terminal!(command, spawn);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_aliased_macro_method_ident_indirection() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    let alias = command;
    terminal!(alias, spawn);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_rejects_nested_macro_method_ident_indirection() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    terminal!((command), spawn);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn"]
    );
    assert!(calls.iter().all(|call| !call.guarded));
}

#[test]
fn token_terminal_discovery_normalizes_raw_terminal_identifiers() {
    let calls = terminal_calls(
        r#"
fn launch(command: &mut std::process::Command) {
    let _ = std::process::Command::r#spawn(command);
}
"#,
    )
    .expect("fixture must tokenize");

    assert_eq!(
        calls
            .iter()
            .map(|call| call.name.as_str())
            .collect::<Vec<_>>(),
        ["spawn"]
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
    let violations = creation_boundary_violations(
        "src/fixture.rs",
        fixture,
        &BTreeMap::new(),
        &BTreeSet::new(),
    )
    .expect("fixture must be inspectable");

    assert_eq!(violations.len(), 4);
    assert!(
        violations
            .iter()
            .all(|violation| violation.contains("unrecognized unguarded")),
        "every terminal missing from the inventory must fail closed: {violations:#?}"
    );
}

#[test]
fn macro_non_process_identity_binds_name_receiver_and_shape() {
    let expected = BTreeMap::from([(
        ("src/fixture.rs".to_string(), "status,".to_string(), 1),
        "NON-PROCESS",
    )]);
    let approved = r#"
fn inspect(value: Value) {
    approved!(
        value,
        status,
    );
}
"#;
    let context = terminal_calls(approved)
        .expect("approved fixture must tokenize")
        .into_iter()
        .find_map(|call| call.macro_context)
        .expect("approved terminal must have a macro context");
    let macro_non_process = BTreeSet::from([(
        "src/fixture.rs".to_string(),
        context.digest,
        context.occurrence,
    )]);
    assert!(
        creation_boundary_violations("src/fixture.rs", approved, &expected, &macro_non_process,)
            .expect("approved fixture must be inspectable")
            .is_empty()
    );

    let reviewer_probe = approved
        .replace("approved!", "terminal!")
        .replace("        value,", "        command,");
    let probe_violations = creation_boundary_violations(
        "src/fixture.rs",
        &reviewer_probe,
        &expected,
        &macro_non_process,
    )
    .expect("reviewer probe must be inspectable");
    assert_eq!(probe_violations.len(), 1);
    assert!(probe_violations[0].contains("unrecognized macro context"));

    let mutations = [
        approved.replace("approved!", "terminal!"),
        approved.replace("        value,", "        command,"),
        approved.replace("        value,", "        (value),"),
    ];
    for mutation in mutations {
        let violations = creation_boundary_violations(
            "src/fixture.rs",
            &mutation,
            &expected,
            &macro_non_process,
        )
        .expect("mutated fixture must be inspectable");
        assert_eq!(
            violations.len(),
            1,
            "changing macro name, receiver, or shape must invalidate the exact non-process identity"
        );
    }

    let duplicate = format!("{approved}\n{approved}");
    let duplicate_expected = BTreeMap::from([
        (
            ("src/fixture.rs".to_string(), "status,".to_string(), 1),
            "NON-PROCESS",
        ),
        (
            ("src/fixture.rs".to_string(), "status,".to_string(), 2),
            "NON-PROCESS",
        ),
    ]);
    let duplicate_violations = creation_boundary_violations(
        "src/fixture.rs",
        &duplicate,
        &duplicate_expected,
        &macro_non_process,
    )
    .expect("duplicate fixture must be inspectable");
    assert_eq!(
        duplicate_violations.len(),
        1,
        "an identical second invocation must not borrow the first occurrence's identity"
    );
}

#[test]
fn macro_non_process_identity_binds_full_nesting_chain() {
    fn terminal_context(source: &str) -> MacroContext {
        terminal_calls(source)
            .expect("fixture must tokenize")
            .into_iter()
            .find_map(|call| call.macro_context)
            .expect("fixture terminal must have a macro context")
    }

    fn assert_rejected(source: &str, approved: &BTreeSet<(String, String, usize)>) {
        let expected = BTreeMap::from([(
            ("src/fixture.rs".to_string(), "status,".to_string(), 1),
            "NON-PROCESS",
        )]);
        let violations =
            creation_boundary_violations("src/fixture.rs", source, &expected, approved)
                .expect("reviewer fixture must be inspectable");
        assert_eq!(violations.len(), 1, "nested context must fail closed");
        assert!(violations[0].contains("unrecognized macro context"));
    }

    let standalone = r#"
fn inspect(value: Value) {
    approved!(
        value,
        status,
    );
}
"#;
    let standalone_context = terminal_context(standalone);
    let approved = BTreeSet::from([(
        "src/fixture.rs".to_string(),
        standalone_context.digest.clone(),
        standalone_context.occurrence,
    )]);

    let nested = r#"
fn inspect(value: Value) {
    outer!(
        approved!(
            value,
            status,
        )
    );
}
"#;
    let nested_context = terminal_context(nested);
    assert_ne!(standalone_context.digest, nested_context.digest);
    assert_rejected(nested, &approved);

    let multi_level = r#"
fn inspect(value: Value) {
    outer!(
        middle!(
            approved!(
                value,
                status,
            )
        )
    );
}
"#;
    let reordered = r#"
fn inspect(value: Value) {
    middle!(
        outer!(
            approved!(
                value,
                status,
            )
        )
    );
}
"#;
    let multi_level_context = terminal_context(multi_level);
    let reordered_context = terminal_context(reordered);
    assert_ne!(nested_context.digest, multi_level_context.digest);
    assert_ne!(multi_level_context.digest, reordered_context.digest);
    assert_rejected(multi_level, &approved);
    assert_rejected(reordered, &approved);

    let duplicate_inner = r#"
fn inspect(value: Value) {
    outer!(
        approved!(
            value,
            status,
        );
        approved!(
            value,
            status,
        )
    );
}
"#;
    let duplicate_contexts: Vec<_> = terminal_calls(duplicate_inner)
        .expect("duplicate fixture must tokenize")
        .into_iter()
        .filter_map(|call| call.macro_context)
        .collect();
    assert_eq!(duplicate_contexts.len(), 2);
    assert_eq!(duplicate_contexts[0].digest, duplicate_contexts[1].digest);
    assert_eq!(duplicate_contexts[0].occurrence, 1);
    assert_eq!(duplicate_contexts[1].occurrence, 2);

    let first_duplicate_approved = BTreeSet::from([(
        "src/fixture.rs".to_string(),
        duplicate_contexts[0].digest.clone(),
        duplicate_contexts[0].occurrence,
    )]);
    let duplicate_expected = BTreeMap::from([
        (
            ("src/fixture.rs".to_string(), "status,".to_string(), 1),
            "NON-PROCESS",
        ),
        (
            ("src/fixture.rs".to_string(), "status,".to_string(), 2),
            "NON-PROCESS",
        ),
    ]);
    let duplicate_violations = creation_boundary_violations(
        "src/fixture.rs",
        duplicate_inner,
        &duplicate_expected,
        &first_duplicate_approved,
    )
    .expect("duplicate fixture must be inspectable");
    assert_eq!(
        duplicate_violations.len(),
        1,
        "the second identical inner invocation must not borrow the first occurrence"
    );
}
