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
                    if path.path.segments.last().is_some_and(|segment| {
                        matches!(
                            segment.ident.to_string().as_str(),
                            "creation_guard" | "creation_guard_until"
                        )
                    })
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

/// Macros that only evaluate or format their arguments. None of them can build
/// a method call from an identifier argument (the way a local `macro_rules!`
/// can with `$object.$method()`), so a bare `spawn` / `output` / `status` in
/// their tokens is a variable, pattern or field name and not process authority.
/// Qualified calls (`.spawn(`, `Command::status(`) inside them stay flagged, and
/// a macro nested inside one of them is judged on its own name.
/// `splice_free_macro_names_are_not_redefined_by_the_crate` fails closed if the
/// crate ever defines a macro with one of these names.
const SPLICE_FREE_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "matches",
    "format",
    "format_args",
    "write",
    "writeln",
    "print",
    "println",
    "eprint",
    "eprintln",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "vec",
    "dbg",
    "select",
    "join",
    "try_join",
    "trace",
    "debug",
    "info",
    "warn",
    "error",
];

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
        fn push_frame(output: &mut Vec<u8>, tag: u8, payload: &[u8]) {
            output.push(tag);
            output.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            output.extend_from_slice(payload);
        }

        fn encode_token(token: &TokenTree) -> Vec<u8> {
            let mut encoded = Vec::new();
            match token {
                TokenTree::Group(group) => {
                    let mut payload = vec![match group.delimiter() {
                        Delimiter::Parenthesis => 1,
                        Delimiter::Brace => 2,
                        Delimiter::Bracket => 3,
                        Delimiter::None => 4,
                    }];
                    let nested: Vec<_> = group.stream().into_iter().collect();
                    payload.extend_from_slice(&(nested.len() as u64).to_be_bytes());
                    for token in &nested {
                        payload.extend_from_slice(&encode_token(token));
                    }
                    push_frame(&mut encoded, 1, &payload);
                }
                TokenTree::Ident(ident) => {
                    let exact = ident.to_string();
                    let normalized = normalized_ident(ident);
                    let mut payload = vec![u8::from(exact.starts_with("r#"))];
                    push_frame(&mut payload, 1, exact.as_bytes());
                    push_frame(&mut payload, 2, normalized.as_bytes());
                    push_frame(&mut encoded, 2, &payload);
                }
                TokenTree::Punct(punct) => {
                    let mut payload = Vec::new();
                    payload.extend_from_slice(&(punct.as_char() as u32).to_be_bytes());
                    payload.push(match punct.spacing() {
                        proc_macro2::Spacing::Alone => 1,
                        proc_macro2::Spacing::Joint => 2,
                    });
                    push_frame(&mut encoded, 3, &payload);
                }
                TokenTree::Literal(literal) => {
                    push_frame(&mut encoded, 4, literal.to_string().as_bytes());
                }
            }
            encoded
        }

        fn invocation_prefix_start(tokens: &[TokenTree], bang_index: usize) -> usize {
            let Some(mut cursor) = bang_index.checked_sub(1) else {
                return bang_index;
            };
            if !matches!(tokens.get(cursor), Some(TokenTree::Ident(_))) {
                return cursor;
            }
            let colon = |token: &TokenTree| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':');
            while cursor >= 3
                && colon(&tokens[cursor - 1])
                && colon(&tokens[cursor - 2])
                && matches!(tokens[cursor - 3], TokenTree::Ident(_))
            {
                cursor -= 3;
            }
            if cursor >= 1
                && matches!(&tokens[cursor - 1], TokenTree::Punct(punct) if punct.as_char() == '$')
            {
                cursor -= 1;
            }
            if cursor >= 2 && colon(&tokens[cursor - 1]) && colon(&tokens[cursor - 2]) {
                cursor -= 2;
            }
            cursor
        }

        fn encode_sequence(tokens: &[TokenTree]) -> Vec<u8> {
            let mut encoded = Vec::new();
            encoded.extend_from_slice(&(tokens.len() as u64).to_be_bytes());
            for token in tokens {
                encoded.extend_from_slice(&encode_token(token));
            }
            encoded
        }

        let prefix_start = if macro_rules_body {
            index.saturating_sub(3)
        } else {
            invocation_prefix_start(tokens, index.saturating_sub(1))
        };
        let mut invocation = vec![u8::from(macro_rules_body)];
        push_frame(
            &mut invocation,
            1,
            &encode_sequence(&tokens[prefix_start..index]),
        );
        push_frame(
            &mut invocation,
            2,
            &encode_token(&TokenTree::Group(group.clone())),
        );

        fn hash_frame(hasher: &mut Sha256, value: &[u8]) {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value);
        }

        let mut hasher = Sha256::new();
        if let Some(parent_context) = parent_context {
            hasher.update(b"mini-agent:macro-context-chain:v1");
            hash_frame(&mut hasher, parent_context.digest.as_bytes());
        } else {
            hasher.update(b"mini-agent:macro-token-tree:v1");
        }
        hash_frame(&mut hasher, &invocation);
        crate::hex::encode_lower(hasher.finalize())
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

    /// Whether the macro whose `!` is at `bang_index` is a [`SPLICE_FREE_MACROS`]
    /// member called by its bare name or through `std`/`core`/`tokio`/`tracing`.
    /// Any other path (`$crate::`, a local module, a metavariable prefix) is not.
    fn is_splice_free_invocation(tokens: &[TokenTree], bang_index: usize) -> bool {
        let colon =
            |token: &TokenTree| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ':');
        let Some(mut cursor) = bang_index.checked_sub(1) else {
            return false;
        };
        let TokenTree::Ident(name) = &tokens[cursor] else {
            return false;
        };
        if !SPLICE_FREE_MACROS.contains(&normalized_ident(name).as_str()) {
            return false;
        }
        while cursor >= 3
            && colon(&tokens[cursor - 1])
            && colon(&tokens[cursor - 2])
            && let TokenTree::Ident(segment) = &tokens[cursor - 3]
        {
            if !matches!(
                normalized_ident(segment).as_str(),
                "std" | "core" | "tokio" | "tracing"
            ) {
                return false;
            }
            cursor -= 3;
        }
        // A leading `::` is fine; `$` (a metavariable prefix) is not.
        !(cursor >= 1
            && matches!(&tokens[cursor - 1], TokenTree::Punct(punct) if punct.as_char() == '$'))
    }

    fn scan(
        stream: TokenStream,
        provenance: &SourceProvenance,
        calls: &mut Vec<TerminalCall>,
        macro_context: Option<MacroContext>,
        macro_occurrences: &mut BTreeMap<String, usize>,
        splice_free: bool,
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
                let group_splice_free = if macro_rules_body {
                    false
                } else if begins_macro {
                    splice_free && is_splice_free_invocation(&tokens, index - 1)
                } else {
                    splice_free
                };
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
                    group_splice_free,
                );
            }
            let TokenTree::Ident(ident) = token else {
                continue;
            };
            let name = normalized_ident(ident);
            let qualified = is_method_or_ufcs(&tokens, index);
            // A bare identifier in a splice-free macro is data, not a method name.
            let macro_method_ident = macro_context.is_some() && !qualified && !splice_free;
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
    scan(
        stream,
        &provenance,
        &mut calls,
        None,
        &mut BTreeMap::new(),
        true,
    );
    calls.sort_by_key(|call| call.line);
    Ok(calls)
}

/// `(path, trimmed source fingerprint, occurrence count, trust class)`.
///
/// Counts make adding an identical launch expression visible without coupling
/// the inventory to source line numbers.
const UNIFORM_SITES: &[(&str, &str, usize, &str)] = &[
    // perf 2026-10-19: test assertions over returned tool output strings.
    // perf 2026-10-19: test-only kill/status, thread spawns and a signature type.
    ("src/extras/acp/mod.rs", ".status()", 1, "TEST-ONLY"),
    // perf 2026-10-19: test-only kill/status, thread spawns and a signature type.
    (
        "src/extras/acp/mod.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    // perf 2026-10-19: test-only kill/status, thread spawns and a signature type.
    (
        "src/extras/hooks/subprocess.rs",
        ") -> Result<(tokio::process::Command, std::path::PathBuf), Box<HookOutput>> {",
        1,
        "NON-PROCESS",
    ),
    // perf 2026-10-19: test-only kill/status, thread spawns and a signature type.
    ("src/sandbox.rs", ".spawn(move || {", 2, "NON-PROCESS"),
    // 2026-11-07: Cli::parse_command_line runs clap on an 8 MiB thread (stack headroom); a thread, not a process.
    ("src/cli.rs", ".spawn(parse)", 1, "NON-PROCESS"),
    // perf 2026-10-19: test-only kill/status, thread spawns and a signature type.
    (
        "src/sandbox.rs",
        "scope.spawn(move || windows::terminate_helper(*pid));",
        1,
        "NON-PROCESS",
    ),
    // Identity-checked startup-only original Git --exec-path query; never model argv.
    (
        "src/git/runner.rs",
        "let mut command = Command::new(&program);",
        1,
        "TC-INTERNAL-GIT",
    ),
    // Guarded Git config fixture constructor and borrowed environment validator.
    (
        "src/git/global_snapshot.rs",
        "let mut command = tokio::process::Command::new(runner.guarded_runtime().unwrap().program());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/global_snapshot.rs",
        "pub(super) fn validate_environment(command: &tokio::process::Command) -> Result<(), String> {",
        1,
        "NON-PROCESS",
    ),
    // Provider credential and HTTP status values are application data.
    // Copilot HTTP/status data and explicitly owned test-only Tokio actors.
    (
        "src/auth/copilot.rs",
        "let response_code = response.status().as_u16();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/slash/providers.rs",
        "for line in crate::auth::copilot::status()? {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { sign_in_profile(p, dir).await });",
        4,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { r.auth_context().await });",
        4,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { registration.auth_context().await });",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/slash/providers.rs",
        "for line in crate::auth::oauth::status()? {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/oauth.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/oauth.rs",
        ".is_ok_and(|r| r.status().as_u16() == 200)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        ".status(http::StatusCode::OK)",
        1,
        "NON-PROCESS",
    ),
    // ACP AvailableCommand constructors build protocol descriptions, not processes.
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(",
        4,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(\"compact\", \"Summarize the completed session context\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(\"rename\", \"Rename this saved session\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "commands.push(AvailableCommand::new(",
        2,
        "NON-PROCESS",
    ),
    // ACP protocol statuses and the Bedrock HTTP response builder are data.
    (
        "src/extras/acp/sessions.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/sessions.rs",
        ".status(ToolCallStatus::Failed)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_bedrock.rs",
        ".status(response.status())",
        1,
        "NON-PROCESS",
    ),
    // ReadImage serializes already admitted image data; no process terminal.
    (
        "src/agent/tools/read.rs",
        "output: image.output(&result.output),",
        1,
        "NON-PROCESS",
    ),
    // Exact cfg(test), owned/reaped current-image publication helpers.
    (
        "src/session/native_publication.rs",
        "let mut child = Command::new(std::env::current_exe().unwrap())",
        2,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        "let mut command = Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        ".spawn()",
        2,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        "command.spawn().unwrap()",
        1,
        "TEST-ONLY",
    ),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("instruction61-app-one-mib".into()).stack_size(1024 * 1024).spawn(|| {"#,
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "let child = cmd.spawn()?;",
        1,
        "TC-LIFECYCLE-HELPER",
    ),
    // reqwest response status is HTTP metadata, never a process terminal.
    (
        "src/update.rs",
        "if response.status() != reqwest::StatusCode::OK {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/context/commands.rs",
        "let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/cli.rs",
        "tokio::process::Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/provider_http.rs",
        "let listing_status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("model48-app-one-mib".into()).stack_size(1024*1024).spawn(|| {"#,
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "let response_code = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        ".status(response.status())",
        2,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "if !response.status().is_success() {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/coordinator.rs",
        "let holder = HeldTestLock::spawn(move |wait| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/session.rs",
        "let holder = HeldTestLock::spawn(move |wait| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/operations.rs",
        "let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/js/skills/operations.rs",
        "let restarted = TelemetryDispatcher::spawn(&paths).unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "std::mem::drop(runtime.spawn(crate::spend::run::scope(run, async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/runner.rs",
        "std::mem::drop(self.runtime.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    // Fixed operator clipboard helpers delegate to the existing bounded owner.
    (
        "src/ui/clipboard_image.rs",
        "let mut command = tokio::process::Command::new(\"/usr/bin/osascript\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/clipboard_image.rs",
        "let mut command = tokio::process::Command::new(program);",
        2,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/clipboard_image.rs",
        "mut command: tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    // Small-stack thread for the turn-spawn stack regression (mini-agent-ood0d).
    ("src/agent/runner.rs", ".spawn(|| {", 1, "NON-PROCESS"),
    // Exact cfg(test) startup consumer on a joined 1 MiB Rust thread.
    ("src/startup.rs", ".spawn(|| {", 1, "NON-PROCESS"),
    // Owned one-MiB native capture consumer, joined before fixture teardown.
    (
        "src/provider_effort_replay_probe.rs",
        ".spawn(|| {",
        3,
        "NON-PROCESS",
    ),
    // Exact owned/joined context63 1MiB Rust thread, never an OS process.
    (
        "src/context/instruction_wire_tests.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    // Owned fixed-stack App picker/Git/plan/compaction tests use std::thread.
    ("src/ui/app.rs", ".spawn(|| {", 7, "NON-PROCESS"),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("config68-app".into()).stack_size(1024*1024).spawn(||{"#,
        1,
        "NON-PROCESS",
    ),
    ("src/ui/app.rs", "task.spawn(async move {", 1, "NON-PROCESS"),
    ("src/ui/app.rs", ".spawn(move || {", 1, "NON-PROCESS"),
    // Adopts an already-spawned trusted Windows hook into a Job (mini-agent-8cxmy).
    (
        "src/sandbox.rs",
        "pub(crate) fn adopt(child: &tokio::process::Child) -> Self {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "let output = command.output().await.unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/lsp/client.rs",
        "command: &mut tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/lsp/client.rs",
        "_command: &mut tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/docs.rs",
        "match std::process::Command::new(\"less\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/docs.rs", ".status()", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        2,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", ".status(status)", 1, "NON-PROCESS"),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    (
        "src/extras/acp/mod.rs",
        "cx.spawn(tasks::watch(",
        1,
        "NON-PROCESS",
    ),
    // The ACP /cron driver is a connection task on the ACP executor, not a process.
    (
        "src/extras/acp/cron.rs",
        "&& let Err(error) = cx.spawn(drive(",
        1,
        "NON-PROCESS",
    ),
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
        "src/git/runner.rs",
        "let mut command = Command::new(self.program.as_path());",
        1,
        "TC-INTERNAL-GIT",
    ),
    // Owned real-Git fixture: config probe, old-authority marker, and hardened add.
    (
        "src/git/runner.rs",
        "let init = std::process::Command::new(git.program.as_path())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "let probe = std::process::Command::new(git.program.as_path())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "let mut command = std::process::Command::new(git.program.as_path());",
        2,
        "TEST-ONLY",
    ),
    ("src/git/runner.rs", ".output()", 2, "TEST-ONLY"),
    (
        "src/git/runner.rs",
        "command.output().unwrap()",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "command.args(args).output().unwrap()",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "use tokio::process::Command;",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "use tokio::process::Child;",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        ".with_validated_root(project_dir, || cmd.spawn())",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        "None => cmd.spawn().map_err(HookLaunchError::Spawn),",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/agent/tools/web_http.rs",
        "if is_followable_redirect(response.status()) {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/web_http.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    // Native web exchange: the HTTP status code of a response.
    (
        "src/agent/tools/web_http.rs",
        "let status = response.status().as_u16();",
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
        "src/extras/js/skills/verify.rs",
        ".spawn(|| supervisor.verify_blocking(request))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/worker.rs",
        ".spawn(&program, &arguments)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/worker.rs",
        "\"status\": status.as_str(),",
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
        "src/extras/js/supervisor.rs",
        ".spawn(move || run_verification_scheduler(supervisor, receiver, worker_queue))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || retire_idle_generation(weak, generation, retire_at, waiting_ticket));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/tls.rs",
        "pending.spawn(activity.track(authentication));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "let check = scope.spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    // Native held-ledger fixture re-executes only this owned test binary.
    (
        "src/spend/request_tests.rs",
        "let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/spend/request_tests.rs",
        ".env(CHILD_ENV, root.path()).stdout(stdout).stderr(stderr).spawn().unwrap());",
        1,
        "TEST-ONLY",
    ),
    // Owned concurrent ledger fixture: std::thread::Scope, never an OS process.
    (
        "src/spend/ledger.rs",
        "scope.spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/macos.rs",
        "let output = std::process::Command::new(SW_VERS)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let child = command.spawn()?;",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn(move || {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(executable);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(image_path);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    // Concurrent anonymous-source image fixture uses joined Rust scope threads.
    (
        "src/sandbox/worker/macos.rs",
        "scope.spawn(move || -> io::Result<_> {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut worker = Command::new(SANDBOX_EXEC);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "Command::new(\"/private/tmp/mini-agent-definitely-missing-guardian-executable\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(\"/usr/bin/true\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let child = std::process::Command::new(\"/bin/sleep\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut piped = Command::new(sandbox_exec)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut child = Command::new(sandbox_exec)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let spawned = Command::new(path)",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn();",
        1,
        "TC-BROKER-JS-WORKER",
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
        "fn lsp_command(cfg: &LspServerConfig, root: &Path) -> anyhow::Result<tokio::process::Command> {",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        ") -> anyhow::Result<tokio::process::Command> {",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "let mut command = tokio::process::Command::new(program);",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/mod.rs",
        "let spawned = LspClient::spawn(",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/lsp/client.rs",
        "stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "stdin: &Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "stdin: Option<tokio::sync::MutexGuard<'a, tokio::process::ChildStdin>>,",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/lsp/client.rs",
        "fn writer(&mut self) -> &mut tokio::process::ChildStdin {",
        1,
        "TC-LSP-SERVICE",
    ),
    (
        "src/extras/mcp/bounded_http.rs",
        "response.status()",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/client.rs",
        "let mut child = command.spawn()?;",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/client.rs",
        "Option<tokio::process::ChildStderr>,",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/client.rs",
        "assert_eq!(response.status(), reqwest::StatusCode::FOUND);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        ".status(response.status())",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "let code = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "assert!(response.status().is_success());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "assert_eq!(response.status(), http::StatusCode::FOUND);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/client.rs",
        "use tokio::process::{ChildStdin, ChildStdout, Command};",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/client.rs",
        "let mut child = Command::new(program);",
        1,
        "TC-MCP-STDIO",
    ),
    // `--mcp-server` relaunches this executable as a headless `-p` child
    // with the operator's own flags and the client prompt as one literal
    // argv element after `--`.
    (
        "src/extras/mcp/server.rs",
        "let mut child = tokio::process::Command::new(&command.program);",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/extras/mcp/server.rs",
        "let mut child = match owned_command.spawn() {",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    // The preflight and the option-parsing `--ro-bind-fd` support probe
    // (mini-agent-4jsau), which exits at `--version` before any namespace.
    (
        "src/sandbox.rs",
        "let mut command = std::process::Command::new(bwrap);",
        2,
        "TC-INTERNAL-VERIFICATION",
    ),
    // Host `git` fixture and log check for the real-backend workspace Git
    // metadata tests (mini-agent-4jsau).
    (
        "src/sandbox.rs",
        "let output = std::process::Command::new(\"git\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let log = std::process::Command::new(\"git\")",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox.rs", ".output()", 2, "TEST-ONLY"),
    (
        "src/sandbox.rs",
        "let mut command = std::process::Command::new(seatbelt);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(program);",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/sandbox.rs",
        "use tokio::process::{Child, Command};",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut command = Command::new(\"inert-command-108\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut command = Command::new(\"inert-helper-108\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "assert!(!ExecutionCommand::from(Command::new(\"inert-helper-108\")).helper_owns_tree());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        "use tokio::process::{Child as TokioChild, Command as TokioCommand};",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        "let mut command = TokioCommand::new(\"powershell.exe\");",
        5,
        "TEST-ONLY",
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
        "src/sandbox/spawner.rs",
        ".spawn(move || spawner_loop(receiver))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/spawner.rs",
        "let result = job.command.spawn();",
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
        "src/sandbox.rs",
        "let mut child_command = std::process::Command::new(\"/bin/sh\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut child = child_command.spawn().unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut hook = command.spawn().unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut trusted = Command::new(\"/bin/sleep\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut sandboxed = Command::new(\"/bin/sleep\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut isolated = std::process::Command::new(current_exe);",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let status = isolated.status().unwrap();",
        1,
        "TEST-ONLY",
    ),
    // Inert source-policy search string, not a process constructor.
    (
        "src/sandbox.rs",
        r#"let launch = parent_transport.find("Command::new(executable)").unwrap();"#,
        1,
        "NON-PROCESS",
    ),
    // Explicit installed self-check observer: one joined Rust writer, same
    // fixed production helper constructor/creation boundary; no new target.
    (
        "src/sandbox/windows/preflight_witness.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "command: tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "let mut helper = Command::new(executable);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "let mut command = tokio::process::Command::from(helper);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        ") -> Result<tokio::process::Command, String> {",
        7,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut helper = Command::new(executable);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut helper = tokio::process::Command::from(helper);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        "let output = Command::new(std::env::current_exe().expect(\"resolve test executable\"))",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows.rs",
        "let status = Command::new(\"cmd\")",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/windows.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/sandbox/windows.rs",
        "command: &mut tokio::process::Command,",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut command = Command::new(std::env::current_exe().expect(\"test image\"));",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn(move || {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = match Command::new(tool)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = Command::new(descendant_executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut parent = Command::new(executable)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut descendant_command = Command::new(executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut descendant = match descendant_command.spawn() {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut breakaway = Command::new(breakaway_executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "match breakaway.status() {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/ui/app.rs",
        "let mut command = tokio::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let mut probe = tokio::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/ui/input/mod.rs", ".status();", 1, "TC-SUPPORT-UTILITY"),
    (
        "src/ui/input/mod.rs",
        "let result = std::process::Command::new(\"sh\")",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/renderer.rs",
        "let Ok(mut child) = command.spawn() else {",
        2,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/renderer.rs",
        "let mut command = tokio::process::Command::new(cmd);",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/renderer.rs",
        "mut command: tokio::process::Command,",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/renderer.rs",
        "commands: &mut [tokio::process::Command],",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/renderer.rs",
        "let mut command = tokio::process::Command::new(program);",
        1,
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
        "src/extras/mcp/headers_helper.rs",
        "let mut child = command.spawn().map_err(|error| {",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/server.rs",
        "mut stdin: Option<tokio::process::ChildStdin>,",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/key_command.rs", ".spawn()", 2, "TC-CREDENTIAL-HELPER"),
    (
        "src/key_command.rs",
        "let mut command = Command::new(&spec.command[0]);",
        1,
        "TC-CREDENTIAL-HELPER",
    ),
    (
        "src/key_command.rs",
        "let mut command = tokio::process::Command::new(&spec.command[0]);",
        1,
        "TC-CREDENTIAL-HELPER",
    ),
    (
        "src/sandbox/net_forwarder.rs",
        "let error = std::process::Command::new(&command[0])",
        1,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/self_update.rs",
        "let mut child = command.spawn()?;",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/self_update.rs",
        "run_bounded(std::process::Command::new(program).args(args), limits)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/self_update.rs",
        "std::process::Command::new(&probe_clone).arg(\"--version\"),",
        1,
        "TC-SUPPORT-UTILITY",
    ),
];

/// Exact macro-token fingerprints whose terminal spelling is data rather than
/// a process method. Any unlisted `spawn`, `output`, or `status` identifier in
/// macro-controlled tokens remains process authority and fails closed.
const MACRO_IDENTIFIER_NON_PROCESS_SITES: &[(&str, &str, usize, &str)] = &[
    // Native NTSTATUS conversion assertion: `status` is input data, never
    // a process method. The complete reviewed macro digest is pinned below.
    // Exact capture/JSON data tokens within the reviewed full macro digests.
    (
        "src/git/tool.rs",
        "\"path_records\": parse_status_paths(&output.stdout, truncated),",
        1,
        "NON-PROCESS",
    ),
    // Wakeup tool and App /cron assertions inspect returned or rendered strings.
    // Exact native-result and compact-status assertion identifiers are data.
    // Native persisted-result CompactString payload, not process output.
    // Assertions inspect already-owned Git stdout/stderr/status; never launch methods.
    (
        "src/auth/oauth.rs",
        "anyhow::bail!(\"Provider authentication HTTP status {}\", status.as_u16());",
        1,
        "NON-PROCESS",
    ),
    // TodoItem status data in the exact cancellation relay fixture below.
    // Reviewed local HookOutput fixture calls inside exact matches! contexts.
    // Numeric NTSTATUS arguments in the two test-only native diagnostics.
    // Literal formatter-result assertion data; exact full-token digests below.
    // Exact captured model/text data assertions in context63 controls.
    // Exact async-only MCP HTTP fixture and HTTP response status tuple.
    (
        "src/ui/mcp_admin_app_tests.rs",
        "clients.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    // Pressure continuation assertion inspects SDK ToolResult output text.
    // Captured native SDK output text and child ExitStatus assertions are data.
    (
        "src/spend/request_tests.rs",
        r#"!output.contains("[failed:") && !output.contains("[partial:"),"#,
        1,
        "NON-PROCESS",
    ),
    // Context diagnostics inspect captured renderer String values.
    // Doctor assertions inspect captured text; complete macro digests below
    // also bind these exact terminal-looking string methods.
    // Price-pair bindings inside assertion/rendering macros are numeric data.
    // These inline shell tests inspect captured String output, not commands.
    // Bounded read/replay and file-search regression outputs are strings.
    // `status` in the goal module is a `GoalStatus`, not a process exit
    // status, and `report_tool`'s is a `ReportStatus`. The identifier scanner
    // cannot tell them apart inside a macro, so each site is classified here.
    // The goal module launches nothing: its only commands run through the
    // shared validation runner (`TC-GOAL-CHECK`).
    // Distiller test fixtures: `output` is a persisted tool-result field, not a
    // process output.
    // Learned-skill operator commands: `status` here is a lifecycle status
    // rendered into a log field or an error message, never a process exit.
    (
        "src/extras/js/skills/operations.rs",
        "status.as_deref().unwrap_or(\"absent\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/docs.rs",
        "Ok(status) => anyhow::bail!(\"less exited with {}\", status),",
        1,
        "NON-PROCESS",
    ),
    // Notebook parser/view assertions inspect owned String data, not processes.
    (
        "src/extras/export.rs",
        "anyhow::bail!(\"GitHub API returned {}: {}\", status, text.trim());",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/js/skills/store.rs", "status,", 8, "NON-PROCESS"),
    // Authored native heading formats a String, never a process receiver.
    (
        "src/extras/loop/mod.rs",
        "status.success(),",
        1,
        "NON-PROCESS",
    ),
    // FakeStep response fields in the rendered-output budget regressions.
    // These vec! bodies contain no process receiver or terminal call.
    // Captured Sandbox command-result fields in native boundary tests; the
    // added Seatbelt assertions are data inspection, not process terminals.
    (
        "src/git/tool.rs",
        "let output = Command::new(\"git\")",
        1,
        "TEST-ONLY",
    ),
    ("src/git/tool.rs", ".output()", 1, "TEST-ONLY"),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(\"cost\", \"Show this session's cost and context use\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "let mut command = AvailableCommand::new(name, description);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        "let exit_code = if response.status().is_success() { 0 } else { 1 };",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/server.rs",
        "tasks.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "if response.status() != http::StatusCode::UNAUTHORIZED",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/conpty.rs",
        ".spawn(move || {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/net_forwarder.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/net_proxy.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/self_update.rs",
        "if !response.status().is_success() {",
        1,
        "NON-PROCESS",
    ),
    ("src/self_update.rs", "response.status()", 1, "NON-PROCESS"),
    (
        "src/update.rs",
        "return Err(format!(\"{endpoint} answered HTTP {}\", response.status()));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/headers_helper.rs",
        "output.len() <= MAX_OUTPUT_BYTES,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/headers_helper.rs",
        "status",
        1,
        "NON-PROCESS",
    ),
    ("src/provider.rs", "output,", 9, "NON-PROCESS"),
    (
        "src/provider_effort_tests.rs",
        "Self::spawn(responses, \"/v1/messages\", true)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_effort_tests.rs",
        "Self::spawn(responses, path, false)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/spend/request_tests.rs",
        "assert_eq!(output.summary, \"A retried summary\");",
        1,
        "NON-PROCESS",
    ),
];

/// SHA-256 of the length-framed structural macro path and complete token tree.
/// Token kinds, root qualification, raw identifier spelling, punctuation
/// spacing, and every nested delimiter are identity-bearing.
/// Occurrence counts prevent an identical invocation from borrowing an earlier
/// approval in the same source file.
const MACRO_NON_PROCESS_CONTEXTS: &[(&str, &[(&str, usize)])] = &[
    // perf 2026-10-19 tests: assertions over returned tool/hook output values.
    // Wakeup tool and App /cron assertions inspect a returned tool-result
    // String or captured renderer output; no process is constructed.
    // Task cost weight assertions compare a struct whose field is named
    // `output`; the token tree constructs or executes no process.
    // Compact partial marker assertions inspect a String, never a process.
    // Native profile fixtures inspect CommandOutput fields in exact assertions;
    // these token trees contain no process construction or execution call.
    // Native real-path tests inspect bounded CommandOutput data only.
    // Exact full assertion tokens for frozen-global and Windows service data.
    (
        "src/auth/oauth.rs",
        &[(
            "54ce06e836d9bacb3b577c577930f047fc5e11b66494159d86f4ec436fc1af30",
            1,
        )],
    ),
    // Test-only diagnostic prints a numeric NTSTATUS, not a process status terminal.
    // Bounded listing response status is metadata, not a subprocess terminal.
    // Complete tokens of the exact context63 captured data macros above.
    (
        "src/ui/mcp_admin_app_tests.rs",
        &[(
            "523c9c17d8bf92e6dfeeb73177d8ef9fd174d1406a081b1418ef1c825ff40a13",
            1,
        )],
    ),
    // CLI executor output fields and todo labels are data. Full macro tokens
    // bind these exemptions; constructors remain TEST-ONLY, not exempted here.
    // These assertions compare mathematical output-token prices and a
    // persisted tool-result string. Their full macro identities are bound;
    // none of the receivers is a process builder or launches a command.
    // Goal records carry a `status` field, so `goal.status` and
    // `outcome.status` inside an assertion or a format string look like a
    // process `status()` terminal to the scanner. None of these launch
    // anything: the goal module builds no process and reaches commands only
    // through the shared validation runner (`TC-GOAL-CHECK`).
    (
        "src/extras/export.rs",
        &[(
            "2642f045409085bb7c6fe9d020dad6e7a3f2915275c0c3686ac43710af88e0bc",
            1,
        )],
    ),
    (
        "src/extras/js/worker.rs",
        &[(
            "2ae1572a8e3e684cff68f2fdcdb44d0c5ee3808f1bb261e9b1818446ee024674",
            1,
        )],
    ),
    (
        "src/extras/js/skills/operations.rs",
        &[(
            "7e2e1921ca489c8880183f827afffd928a982893eabe1ec6ead9228af5d99d36",
            1,
        )],
    ),
    (
        "src/extras/js/skills/store.rs",
        &[(
            "37b1d55d03713d423bc79e2bdfc046830486adddb9997708182cbd6434852b94",
            1,
        )],
    ),
    (
        "src/extras/js/tool.rs",
        &[(
            "34e3105112941e29707d7c89c27658d9e34c02bcf446c9bf49b08d275c5fbdfa",
            1,
        )],
    ),
    (
        "src/extras/loop/mod.rs",
        &[(
            "e225d1fcf2e8c60201d067ff175cbdcbbf980e3b0a3b09a34a8aa0279bfe11e7",
            1,
        )],
    ),
    // Exact full-token contexts of those four notebook String assertions.
    (
        "src/extras/mcp/client.rs",
        &[(
            "6b211d8f8be30409f924287de2e683de0270d27a917d5308932c7e693216b759",
            1,
        )],
    ),
    // HTTP response status assertions in the MCP egress (DNS pinning) tests.
    (
        "src/extras/mcp/egress.rs",
        &[
            (
                "56f9f8b16d35fc1be3c432a7d764aadd96ea501839ca14a09963e45bf406bc4f",
                1,
            ),
            (
                "df267c0700d3a41de9a0eb6a981b1b50b28f85cb3af770b7d4b261d3f6715198",
                1,
            ),
        ],
    ),
    (
        "src/docs.rs",
        &[(
            "eaf04cf21084dd8d714729cf31f39143471900f5d86a880cffc12bbfe31be9a0",
            1,
        )],
    ),
    (
        "src/extras/mcp/headers_helper.rs",
        &[(
            "799a331a350925d5bf10f3be8ab35494422be724608e4aff3caf7aa21852bbfe",
            1,
        )],
    ),
    (
        "src/extras/mcp/headers_helper.rs",
        &[(
            "d64637dd113937a5405720028943ee1109608e5a2244f4176e70ba0ece55c032",
            1,
        )],
    ),
    (
        "src/extras/mcp/server.rs",
        &[(
            "d6d38b65394d86e02f59accaf62ebf13b460f5cddfc3914e71a90e7713f6d042",
            1,
        )],
    ),
    (
        "src/provider.rs",
        &[(
            "2544a43f58db98f7e0aa66a33286959c9587ce7dd8ee92935ac05d7f8b6a2484",
            1,
        )],
    ),
    (
        "src/self_update.rs",
        &[(
            "d6ca57f4bacf12246c753e4844e96388bc16a0703953a6150782adea1958b2ea",
            1,
        )],
    ),
    (
        "src/update.rs",
        &[(
            "fd6a7fea2777cec3193c620b5076e42fc813fd793ca7f909ed003c722e72009a",
            1,
        )],
    ),
    // Exact select bodies inspect captured I/O/status and JoinSet tasks;
    // actual child creation remains outside these macros and guarded.
    // This status JSON reads captured CommandOutput.stdout; no launch occurs.
    (
        "src/git/tool.rs",
        &[(
            "1735ccb352820a631e226dc84e5f3519ce50986a4f53c07de077ce1528e49969",
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
const MIXED_SITES: &[(&str, &str, &[&str])] = &[
    (
        "src/sandbox/windows.rs",
        ".spawn()",
        &[
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox/windows.rs",
        ".output()",
        &[
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".output()",
        &["TC-BROKER-JS-WORKER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn()",
        &[
            "TC-BROKER-JS-WORKER",
            "TEST-ONLY",
            "TEST-ONLY",
            "TEST-ONLY",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(zerobox);",
        &["TC-MODEL-ACTION", "TC-PROJECT-AUTOMATION", "TC-MCP-STDIO"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        &["TC-PROJECT-AUTOMATION", "TC-MCP-STDIO", "TC-MODEL-ACTION"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        &["TC-PROJECT-AUTOMATION", "TC-MCP-STDIO", "TC-MODEL-ACTION"],
    ),
];

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
    "TC-CREDENTIAL-HELPER",
    "TC-PROJECT-AUTOMATION",
    "TC-SUPPORT-UTILITY",
];

/// Exact ownership for every lexical disposition and every site in a source
/// file that contains more than one production trust class.
const EXACT_UNIFORM_SITE_CLASSES: &[(&str, &str, usize, &str)] = &[
    // perf 2026-10-19 sites (see UNIFORM_SITES).
    ("src/extras/acp/mod.rs", ".status()", 1, "TEST-ONLY"),
    ("src/sandbox.rs", ".spawn(move || {", 2, "NON-PROCESS"),
    ("src/cli.rs", ".spawn(parse)", 1, "NON-PROCESS"),
    (
        "src/extras/acp/mod.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/hooks/subprocess.rs",
        ") -> Result<(tokio::process::Command, std::path::PathBuf), Box<HookOutput>> {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "scope.spawn(move || windows::terminate_helper(*pid));",
        1,
        "NON-PROCESS",
    ),
    // Guarded Git config fixture constructor and borrowed environment validator.
    (
        "src/git/global_snapshot.rs",
        "let mut command = tokio::process::Command::new(runner.guarded_runtime().unwrap().program());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/global_snapshot.rs",
        "pub(super) fn validate_environment(command: &tokio::process::Command) -> Result<(), String> {",
        1,
        "NON-PROCESS",
    ),
    // Provider credential and HTTP status values are application data.
    // Copilot HTTP/status data and explicitly owned test-only Tokio actors.
    (
        "src/auth/copilot.rs",
        "let response_code = response.status().as_u16();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/slash/providers.rs",
        "for line in crate::auth::copilot::status()? {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { sign_in_profile(p, dir).await });",
        4,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { r.auth_context().await });",
        4,
        "NON-PROCESS",
    ),
    (
        "src/auth/copilot_protocol_tests.rs",
        "tasks.spawn(async move { registration.auth_context().await });",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/slash/providers.rs",
        "for line in crate::auth::oauth::status()? {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/oauth.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/auth/oauth.rs",
        ".is_ok_and(|r| r.status().as_u16() == 200)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        ".status(http::StatusCode::OK)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(",
        4,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(\"compact\", \"Summarize the completed session context\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "AvailableCommand::new(\"rename\", \"Rename this saved session\"),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/commands.rs",
        "commands.push(AvailableCommand::new(",
        2,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/sessions.rs",
        ".status(ToolCallStatus::Completed)",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_bedrock.rs",
        ".status(response.status())",
        1,
        "NON-PROCESS",
    ),
    // ReadImage serializes already admitted image data; no process terminal.
    (
        "src/agent/tools/read.rs",
        "output: image.output(&result.output),",
        1,
        "NON-PROCESS",
    ),
    // Exact owned cfg(test) native publication helper sites.
    (
        "src/session/native_publication.rs",
        "let mut child = Command::new(std::env::current_exe().unwrap())",
        2,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        "let mut command = Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        ".spawn()",
        2,
        "TEST-ONLY",
    ),
    (
        "src/session/native_publication.rs",
        "command.spawn().unwrap()",
        1,
        "TEST-ONLY",
    ),
    ("src/startup.rs", ".spawn(|| {", 1, "NON-PROCESS"),
    (
        "src/ui/clipboard_image.rs",
        "mut command: tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("instruction61-app-one-mib".into()).stack_size(1024 * 1024).spawn(|| {"#,
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "let child = cmd.spawn()?;",
        1,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/sandbox.rs",
        "let mut command = Command::new(\"inert-command-108\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut command = Command::new(\"inert-helper-108\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "assert!(!ExecutionCommand::from(Command::new(\"inert-helper-108\")).helper_owns_tree());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        "use tokio::process::{Child as TokioChild, Command as TokioCommand};",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows/general_owner.rs",
        "let mut command = TokioCommand::new(\"powershell.exe\");",
        5,
        "TEST-ONLY",
    ),
    (
        "src/context/commands.rs",
        "let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    // reqwest response status is HTTP metadata, never a process terminal.
    (
        "src/update.rs",
        "if response.status() != reqwest::StatusCode::OK {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/cli.rs",
        "tokio::process::Command::new(std::env::current_exe().unwrap());",
        1,
        "TEST-ONLY",
    ),
    (
        "src/provider_http.rs",
        "let listing_status = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("model48-app-one-mib".into()).stack_size(1024*1024).spawn(|| {"#,
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "let response_code = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        ".status(response.status())",
        2,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "if !response.status().is_success() {",
        3,
        "NON-PROCESS",
    ),
    (
        "src/provider_http.rs",
        "let status = response.status();",
        1,
        "NON-PROCESS",
    ),
    // Owned real-Git fixture: config probe, old-authority marker, and hardened add.
    (
        "src/git/runner.rs",
        "let init = std::process::Command::new(git.program.as_path())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "let probe = std::process::Command::new(git.program.as_path())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "let mut command = std::process::Command::new(git.program.as_path());",
        2,
        "TEST-ONLY",
    ),
    ("src/git/runner.rs", ".output()", 2, "TEST-ONLY"),
    (
        "src/git/runner.rs",
        "command.output().unwrap()",
        1,
        "TEST-ONLY",
    ),
    (
        "src/git/runner.rs",
        "command.args(args).output().unwrap()",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/js/skills/coordinator.rs",
        "let holder = HeldTestLock::spawn(move |wait| {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/skills/session.rs",
        "let holder = HeldTestLock::spawn(move |wait| {",
        1,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", ".status(status)", 1, "NON-PROCESS"),
    (
        "src/extras/js/skills/operations.rs",
        "let dispatcher = TelemetryDispatcher::spawn(&paths).unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/js/skills/operations.rs",
        "let restarted = TelemetryDispatcher::spawn(&paths).unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/mcp/bounded_http.rs",
        "response.status()",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/client.rs",
        "assert_eq!(response.status(), reqwest::StatusCode::FOUND);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        ".status(response.status())",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "let code = response.status();",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "assert!(response.status().is_success());",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/mcp/egress.rs",
        "assert_eq!(response.status(), http::StatusCode::FOUND);",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "let mut command = std::process::Command::new(bwrap);",
        2,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox.rs",
        "let output = std::process::Command::new(\"git\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let log = std::process::Command::new(\"git\")",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox.rs", ".output()", 2, "TEST-ONLY"),
    (
        "src/sandbox.rs",
        "let mut command = std::process::Command::new(seatbelt);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox.rs",
        "let Ok(mut child) = command.spawn() else {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox.rs",
        "std::mem::drop(runtime.spawn(crate::spend::run::scope(run, async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let output = std::process::Command::new(SW_VERS)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let child = command.spawn()?;",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn(move || {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(executable);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(image_path);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "scope.spawn(move || -> io::Result<_> {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut worker = Command::new(SANDBOX_EXEC);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "Command::new(\"/private/tmp/mini-agent-definitely-missing-guardian-executable\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(\"/usr/bin/true\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let child = std::process::Command::new(\"/bin/sleep\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut piped = Command::new(sandbox_exec)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut child = Command::new(sandbox_exec)",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let spawned = Command::new(path)",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn();",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/agent/runner.rs",
        "std::mem::drop(self.runtime.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    // Small-stack thread for the turn-spawn stack regression (mini-agent-ood0d).
    ("src/agent/runner.rs", ".spawn(|| {", 1, "NON-PROCESS"),
    // Owned one-MiB native capture consumer, joined before fixture teardown.
    (
        "src/provider_effort_replay_probe.rs",
        ".spawn(|| {",
        3,
        "NON-PROCESS",
    ),
    // Exact owned/joined context63 1MiB Rust thread, never an OS process.
    (
        "src/context/instruction_wire_tests.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    // Owned fixed-stack App picker/Git/plan/compaction tests use std::thread.
    ("src/ui/app.rs", ".spawn(|| {", 7, "NON-PROCESS"),
    (
        "src/ui/app.rs",
        r#"std::thread::Builder::new().name("config68-app".into()).stack_size(1024*1024).spawn(||{"#,
        1,
        "NON-PROCESS",
    ),
    ("src/ui/app.rs", "task.spawn(async move {", 1, "NON-PROCESS"),
    ("src/ui/app.rs", ".spawn(move || {", 1, "NON-PROCESS"),
    // Adopts an already-spawned trusted Windows hook into a Job (mini-agent-8cxmy).
    (
        "src/sandbox.rs",
        "pub(crate) fn adopt(child: &tokio::process::Child) -> Self {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "let output = command.output().await.unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/lsp/client.rs",
        "command: &mut tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/lsp/client.rs",
        "_command: &mut tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/worker.rs",
        "\"status\": status.as_str(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || run_verification_scheduler(supervisor, receiver, worker_queue))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/supervisor.rs",
        ".spawn(move || retire_idle_generation(weak, generation, retire_at, waiting_ticket));",
        1,
        "NON-PROCESS",
    ),
    ("src/agent/tools/bash.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/agent/tools/bash.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/acp/mod.rs",
        ".status(ToolCallStatus::Completed)",
        2,
        "NON-PROCESS",
    ),
    ("src/extras/acp/mod.rs", "cx.spawn({", 1, "NON-PROCESS"),
    // The ACP /cron driver is a connection task on the ACP executor, not a process.
    (
        "src/extras/acp/cron.rs",
        "&& let Err(error) = cx.spawn(drive(",
        1,
        "NON-PROCESS",
    ),
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
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/agent/tools/web_http.rs",
        "if is_followable_redirect(response.status()) {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/web_http.rs",
        "status: response.status().as_u16(),",
        1,
        "NON-PROCESS",
    ),
    // Native web exchange: the HTTP status code of a response.
    (
        "src/agent/tools/web_http.rs",
        "let status = response.status().as_u16();",
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
        "src/extras/js/skills/verify.rs",
        ".spawn(|| supervisor.verify_blocking(request))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/worker.rs",
        ".spawn(&program, &arguments)",
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
        "src/extras/acp/tls.rs",
        "pending.spawn(activity.track(authentication));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/tool.rs",
        "let check = scope.spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    // Native held-ledger fixture re-executes only this owned test binary.
    (
        "src/spend/request_tests.rs",
        "let mut child = Child(std::process::Command::new(std::env::current_exe().unwrap())",
        1,
        "TEST-ONLY",
    ),
    (
        "src/spend/request_tests.rs",
        ".env(CHILD_ENV, root.path()).stdout(stdout).stderr(stderr).spawn().unwrap());",
        1,
        "TEST-ONLY",
    ),
    // Owned concurrent ledger fixture: std::thread::Scope, never an OS process.
    (
        "src/spend/ledger.rs",
        "scope.spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/lsp/mod.rs",
        "let spawned = LspClient::spawn(",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(&self.shell);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(program);",
        1,
        "TC-PROJECT-AUTOMATION",
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
        "src/sandbox/spawner.rs",
        ".spawn(move || spawner_loop(receiver))",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/spawner.rs",
        "let result = job.command.spawn();",
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
        "src/sandbox.rs",
        "let mut child_command = std::process::Command::new(\"/bin/sh\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut child = child_command.spawn().unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut hook = command.spawn().unwrap();",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut trusted = Command::new(\"/bin/sleep\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut sandboxed = Command::new(\"/bin/sleep\");",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let mut isolated = std::process::Command::new(current_exe);",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox.rs",
        "let status = isolated.status().unwrap();",
        1,
        "TEST-ONLY",
    ),
    // Inert source-policy search string, not a process constructor.
    (
        "src/sandbox.rs",
        r#"let launch = parent_transport.find("Command::new(executable)").unwrap();"#,
        1,
        "NON-PROCESS",
    ),
    // Explicit installed self-check observer: one joined Rust writer, same
    // fixed production helper constructor/creation boundary; no new target.
    (
        "src/sandbox/windows/preflight_witness.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "command: tokio::process::Command,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "let mut helper = Command::new(executable);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows/preflight_witness.rs",
        "let mut command = tokio::process::Command::from(helper);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        ") -> Result<tokio::process::Command, String> {",
        7,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut helper = Command::new(executable);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut helper = tokio::process::Command::from(helper);",
        1,
        "TC-MODEL-ACTION",
    ),
    (
        "src/sandbox/windows.rs",
        "let output = Command::new(std::env::current_exe().expect(\"resolve test executable\"))",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows.rs",
        "let status = Command::new(\"cmd\")",
        1,
        "TEST-ONLY",
    ),
    ("src/sandbox/windows.rs", ".status()", 1, "TEST-ONLY"),
    // The observed preflight split adds a second inert borrowed-command parameter.
    (
        "src/sandbox/windows.rs",
        "command: &mut tokio::process::Command,",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut command = Command::new(std::env::current_exe().expect(\"test image\"));",
        1,
        "TEST-ONLY",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn(move || {",
        2,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = match Command::new(tool)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = Command::new(descendant_executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut parent = Command::new(executable)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut descendant_command = Command::new(executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut descendant = match descendant_command.spawn() {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut breakaway = Command::new(breakaway_executable);",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "match breakaway.status() {",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/ui/app.rs",
        "let mut command = tokio::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/app.rs",
        "let mut probe = tokio::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/extras/mcp/headers_helper.rs",
        "let mut child = command.spawn().map_err(|error| {",
        1,
        "TC-MCP-STDIO",
    ),
    (
        "src/extras/mcp/server.rs",
        "mut stdin: Option<tokio::process::ChildStdin>,",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    ("src/key_command.rs", ".spawn()", 2, "TC-CREDENTIAL-HELPER"),
    (
        "src/key_command.rs",
        "let mut command = Command::new(&spec.command[0]);",
        1,
        "TC-CREDENTIAL-HELPER",
    ),
    (
        "src/key_command.rs",
        "let mut command = tokio::process::Command::new(&spec.command[0]);",
        1,
        "TC-CREDENTIAL-HELPER",
    ),
    (
        "src/sandbox/net_forwarder.rs",
        "let error = std::process::Command::new(&command[0])",
        1,
        "TC-LIFECYCLE-HELPER",
    ),
    (
        "src/self_update.rs",
        "let mut child = command.spawn()?;",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/self_update.rs",
        "run_bounded(std::process::Command::new(program).args(args), limits)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/self_update.rs",
        "std::process::Command::new(&probe_clone).arg(\"--version\"),",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    // ACP connection tasks and a builder method named `status`; no process is launched.
    (
        "src/extras/acp/mod.rs",
        "cx.spawn(tasks::watch(",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/sessions.rs",
        ".status(ToolCallStatus::Failed)",
        1,
        "NON-PROCESS",
    ),
];

const EXACT_MIXED_SITE_CLASSES: &[(&str, &str, &[&str])] = &[
    (
        "src/sandbox/windows.rs",
        ".spawn()",
        &[
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox/windows.rs",
        ".output()",
        &[
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TC-INTERNAL-VERIFICATION",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".output()",
        &["TC-BROKER-JS-WORKER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn()",
        &[
            "TC-BROKER-JS-WORKER",
            "TEST-ONLY",
            "TEST-ONLY",
            "TEST-ONLY",
            "TEST-ONLY",
        ],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(zerobox);",
        &["TC-MODEL-ACTION", "TC-PROJECT-AUTOMATION", "TC-MCP-STDIO"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(bwrap);",
        &["TC-PROJECT-AUTOMATION", "TC-MCP-STDIO", "TC-MODEL-ACTION"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(seatbelt);",
        &["TC-PROJECT-AUTOMATION", "TC-MCP-STDIO", "TC-MODEL-ACTION"],
    ),
];

/// Files whose non-disposition launch expressions all have one owner.
const SINGLE_CLASS_FAMILIES: &[(&str, &str)] = &[
    ("src/docs.rs", "TC-SUPPORT-UTILITY"),
    ("src/git/runner.rs", "TC-INTERNAL-GIT"),
    ("src/extras/hooks/subprocess.rs", "TC-PROJECT-AUTOMATION"),
    ("src/extras/loop/mod.rs", "TC-INTERNAL-VERIFICATION"),
    ("src/extras/lsp/client.rs", "TC-LSP-SERVICE"),
    ("src/extras/mcp/client.rs", "TC-MCP-STDIO"),
    ("src/extras/mcp/server.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/input/mod.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/renderer.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/clipboard_image.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/slash/memory.rs", "TC-SUPPORT-UTILITY"),
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

    for (path, source, occurrence) in exact.keys() {
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

fn cfg_value_without_tests(meta: &syn::Meta) -> Option<bool> {
    match meta {
        syn::Meta::Path(path) if path.is_ident("test") => Some(false),
        syn::Meta::Path(_) | syn::Meta::NameValue(_) => None,
        syn::Meta::List(list) => {
            let nested = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .ok()?;
            if list.path.is_ident("all") {
                let values: Vec<_> = nested.iter().map(cfg_value_without_tests).collect();
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("any") {
                let values: Vec<_> = nested.iter().map(cfg_value_without_tests).collect();
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|value| *value == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else if list.path.is_ident("not") && nested.len() == 1 {
                cfg_value_without_tests(&nested[0]).map(|value| !value)
            } else {
                None
            }
        }
    }
}

fn production_source(source: &str) -> Result<String, String> {
    let file =
        syn::parse_file(source).map_err(|error| format!("Rust source did not parse: {error}"))?;
    let mut lines: Vec<_> = source.split('\n').collect();
    for item in file.items {
        let syn::Item::Mod(module) = item else {
            continue;
        };
        let test_only = module.attrs.iter().any(|attribute| {
            attribute.path().is_ident("cfg")
                && attribute
                    .parse_args::<syn::Meta>()
                    .ok()
                    .and_then(|meta| cfg_value_without_tests(&meta))
                    == Some(false)
        });
        if !test_only {
            continue;
        }
        let start = module
            .attrs
            .first()
            .map_or_else(
                || module.span().start().line,
                |attribute| attribute.span().start().line,
            )
            .saturating_sub(1);
        let end = module.span().end().line.min(lines.len());
        lines[start..end].fill("");
    }
    Ok(lines.join("\n"))
}

#[test]
fn production_source_excludes_test_only_modules() {
    let source = r#"
fn production() {
    std::process::Command::new("production");
}

#[cfg(test)]
mod tests {
    fn fixture() {
        std::process::Command::new("fixture");
    }
}
"#;

    let production = production_source(source).unwrap();
    assert!(production.contains("Command::new(\"production\")"));
    assert!(!production.contains("Command::new(\"fixture\")"));
    assert_eq!(production.lines().count(), source.lines().count());
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
            None => {
                if let Some(context) = &call.macro_context {
                    violations.push(format!(
                        "{relative}:{} unrecognized macro context {}#{} for {} terminal",
                        call.line, context.digest, context.occurrence, call.name
                    ));
                } else {
                    violations.push(format!(
                        "{relative}:{} unrecognized unguarded {} terminal",
                        call.line, call.name
                    ));
                }
            }
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
    let mut all_seen = BTreeMap::<(String, String), usize>::new();
    let mut all_observed = BTreeSet::<(String, String, usize)>::new();

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
            let occurrence = all_seen.entry(fingerprint.clone()).or_default();
            *occurrence += 1;
            all_observed.insert((fingerprint.0, fingerprint.1, *occurrence));
        }
        let contents = production_source(&contents).expect("Rust source must be inspectable");
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
        .filter(|site| !all_observed.contains(*site))
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
    let mut observed_macro_contexts = BTreeSet::new();

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
                "src/sandbox/worker/linux.rs"
                    | "src/sandbox/worker/macos.rs"
                    | "src/sandbox/spawner.rs"
            )
        {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        for call in terminal_calls(&contents).expect("Rust terminals must be discoverable") {
            if !call.guarded
                && let Some(context) = call.macro_context
            {
                observed_macro_contexts.insert((
                    relative.clone(),
                    context.digest,
                    context.occurrence,
                ));
            }
        }
        let contents = production_source(&contents).expect("Rust source must be inspectable");
        unguarded.extend(
            creation_boundary_violations(&relative, &contents, &expected, &macro_non_process)
                .unwrap_or_else(|error| panic!("could not inspect {relative}: {error}")),
        );
    }

    assert!(
        unguarded.is_empty(),
        "Windows-capable production process terminals bypass the crate creation boundary: {unguarded:#?}"
    );
    let stale: Vec<_> = macro_non_process
        .difference(&observed_macro_contexts)
        .collect();
    assert!(
        stale.is_empty(),
        "stale macro non-process exemptions: {stale:#?}"
    );
}

#[test]
fn process_creation_raw_terminals_are_exact_and_guard_dominated() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = std::fs::read_to_string(root.join("src/process_creation.rs"))
        .expect("process creation source must be readable");
    let expected = BTreeMap::from([
        (
            "output_std_once|std::process::Command::output(command)".to_string(),
            1,
        ),
        (
            "spawn_tokio_once|TokioCommand::spawn(command)".to_string(),
            1,
        ),
        (
            "spawn_guarded_until|TokioCommand::spawn(self)".to_string(),
            1,
        ),
        (
            "launch_tokio_with_creation_guard_until|let child = TokioCommand::spawn(command)?;"
                .to_string(),
            1,
        ),
        (
            "spawn_wrap_once|process_wrap::tokio::CommandWrap::spawn(command)".to_string(),
            1,
        ),
        (
            "spawn_std_once|std::process::Command::spawn(command)".to_string(),
            1,
        ),
        (
            "spawn_guarded_until|std::process::Command::spawn(self)".to_string(),
            1,
        ),
    ]);
    let exact_non_process = BTreeSet::new();

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

    let mut relabeled = checked_inventory();
    let site = relabeled
        .iter()
        .find(|((path, _, _), class)| {
            path == "src/agent/tools/bash.rs" && **class != "TC-LIFECYCLE-HELPER"
        })
        .map(|(site, _)| site.clone())
        .expect("bash inventory contains a non-lifecycle site");
    relabeled.insert(site, "TC-LIFECYCLE-HELPER");
    let error = validate_class_assignments(&relabeled)
        .expect_err("cross-family relabeling must be rejected");
    assert!(
        error.contains("cannot own"),
        "unexpected rejection: {error}"
    );

    for (source, wrong_class) in [
        (
            "let mut command = Command::new(image_path);",
            "TC-INTERNAL-VERIFICATION",
        ),
        (
            "scope.spawn(move || -> io::Result<_> {",
            "TC-BROKER-JS-WORKER",
        ),
    ] {
        let mut relabeled = checked_inventory();
        let site = (
            "src/sandbox/worker/macos.rs".to_owned(),
            source.to_owned(),
            1,
        );
        assert!(relabeled.insert(site, wrong_class).is_some());
        let error = validate_class_assignments(&relabeled)
            .expect_err("retained-image sites must preserve their exact ownership");
        assert!(
            error.contains("cannot own"),
            "unexpected rejection: {error}"
        );
    }
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
            "src/sandbox.rs",
            "let mut cmd = Command::new(zerobox);",
            1,
            "TC-LIFECYCLE-HELPER",
            "a model action in the mixed sandbox family",
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
            "let mut command = tokio::process::Command::new(\"lazygit\");",
            1,
            "TC-EXPLICIT-USER-SHELL",
            "the lazygit interactive utility launch",
        ),
        (
            "src/ui/app.rs",
            "let mut probe = tokio::process::Command::new(\"lazygit\");",
            1,
            "TC-EXPLICIT-USER-SHELL",
            "the lazygit version probe",
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

fn terminal_names(source: &str) -> Vec<String> {
    terminal_calls(source)
        .expect("fixture must tokenize")
        .into_iter()
        .map(|call| call.name)
        .collect()
}

#[test]
fn splice_free_macros_ignore_bare_identifiers_that_are_data() {
    // Variable, pattern and field spellings that merely look like terminals.
    let calls = terminal_names(
        r#"
fn inspect(status: Status, output: Output) {
    let ok = matches!(status.as_u16(), 429 | 529);
    assert_eq!(output, expected);
    assert!(output.status.success(), "{status}");
    let _ = format!("{}", status);
    tracing::info!(status = status.as_u16(), output = ?output);
    match (spawn, status) { (Some(spawn), status) if matches!(status, Ok(_)) => {} _ => {} }
    std::println!("{}", output);
}
"#,
    );
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn splice_free_macros_still_flag_real_process_calls() {
    // Method and path calls stay flagged however deeply a std macro wraps them.
    for fixture in [
        "fn l(c: &mut std::process::Command) { assert!(c.spawn().is_ok()); }",
        "fn l(c: &mut std::process::Command) { assert_eq!(c.status().unwrap().code(), Some(0)); }",
        "fn l(c: &mut std::process::Command) { let _ = format!(\"{:?}\", c.output()); }",
        "fn l(c: &mut std::process::Command) { matches!(c.status(), Ok(_)); }",
        "fn l(c: &mut std::process::Command) { let _ = vec![c.spawn()]; }",
        "async fn l(mut child: tokio::process::Child) { tokio::select! { r = child.status() => { drop(r); } } }",
        "fn l(c: &mut std::process::Command) { assert!(std::process::Command::spawn(c).is_ok()); }",
        "fn l(c: &mut std::process::Command) { assert!(matches!(c.spawn(), Ok(_))); }",
    ] {
        assert!(
            !terminal_names(fixture).is_empty(),
            "a real launch inside a splice-free macro was not flagged: {fixture}"
        );
    }
}

#[test]
fn splice_free_macros_do_not_vouch_for_macros_nested_inside_them() {
    // The nested macro is judged on its own name: an unknown or local macro can
    // splice the identifier into a method position.
    for fixture in [
        "fn l(c: &mut std::process::Command) { assert!(terminal!(c, spawn).is_ok()); }",
        "fn l(c: &mut std::process::Command) { matches!(run!(c, status), Ok(_)); }",
        "fn l(c: &mut std::process::Command) { assert_eq!(crate::terminal!(c, output), 1); }",
        "fn l(c: &mut std::process::Command) { assert_eq!(local::format!(c, spawn), 1); }",
        "fn l(c: &mut std::process::Command) { assert_eq!($crate::format!(c, spawn), 1); }",
    ] {
        assert_eq!(
            terminal_names(fixture).len(),
            1,
            "nested non-splice-free macro was exempted: {fixture}"
        );
    }
}

#[test]
fn splice_free_exemption_never_applies_inside_macro_rules_bodies() {
    let calls = terminal_names(
        r#"
macro_rules! launch {
    ($command:ident, $method:ident) => {
        assert!($command.$method().is_ok());
        matches!($method, spawn)
    };
}
"#,
    );
    assert!(!calls.is_empty());
}

#[test]
fn splice_free_macro_names_are_not_redefined_by_the_crate() {
    // The exemption is by macro name. If the crate ever defines a macro with one
    // of these names (or re-exports one under it) the name no longer proves the
    // macro cannot splice an identifier, so this fails until the list is revisited.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for source in rust_sources(&root.join("src")) {
        let relative = source
            .strip_prefix(root)
            .expect("source must be below manifest root")
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "src/tests/subprocess_inventory_tests.rs" {
            continue;
        }
        let contents = std::fs::read_to_string(&source).expect("Rust source must be UTF-8");
        let Ok(stream) = TokenStream::from_str(&contents) else {
            continue;
        };
        fn walk(stream: TokenStream, relative: &str, offenders: &mut Vec<String>) {
            let tokens: Vec<_> = stream.into_iter().collect();
            for (index, token) in tokens.iter().enumerate() {
                if let TokenTree::Group(group) = token {
                    walk(group.stream(), relative, offenders);
                }
                let TokenTree::Ident(ident) = token else {
                    continue;
                };
                let name = ident.to_string();
                let is_exempt_name = SPLICE_FREE_MACROS.contains(&name.as_str());
                let defines = index >= 2
                    && matches!(&tokens[index - 2], TokenTree::Ident(keyword) if keyword == "macro_rules")
                    && matches!(&tokens[index - 1], TokenTree::Punct(punct) if punct.as_char() == '!');
                let renames = index >= 1
                    && matches!(&tokens[index - 1], TokenTree::Ident(keyword) if keyword == "as");
                if is_exempt_name && (defines || renames) {
                    offenders.push(format!("{relative}:{} {name}", ident.span().start().line));
                }
            }
        }
        walk(stream, &relative, &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "macro names on the subprocess inventory's splice-free list were redefined or aliased: {offenders:#?}"
    );
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

#[test]
fn macro_non_process_identity_structurally_binds_exact_tokens() {
    fn context(source: &str) -> MacroContext {
        terminal_calls(source)
            .expect("fixture must parse and tokenize")
            .into_iter()
            .find_map(|call| call.macro_context)
            .expect("fixture terminal must have a macro context")
    }

    let relative = r#"
fn inspect(value: Value) {
    approved!(
        value,
        status,
        marker += value,
    );
}
"#;
    let absolute = relative.replace("    approved!", "    ::approved!");
    let relative_context = context(relative);
    let absolute_context = context(&absolute);
    assert_ne!(
        relative_context.digest, absolute_context.digest,
        "root qualification must be part of the macro identity"
    );

    let expected = BTreeMap::from([(
        ("src/fixture.rs".to_string(), "status,".to_string(), 1),
        "NON-PROCESS",
    )]);
    let approved = BTreeSet::from([(
        "src/fixture.rs".to_string(),
        relative_context.digest.clone(),
        relative_context.occurrence,
    )]);
    let violations =
        creation_boundary_violations("src/fixture.rs", &absolute, &expected, &approved)
            .expect("absolute reviewer probe must be inspectable");
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("unrecognized macro context"));

    let mutations = [
        relative.replace("    approved!", "    crate::approved!"),
        relative.replace("    approved!", "    self::approved!"),
        relative.replace("    approved!", "    super::approved!"),
        relative.replace("    approved!", "    r#approved!"),
        relative.replace("marker += value", "marker + = value"),
        relative
            .replace("approved!(", "approved!{")
            .replace("    );", "    };")
            .to_string(),
        relative
            .replace("approved!(", "approved![")
            .replace("    );", "    ];")
            .to_string(),
    ];
    let mut identities = BTreeSet::from([relative_context.digest]);
    for mutation in mutations {
        assert!(
            identities.insert(context(&mutation).digest),
            "path, raw spelling, punctuation spacing, and delimiter mutations must not collide"
        );
    }
}
