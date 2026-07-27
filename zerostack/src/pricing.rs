/// Anthropic prompt-cache billing multipliers, relative to the base input rate.
/// Anthropic bills cache *writes* (`cache_creation_input_tokens`) at 1.25× and
/// cache *reads* (`cached_input_tokens`) at 0.10× the base input token price.
/// These ratios are fixed across Anthropic models, so we keep them as constants
/// rather than carrying per-model cache rates in the catalog (`data/models.json`
/// only has base input/output prices). WRITE assumes the default 5-minute cache
/// TTL; the 1-hour TTL bills writes at 2.0× — revisit if that is ever enabled
/// (see `provider.rs`'s `.with_prompt_caching()`).
const ANTHROPIC_CACHE_WRITE_MULT: f64 = 1.25;
const ANTHROPIC_CACHE_READ_MULT: f64 = 0.10;

/// Effective (billable) input-token count, folding Anthropic's cache tiers into
/// a single number priced at the base input rate. The cost-side sibling of
/// [`Session::real_input_tokens`](crate::session::Session::real_input_tokens):
/// where that one answers "how big is the context" (for compaction), this
/// answers "how much input do we pay for".
///
/// Anthropic's reported `input_tokens` *excludes* both cached and cache-creation
/// tokens, but both are billed — reads at 0.10× and writes at 1.25× — so pricing
/// on raw `input_tokens` alone undercounts massively whenever caching is active
/// (the fixed request prefix — tool defs + system prompt — is re-sent every turn
/// and lands almost entirely in the cache tiers). Every other provider folds the
/// cached subset into `input_tokens` and reports no separate cache-creation, so
/// this is a no-op for them.
///
/// `anthropic_native` must be the resolved protocol route — compute it with
/// [`Config::is_anthropic_native`](crate::config::Config::is_anthropic_native),
/// exactly as `real_input_tokens` callers do.
pub fn billable_input_tokens(
    anthropic_native: bool,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> u64 {
    if anthropic_native {
        (input_tokens as f64
            + cache_creation_input_tokens as f64 * ANTHROPIC_CACHE_WRITE_MULT
            + cached_input_tokens as f64 * ANTHROPIC_CACHE_READ_MULT)
            .round() as u64
    } else {
        input_tokens
    }
}

/// Estimate cost for given token counts and per-million-token prices.
/// Returns 0.0 if either price is 0.0.
///
/// `input_tokens` is the *billable* input count — for Anthropic routes, pass the
/// output of [`billable_input_tokens`] so cache reads/writes are priced, not the
/// raw provider-reported `input_tokens` (which excludes both cache tiers).
pub fn estimate_cost(
    input_tokens: u64,
    output_tokens: u64,
    input_token_cost: f64,
    output_token_cost: f64,
) -> f64 {
    let input_cost = input_tokens as f64 * input_token_cost / 1_000_000.0;
    let output_cost = output_tokens as f64 * output_token_cost / 1_000_000.0;
    input_cost + output_cost
}
