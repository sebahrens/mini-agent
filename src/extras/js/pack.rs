//! Lazily evaluated pack of pure helpers plus the `help(name?)` global.
//!
//! The pack is a list of SEGMENTS (one JS file per topic, each with its own `//@ ` catalogue
//! lines); `pack_boot.js` installs `help` and one configurable getter per catalogued name. The
//! first access of a name evaluates only ITS segment, once, and the getters of that segment are
//! replaced by the real functions, so `typeof name` is `'function'` and an unused segment costs
//! only its catalogue. Adding a helper means adding a segment file and one line to `SEGMENTS`.

use rquickjs::{Ctx, Function};

/// One entry per topic file. Add new segments here (each file carries its own catalogue).
const SEGMENTS: &[&str] = &[
    include_str!("pack_helpers.js"),
    include_str!("pack_xml.js"),
    include_str!("pack_bpmn.js"),
    include_str!("pack_bpmn_outline.js"),
    include_str!("pack_diff.js"),
    include_str!("pack_iac.js"),
    include_str!("pack_path_stats.js"),
    include_str!("pack_semver.js"),
    include_str!("pack_yaml.js"),
];
const PACK_BOOT_JS: &str = include_str!("pack_boot.js");

/// Catalogue entries `[name, signature, purpose, usage]` from the `//@ ` header lines.
fn catalog(source: &str) -> Vec<Vec<String>> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix("//@ "))
        .map(|entry| entry.splitn(4, '|').map(|f| f.trim().to_string()).collect())
        .filter(|fields: &Vec<String>| fields.len() == 4 && !fields[0].is_empty())
        .collect()
}

/// Install `help` and the lazy getters for the first-party pack (both effect profiles).
pub(super) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    install_sources(ctx, SEGMENTS)
}

#[cfg(test)]
fn install_source(ctx: &Ctx<'_>, source: &'static str) -> rquickjs::Result<()> {
    install_sources(ctx, &[source])
}

fn install_sources(ctx: &Ctx<'_>, sources: &[&'static str]) -> rquickjs::Result<()> {
    let boot: Function = ctx.eval(PACK_BOOT_JS)?;
    let segments = rquickjs::Array::new(ctx.clone())?;
    for (index, source) in sources.iter().copied().enumerate() {
        let load = Function::new(ctx.clone(), move |ctx: Ctx<'_>| ctx.eval::<(), _>(source))?;
        let segment = rquickjs::Array::new(ctx.clone())?;
        segment.set(0, catalog(source))?;
        segment.set(1, load)?;
        segments.set(index, segment)?;
    }
    boot.call::<_, ()>((ctx.globals(), segments))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "//@ demo_echo | demo_echo(x) | returns x | Example: demo_echo(1)\n\
        //@ demo_twice | demo_twice(x) | doubles x | Example: demo_twice(2)\n\
        globalThis.__loads = (globalThis.__loads || 0) + 1;\n\
        Object.defineProperty(globalThis, 'demo_echo', {value: (x) => x, writable: true, configurable: true});\n\
        Object.defineProperty(globalThis, 'demo_twice', {value: (x) => x * 2, writable: true, configurable: true});";

    fn eval(ctx: &Ctx<'_>, code: &str) -> String {
        ctx.eval::<String, _>(code).unwrap()
    }

    fn with_pack(source: &'static str, body: impl FnOnce(&Ctx<'_>)) {
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            install_source(&ctx, source).unwrap();
            body(&ctx);
        });
    }

    #[test]
    fn first_party_pack_catalogue_is_well_formed() {
        let mut names = Vec::new();
        for source in SEGMENTS {
            for line in source.lines().filter(|l| l.starts_with("//@ ")) {
                assert_eq!(line.matches('|').count(), 3, "{line}");
            }
            names.extend(catalog(source).into_iter().map(|e| e[0].clone()));
        }
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            names.len(),
            unique.len(),
            "duplicate pack names across segments"
        );
    }

    #[test]
    fn segments_load_independently_and_only_on_first_access() {
        const A: &str = "//@ seg_a | seg_a(x) | a | Example: seg_a(1)\n\
            globalThis.__a = (globalThis.__a || 0) + 1;\n\
            Object.defineProperty(globalThis, 'seg_a', {value: (x) => x + 1, writable: true, configurable: true});";
        const B: &str = "//@ seg_b | seg_b(x) | b | Example: seg_b(1)\n\
            globalThis.__b = (globalThis.__b || 0) + 1;\n\
            Object.defineProperty(globalThis, 'seg_b', {value: (x) => x + 2, writable: true, configurable: true});";
        let runtime = rquickjs::Runtime::new().unwrap();
        let context = rquickjs::Context::full(&runtime).unwrap();
        context.with(|ctx| {
            install_sources(&ctx, &[A, B]).unwrap();
            assert_eq!(eval(&ctx, "String(seg_a(1))"), "2");
            assert_eq!(
                eval(&ctx, "String(typeof __a) + String(typeof __b)"),
                "numberundefined"
            );
            assert_eq!(eval(&ctx, "String(seg_b(1))"), "3");
            assert_eq!(eval(&ctx, "String(__a) + String(__b)"), "11");
            assert!(
                eval(&ctx, "help()").contains("seg_a") && eval(&ctx, "help()").contains("seg_b")
            );
        });
    }

    #[test]
    fn pack_loads_lazily_once_and_replaces_its_getters() {
        with_pack(FIXTURE, |ctx| {
            assert_eq!(eval(ctx, "String(globalThis.__loads)"), "undefined");
            let list = eval(ctx, "help()");
            assert!(list.contains("demo_echo(x) - returns x"), "{list}");
            assert!(list.contains("demo_twice(x) - doubles x"), "{list}");
            assert!(eval(ctx, "help('demo_twice')").ends_with("Example: demo_twice(2)"));
            assert_eq!(eval(ctx, "String(globalThis.__loads)"), "undefined");
            assert_eq!(eval(ctx, "typeof demo_echo"), "function");
            assert_eq!(eval(ctx, "String(globalThis.__loads)"), "1");
            assert_eq!(
                eval(ctx, "String(demo_twice(4)) + typeof demo_twice"),
                "8function"
            );
            assert_eq!(eval(ctx, "String(globalThis.__loads)"), "1");
            let d = "JSON.stringify(Object.getOwnPropertyDescriptor(globalThis, 'demo_echo'))";
            assert!(eval(ctx, d).contains("\"configurable\":true"));
            let bad = eval(ctx, "try { help('nope') } catch (e) { e.message }");
            assert!(bad.contains("unknown helper 'nope'"), "{bad}");
        });
    }

    #[test]
    fn assigning_a_pack_name_overrides_it_and_enumeration_does_not_load() {
        with_pack(FIXTURE, |ctx| {
            assert_eq!(
                eval(
                    ctx,
                    "JSON.stringify(Object.keys(globalThis).filter(k => k.startsWith('demo')))"
                ),
                "[]"
            );
            assert_eq!(eval(ctx, "String(globalThis.__loads)"), "undefined");
            assert_eq!(eval(ctx, "demo_echo = () => 'mine'; demo_echo()"), "mine");
        });
    }

    #[test]
    fn a_pack_that_misses_a_listed_name_fails_loudly() {
        with_pack("//@ gone | gone() | nothing | none\n", |ctx| {
            let out = eval(ctx, "try { gone } catch (e) { e.message }");
            assert!(out.contains("gone was not defined"), "{out}");
        });
    }

    #[test]
    fn empty_pack_still_has_help() {
        with_pack("", |ctx| {
            assert_eq!(eval(ctx, "typeof help"), "function");
            assert!(eval(ctx, "help()").contains("No extra helpers"));
        });
    }
}

#[cfg(test)]
#[path = "pack_xml_bpmn_tests.rs"]
mod pack_xml_bpmn_tests;
