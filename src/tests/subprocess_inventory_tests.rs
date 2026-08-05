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
    (
        "src/agent/runner.rs",
        "std::mem::drop(self.runtime.spawn(async move {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/runner.rs",
        "let hook_is_live = std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    ("src/agent/runner.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/acp/mod.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/acp/mod.rs",
        "!std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    ("src/extras/acp/mod.rs", ".status()", 2, "TEST-ONLY"),
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
        "let mut command = Command::new(&self.program);",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/extras/git_worktree/mod.rs",
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
        ".with_validated_root(&project_dir, || cmd.spawn())",
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
        "src/extras/js/engine.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(zerobox);",
        1,
        "TC-PROJECT-AUTOMATION",
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
        "src/extras/js/host.rs",
        "assert_eq!(output.status, CommandStatus::Completed);",
        2,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "assert_eq!(output.stdout, b\"approved\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "assert_eq!(output.stdout, b\"elf-snapshot\");",
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
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
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
        "let mut command = Command::new(&executable);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
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
        "let output = Command::new(executable)",
        1,
        "TC-BROKER-JS-WORKER",
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
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "TEST-ONLY",
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
    (
        "src/extras/mcp/client.rs",
        "let mut child = Command::new(program);",
        1,
        "TC-MCP-STDIO",
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
    ("src/sandbox/worker/windows.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/windows.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/windows.rs",
        "Command::new(executable)",
        1,
        "TEST-ONLY",
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
    (
        "src/sandbox/windows.rs",
        ") -> Result<tokio::process::Command, String> {",
        6,
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
        ".output()",
        6,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn()",
        7,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "if !Command::new(tool)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = Command::new(",
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
        "let descendant = Command::new(executable)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut breakaway = Command::new(",
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
        "src/sandbox/windows.rs",
        ".status()",
        2,
        "TC-INTERNAL-VERIFICATION",
    ),
    ("src/session/mod.rs", ".output()", 1, "TC-INTERNAL-GIT"),
    (
        "src/session/mod.rs",
        "let out = std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
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
        "src/ui/mod.rs",
        "let mut command = std::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/mod.rs",
        "std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/ui/mod.rs",
        "std::process::Command::new(shell)",
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
];

/// Exact macro-token fingerprints whose terminal spelling is data rather than
/// a process method. Any unlisted `spawn`, `output`, or `status` identifier in
/// macro-controlled tokens remains process authority and fails closed.
const MACRO_IDENTIFIER_NON_PROCESS_SITES: &[(&str, &str, usize, &str)] = &[
    (
        "src/agent/runner.rs",
        "assert_eq!(output, UNKNOWN_TOOL_OUTCOME);",
        2,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(output.contains(\"No files found\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/find_files.rs",
        "assert!(!output.contains(\"must_not_be_returned.txt\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(output.contains(\"No matches found\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/agent/tools/grep.rs",
        "assert!(!output.contains(\"must_not_be_returned\"));",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "output: \"b-result\".into(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        "output: \"a-result\".into(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/acp/mod.rs",
        ".is_ok_and(|status| status.success()),",
        1,
        "NON-PROCESS",
    ),
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
    (
        "src/sandbox/windows.rs",
        "String::from_utf8_lossy(&output.stderr)",
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
        "src/extras/hooks/dispatcher.rs",
        "status = ?output.status,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        "containment = output.diagnostics.containment,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        "environment = output.diagnostics.environment,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        "filesystem = output.diagnostics.filesystem,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        "network = output.diagnostics.network,",
        1,
        "NON-PROCESS",
    ),
    ("src/sandbox.rs", "status.success(),", 1, "NON-PROCESS"),
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
    (
        "src/sandbox.rs",
        "output.stdout.is_empty(),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        "outcome = ?output.status,",
        1,
        "NON-PROCESS",
    ),
    (
        "src/ui/app.rs",
        "&format!(\"warning: lazygit ended with {:?}\", output.status),",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox.rs",
        "status = child.wait() => CommandTermination::Exited(status),",
        2,
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

/// SHA-256 of the length-framed structural macro path and complete token tree.
/// Token kinds, root qualification, raw identifier spelling, punctuation
/// spacing, and every nested delimiter are identity-bearing.
/// Occurrence counts prevent an identical invocation from borrowing an earlier
/// approval in the same source file.
const MACRO_NON_PROCESS_CONTEXTS: &[(&str, &[(&str, usize)])] = &[
    (
        "src/agent/runner.rs",
        &[
            (
                "171453e5b2606177e62ffc589caee645490fd18305ae366b007a993e16b669db",
                1,
            ),
            (
                "2e9d6fe2b541c8535dfabf335779690424c0bb5bb198ff7429dbaede8642b50f",
                1,
            ),
            (
                "9b95c26fba2e0e1c67f90565d197425d7c3bdecbbe365dd0c071de7130a7eafb",
                1,
            ),
            (
                "a65ff78b30cd28567d54a354081c187246782a6f49f314a7d5ce6289ebdc97f3",
                1,
            ),
            (
                "c7984bb9ee969797484f192f11aa877a68f90f5b456d5d2ee40a2a7e7f834fbd",
                1,
            ),
            (
                "225002167cc20f4e6df35ecd65a6c50e007af4b1cfdcb530eb6a48de70d02689",
                2,
            ),
        ],
    ),
    (
        "src/agent/tools/bash.rs",
        &[
            (
                "11590494b23b272022bf8aa154b2fcfb865b05cc25efb8a449623f43dbd184c8",
                1,
            ),
            (
                "1a4d10559a457b95bab2fbea7dcd65fefebe980c444cd00e9de9a60d1ece599d",
                1,
            ),
            (
                "7340793918840ff0a06131325b389589d2873616472f19c449062b5be4bc1380",
                1,
            ),
            (
                "761e9d94916bbe3a5d5de4cbbb2d1ceb08208ec28b457cd8d41fbac9e1f8a6fe",
                1,
            ),
            (
                "ad2596fb4e92d03c6e11f4c1aa11829076d1156dd173cc7b094036343a52b445",
                1,
            ),
            (
                "add018d80d10cb6a3d585ccb97bcdbe26bd63ccec9cca4519b6075fa172c2eb8",
                1,
            ),
            (
                "bb696e69e39aa81f50ef752ce4bc1564021298890472dce79dedaf55263346f6",
                1,
            ),
            (
                "d70f631d43aa86b91b591554981d4ba8cb2a723cd349bc1742f6a6a2e2f2fd54",
                1,
            ),
            (
                "dacb7feff2f8401db6fff30373bb92be0bc1bdfa88d247cabcef3edd4d93d16c",
                1,
            ),
            (
                "f67e03a1d8c8573ffae016bbaf134c13aa8d999566a8ed47610219e3e5507457",
                1,
            ),
        ],
    ),
    (
        "src/agent/tools/find_files.rs",
        &[
            (
                "115510326f281b766408701156e3b61bd28b08fb8a83e1a83ae95034bfab337e",
                1,
            ),
            (
                "205b255efe38d50d4e3ffd4840977d3744af25b4be13bacd284867d626d6eea0",
                1,
            ),
            (
                "44b3168425ceb3c6808eea846f357f1369ebc7da14a5bddef0ed7090e353dafb",
                1,
            ),
            (
                "55753c0c181d8223928b02a8bb73ee136fcf855df473a0c9e2e7662d79f4f026",
                1,
            ),
            (
                "56e99f3288d12b12f3643fdb1a5a4e56eddec6c5ce068190a6056d9fd6e7412e",
                1,
            ),
            (
                "645122c3dee305566d960b4846b998a32de6fdaeac386e21e3d0e19e644409d7",
                1,
            ),
            (
                "94179e11a3f89f5be978ecad4f61da60c1d2e431b78531090945f40b629d2541",
                1,
            ),
            (
                "ae8fedde9b6e8bece82821b0de38178334b0c1ca387c872260b80d2e138c6e06",
                2,
            ),
            (
                "f25fa463789bdff96f2ac1db892cb80aa2acf858cae6fce6207f6faa70bc85b8",
                1,
            ),
            (
                "bf75191f6049b49ba3066fe7c1afe9dff0a585c144d3231ac53532058b16f0a7",
                1,
            ),
            (
                "3462a253774a87b69c35808079213817cbb63784594c3b6c36e1cf0d433edd76",
                1,
            ),
        ],
    ),
    (
        "src/agent/tools/grep.rs",
        &[
            (
                "0a94ede10a0163a59731cf2f78ca877c9139de14b8b94e4daa8d72347ddaadd0",
                1,
            ),
            (
                "0acbebee88d292c8ec6ee41a10395563fb9ffe96c71dbc580dbdc484b64dcea6",
                1,
            ),
            (
                "174b3659662ea4f4bcc7d2f7de1f632e16b959c70fd2f7e186199bfe8474a639",
                1,
            ),
            (
                "21cc41b46381ab04832f9a6c9f3d598ed2fe4d747d481237f990b7158c52a58a",
                1,
            ),
            (
                "371266118d5513014528466d57966ed6c1909cb33bab1a9fd302e5c6256ad82b",
                1,
            ),
            (
                "99092c71e9e39b0963c34a3241c195133962a8db5ec43f09bb6cd66917665b9b",
                1,
            ),
            (
                "ae8fedde9b6e8bece82821b0de38178334b0c1ca387c872260b80d2e138c6e06",
                2,
            ),
            (
                "ddc400f7816d0659ba5fe6e89b565e7e22473e6551c6c5be7a2ee53dca51c0ab",
                1,
            ),
            (
                "41db4c0ce2690c675124ac22b2e8c8e42e31dc78f69918c069d5683fdb587955",
                1,
            ),
            (
                "f9bb10578ae06a1346990778eb26b16f15fbed1f8f35faef03215060ce6723e5",
                1,
            ),
        ],
    ),
    (
        "src/extras/acp/mod.rs",
        &[
            (
                "295618629907043194c80029881ca6c0c7fd6c758a98c278f3d845cdf7e45f7f",
                1,
            ),
            (
                "4a380a581094e084f73541311b41c717d6b4aff6302b66a691885543bc79a92b",
                1,
            ),
            (
                "458562ae8615a7b8f5df26c3c5b4fb1f20bd7bb2c54191d9011ed73396156315",
                1,
            ),
        ],
    ),
    (
        "src/extras/export.rs",
        &[(
            "2642f045409085bb7c6fe9d020dad6e7a3f2915275c0c3686ac43710af88e0bc",
            1,
        )],
    ),
    (
        "src/extras/js/audit.rs",
        &[(
            "01ea14b48b35ce7c8b962bcb1ba243817f0a63f230d414850118cee9fed9f6cc",
            1,
        )],
    ),
    (
        "src/extras/js/host.rs",
        &[
            (
                "04c86182a5a99a0f160452606423562347d2b4f8bb1aad73f1651fee6fdc98bf",
                1,
            ),
            (
                "0397b840693f6b293544b512fce5572f9d450833533087168f099b3af77a5cb9",
                1,
            ),
            (
                "1b71d7d643b2f7208bbb297fc64a33265b776a2b8c934c32441cc124083abcd4",
                1,
            ),
            (
                "2f073adf370dfed835a37ffd305529ca895486178a4033f395abb56386e6cb63",
                1,
            ),
            (
                "2af98358d4d0cc429b3ce4d3d0c3e5c8b100dd2d15a699dceef636c9092c420f",
                1,
            ),
            (
                "2990b99216f07cf0bf799c8742347e406c9d3f08ec875af2101bfdf3182519c7",
                2,
            ),
            (
                "4b4be55dac813da63579ff80d76a858484598b955c750180378e47acac2ec2b1",
                1,
            ),
            (
                "5190609317dbab79fde23dab3907dc07258c750dd05ff75ec44868499bf4bcff",
                1,
            ),
            (
                "7517f2b69b6a018823611af16ad46cf509fd34b973baa82547d55ace719d28ca",
                2,
            ),
            (
                "8198bc7a5a750904ebdbef0a4b5a9371aaae8034a46471b5ce5a66e448c5b4e9",
                1,
            ),
            (
                "891a8bf5535d2f446d9d641b9d740ab7042ca41a7408f9ab36d76624b10cb05e",
                1,
            ),
            (
                "a2934036f2abb93e08faec85d7d7c126860a0fda0a8d26e879dd0f45ed38ac42",
                1,
            ),
            (
                "e44a0994c1278803b6647fefeb9a3c5bfeecf526b1c19fcc05d8fdb2a174d406",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/worker.rs",
        &[(
            "2ae1572a8e3e684cff68f2fdcdb44d0c5ee3808f1bb261e9b1818446ee024674",
            1,
        )],
    ),
    (
        "src/extras/js/skills/capability.rs",
        &[(
            "20ff2e59a38df1c73b89cf24447c44d4ad82d7a0ebeaf8b89a8cf70929c78661",
            1,
        )],
    ),
    (
        "src/extras/js/skills/store.rs",
        &[
            (
                "24f6070b5204f83ee8d5fc7a6bd756f7991bbe7fca840e62752f54e090661491",
                1,
            ),
            (
                "37b1d55d03713d423bc79e2bdfc046830486adddb9997708182cbd6434852b94",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/skills/telemetry.rs",
        &[(
            "060c076e1ddfc773151d0c424432a12ad46d0d66f893b7ef6c39b5070b3a73b5",
            1,
        )],
    ),
    (
        "src/extras/js/skills/turn.rs",
        &[
            (
                "09e954887c1888a296255a04fc1d10ba7806e8fc67dbd9d9b8a34d53a537dc5d",
                1,
            ),
            (
                "28e6f4ffe8d995be8ac3f65f2da51fc77225808714039ef01a22669ad78e1143",
                1,
            ),
            (
                "3fc20275d0f702215459b51e6b690a67ad684f7bd750513503d007b6fe8635ac",
                1,
            ),
            (
                "512f23c6f08f33c0ac29a8dee50052a6e6095547c7cad697b269fe69e02bff5a",
                1,
            ),
            (
                "53b45424ea372be94559be65c6f1ac40932431c7093e9cfe824d256dc55454ec",
                1,
            ),
            (
                "5eeb4ebba574fc8359881819f6b7d27f879be4b7a64a5993cc4b89429e7d5831",
                1,
            ),
            (
                "90ca522aa88240dba89634b772026c3cddd99b27e54673ff13874f2462c73f4d",
                1,
            ),
            (
                "a3ed78f809040e7856ccd224f3040f2a334937ea2702a6aef1f0584bba41d417",
                1,
            ),
            (
                "b3292455b91810203a439b84d52e261c6838155a4d035a19eb84fbd763610159",
                1,
            ),
            (
                "bc4fb0706aaa17dcb6d23c47773435403f4660ebf1230b57349ea16e489978c3",
                1,
            ),
            (
                "bd88f4885b162d21bd6c9c9819b78cba7017fda84d546275d93eb40ee3f05037",
                1,
            ),
            (
                "c9126084bcc36b3b03f6bcfb1a0fe17fc36b65edcb75386d9dcfefc20954df9d",
                1,
            ),
            (
                "d4ee4a31960023c4dca62084241f8e7e0e63866d5367a3c02886b4c0a29bb428",
                1,
            ),
            (
                "dac84961f14ecf95f7c7977967ca0956483de20be39a59e3e535972c86df536d",
                1,
            ),
            (
                "e134a67930eed0c8e0c1a02bcac6efda89c87f216b2f1a438a90737d63907acb",
                1,
            ),
            (
                "f4faf0ed27164d65181a5963d363f1eaef80f3ed8d260e6e0aef509bfbbdbe31",
                1,
            ),
        ],
    ),
    (
        "src/extras/js/supervisor.rs",
        &[(
            "e30de734188566fa21d676314e3cff927ee853afaa7e694f5f33d20ecaa58413",
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
    (
        "src/extras/subagents/task_tool.rs",
        &[
            (
                "5b355171d273f77d7a74c57959ccae1ff1c7f4e15189cc8f15c170bf3be44c3b",
                1,
            ),
            (
                "6884bc264c3021e29f258708b6c9047a22a2d755048dd069b549824131e86d7a",
                1,
            ),
            (
                "7308fdc8eff948ba4a55b0e6e5bc4490cd09ba72db71d7553393f8ec71aa2da4",
                1,
            ),
            (
                "e43f6b1a9e960b4dbeb45d38462dcdee2075a1a7dbb5bee387bf5fc8ad238280",
                1,
            ),
            (
                "ed11a97886914baeefe43d2157c89851a90e5f5bb1af6d4d0d4545046d5c8fc9",
                1,
            ),
        ],
    ),
    (
        "src/print.rs",
        &[
            (
                "7754ad9d4a4920c203d43d087fc405e95102f7a4801855582533ab54e3a86b86",
                1,
            ),
            (
                "8a4d18f5eab2de9d10ef609d4c51cef612fd6652674c5ee503fb77a75a66cc1a",
                1,
            ),
            (
                "a9b0b5cf240a3b7a378fc6c6dedd61ea1555a7bd03bcd115629965b05dd8d7fc",
                1,
            ),
        ],
    ),
    (
        "src/extras/hooks/dispatcher.rs",
        &[(
            "4455fff110b3f344a84e2da4538deb7323313eafb4e100e51a659498ebf49077",
            1,
        )],
    ),
    (
        "src/sandbox.rs",
        &[
            (
                "b9ba7b946e716954ba269b40394f014187892aff4dbe028a7cffda9ae5c63bea",
                1,
            ),
            (
                "77288e0715e2e881aa2f70d0c79e2dc255ad6bb1cb187ddef46359df73a23762",
                1,
            ),
            (
                "21bedd3fe703d7764c0026616b47d5daf97fbfa36447f47f26a10b5cfefd84b4",
                1,
            ),
            (
                "4e84ceaa1caa195958b88ed22d26b6d1e789ca8417fe59629074614a4e7100b8",
                1,
            ),
            (
                "5940f63c8bdc3c394d8e54c7b6deba8e8529d0a5b921c897cf45a23f4d94f94b",
                1,
            ),
            (
                "6e4cd00c8c8dced3ba1dfe6bfdf749624335b102a8b952c6bc33430cadd913df",
                1,
            ),
            (
                "7109de400e0c4f3dadd8116f47f0a2974119a18a8d7488093158d54d032b4798",
                1,
            ),
            (
                "87927df2c7d794eca3102771a06a4b3da45f0924beeb7715e6bd2fd0c917a654",
                1,
            ),
            (
                "8b1974fee65ca7148d7b05d7e2a5e68f3c46a111c04dfeb6bebf139a9c0ee5ed",
                1,
            ),
            (
                "8bf1360ab3a1b48ffbec38dcab906b7c7b5e8681952a391bf1323aca036db123",
                2,
            ),
            (
                "8de02a606709e72b294f7747449b50f5cd1b28d5ee73898fac28d59e83d6d77b",
                1,
            ),
            (
                "8f720ce431f239fd3832c71566aa0b9e7b4320c2183f6760d7385047da7fba0a",
                1,
            ),
            (
                "b27051c5dbe7a008d67062f63095b37014ef1b88f4f1f0180d289237271cab70",
                1,
            ),
        ],
    ),
    (
        "src/ui/app.rs",
        &[
            (
                "842d29cd63634e7d8359ed65fd83710f1b986f46e826f81235b6c5a6c07e71c4",
                1,
            ),
            (
                "b7af79d5aa59730634d98c20d3fec794b65f77a4c75b6d6e476c8fbe491a0a8d",
                1,
            ),
            (
                "efbf1e616edc14fac2436cfd32c40c1a31fa90cb5f2516dad7ab725a76e5d4d4",
                1,
            ),
        ],
    ),
    (
        "src/ui/renderer.rs",
        &[(
            "f915b7e6296aa7d6023439adbe8a3ba928a784a87d51c606c30480b43ef54183",
            2,
        )],
    ),
    (
        "src/ui/slash/features.rs",
        &[(
            "e59a3142b34d4535d362822c3bf7afc7f4284fda8c3ceec063a0ddfba3296ea1",
            1,
        )],
    ),
    (
        "src/sandbox/windows.rs",
        &[(
            "48b38d261851e6e3c7bb387f8fb5d093448cb69f8047816907f7b7dc699ca584",
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
        "src/sandbox/worker/macos.rs",
        ".output()",
        &["TC-BROKER-JS-WORKER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn()",
        &["TC-BROKER-JS-WORKER", "TEST-ONLY"],
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        &["TC-LIFECYCLE-HELPER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        &["TC-MODEL-ACTION", "TC-MCP-STDIO"],
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
    (
        "src/ui/mod.rs",
        ".output()",
        &["TC-EXPLICIT-USER-SHELL", "TC-INTERNAL-GIT"],
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
    "TC-PROJECT-AUTOMATION",
    "TC-SUPPORT-UTILITY",
];

/// Exact ownership for every lexical disposition and every site in a source
/// file that contains more than one production trust class.
const EXACT_UNIFORM_SITE_CLASSES: &[(&str, &str, usize, &str)] = &[
    (
        "src/sandbox/worker/macos.rs",
        "let output = std::process::Command::new(SW_VERS)",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut command = Command::new(&executable);",
        1,
        "TC-BROKER-JS-WORKER",
    ),
    (
        "src/sandbox/worker/macos.rs",
        "let mut child = command.spawn().map_err(|source| WorkerLaunchError::Io {",
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
        "let output = Command::new(executable)",
        1,
        "TC-BROKER-JS-WORKER",
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
    (
        "src/agent/runner.rs",
        "let hook_is_live = std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    ("src/agent/runner.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/acp/mod.rs",
        "std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/acp/mod.rs",
        "!std::process::Command::new(\"kill\")",
        1,
        "TEST-ONLY",
    ),
    ("src/extras/acp/mod.rs", ".status()", 2, "TEST-ONLY"),
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
        "src/sandbox.rs",
        "let mut cmd = Command::new(zerobox);",
        1,
        "TC-PROJECT-AUTOMATION",
    ),
    (
        "src/extras/js/host.rs",
        "assert_eq!(output.status, CommandStatus::Completed);",
        2,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "assert_eq!(output.stdout, b\"approved\");",
        1,
        "NON-PROCESS",
    ),
    (
        "src/extras/js/host.rs",
        "assert_eq!(output.stdout, b\"elf-snapshot\");",
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
    ("src/extras/loop/validation.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/extras/loop/validation.rs",
        "assert!(!headless.contains(\"tokio::process::Command::new\"));",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/loop/validation.rs",
        "assert!(!interactive.contains(\"tokio::process::Command::new\"));",
        1,
        "TEST-ONLY",
    ),
    (
        "src/extras/loop/validation.rs",
        "std::process::Command::new(\"/bin/kill\")",
        1,
        "TEST-ONLY",
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
        "src/extras/js/tool.rs",
        "requests.spawn(async move {",
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
    ("src/sandbox/worker/windows.rs", ".status()", 1, "TEST-ONLY"),
    (
        "src/sandbox/worker/windows.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/worker/windows.rs",
        "Command::new(executable)",
        1,
        "TEST-ONLY",
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
    (
        "src/sandbox/windows.rs",
        ") -> Result<tokio::process::Command, String> {",
        6,
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
        ".output()",
        6,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn()",
        7,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        ".spawn(move || {",
        1,
        "NON-PROCESS",
    ),
    (
        "src/sandbox/windows.rs",
        "if !Command::new(tool)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut child = Command::new(",
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
        "let descendant = Command::new(executable)",
        1,
        "TC-INTERNAL-VERIFICATION",
    ),
    (
        "src/sandbox/windows.rs",
        "let mut breakaway = Command::new(",
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
        "src/sandbox/windows.rs",
        ".status()",
        2,
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
        "src/ui/mod.rs",
        "let mut command = std::process::Command::new(\"lazygit\");",
        1,
        "TC-SUPPORT-UTILITY",
    ),
    (
        "src/ui/mod.rs",
        "std::process::Command::new(\"git\")",
        1,
        "TC-INTERNAL-GIT",
    ),
    (
        "src/ui/mod.rs",
        "std::process::Command::new(shell)",
        1,
        "TC-EXPLICIT-USER-SHELL",
    ),
];

const EXACT_MIXED_SITE_CLASSES: &[(&str, &str, &[&str])] = &[
    (
        "src/sandbox/worker/macos.rs",
        ".output()",
        &["TC-BROKER-JS-WORKER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox/worker/macos.rs",
        ".spawn()",
        &["TC-BROKER-JS-WORKER", "TEST-ONLY"],
    ),
    (
        "src/sandbox.rs",
        "let mut child = match cmd.spawn() {",
        &["TC-LIFECYCLE-HELPER", "TC-SUPPORT-UTILITY"],
    ),
    (
        "src/sandbox.rs",
        "let mut cmd = Command::new(\"zerobox\");",
        &["TC-MODEL-ACTION", "TC-MCP-STDIO"],
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
    (
        "src/ui/mod.rs",
        ".output()",
        &["TC-EXPLICIT-USER-SHELL", "TC-INTERNAL-GIT"],
    ),
];

/// Files whose non-disposition launch expressions all have one owner.
const SINGLE_CLASS_FAMILIES: &[(&str, &str)] = &[
    ("src/docs.rs", "TC-SUPPORT-UTILITY"),
    ("src/extras/git_worktree/mod.rs", "TC-INTERNAL-GIT"),
    ("src/extras/hooks/subprocess.rs", "TC-PROJECT-AUTOMATION"),
    ("src/extras/loop/mod.rs", "TC-INTERNAL-VERIFICATION"),
    ("src/extras/lsp/client.rs", "TC-LSP-SERVICE"),
    ("src/extras/mcp/client.rs", "TC-MCP-STDIO"),
    ("src/session/mod.rs", "TC-INTERNAL-GIT"),
    ("src/ui/input/mod.rs", "TC-SUPPORT-UTILITY"),
    ("src/ui/renderer.rs", "TC-SUPPORT-UTILITY"),
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
            "spawn_guarded|process_wrap::tokio::CommandWrap::spawn(self)".to_string(),
            1,
        ),
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
            "7731da00f43136911c6dc28ad641ac88c4af40639bd4738168ad235d98d9effb".to_string(),
            1,
        ),
        (
            "793b5bb0809fd91c3813d13713e2e3cf9cc19325c876bea759bc1631905cb673".to_string(),
            1,
        ),
        (
            "bfb1f62966ac90ca4e21d0f6749754af021b8f31716fb4396b11c1298ef735f4".to_string(),
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
            "src/sandbox.rs",
            "let mut cmd = Command::new(\"zerobox\");",
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
        (
            "src/ui/mod.rs",
            ".output()",
            1,
            "TC-SUPPORT-UTILITY",
            "the explicit shell output occurrence in the mixed UI family",
        ),
        (
            "src/ui/mod.rs",
            "std::process::Command::new(\"git\")",
            1,
            "TC-EXPLICIT-USER-SHELL",
            "the internal Git launch in the mixed UI family",
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
