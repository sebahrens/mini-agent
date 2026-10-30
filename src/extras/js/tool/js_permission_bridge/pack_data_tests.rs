//! Tests for the data-oriented pack segments (`pack_path_stats.js`, `pack_semver.js`,
//! `pack_yaml.js`); included from the `js` tool tests so it can use `helper_tool` and `run_js`.

use super::*;

const PROFILES: [ModelEffectProfile; 2] = [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly];

/// Every helper of the data segments; each must be listed by `help()` and installed.
const DATA_PACK_NAMES: &[&str] = &["path", "tally", "stats"];

#[tokio::test]
async fn data_pack_helpers_are_listed_by_help_and_installed_in_both_profiles() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let catalogue = run_js(&tool, "help()").await;
        for name in DATA_PACK_NAMES {
            assert!(
                catalogue.contains(name),
                "{profile:?} help() must list {name}: {catalogue}"
            );
            assert_eq!(
                run_js(&tool, &format!("typeof {name}")).await,
                "function",
                "{profile:?} {name}"
            );
            assert!(
                run_js(&tool, &format!("help('{name}')"))
                    .await
                    .contains(name)
            );
        }
    }
}

#[tokio::test]
async fn path_matches_the_node_posix_table_and_handles_dotfiles() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        // Expected values were produced by node's path.posix; the documented differences
        // (dirname of a//b, a basename equal to ext) are not in the table.
        let code = r##"
            const rows = [
['normalize', [""], "."],
['extname', [""], ""],
['basename', [""], ""],
['dirname', [""], "."],
['normalize', ["/"], "/"],
['extname', ["/"], ""],
['basename', ["/"], ""],
['dirname', ["/"], "/"],
['normalize', ["."], "."],
['extname', ["."], ""],
['basename', ["."], "."],
['dirname', ["."], "."],
['normalize', [".."], ".."],
['extname', [".."], ""],
['basename', [".."], ".."],
['dirname', [".."], "."],
['normalize', ["a"], "a"],
['extname', ["a"], ""],
['basename', ["a"], "a"],
['dirname', ["a"], "."],
['normalize', ["a/"], "a/"],
['extname', ["a/"], ""],
['basename', ["a/"], "a"],
['dirname', ["a/"], "."],
['normalize', ["/a"], "/a"],
['extname', ["/a"], ""],
['basename', ["/a"], "a"],
['dirname', ["/a"], "/"],
['normalize', ["/a/"], "/a/"],
['extname', ["/a/"], ""],
['basename', ["/a/"], "a"],
['dirname', ["/a/"], "/"],
['normalize', ["a/b"], "a/b"],
['extname', ["a/b"], ""],
['basename', ["a/b"], "b"],
['dirname', ["a/b"], "a"],
['normalize', ["a/b/"], "a/b/"],
['extname', ["a/b/"], ""],
['basename', ["a/b/"], "b"],
['dirname', ["a/b/"], "a"],
['normalize', ["a//b"], "a/b"],
['extname', ["a//b"], ""],
['basename', ["a//b"], "b"],
['normalize', ["./a"], "a"],
['extname', ["./a"], ""],
['basename', ["./a"], "a"],
['dirname', ["./a"], "."],
['normalize', ["../a"], "../a"],
['extname', ["../a"], ""],
['basename', ["../a"], "a"],
['dirname', ["../a"], ".."],
['normalize', ["a/.."], "."],
['extname', ["a/.."], ""],
['basename', ["a/.."], ".."],
['dirname', ["a/.."], "a"],
['normalize', ["a/../"], "./"],
['extname', ["a/../"], ""],
['basename', ["a/../"], ".."],
['dirname', ["a/../"], "a"],
['normalize', ["/a/.."], "/"],
['extname', ["/a/.."], ""],
['basename', ["/a/.."], ".."],
['dirname', ["/a/.."], "/a"],
['normalize', ["a/./b/../c"], "a/c"],
['extname', ["a/./b/../c"], ""],
['basename', ["a/./b/../c"], "c"],
['dirname', ["a/./b/../c"], "a/./b/.."],
['normalize', ["/.."], "/"],
['extname', ["/.."], ""],
['basename', ["/.."], ".."],
['dirname', ["/.."], "/"],
['normalize', ["/../a"], "/a"],
['extname', ["/../a"], ""],
['basename', ["/../a"], "a"],
['dirname', ["/../a"], "/.."],
['normalize', ["../../a"], "../../a"],
['extname', ["../../a"], ""],
['basename', ["../../a"], "a"],
['dirname', ["../../a"], "../.."],
['normalize', [".bashrc"], ".bashrc"],
['extname', [".bashrc"], ""],
['basename', [".bashrc"], ".bashrc"],
['dirname', [".bashrc"], "."],
['normalize', ["a/.bashrc"], "a/.bashrc"],
['extname', ["a/.bashrc"], ""],
['basename', ["a/.bashrc"], ".bashrc"],
['dirname', ["a/.bashrc"], "a"],
['normalize', ["Makefile"], "Makefile"],
['extname', ["Makefile"], ""],
['basename', ["Makefile"], "Makefile"],
['dirname', ["Makefile"], "."],
['normalize', ["src/Makefile"], "src/Makefile"],
['extname', ["src/Makefile"], ""],
['basename', ["src/Makefile"], "Makefile"],
['dirname', ["src/Makefile"], "src"],
['normalize', ["a.tar.gz"], "a.tar.gz"],
['extname', ["a.tar.gz"], ".gz"],
['basename', ["a.tar.gz"], "a.tar.gz"],
['dirname', ["a.tar.gz"], "."],
['normalize', ["a."], "a."],
['extname', ["a."], "."],
['basename', ["a."], "a."],
['dirname', ["a."], "."],
['normalize', ["..."], "..."],
['extname', ["..."], "."],
['basename', ["..."], "..."],
['dirname', ["..."], "."],
['normalize', [".a.b"], ".a.b"],
['extname', [".a.b"], ".b"],
['basename', [".a.b"], ".a.b"],
['dirname', [".a.b"], "."],
['normalize', ["/a/b.c/"], "/a/b.c/"],
['extname', ["/a/b.c/"], ".c"],
['basename', ["/a/b.c/"], "b.c"],
['dirname', ["/a/b.c/"], "/a"],
['normalize', ["a/b/c.txt"], "a/b/c.txt"],
['extname', ["a/b/c.txt"], ".txt"],
['basename', ["a/b/c.txt"], "c.txt"],
['dirname', ["a/b/c.txt"], "a/b"],
['normalize', ["x/../../y"], "../y"],
['extname', ["x/../../y"], ""],
['basename', ["x/../../y"], "y"],
['dirname', ["x/../../y"], "x/../.."],
['normalize', ["dir.d/file"], "dir.d/file"],
['extname', ["dir.d/file"], ""],
['basename', ["dir.d/file"], "file"],
['dirname', ["dir.d/file"], "dir.d"],
['normalize', ["dir.d/"], "dir.d/"],
['extname', ["dir.d/"], ".d"],
['basename', ["dir.d/"], "dir.d"],
['dirname', ["dir.d/"], "."],
['normalize', ["C:\\x\\y.txt"], "C:\\x\\y.txt"],
['extname', ["C:\\x\\y.txt"], ".txt"],
['basename', ["C:\\x\\y.txt"], "C:\\x\\y.txt"],
['dirname', ["C:\\x\\y.txt"], "."],
['basename', ["a/b.js", ".js"], "b"],
['basename', ["a/b.js", "b.js"], "b.js"],
['basename', ["x.tar.gz", ".gz"], "x.tar"],
['basename', ["dir/", "r"], "di"],
['basename', ["a", ""], "a"],
['basename', ["a.js/", ".js"], "a"],
['join', ["a","b"], "a/b"],
['join', ["/a","../b","c.txt"], "/b/c.txt"],
['join', ["",""], "."],
['join', ["","a"], "a"],
['join', ["a","","b/"], "a/b/"],
['join', ["..","..","x"], "../../x"],
['join', ["/","a"], "/a"],
['join', ["a/","/b"], "a/b"],
['join', [".","a"], "a"],
['join', ["a",".."], "."],
['join', ["a","../..","b"], "../b"],
['join', ["/a","../../.."], "/"],
['join', [], '.'],
['relative', ["/a/b/c", "/a/d"], "../../d"],
['relative', ["/a", "/a/b/c"], "b/c"],
['relative', ["/a/b", "/a"], ".."],
['relative', ["/a", "/a"], ""],
['relative', ["/", "/x/y"], "x/y"],
['relative', ["/x/y", "/"], "../.."],
['relative', ["/a/./b", "/a/b/../c"], "../c"],
['relative', ['a/b', 'a/c/d'], '../c/d'],
['relative', ['a', 'a'], ''],
['relative', ['a/b/', 'a/b/c'], 'c'],
['relative', ['a', 'b'], '../b'],
['relative', ['', 'a'], 'a'],
['relative', ['a/b', ''], '../..']
            ];
            const bad = rows.filter(([f, a, e]) => {
              try { return path[f](...a) !== e } catch (x) { return true }
            }).map(r => r[0] + JSON.stringify(r[1]));
            bad.length + ':' + rows.length + ':' + bad.join(';')"##;
        let out = run_js(&tool, code).await;
        assert!(out.starts_with("0:167:"), "{profile:?} {out}");
        let code = r##"JSON.stringify([
            path.extname('Makefile'), path.extname('.bashrc'), path.extname('a.tar.gz'),
            path.extname('dir.d/file'), path.basename('/x/.env'), path.dirname('a//b'),
            path.basename('.js', '.js'), path.join('/a', '../b', 'c.txt'),
            'src/Makefile'.split('.').pop()
        ])"##;
        assert_eq!(
            run_js(&tool, code).await,
            r#"["","",".gz","",".env","a",".js","/b/c.txt","src/Makefile"]"#,
            "{profile:?}"
        );
    }
}

#[tokio::test]
async fn path_rejects_bad_input_and_stays_linear_on_hostile_paths() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let started = std::time::Instant::now();
        let code = r##"
            const names = f => { try { f(); return 'no throw' } catch (e) { return e.name } };
            const out = [
              names(() => path('x')), names(() => path.join(1)), names(() => path.basename(null)),
              names(() => path.extname(undefined)), names(() => path.relative('/a', 'b')),
              names(() => path.relative('../a', 'b')), names(() => path.normalize('a'.repeat(65537))),
              names(() => path.join(...Array(1025).fill('a'))),
              names(() => path.join('a'.repeat(40000), 'b'.repeat(40000))),
              path.normalize('../'.repeat(20000)).length,
              path.normalize('a/'.repeat(10000) + '../'.repeat(10000)),
              path.normalize('/' + '..//'.repeat(16000)),
              path.relative('/' + 'a/'.repeat(20000), '/' + 'a/'.repeat(19999) + 'b').length,
              path.join(...Array(1000).fill('x')).length,
              path.dirname('/'.repeat(60000)), path.basename('/'.repeat(60000)),
            ];
            out.join(',')"##;
        assert_eq!(
            run_js(&tool, code).await,
            "TypeError,TypeError,TypeError,TypeError,RangeError,RangeError,RangeError,RangeError,RangeError,60000,./,/,4,1999,/,",
            "{profile:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{profile:?} {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn tally_sorts_by_count_descending_then_key_ascending() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let code = r##"JSON.stringify([
            tally(['b', 'a', 'b', 'c', 'a', 'd']),
            tally([3, 1, 2, 10, 1, 10, 2, 3, 20]),
            tally([{t: 'x'}, {t: 'y'}, {t: 'x'}], 't'),
            tally(['aa', 'b', 'cc', 'd', 'e'], s => s.length),
            tally(['B', 'a', 'A', 'b', 'é', 'z']),
            tally([1, '1', 1, true, null, undefined, null]).map(([k, n]) => [String(k) + typeof k, n]),
            tally([]),
            tally([-0, 0, 0.5, -1, -1]),
            tally(['x', 'y'], (v, i) => i % 2),
        ])"##;
        assert_eq!(
            run_js(&tool, code).await,
            concat!(
                r#"[[["a",2],["b",2],["c",1],["d",1]],"#,
                r#"[[1,2],[2,2],[3,2],[10,2],[20,1]],"#,
                r#"[["x",2],["y",1]],"#,
                r#"[[1,3],[2,2]],"#,
                r#"[["A",1],["B",1],["a",1],["b",1],["z",1],["é",1]],"#,
                r#"[["nullobject",2],["1number",2],["undefinedundefined",1],"#,
                r#"["trueboolean",1],["1string",1]],[],[[-1,2],[0,2],[0.5,1]],[[0,1],[1,1]]]"#
            ),
            "{profile:?}"
        );
        let code = r##"
            const names = f => { try { f(); return 'no throw' } catch (e) { return e.name } };
            [names(() => tally('abc')), names(() => tally([1], 5)), names(() => tally([{}], 'a')),
             names(() => tally([null], 'a')), names(() => tally([1], () => ({}))),
             names(() => tally(new Array(1000001)))].join(',')"##;
        assert_eq!(
            run_js(&tool, code).await,
            "TypeError,TypeError,no throw,TypeError,TypeError,RangeError",
            "{profile:?}"
        );
    }
}

#[tokio::test]
async fn stats_uses_linear_interpolation_and_rejects_bad_numbers() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        // Expected values come from an independent python implementation (math.fsum and
        // statistics.quantiles(method='inclusive'), the same linear interpolation).
        let code = r##"
            const close = (a, b) => Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(b));
            const keys = ['n', 'sum', 'mean', 'p50', 'p90', 'p95', 'p99', 'min', 'max'];
            const sets = [
              [[1, 2, 3, 4], [4, 10, 2.5, 2.5, 3.7, 3.85, 3.97, 1, 4]],
              [[5], [1, 5, 5, 5, 5, 5, 5, 5, 5]],
              [[10, 0.5, 7, 3, 3, 9, 12.25, -4, 100, 2], [10, 142.75, 14.275, 5, 21.02499999999997, 60.5124999999999, 92.1025, -4, 100]],
              [Array.from({length: 200}, (_, i) => 1e-3 * (i + 1) * (i + 1)),
               [200, 2686.7000000000003, 13.433500000000002, 10.1005, 32.436099999999996, 36.119049999999994, 39.207969999999996, 0.001, 40]],
              [Array(10).fill(0.1), [10, 1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1]],
              [[3, 3, 3], [3, 9, 3, 3, 3, 3, 3, 3, 3]],
            ];
            const bad = [];
            for (const [input, want] of sets) {
              const s = stats(input);
              if (Object.keys(s).join() !== keys.join()) bad.push('keys');
              keys.forEach((k, i) => { if (!close(s[k], want[i])) bad.push(k + ':' + s[k] + '!=' + want[i]); });
            }
            const reversed = stats([4, 3, 2, 1]).p50 === 2.5 && stats([2, 1, 4, 3]).p95 === stats([1, 2, 3, 4]).p95;
            const input = [3, 1, 2];
            stats(input);
            bad.length + ':' + bad.join(';') + ':' + reversed + ':' + input.join()"##;
        assert_eq!(run_js(&tool, code).await, "0::true:3,1,2", "{profile:?}");
        let code = r##"
            const names = f => { try { f(); return 'no throw' } catch (e) { return e.name } };
            [names(() => stats([])), names(() => stats([1, NaN])), names(() => stats([1, Infinity])),
             names(() => stats([1, '2'])), names(() => stats([1n])), names(() => stats('12')),
             names(() => stats([1, undefined])), names(() => stats(new Array(1000001)))].join(',')"##;
        assert_eq!(
            run_js(&tool, code).await,
            "RangeError,TypeError,TypeError,TypeError,TypeError,TypeError,TypeError,RangeError",
            "{profile:?}"
        );
    }
}

#[tokio::test]
async fn tally_and_stats_are_bounded_on_large_inputs() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let started = std::time::Instant::now();
        let code = r##"
            const big = Array.from({length: 500000}, (_, i) => (i * 7919) % 1009);
            const t = tally(big);
            const s = stats(big);
            const many = tally(Array.from({length: 100000}, (_, i) => 'k' + i));
            [t.length, t[0][1], s.n, s.max, many.length, many[0][0]].join(',')"##;
        assert_eq!(
            run_js(&tool, code).await,
            "1009,496,500000,1008,100000,k0",
            "{profile:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "{profile:?} {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn semver_helpers_are_listed_by_help_and_installed_in_both_profiles() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let catalogue = run_js(&tool, "help()").await;
        for name in ["semver_cmp", "semver_sat", "yaml_parse", "yaml_stringify"] {
            assert!(catalogue.contains(name), "{profile:?} {catalogue}");
            assert_eq!(
                run_js(&tool, &format!("typeof {name}")).await,
                "function",
                "{profile:?} {name}"
            );
        }
        assert!(
            run_js(&tool, "help('semver_sat')")
                .await
                .contains("prerelease")
        );
    }
}

#[tokio::test]
async fn semver_cmp_orders_the_spec_chain_and_rejects_malformed_versions() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        assert_eq!(
            run_js(&tool, CMP_CODE).await,
            "|0,0,0,-1,-1,1,1,1,0",
            "{profile:?}"
        );
        assert_eq!(
            run_js(&tool, CMP_ERRORS_CODE).await,
            "SyntaxError,".repeat(16) + "TypeError,TypeError",
            "{profile:?}"
        );
    }
}

#[tokio::test]
async fn semver_sat_agrees_with_the_npm_semver_package_on_a_generated_table() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        // Rows are version, range, style, expected (1 or 0). npm-style rows come from the npm
        // `semver` package; cargo-style rows use its caret operator, since a bare cargo
        // requirement is a caret requirement.
        let code = format!(
            "const rows = {table:?}.split('\\n').map(l => l.split('\\t'));\n{body}",
            table = SEMVER_TABLE,
            body = r##"
            const bad = rows.filter(([v, r, st, e]) => {
              try { return (semver_sat(v, r, st) ? '1' : '0') !== e } catch (x) { return true }
            }).map(r => r.join(' | '));
            bad.length + ':' + rows.length + ':' + bad.slice(0, 5).join(';')"##
        );
        assert_eq!(run_js(&tool, &code).await, "0:520:", "{profile:?}");
    }
}

#[tokio::test]
async fn semver_sat_follows_caret_tilde_hyphen_prerelease_and_style_rules() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        assert_eq!(
            run_js(&tool, SAT_CODE).await,
            "true,false,true,false,true,true,false,true,true,false,true,false,false,true,false,true,false,true,false,true,true,false,true,false,true,false,true,true,false,true,true,true,true,false,true,false,false,false,true,true,false,true,true|SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,no throw,SyntaxError,SyntaxError,SyntaxError,RangeError,TypeError,TypeError,SyntaxError,SyntaxError,no throw,SyntaxError,SyntaxError,SyntaxError",
            "{profile:?}"
        );
    }
}

#[tokio::test]
async fn semver_helpers_are_bounded_on_hostile_input() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let started = std::time::Instant::now();
        assert_eq!(
            run_js(&tool, BOUNDED_CODE).await,
            "1,-1,true,SyntaxError,true,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,false,true,SyntaxError",
            "{profile:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{profile:?} {:?}",
            started.elapsed()
        );
    }
}

const CMP_CODE: &str = r##"
const chain = ['1.0.0-alpha', '1.0.0-alpha.1', '1.0.0-alpha.beta', '1.0.0-beta', '1.0.0-beta.2', '1.0.0-beta.11',
               '1.0.0-rc.1', '1.0.0', '1.0.1', '1.1.0', '2.0.0', '10.0.0', '100000000000000000000.0.0'];
const bad = [];
for (let i = 0; i < chain.length; i++) for (let j = 0; j < chain.length; j++) {
  const want = i < j ? -1 : i > j ? 1 : 0;
  if (semver_cmp(chain[i], chain[j]) !== want) bad.push(chain[i] + ' ' + chain[j]);
}
const extra = [
  semver_cmp('1.0.0+a', '1.0.0+b'), semver_cmp('v1.2.3', '1.2.3'), semver_cmp(' 1.2.3 ', '1.2.3'),
  semver_cmp('1.0.0-1', '1.0.0-a'), semver_cmp('1.0.0-0.3.7', '1.0.0-x.7.z.92'), semver_cmp('1.0.0-a-b', '1.0.0-a'),
  semver_cmp('1.0.0-alpha', '1.0.0-Alpha'), semver_cmp('9007199254740993.0.0', '9007199254740992.0.0'),
  semver_cmp('1.0.0-rc.1+build.5', '1.0.0-rc.1'),
];
bad.join(';') + '|' + extra.join(',')"##;
const CMP_ERRORS_CODE: &str = r##"
const name = f => { try { f(); return 'no throw' } catch (e) { return e.name } };
['1.2', '1.2.3.4', '01.2.3', '1.02.3', '1.2.3-', '1.2.3-01', '1.2.3-a..b', '1.2.3+', 'a.b.c', '', '1.2.3-é', 'x'.repeat(300), '1.2.3 4', '1.2.x', '=1.2.3', '^1.2.3']
  .map(v => name(() => semver_cmp(v, '1.0.0'))).concat([name(() => semver_cmp(1, '1.0.0')), name(() => semver_cmp('1.0.0'))]).join(',')"##;
const SAT_CODE: &str = r##"
const s = semver_sat;
const name = f => { try { f(); return 'no throw' } catch (e) { return e.name } };
[
  s('1.4.2', '^1.2.3'), s('2.0.0', '^1.2.3'), s('1.9.0', '1.2.3'), s('1.9.0', '1.2.3', 'npm'), s('1.2.3', '1.2.3', 'npm'),
  s('1.2.9', '1.2', 'npm'), s('1.3.0', '1.2', 'npm'), s('1.3.0', '1.2'), s('0.2.9', '^0.2.3'), s('0.3.0', '^0.2.3'),
  s('0.0.3', '^0.0.3'), s('0.0.4', '^0.0.3'), s('1.2.4-beta', '^1.2.3'), s('1.2.3-beta.4', '^1.2.3-beta.2'),
  s('1.2.4-beta', '>=1.2.3'), s('1.2.3-rc.1', '>=1.2.3-rc.0 <1.3.0'), s('2.0.0-rc.1', '^1.2.3'),
  s('2.0.0', '>=1.2.7 <1.3.0 || >=2', 'npm'), s('1.3.0', '>=1.2.7 <1.3.0 || >=2', 'npm'), s('1.2.8', '>=1.2.7 <1.3.0 || >=2', 'npm'),
  s('1.2.5', '1.2.3 - 1.2.9'), s('1.3.0', '1.2.3 - 1.2'), s('1.3.0', '1.2.3 - 1.3'), s('3.0.0', '1.2.3 - 2'), s('2.9.9', '1.2.3 - 2'),
  s('1.5.0', '>=1.2, <1.5'), s('1.4.9', '>=1.2, <1.5'), s('7.7.7', '*'), s('7.7.7-pre', '*'), s('1.0.0', ''.concat('x'), 'npm'),
  s('1.5.0', '>= 1.2.0'), s('1.2.3', '~1.2.3'), s('1.2.9', '~1.2.3'), s('1.3.0', '~1.2.3'), s('1.9.9', '~1'), s('2.0.0', '~1'),
  s('1.0.0', '>*'), s('1.0.0', '<*'), s('1.0.0', '>=*'), s('1.2.3', '=1.2.3'), s('1.2.4', '=1.2.3'),
  s('v1.2.3', '1.2.3'), s('100000000000000000000.0.0', '>=99999999999999999999.0.0'),
].join(',') + '|' + [
  name(() => s('1.2.3', '>=')), name(() => s('1.2.3', '^')), name(() => s('1.2.3', '1.2.3 -')), name(() => s('1.2.3', '1.x.3')),
  name(() => s('1.2.3', '1.2.3.4')), name(() => s('1.2.3', '>=>1.2.3')), name(() => s('1.2.3', '1.2.3-beta.x')), name(() => s('1.2.3', '1.2-beta')),
  name(() => s('1.2.3', '')), name(() => s('1.2.3', '   ')), name(() => s('1.2.3', '>=1.0.0', 'pep')), name(() => s('1.2.3', 5)),
  name(() => s(5, '^1')), name(() => s('1.2', '^1')), name(() => s('1.2.3', '1.2.3 || ')), name(() => s('1.2.3', '1.2.3 || ', 'npm')),
  name(() => s('1.2.3', '~=1.2')), name(() => s('1.2.3', '!1.2.3')), name(() => s('1.2.3', '1.2.3, 1.2.3', 'npm')),
].join(',')"##;
const BOUNDED_CODE: &str = r##"
const name = f => { try { return String(f()) } catch (e) { return e.name } };
const sets = n => Array(n).fill('^1.2.3').join(' || ');
[
  semver_cmp('9'.repeat(250) + '.0.0', '9'.repeat(249) + '.0.0'),
  semver_cmp('1.2.3-' + 'a.'.repeat(100) + 'b', '1.2.3-' + 'a.'.repeat(100) + 'c'),
  name(() => semver_sat('1.2.3', sets(64))), name(() => semver_sat('1.2.3', sets(65))),
  name(() => semver_sat('1.2.3', Array(64).fill('>=1').join(' '))), name(() => semver_sat('1.2.3', Array(65).fill('>=1').join(' '))),
  name(() => semver_sat('1.2.3', '^'.repeat(500))), name(() => semver_sat('1.2.3', '>='.repeat(300))),
  name(() => semver_sat('1.2.3', ' '.repeat(1025))), name(() => semver_sat('1.2.3', '1.2.3 - '.repeat(100))),
  name(() => semver_sat('1.2.3', '1 - 2 - 3')), name(() => semver_sat('1.2.3', 'x.'.repeat(400))),
  name(() => semver_sat('1.2.3', '>=' + '9'.repeat(900) + '.0.0')), name(() => semver_sat('1.2.3', '^1.2.3-' + 'a'.repeat(900))),
  name(() => semver_sat('1.2.3', '~' + '1.'.repeat(500))),
].join(',')"##;
const SEMVER_TABLE: &str = r##"1.2.10-0	=2 ^0	npm	0
3.0.0-rc.1	3	npm	0
1.0.1-alpha.1	1 - 0 || 0 - 1 || <0 ^1 ~0	npm	0
10.3.1	^1 3 ~0 || <10 >=10	npm	0
1.1.1-beta.2	3 - 0 || 3 - 10	npm	0
1.0.1	>=10 || 1 - 0 || =1 3 ~1	npm	0
3.1.0-alpha	1 0	npm	0
2.1.1-alpha	2 - 0 || >0 >10	npm	0
1.3.1-0	~0 <=1 || 2 ~0 1	npm	0
1.1.0	2 =1 >=0 || =3 <2 || 0 - 0	npm	0
0.3.0-beta.2	10 <3	npm	0
0.0.3	>=0 >=0 =2 || ^0 ^10.0	npm	0
10.10.2-beta.2	10 - 1	npm	0
3.0.2	>3 ^0 ^1	npm	0
1.3.0-0	3 - 1 || 3 <=3 ~2 || 10	npm	0
10.2.2	>=1 || ^2 ^1 10	npm	1
1.3.2-rc.1	3 - 2	npm	0
0.0.1	1 - 0 || 10 - 0	npm	0
2.1.10-alpha	2 || ~10 ^1 10 || >=1	npm	0
0.10.10-beta.2	~0 >1 >1 || 1 - 1	npm	0
2.10.10	~3 >=2 <3 || =0 2 ^0	npm	0
0.2.10-alpha.1	3 - 2 || >=1 ~0 1	npm	0
10.1.1	3 - 3 || 10 - 3 || <1 ~10 <2	npm	0
0.2.10-0	>=0 ^2 <0	npm	0
2.0.1-alpha	>=10 || ^1 <=10 <3 || 1 - 2	npm	0
10.1.1-beta.11	1 - 0 || 1 - 1 || <10	npm	0
1.1.2-beta.2	>0 >=0 || 1 - 3 || 0 - 0	npm	0
10.1.0-beta.2	0 - 1	npm	0
3.0.1-beta.11	>=1	npm	0
2.1.10	1 =3 <10 || ^0 <1 =10 || <10	npm	1
1.1.3-0	1 || 2 =0	npm	0
0.10.10-0	>=3 0	npm	0
1.0.3-beta.2	^3 0 ~3	npm	0
3.3.3-rc.1	=0 || 0	npm	0
2.10.2-alpha.1	^1 || 1 <=1 0	npm	0
0.10.0-alpha.1	>=3 <=0 <1 || =1 || 10 - 10	npm	0
0.0.0-beta.11	1	npm	0
0.0.1-beta.2	=3	npm	0
10.2.10-beta.2	1 - 0 || ~0 =0 ~0 || <=1 <=0	npm	0
1.10.1-alpha.1	~1 <=1	npm	0
0.0.0	3 2 || 3 ^3 >=3 || 10 - 2	npm	0
1.3.1	>0 =0	npm	0
1.1.2	=1 10	npm	0
0.1.0-0	>0 >=2 <=3 || 0 - 3 || 1 - 1	npm	0
1.2.0-rc.1	0 - 0 || 1 <0 || <0 <1	npm	0
0.10.2-beta.11	<1 || 2 - 1 || 0 ~1	npm	0
1.0.2	10 - 10 || 1 - 0	npm	0
2.10.1	0 <=2 ^1	npm	0
1.0.3-beta.2	3 - 10 || 3 - 3	npm	0
10.0.1-alpha.1	^2 || <=3 ~0 =1 || ^1	npm	0
2.2.0-rc.1	>1 ~1	npm	0
10.0.10-alpha	3 =0 || <=2 0 >=3 || ^0	npm	0
1.1.0-beta.2	~1 2	npm	0
0.3.1	3 - 0	npm	0
1.2.3	<=0 >1 10 || >3 || 1 - 1	npm	1
10.3.1-0	>=10 >=0	npm	0
1.2.10-beta.11	~3	npm	0
0.1.1-alpha	0 - 1 || ^0 <=1 >0 || 0 >1	npm	0
1.2.1-alpha	0 - 1 || ~1 || 10 =10 <3	npm	0
0.1.2-0	>=3 =2 <=1	npm	0
1.3.10	1 - 10	npm	1
2.1.3-beta.2	=2 2 1 || 1 - 0	npm	0
10.0.10	3	npm	0
3.10.2-rc.1	>3 >=0 >0 || 0 - 10	npm	0
0.1.1-rc.1	3 - 0 || ^1	npm	0
3.2.0	1 - 1 || =10 <=0 1	npm	0
0.1.2	^0 >=1 <=0 || <1 ~10 <1 || 0 - 0	npm	1
10.0.2-beta.2	<3	npm	0
1.2.2-beta.11	1 - 1 || ~1 ^2	npm	0
3.1.3-rc.1	10 >2 =2	npm	0
3.3.0	^2 1 || >1 10 <3 || ^10	npm	0
3.0.0-alpha.1	>=0 <10 <=2	npm	0
0.2.0-alpha	1 - 10	npm	0
2.1.3-beta.11	>0 || 2 - 10 || >=0 >2	npm	0
0.3.10-rc.1	3 - 3 || 2 - 10	npm	0
1.2.0-alpha.1	10 - 10 || 2 - 1 || 2 - 3	npm	0
3.1.1-beta.11	3 - 0	npm	0
1.1.0	0 - 1 || >=3	npm	1
0.1.1	1 - 0	npm	0
3.3.1	>=1 || =3	npm	1
0.3.1	>1 >=1 || <=1 1 <=2 || 0 - 10	npm	1
2.1.1	0 - 0 || <10 10 || 1 - 1	npm	0
0.0.3-rc.1	^3 || 1 - 1	npm	0
1.0.2-alpha.1	^1 || 2 - 1 || ^2 <=0	npm	0
1.10.1-beta.11	3 - 2 || <=3 ^2 <=2 || <=1	npm	0
3.10.0-beta.11	0 ~2 <1 || 2 - 0	npm	0
1.1.3-0	<=0 || >0 >=1 || 0 - 0	npm	0
3.10.0	>3 >=10 2 || <=3 <1	npm	0
0.2.1-rc.1	2 - 0	npm	0
1.3.2	^3	npm	0
1.2.0-0	1 0	npm	0
2.3.2-alpha	~0 <=0	npm	0
1.3.0-0	>2	npm	0
1.2.1	=1 || 1 - 1 || >=1	npm	1
0.1.3-alpha.1	>=0 || 0 =3 >0	npm	0
0.0.1-rc.1	0 ^10 1 || ^10 10	npm	0
1.0.0-alpha	>2	npm	0
1.10.3-beta.11	=10	npm	0
0.2.0	0 >0 || 1 ^0 <=0	npm	0
2.2.1	<=0 >=3 || >=10	npm	0
10.0.0-rc.1	3 <=10 || 1 - 10	npm	0
2.3.1	>0 >=1	npm	1
2.2.1-alpha.1	>=3 ^0	npm	0
1.0.1-0	0 - 10 || 10 - 2	npm	0
0.10.0-0	2 - 1 || <3 3 10 || 0 - 3	npm	0
0.0.10	3 >10 <=0 || 1 - 0 || 1 1 =2	npm	0
10.1.0	~0 >=0 || =3 || 0 - 1	npm	0
1.1.1	0 - 0 || 1 >=0 || <=10 =0 <=3	npm	1
0.1.0-rc.1	1 - 3 || 10 - 0 || 2 - 2	npm	0
10.3.2-alpha.1	1 - 1	npm	0
3.2.10	2 - 1 || 10 - 3 || >0 2 <=1	npm	0
0.3.0	>=3 >2 =2	npm	0
0.10.10-rc.1	<10 >1 || <0	npm	0
0.0.0-beta.11	2 1 2 || >=1 2 || 0 ^3 =1	npm	0
0.3.2-alpha	<1 1	npm	0
1.1.0	1 <=1 =0	npm	0
1.10.2-beta.11	>2 ~1 || <=0 >=3 >1 || =1	npm	0
10.0.2-rc.1	3 - 0 || 0 - 1 || >0 ~1 =10	npm	0
1.0.1-alpha.1	2 - 1	npm	0
2.1.3-beta.2	<2 || 3 0	npm	0
1.3.2	2 - 2	npm	0
3.1.2	>=10 || 1 - 2 || 2 - 1	npm	0
3.10.10-beta.2	1 - 3 || <=0 1 2 || >=3 <3	npm	0
3.1.1-0	0 - 1 || <0	npm	0
0.0.2-alpha	>0	npm	0
1.2.1-alpha	0 - 2 || =1 0 ^1	npm	0
2.0.3-0	10 - 0	npm	0
0.0.3	3 - 0 || 10 - 0	npm	0
1.0.3	10 - 0	npm	0
1.2.1	>1 || <=1 =0 || 1 - 10	npm	1
1.3.1	^10 || 3 - 0 || 2 - 2	npm	0
1.10.2-beta.2	0 - 1	npm	0
3.0.10-rc.1	<=3 >1 0 || 10 ~1	npm	0
3.2.0-alpha	<=1 ^2	npm	0
0.1.10	<=1 =1 >=1 || 0 - 1 || 1 - 0	npm	1
1.3.1	2 - 1	npm	0
0.0.2	10 <=0 ~2	npm	0
2.0.3-alpha	0 ^0 >10	npm	0
3.2.3-0	>1 || 0 - 0 || >0	npm	0
0.1.3-0	=2 =2 =1	npm	0
2.1.0	=2	npm	1
1.0.2-beta.2	>1 <=10 >=3	npm	0
2.3.1	1 - 3 || <2 ^2 || <0	npm	1
0.1.1-alpha	^2 1 >=1 || >2 ^2 || 1 - 1	npm	0
3.1.0-beta.11	<2 10 || 10 - 0	npm	0
10.0.1	3 - 0 || 10 - 2 || 3 - 0	npm	0
10.0.1	>=0 >0 || 2 - 0 || 10 - 10	npm	1
2.3.3	=1 =0 ^1 || >=3	npm	0
2.10.1-alpha.1	1 || ^0 <1 >1	npm	0
3.0.0	<=2 =0 <=0 || >=2 1	npm	0
1.10.1-beta.2	0 - 10	npm	0
2.0.1	<=1	npm	0
0.0.3-rc.1	2 =0 >=3 || 0	npm	0
1.1.3-0	<=0 <10 ^2	npm	0
0.0.2	<=0 ~0 || 1 - 10	npm	1
0.1.1-0	>=1 || 1 - 0	npm	0
2.2.1-0	0 - 10 || 0 - 0 || 3 - 3	npm	0
0.1.2-alpha	>1 ^1 || 0 - 0 || >=2 =0 =10	npm	0
10.0.1	~10 ^2 ~0	npm	0
2.1.0-alpha.1	0 ^1	npm	0
10.1.3-beta.11	<=3 <1 3 || 1 - 1 || 10 - 10	npm	0
1.10.3	=0 2 || 3 - 2	npm	0
3.1.3-alpha.1	>=2 ^0 || <10 <0 >3 || 3 - 2	npm	0
0.1.10-alpha	^3 ~1 >0 || 1 || =3 1	npm	0
2.1.1	0 - 1 || <1 ^0 || <=1	npm	0
10.1.1-rc.1	>1 || 1 - 0	npm	0
1.1.2	<=2 ^1 || 1 ^1 ~2	npm	1
1.3.2-rc.1	<=1	npm	0
1.1.0-beta.2	>=1 ^3	npm	0
2.0.2	>=1	npm	1
0.0.10-beta.11	>=2 <=0 =0 || 3 - 1 || 1	npm	0
0.0.2-alpha.1	>10 0 =0 || ~0 1 || 1	npm	0
0.2.1-alpha	3 - 10 || =10 || 0 - 10	npm	0
1.2.1	~1	npm	1
3.3.1-alpha.1	>=10 ~0 || 3 - 0	npm	0
10.1.3-alpha.1	>1 >=1 >=3.1 || 1 - 1 || =1 ~3 >0	npm	0
1.2.2	<1 ^2 || 0 =0 <0	npm	0
2.1.1-beta.11	>0	npm	0
3.0.0-0	10 - 1 || 10 - 0	npm	0
2.1.3-alpha	0 - 0 || 0 - 3	npm	0
10.1.0-alpha.1	1 - 3	npm	0
0.1.3-rc.1	<10 >0 =0 || 1 - 1	npm	0
2.0.10-beta.11	1 - 2 || <10 <3	npm	0
1.1.10	0 - 1 || >1 0	npm	1
10.2.3	=1 <=1 || <=0 =0 >3	npm	0
0.0.0-0	0 || 1 ~1 >=0	npm	0
10.2.1	0 - 1 || <0 0.2 2	npm	0
1.2.10-rc.1	=0 >=0 || 3 - 1 || >0 2 1	npm	0
2.0.1	1 - 1	npm	0
1.10.10-beta.2	1 10 <10	npm	0
1.0.1-alpha.1	<3 ~1 ^0 || <3 <10 >1	npm	0
1.10.0-beta.2	2 - 10	npm	0
0.1.0-rc.1	10 || 3 - 10 || 2 - 3	npm	0
2.10.1	1 - 0 || >2 <=0 || 0 - 1	npm	0
0.1.1	^3 || 2 - 2 || ^0 >2 ^1	npm	0
1.3.0	~3 >1	npm	0
3.2.10-alpha	>10 || 1 - 0 || 3	npm	0
0.0.3-rc.1	3 || <1 0	npm	0
1.1.1	=3 =0 10 || >1 || >=10	npm	0
0.0.2	>2 ~3 >=3 || 0 10 0	npm	0
10.1.10	3 <1	npm	0
2.0.1-beta.2	1 || ^1 || =10 =3 >=1	npm	0
10.0.0-alpha	<=0 =0 <10 || 1 - 1 || 1 - 0	npm	0
1.0.1-alpha.1	10 =3 >0 || 0 >=10 || ^0	npm	0
1.3.10	0 - 0	npm	0
2.1.3	~2 || 0 - 0	npm	1
0.10.2	<=10 || >=10 0 || 10 ~0 1	npm	1
0.2.3-beta.11	=0 <1 || 1	npm	0
1.2.1	1 >2 || 3 =10 0	npm	0
1.10.1	<=2 <0 || ~1 2 >1	npm	0
0.0.2-0	1 <1 || 0 ~0 >=0	npm	0
2.0.1-alpha	0 <1 10 || <0 0 >=2 || <=0 2	npm	0
1.0.0	>2 >1 <10 || >=1 || 3 - 1	npm	1
1.0.2	1 >1 ~10	npm	0
10.3.1	>1 =0 || >=10 <=0 || ~0 =2 ^0	npm	0
3.0.10-alpha.1	10 - 10 || 3 - 1 || ~1	npm	0
1.1.0-alpha.1	0 >=1 || 2	npm	0
0.10.10-0	2 - 1 || 1 - 0	npm	0
2.2.3-0	0 - 3 || <=3 ^3 || <0 1	npm	0
0.2.10-0	>=10 || >3 =3	npm	0
1.1.3	>=2 0 || <0 1 ^1	npm	0
1.1.10-alpha.1	>=1 0 || 1 <=10 || ~1	npm	0
10.2.1-rc.1	=3 || >=0 <=10	npm	0
3.0.2-beta.11	^2 =1 ^1 || <0 || <=0	npm	0
3.2.10-beta.2	=1 || 2 - 0	npm	0
0.0.2	3 - 10 || ~0 0	npm	1
3.1.0-0	1 - 2 || 3 3 || 1 - 0	npm	0
0.2.10-rc.1	<=10 <0	npm	0
0.0.2-alpha.1	<3 =10 0 || 2 - 10	npm	0
10.10.0-beta.2	10	npm	0
2.2.2	1 - 2	npm	1
0.3.1	2 - 1 || 3 >=3 >10 || ~1 >1 =3	npm	0
0.0.10	^1 <=2 =2	npm	0
0.1.0-rc.1	>=3 <10 || >=2 2 >=0 || =1 ~1 <=0	npm	0
0.2.1-beta.2	3 || 3 - 0 || 2 - 3	npm	0
0.0.1-0	=0 <=10 || <1 >=0 || 10 >10	npm	0
0.1.1-alpha	<1 >10 ~0 || 10 - 1	npm	0
3.3.1-rc.1	~10 ^0 >10 || 1 - 1	npm	0
0.1.0-beta.11	~10 || 1 - 1 || >=1 >3 >1	npm	0
0.1.10-beta.2	>2 0 3 || 0 1	npm	0
0.0.2-alpha	<0 0	npm	0
3.0.1-beta.11	10 || <1 <3 1 || <=10	npm	0
10.2.3-beta.11	0 - 2	npm	0
1.2.10	3 || <=0 3 || 2	npm	0
10.3.10	0 - 10 || >=3 ^10	npm	1
0.1.0-alpha.1	10 - 3 || 0 - 1 || 0 - 3	npm	0
3.0.0-beta.11	1 - 3 || 10 - 10	npm	0
3.3.10-rc.1	<=2 <1 ^0 || 10 - 2	npm	0
1.1.3	3 - 3	npm	0
0.1.0-rc.1	2 - 1 || <1	npm	0
0.1.10-beta.2	1 - 2 || 0 - 0 || 0 - 0	npm	0
2.1.10-0	>1 <0 =10 || <10 >1 =0 || >3 <3	npm	0
10.1.10-beta.11	0 - 10 || 0	npm	0
0.3.0-rc.1	^2	npm	0
1.1.3-alpha.1	^3 >=2 <3 || 10 - 3	npm	0
10.1.0-0	0 - 10 || >=3 2 <=1	npm	0
0.3.10-0	0 >=1 >=10 || ~10 || >10 <=1 <10	npm	0
2.1.0	0 - 3 || >=10 <2 =0 || =0 <=1 10	npm	1
0.1.2-rc.1	>0 ^2 2 || ~1 <=0 || >1	npm	0
0.3.3	>2 10 || >3 || ^0	npm	1
1.2.0	1 - 1 || 1 - 0	npm	1
2.10.10	^3 >=0 || 10 || 10 - 0	npm	0
3.0.10	^0 =2 ^1 || 0 - 0 || <0 0 ~1	npm	0
10.1.3-beta.2	2 ^1 <=2 || >1 10 0 || ^10 10	npm	0
0.0.0	=3 || >2.10 =1	npm	0
3.0.0-alpha.1	~0 0 =3 || 0 - 0 || >1 =10 ^1	npm	0
2.2.2-alpha.1	<=0 || 1 - 3	npm	0
1.0.10	>2 ~0 || >1 <10	npm	0
0.0.0-beta.11	<0 >=0 || <0 0 || 10 - 2	npm	0
10.1.1	^2 || >2 || ^1	npm	1
1.1.1-beta.2	>=1 <=1 ^10 || 0 || ^3 3 >=0	npm	0
1.2.1-alpha.1	3 - 0 || 0 ~10 <2	npm	0
0.10.3-alpha.1	~0 1 || >2	npm	0
2.2.3-alpha	=1	npm	0
2.2.10-alpha	<1 10 0	npm	0
3.1.1	<0 2 || 10 - 1 || <=3 >=0 =0	npm	0
3.10.1	0 || 1 - 1	npm	0
0.3.2-beta.11	>=0	npm	0
1.10.0-alpha	>3 0 || 3 - 3	npm	0
0.1.0-beta.2	^1 || 0 - 0	npm	0
10.1.2-alpha	>=0 || 2 - 3 || <=10 ^0 >0	npm	0
0.2.2-0	=1.3 || >0 ~3 3 || ~1	npm	0
0.3.0	0 || 10 || >0 ^3	npm	1
3.2.2-0	1 - 0 || ^2 || 1 ^0	npm	0
0.1.3-beta.2	^10 >0 =10 || 2 - 1	npm	0
1.3.3-beta.11	2 - 2 || >3 || 1 - 10	npm	0
3.10.0	1 - 2	npm	0
1.1.1-rc.1	3 - 2 || ~3 =3	npm	0
0.3.10	10 >=0 ~1	npm	0
1.0.1-rc.1	=0 ^2 || >=3 <=2 <0 || 1 - 1	npm	0
10.3.3-beta.11	~10 >=2 <0	npm	0
10.0.0-0	0 =0 || >2 10 || 2 - 0	npm	0
10.1.3-rc.1	>=10 <=1 || <=1	npm	0
3.10.1	=0 =3 <=2	npm	0
0.0.0-beta.11	^1 >=1	npm	0
3.1.0	1 - 2 || =2 =1 ^1 || ~1	npm	0
0.0.0	=1 || <2	npm	1
0.1.3-rc.1	>3 =0 ~1 || >1 10 || <=0 >=0	npm	0
0.3.0-alpha	^10	npm	0
0.1.10-0	^1 ^2 || <=0 || 1 - 0	npm	0
10.0.2	0.2.3	cargo	0
1.0.0-0	1.2.3	cargo	0
1.0.0	=1.2.3-beta.2	cargo	0
3.1.1-beta.2	*	cargo	0
2.1.1-beta.2	1.2.3-beta.2, <2	cargo	0
3.2.1-alpha.1	1.2.3, >=1.2	cargo	0
3.2.3-alpha.1	0, <2	cargo	0
10.1.3	*	cargo	1
2.1.0-rc.1	=1	cargo	0
3.1.2-beta.2	0.2.3, <=1.2.3	cargo	0
10.3.3-rc.1	1.2.3	cargo	0
2.1.0-beta.11	0.2.3	cargo	0
3.10.0-beta.11	0.2.3	cargo	0
1.0.0-alpha.1	1.2.3, >=1.2	cargo	0
0.0.1	=0	cargo	1
1.10.3-0	=1	cargo	0
1.1.0	=1.x	cargo	1
10.1.0-alpha	*	cargo	0
1.10.3	=0	cargo	0
1.2.10-alpha	1	cargo	0
10.1.1-0	*	cargo	0
3.2.0-beta.2	1	cargo	0
3.3.2	=1.2.3-beta.2	cargo	0
10.0.1	=*	cargo	1
0.3.1-rc.1	1, <2	cargo	0
10.2.0-beta.2	1.2.3-beta.2, <2	cargo	0
1.1.0-0	=1.2.3	cargo	0
1.1.2-beta.2	1.x, <2	cargo	0
0.1.0	0.2.3, <=1.2.3	cargo	0
0.0.0	=1	cargo	0
0.1.1	0, >=1.2	cargo	0
2.0.1	=0.2.3	cargo	0
1.1.0-beta.11	0	cargo	0
2.1.2	1	cargo	0
0.3.2-rc.1	=0	cargo	0
1.1.10	=0.2.3	cargo	0
1.0.3-beta.2	1.2.3	cargo	0
2.2.3-rc.1	=0.2.3	cargo	0
2.1.2	0.2.3	cargo	0
0.0.0	1.x	cargo	0
0.10.3-alpha	=1.2.3	cargo	0
10.1.3-0	0	cargo	0
10.3.3-alpha	=1.x	cargo	0
0.0.10	0.2.3, >=1.2	cargo	0
2.10.0-rc.1	1.x, <2	cargo	0
1.2.0-beta.2	1.2.3, <=1.2.3	cargo	0
2.0.3-alpha	=*	cargo	0
3.0.2-beta.2	0	cargo	0
1.1.1-rc.1	=1.x	cargo	0
2.0.3-beta.11	=1	cargo	0
3.2.0	*	cargo	1
1.10.0	=1.2.3-beta.2	cargo	0
1.0.0-alpha	=0	cargo	0
0.10.2	=1.2.3	cargo	0
0.10.0-beta.2	1.2.3, >=1.2	cargo	0
0.3.3-0	=0.2.3	cargo	0
0.1.2-rc.1	=1	cargo	0
0.1.1-alpha.1	1.2.3-beta.2	cargo	0
0.10.3	0.2.3	cargo	0
0.1.1	1.x, <2	cargo	0
1.2.2-alpha	=*	cargo	0
1.1.2-alpha.1	1	cargo	0
1.1.0-alpha	0, <=1.2.3	cargo	0
0.3.0-alpha.1	=1	cargo	0
0.3.0-0	=1.x	cargo	0
0.0.0-alpha	0.2.3	cargo	0
10.3.0-alpha	1.2.3-beta.2, <2	cargo	0
0.3.3-beta.2	1.2.3-beta.2	cargo	0
1.3.1-alpha	=0	cargo	0
0.3.2-beta.2	=1.x	cargo	0
10.1.1-beta.2	=1.x	cargo	0
1.2.0-alpha	1.2.3, <=1.2.3	cargo	0
10.3.1-beta.2	1.2.3, <=1.2.3	cargo	0
10.2.1	1.2.3-beta.2, >=1.2	cargo	0
2.3.10-0	0.2.3, <2	cargo	0
2.10.1-alpha.1	0	cargo	0
1.1.10-rc.1	*	cargo	0
1.1.1	1.x	cargo	1
1.3.0	1.2.3-beta.2	cargo	1
0.0.2-beta.2	0.2.3, >=1.2	cargo	0
1.1.0	*	cargo	1
0.0.0-beta.11	1.2.3, >=1.2	cargo	0
3.3.10	=1.x	cargo	0
3.10.0-beta.2	1, <2	cargo	0
1.1.0	=1.x	cargo	1
1.0.0	*	cargo	1
3.1.0	1	cargo	0
1.0.2-rc.1	=1	cargo	0
0.3.10-alpha.1	1.2.3-beta.2, <2	cargo	0
1.0.0-beta.11	1.x, <=1.2.3	cargo	0
1.0.0-0	=1.2.3	cargo	0
2.1.1-0	=1.2.3	cargo	0
10.0.0	=1.2.3	cargo	0
0.1.3-alpha	0	cargo	0
2.0.3-alpha.1	0, <2	cargo	0
3.1.2-alpha	*	cargo	0
1.0.0	*, <2	cargo	1
0.0.0	0, >=1.2	cargo	0
1.1.0	*, <=1.2.3	cargo	1
0.0.1-0	1.2.3-beta.2	cargo	0
0.0.1-alpha	=1.2.3-beta.2	cargo	0
0.1.1	1.x, >=1.2	cargo	0
10.0.1-beta.11	1.2.3-beta.2, >=1.2	cargo	0
3.0.1-alpha.1	0	cargo	0
0.1.2-alpha.1	0.0	cargo	0
10.0.0	1.2.3-beta.2	cargo	0
0.1.10-0	1.2.3-beta.2	cargo	0
10.1.3	*, <2	cargo	0
1.1.10	0.2.3, >=1.2	cargo	0
0.1.0-alpha.1	0, <=1.2.3	cargo	0
1.1.0-alpha.1	*, <=1.2.3	cargo	0
1.0.0-alpha.1	0.2.3, >=1.2	cargo	0
2.3.1	1.2.3-beta.2	cargo	0
1.10.3-beta.2	1, >=1.2	cargo	0
2.3.0-alpha	*	cargo	0
2.1.0	1.2.3-beta.2	cargo	0
1.0.0-beta.11	1	cargo	0
3.10.10-alpha.1	=1.2.3	cargo	0
0.1.3	*, >=1.2	cargo	0
1.1.1	=1.2.3	cargo	0
1.1.0	<0 <10 >=0 || ~1 || 1 - 1	npm	1
1.2.2	1 - 1 || 2 - 2 || >=0 1 >1	npm	1
0.0.3	^3 || ~0 =0 <1	npm	1
10.1.0	>=0 ~10	npm	1
1.0.3	0 10 || ^2 ^0 >10 || =1	npm	1
3.0.3	>2 >=0	npm	1
0.0.2	0 - 0	npm	1
0.10.0	2 - 0 || <1 <=0	npm	1
0.10.3	10 - 0 || >2 0 || <=1	npm	1
2.1.0	2 - 0 || 0 - 2 || 1 - 0	npm	1
1.1.1	0 - 1	npm	1
1.0.1	>=1 0 || >=10 >=0 ^0 || 1 >0	npm	1
3.10.10	<=3 ~3 >0	npm	1
1.0.1	1 - 0 || 0 - 1 || 0 - 0	npm	1
10.1.0	0 - 10	npm	1
0.3.2	1 - 3 || <3	npm	1
0.2.1	>=0 >3 || =0	npm	1
3.0.0	<=1 ^3 10 || >=3 <=10	npm	1
2.2.3	<=2	npm	1
0.10.2	>=0 0 <=2 || 2 || <=0	npm	1
10.3.0	<3 <3 >=10 || 10 || 1	npm	1
0.1.1	<1 || 0 - 0	npm	1
1.0.1	2 - 0 || 10 >1 || 1 - 1	npm	1
1.1.3	>=0 <=2 || <=1 ~1 <0	npm	1
2.0.1	<=0 || 2 - 0 || 2 <10 >=0	npm	1
0.0.1	^0 || =3 ^2 =0	npm	1
1.1.0	0 ^1 || ^1 || >=0	npm	1
1.3.0	1 - 1	npm	1
1.2.1	>=0 <=1 >3 || ~1 <=2	npm	1
0.10.0	0 || <=1 ^1 ^3	npm	1
0.0.2	<10 <3 0 || 3 <2 || 0	npm	1
1.10.0	10 >1 || 1 - 2	npm	1
1.10.1	0 - 0 || ^1 || 0 - 3	npm	1
3.0.0	>=0 || ~0 >=0 || <=0	npm	1
1.0.1	>=1 1 || ^0 3 <1	npm	1
10.2.0	2 || >1	npm	1
1.0.2	1 - 10	npm	1
3.10.0	>1 >=3 >3 || >1	npm	1
1.1.3	0 - 2	npm	1
1.0.0	10 >10 || 1 || 10	npm	1
1.0.1	10 - 0 || 1 - 1 || <0	npm	1
3.1.0	1 - 3 || >1 || 10 - 2	npm	1
1.0.2	1 - 1	npm	1
0.0.2	3 - 0 || 0 - 1 || 1 - 3	npm	1
3.3.2	<=1 0 ^10 || 0 0 || >=1	npm	1
10.0.10	~3 >2 ~10 || <1 >=10 ^2 || 10 - 10	npm	1
1.0.1	>=0 ~1 || <0 || 1 - 3	npm	1
0.2.0	=0 >=3 >=1 || 0 - 0 || >=10 ~10 >3	npm	1
1.0.3	10 - 3 || 0 - 1 || ^1	npm	1
1.10.3	1 <1 >0 || 0 - 1 || 0 >=1	npm	1
1.1.3	0 - 1 || 0	npm	1
1.2.0	1 - 1	npm	1
1.10.1	1 - 1 || =3 >=2 <2 || 1 - 2	npm	1
1.3.2	3 - 3 || <1 ~0 >=0 || 0 - 3	npm	1
1.1.1	>=0 <=2 ~3 || =1 || ^1 >1 1	npm	1
3.0.2	>0 || >=0 >3 <=0	npm	1
10.3.10	>=3 || <0 1	npm	1
3.0.1	>=0 >=3	npm	1
1.2.0	1 - 1	npm	1
2.1.0	<=2	npm	1
0.1.3	<=1 0 || =0 >=0 || <0 ^1 1	npm	1
10.0.0	>1 || <3 >1	npm	1
0.2.0	10 - 0 || <=3 >=0 0 || >=0 >=3 10	npm	1
3.1.2	3 - 10 || ^0 2	npm	1
10.10.1	>1	npm	1
0.0.2	10 - 1 || ~0 <3	npm	1
2.1.0	>=1 >=2 || =1 =0	npm	1
2.1.1	~1 ^0 || 0 - 2 || <=1 >=10 >0	npm	1
0.1.0	~0 <2	npm	1
1.0.10	1 - 1 || ~0 ~2 ^2	npm	1
0.3.3	0 - 3	npm	1
3.10.3	<=10	npm	1
0.2.0	>=0 <=0	npm	1
1.3.2	^1	npm	1
0.1.3	<=0 || 0 - 3	npm	1
3.0.10	^0 || 0 - 3	npm	1
1.0.0	1 - 1	npm	1
2.10.3	>10 10 || =2 >=0 || 2 >=2 =10	npm	1
1.10.2	1 - 3 || 10 || 1 <=1	npm	1
10.2.3	0 =1 0 || >1	npm	1
3.0.10	<=0 ~10 || =3	npm	1
10.2.0	10	npm	1
3.0.0	0 =0 ^1 || 2 - 2 || <=10	npm	1
0.0.10	<1 || 1 - 1 || =3 >0	npm	1
0.0.3	1 >2 =3 || >2.10 =1 || 0 - 1	npm	1
1.10.0	0 - 2 || >2 <=1 >=0 || >2 ~0	npm	1
0.3.1	0 - 0 || 0 >10	npm	1
1.3.0	>=3 ~2 || ~1	npm	1
1.10.0	10 - 3 || ^1	npm	1
0.2.0	>=0 || 0	npm	1
0.1.0	~0 || <0	npm	1
1.10.2	>0 || 2 - 1	npm	1
10.1.3	>=1 >3	npm	1
2.0.3	1 - 3 || >2 >=3 3	npm	1
1.1.10	=1 || 0 >10 || ~1	npm	1
10.2.2	>=0 || >1 >=2 || ~0	npm	1
1.1.1	<=2 ~1	npm	1
1.1.0	3 - 10 || 0 - 2	npm	1
1.0.1	0 - 1 || 2 - 3	npm	1
0.1.1	>3 1 ~0 || 0 - 0 || <=10 >1 >=0	npm	1"##;

const YAML_PRELUDE: &str = r##"
const unnorm = v => {
  if (v && typeof v === 'object' && !Array.isArray(v)) {
    if ('$big' in v) return BigInt(v.$big);
    if ('$num' in v) return v.$num === 'NaN' ? NaN : v.$num === 'Infinity' ? Infinity : -Infinity;
    const o = {};
    for (const k of Object.keys(v)) Object.defineProperty(o, k, {value: unnorm(v[k]), enumerable: true, writable: true, configurable: true});
    return o;
  }
  return Array.isArray(v) ? v.map(unnorm) : v;
};
const norm = v => {
  if (typeof v === 'bigint') return {$big: v.toString()};
  if (typeof v === 'number') return v !== v ? {$num: 'NaN'} : v === Infinity ? {$num: 'Infinity'} : v === -Infinity ? {$num: '-Infinity'} : v === 0 ? 0 : v;
  if (Array.isArray(v)) return v.map(norm);
  if (v && typeof v === 'object') {
    const o = {};
    for (const k of Object.keys(v)) Object.defineProperty(o, k, {value: norm(v[k]), enumerable: true, writable: true, configurable: true});
    return o;
  }
  return v;
};
const canon = v => JSON.stringify(v, (k, x) => x && typeof x === 'object' && !Array.isArray(x)
  ? Object.keys(x).sort().reduce((a, kk) => { a[kk] = x[kk]; return a }, {}) : x);
const name = f => { try { f(); return 'no throw' } catch (e) { return e.name + ': ' + e.message } };
"##;
const YAML_ORACLE_CASES: &str = include_str!("yaml_oracle_cases.json");

fn yaml_code(body: &str) -> String {
    format!("const cases = {YAML_ORACLE_CASES};\n{YAML_PRELUDE}\n{body}")
}

#[tokio::test]
async fn yaml_helpers_are_listed_by_help_and_installed_in_both_profiles() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let catalogue = run_js(&tool, "help()").await;
        for name in ["yaml_parse", "yaml_stringify"] {
            assert!(catalogue.contains(name), "{profile:?} {catalogue}");
            assert_eq!(
                run_js(&tool, &format!("typeof {name}")).await,
                "function",
                "{profile:?} {name}"
            );
        }
        let usage = run_js(&tool, "help('yaml_parse')").await;
        assert!(
            usage.contains("tags") && usage.contains("BigInt"),
            "{usage}"
        );
    }
}

#[tokio::test]
async fn yaml_parse_matches_a_yaml_12_core_schema_oracle() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        // 43 hand-written documents (Kubernetes, compose, GitHub Actions, block scalars, anchors,
        // merge keys, flow collections, big integers, the Norway problem) plus 150 documents
        // emitted by PyYAML in assorted styles; expected values come from PyYAML loaded with a
        // YAML 1.2 core resolver (ints/floats/bools/null per the 1.2 spec, no timestamps).
        let out = run_js(&tool, &yaml_code(YAML_ORACLE_BODY)).await;
        assert_eq!(out, "0:193:", "{profile:?}");
    }
}

#[tokio::test]
async fn yaml_stringify_round_trips_and_quotes_what_would_change_type() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let out = run_js(&tool, &yaml_code(YAML_ROUNDTRIP_BODY)).await;
        let (head, text) = out.split_once('\n').unwrap_or((&out, ""));
        assert_eq!(
            head, "0:150::true:true:1970-01-01T00:00:00.000Z",
            "{profile:?}"
        );
        assert_eq!(text, format!("{YAML_STRINGIFY_TEXT}\n"), "{profile:?}");
    }
}

#[tokio::test]
async fn yaml_parse_throws_with_line_numbers_on_unsupported_or_invalid_syntax() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let out = run_js(&tool, &yaml_code(YAML_ERRORS_BODY)).await;
        assert_eq!(out, YAML_ERRORS_EXPECTED, "{profile:?}");
        assert!(!out.contains("no throw"));
    }
}

#[tokio::test]
async fn yaml_parse_options_documents_and_exact_integers() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let out = run_js(&tool, &yaml_code(YAML_OPTIONS_BODY)).await;
        assert_eq!(out, YAML_OPTIONS_EXPECTED, "{profile:?}");
    }
}

#[tokio::test]
async fn yaml_helpers_are_bounded_on_alias_bombs_deep_nesting_and_large_inputs() {
    let root = std::env::current_dir().unwrap();
    for profile in PROFILES {
        let tool = helper_tool(profile, &root);
        let started = std::time::Instant::now();
        let out = run_js(&tool, &yaml_code(YAML_BOUNDED_BODY)).await;
        assert_eq!(
            out,
            "RangeError,RangeError,object,RangeError,RangeError,RangeError,object,RangeError,object,SyntaxError,SyntaxError,SyntaxError,RangeError,RangeError,TypeError,TypeError,TypeError,RangeError",
            "{profile:?}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{profile:?} {:?}",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn yaml_helpers_handle_large_inputs_within_their_caps() {
    let root = std::env::current_dir().unwrap();
    let tool = helper_tool(ModelEffectProfile::Full, &root);
    let started = std::time::Instant::now();
    let out = run_js(&tool, &yaml_code(YAML_HEAVY_BODY)).await;
    assert_eq!(
        out,
        "object,RangeError,object,object,object,SyntaxError,SyntaxError,object,object,object,object,object,object,string,string"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

const YAML_ORACLE_BODY: &str = r##"
const bad = [];
for (const [id, text, all, expect] of cases) {
  let got;
  try { got = canon(norm(yaml_parse(text, {all}))) } catch (e) { got = 'ERR ' + e.message }
  if (got !== canon(expect)) bad.push(id + ' => ' + got.slice(0, 80));
}
bad.length + ':' + cases.length + ':' + bad.slice(0, 3).join(';')"##;
const YAML_ROUNDTRIP_BODY: &str = r##"
const bad = [];
let n = 0;
for (const [id, text, all, expect] of cases) {
  if (!id.startsWith('fuzz')) continue;
  n++;
  const v = unnorm(expect);
  const out = yaml_stringify(v);
  if (canon(norm(yaml_parse(out))) !== canon(expect)) bad.push(id);
}
const shape = {replicas: 3, ports: [80, 443], image: 'nginx:1.25', empty: [], none: {}, nested: [{a: 1, b: [2, 3]}, [4, [5]]],
  yes: 'true', num: '12', nul: 'null', blank: '', colon: 'a: b', hash: 'a #b', lead: ' x', multi: 'l1\nl2\n', keep: 'k\n\n', strip: 'a\nb',
  big: 12345678901234567890n, f: 1.5, neg0: -0, inf: Infinity, nan: NaN, flag: false, skip: undefined, 'sp key': 1, '': 2, '<<': 3, t: new Date(0),
  uni: 'caf\u00e9 \u4e2d', ctrl: 'a\u0001b\u007f', q: "it's \"q\"", tilde: '~', dot: '.5', hex: '0x1f', url: 'http://x.y:80/p'};
const text = yaml_stringify(shape);
const back = yaml_parse(text);
const same = ['replicas', 'image', 'nested', 'yes', 'num', 'nul', 'blank', 'colon', 'hash', 'lead', 'multi', 'keep', 'strip', 'big', 'f', 'inf', 'flag', 'sp key', '', '<<', 'uni', 'ctrl', 'q', 'tilde', 'dot', 'hex', 'url']
  .filter(k => canon(norm(back[k])) !== canon(norm(shape[k])));
bad.length + ':' + n + ':' + same.join(',') + ':' + (Object.is(back.neg0, 0) || Object.is(back.neg0, -0)) + ':' + (back.nan !== back.nan) + ':' + back.t + '\n' + text"##;
const YAML_STRINGIFY_TEXT: &str = r##"replicas: 3
ports:
  - 80
  - 443
image: nginx:1.25
empty: []
none: {}
nested:
  - a: 1
    b:
      - 2
      - 3
  - - 4
    - - 5
yes: "true"
num: "12"
nul: "null"
blank: ""
colon: "a: b"
hash: "a #b"
lead: " x"
multi: |
  l1
  l2
keep: |+
  k

strip: |-
  a
  b
big: 12345678901234567890
f: 1.5
neg0: -0.0
inf: .inf
nan: .nan
flag: false
sp key: 1
"": 2
"<<": 3
t: 1970-01-01T00:00:00.000Z
uni: café 中
ctrl: "a\x01b\x7f"
q: it's "q"
tilde: "~"
dot: ".5"
hex: "0x1f"
url: http://x.y:80/p"##;
const YAML_ERRORS_BODY: &str = r##"
const must = [
  'a: !!str 5\n', 'a: !Ref x\n', '? a\n: b\n', 'a: 1\n? b\n: 2\n', 'a:\n\tb: 1\n', 'a:\n\t- 1\n', '%YAML 1.2\n---\na: 1\n',
  'a: 1\na: 2\n', 'a: {b: 1, b: 2}\n', 'a: *nope\n', '&a x: 1\n', 'a: &x 1\n*x : 2\n', 'a: b: c\n', 'a: - b\n', 'a:\n  b: 1\n c: 2\n',
  'a: "abc\n', 'a: [1, 2\n', 'a: {b: 1\n', 'a: [b: 1]\n', 'a: 1\n---\nb: 2\n', 'a: 1\n<<: 5\n', 'a: "\\q"\n', '[a]: 1\n', 'a: [1]x\n',
  'a: @x\n', 'a: [,]\n', 'a: 1\n  b: 2\n', 'a: "x" y\n', 'a: ' + '9'.repeat(400) + '\n', '['.repeat(100) + ']'.repeat(100), 'a: &x &y 1\n', 'a: 1\n- b\n',
  'a: |\n  x\n b: 1\n', "a: 'x\n", 'a: "\\ud800x"\n'.replace('ud800', 'U00110000'),
];
must.map(t => name(() => yaml_parse(t))).join('\n')"##;
const YAML_ERRORS_EXPECTED: &str = r##"SyntaxError: yaml_parse: tags (!) are not supported (line 1)
SyntaxError: yaml_parse: tags (!) are not supported (line 1)
SyntaxError: yaml_parse: complex mapping keys are not supported (line 1)
SyntaxError: yaml_parse: complex mapping keys are not supported (line 2)
SyntaxError: yaml_parse: tab characters are not allowed for indentation (line 2)
SyntaxError: yaml_parse: tab characters are not allowed for indentation (line 2)
SyntaxError: yaml_parse: directives (%YAML, %TAG) are not supported (line 1)
SyntaxError: yaml_parse: duplicate mapping key (line 2)
SyntaxError: yaml_parse: duplicate mapping key (line 1)
SyntaxError: yaml_parse: alias refers to an undefined anchor (line 1)
SyntaxError: yaml_parse: an anchor before a mapping key is not supported; put it on its own line (line 1)
SyntaxError: yaml_parse: an alias used as a mapping key is not supported (line 2)
SyntaxError: yaml_parse: mapping values are not allowed here (line 1)
SyntaxError: yaml_parse: a block sequence is not allowed here (line 1)
SyntaxError: yaml_parse: bad indentation of a mapping entry (line 3)
SyntaxError: yaml_parse: unterminated quoted scalar starting (line 1)
SyntaxError: yaml_parse: unterminated flow collection starting (line 1)
SyntaxError: yaml_parse: unterminated flow collection starting (line 1)
SyntaxError: yaml_parse: single-pair entries in flow sequences are not supported (line 1)
SyntaxError: yaml_parse: the text has 2 documents; pass {all: true}
SyntaxError: yaml_parse: the << merge key needs a mapping or a list of mappings (line 2)
SyntaxError: yaml_parse: invalid escape sequence in a double-quoted scalar (line 1)
SyntaxError: yaml_parse: a flow collection used as a mapping key is not supported (line 1)
SyntaxError: yaml_parse: unexpected content after a flow collection (line 1)
SyntaxError: yaml_parse: a plain scalar cannot start with '@' (line 1)
SyntaxError: yaml_parse: empty flow entry (line 1)
SyntaxError: yaml_parse: mapping values are not allowed in this context (line 2)
SyntaxError: yaml_parse: unexpected content after a quoted scalar (line 1)
RangeError: yaml_parse: integer literal is too long
RangeError: yaml_parse: nesting deeper than 64
SyntaxError: yaml_parse: a node cannot have two anchors (line 1)
SyntaxError: yaml_parse: unexpected sequence entry inside a mapping (line 2)
SyntaxError: yaml_parse: bad indentation of a mapping entry (line 3)
SyntaxError: yaml_parse: unterminated quoted scalar starting (line 1)
SyntaxError: yaml_parse: invalid escape sequence in a double-quoted scalar (line 1)"##;
const YAML_OPTIONS_BODY: &str = r##"
[
  JSON.stringify(yaml_parse('a: 1\n---\nb: 2\n', {all: true})), JSON.stringify(yaml_parse('---\na: 1\n---\n', {all: true})),
  JSON.stringify(yaml_parse('', {all: true})), JSON.stringify(yaml_parse('# c\n')), JSON.stringify(yaml_parse('---\n')),
  JSON.stringify(yaml_parse('a: 1\n...\n')), JSON.stringify(yaml_parse('x', {})), JSON.stringify(yaml_parse('- 1\n- 2\n', undefined)),
  name(() => yaml_parse(5)), name(() => yaml_parse('a', 'all')), name(() => yaml_parse('a', null)), name(() => yaml_parse('#'.repeat(4 * 1024 * 1024 + 1))),
  Object.getPrototypeOf(yaml_parse('__proto__: 1')) === Object.prototype, Object.keys(yaml_parse('__proto__: 1')).join(),
  typeof yaml_parse('a: 12345678901234567890\n').a, typeof yaml_parse('a: 9007199254740991\n').a, typeof yaml_parse('a: 9007199254740992\n').a,
  String(yaml_parse('a: -9007199254740993\n').a), yaml_parse('a: 0o17\nb: 0x1F\nc: 007\nd: yes\ne: .NaN\n').d,
].join('\n')"##;
const YAML_OPTIONS_EXPECTED: &str = r##"[{"a":1},{"b":2}]
[{"a":1}]
[]
null
null
{"a":1}
"x"
[1,2]
TypeError: yaml_parse: text must be a string
TypeError: yaml_parse: options must be an object like {all: true}
TypeError: yaml_parse: options must be an object like {all: true}
RangeError: yaml_parse: input exceeds 4194304 characters
true
__proto__
bigint
number
bigint
-9007199254740993
yes"##;
const YAML_BOUNDED_BODY: &str = r##"
const t = (f) => { try { const r = f(); return typeof r } catch (e) { return e.name } };
const bomb = (levels, fan) => {
  let s = 'a0: &a0 [' + Array(fan).fill('x').join(', ') + ']\n';
  for (let k = 1; k < levels; k++) s += 'a' + k + ': &a' + k + ' [' + Array(fan).fill('*a' + (k - 1)).join(', ') + ']\n';
  return s;
};
const wide = Array.from({length: 1000}, (_, i) => 'k' + i + ': 1').join(', ');
[
  t(() => yaml_parse(bomb(10, 9))), t(() => yaml_parse(bomb(60, 2))), t(() => yaml_parse(bomb(3, 9))),
  t(() => yaml_parse('b: &b {' + wide + '}\n' + Array.from({length: 300}, (_, i) => 'c' + i + ': {<<: *b}').join('\n'))),
  t(() => yaml_parse(Array.from({length: 100}, (_, i) => ' '.repeat(i) + 'a:').join('\n') + ' 1\n')),
  t(() => yaml_parse('['.repeat(100) + ']'.repeat(100))), t(() => yaml_parse('['.repeat(60) + ']'.repeat(60))),
  t(() => yaml_parse('- '.repeat(100) + 'x')), t(() => yaml_parse('- '.repeat(30) + 'x')),
  t(() => yaml_parse('a: [' + '{'.repeat(70) + '}'.repeat(70) + ']')),
  t(() => yaml_parse('a: ' + '"'.repeat(20000))), t(() => yaml_parse('a: ' + 'b:'.repeat(20000))),
  t(() => yaml_stringify((() => { let v = []; for (let i = 0; i < 100; i++) v = [v]; return v })())),
  t(() => yaml_stringify((() => { const o = {}; o.o = o; return o })())), t(() => yaml_stringify(() => 1)), t(() => yaml_stringify(Symbol('s'))),
  t(() => yaml_stringify(new Map())), t(() => yaml_stringify({d: new Date(NaN)})),
].join(',')"##;
const YAML_HEAVY_BODY: &str = r##"
const t = (f) => { try { const r = f(); return typeof r } catch (e) { return e.name } };
[
  t(() => yaml_parse('#'.repeat(4 * 1024 * 1024))), t(() => yaml_parse('- 1\n'.repeat(250000))),
  t(() => yaml_parse('- 1\n'.repeat(50000))), t(() => yaml_parse('a: ' + 'x '.repeat(100000))),
  t(() => yaml_parse('a: x\n' + '  y\n'.repeat(50000))), t(() => yaml_parse('a: ' + 'b:'.repeat(100000))),
  t(() => yaml_parse('a: ' + '"'.repeat(100000))), t(() => yaml_parse("a: '" + "''".repeat(100000) + "'")),
  t(() => yaml_parse('[' + 'a,'.repeat(50000) + ']')), t(() => yaml_parse('a: |\n' + '  line of text here\n'.repeat(30000))),
  t(() => yaml_parse('a: >\n' + '\n'.repeat(100000))), t(() => yaml_parse('a: ' + ' #'.repeat(100000))),
  t(() => yaml_parse('---\na: 1\n'.repeat(10000), {all: true})),
  t(() => yaml_stringify({s: 'a\n'.repeat(100000)})), t(() => yaml_stringify(Array.from({length: 50000}, (_, i) => ({id: i})))),
].join(',')"##;

/// Manual measurement (`cargo test -- --ignored --nocapture first_access_cost`): time to install
/// the catalogue and to evaluate each data segment on first access, in a bare QuickJS context.
#[test]
#[ignore]
fn first_access_cost_of_each_data_segment() {
    let runtime = rquickjs::Runtime::new().unwrap();
    let context = rquickjs::Context::full(&runtime).unwrap();
    context.with(|ctx| {
        let started = std::time::Instant::now();
        crate::extras::js::pack::install(&ctx).unwrap();
        eprintln!("install (all catalogues): {:?}", started.elapsed());
        for probe in [
            "path",
            "tally",
            "stats",
            "semver_cmp",
            "semver_sat",
            "yaml_parse",
            "yaml_stringify",
        ] {
            let started = std::time::Instant::now();
            let kind: String = ctx.eval(format!("typeof {probe}")).unwrap();
            eprintln!("first access {probe}: {kind} in {:?}", started.elapsed());
        }
    });
}
