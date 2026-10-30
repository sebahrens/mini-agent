//! Tests of the diff and IaC pack segments through the real tool, in both effect profiles.

use super::*;

fn tool_for(profile: ModelEffectProfile) -> JsTool {
    let supervisor = Arc::new(JsWorkerSupervisor::with_launcher_for_test(
        crate::sandbox::worker::TestWorkerLauncher::internal_worker_process(),
    ));
    let root = std::env::current_dir().unwrap();
    let mut tool = JsTool::new_with_runtime_for_test(
        Sandbox::new(false, "bwrap"),
        None,
        None,
        AllowConfig::unrestricted(&root),
        supervisor,
        shared_effect_audit().expect("test effect audit"),
    );
    tool.profile = profile;
    tool
}

async fn run(tool: &JsTool, code: &str) -> String {
    use rig::tool::Tool;
    tool.call(JsArgs { code: code.into() })
        .await
        .unwrap_or_else(|error| format!("TOOL ERROR: {error}"))
}

/// Run `code` in both effect profiles and assert it evaluates to `expected`.
async fn expect_both(code: &str, expected: &str) {
    for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
        let tool = tool_for(profile);
        assert_eq!(run(&tool, code).await, expected, "{profile:?}");
    }
}

#[tokio::test]
async fn diff_lines_produces_minimal_unified_diffs() {
    let code = r##"
        const bad = [];
        const eq = (label, got, want) => { if (got !== want) bad.push(label + ': ' + JSON.stringify(got)); };
        eq('basic', diff_lines('a\nb\nc\n', 'a\nx\nc\n'), '--- a\n+++ b\n@@ -1,3 +1,3 @@\n a\n-b\n+x\n c\n');
        eq('same', diff_lines('a\nb\n', 'a\nb\n'), '');
        eq('empty both', diff_lines('', ''), '');
        eq('from empty', diff_lines('', 'a\nb\n'), '--- a\n+++ b\n@@ -0,0 +1,2 @@\n+a\n+b\n');
        eq('to empty', diff_lines('a\n', ''), '--- a\n+++ b\n@@ -1 +0,0 @@\n-a\n');
        eq('names', diff_lines('x\n', 'y\n', {a_name: 'old.cfg', b_name: 'new.cfg'}).split('\n')[0], '--- old.cfg');
        eq('noeol', diff_lines('a\nb', 'a\nb\n'),
           '--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n');
        eq('crlf differs', diff_lines('a\r\n', 'a\n') !== '', true);
        const lines = Array.from({length: 30}, (_, i) => 'l' + i);
        const far = lines.slice(); far[2] = 'X'; far[25] = 'Y';
        const hunks = (d) => d.split('\n').filter(l => l.startsWith('@@')).join(';');
        eq('hunks', hunks(diff_lines(lines.join('\n') + '\n', far.join('\n') + '\n')), '@@ -1,6 +1,6 @@;@@ -23,7 +23,7 @@');
        const near = lines.slice(); near[2] = 'X'; near[8] = 'Y';
        eq('merged', hunks(diff_lines(lines.join('\n') + '\n', near.join('\n') + '\n')), '@@ -1,12 +1,12 @@');
        eq('ctx0', hunks(diff_lines(lines.join('\n') + '\n', far.join('\n') + '\n', {context: 0})), '@@ -3 +3 @@;@@ -26 +26 @@');
        let seed = 7;
        const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
        const lcs = (a, b) => {
          const dp = Array.from({length: a.length + 1}, () => new Array(b.length + 1).fill(0));
          for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--)
            dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
          return dp[0][0];
        };
        // Full-context diffs: the ' ' and '-' lines are exactly A, the ' ' and '+' lines exactly B,
        // and the number of edits equals n + m - 2 * LCS (so the diff is minimal).
        for (let t = 0; t < 300; t++) {
          const A = Array.from({length: Math.floor(rnd() * 14)}, () => 'abc'[Math.floor(rnd() * 3)]);
          const B = Array.from({length: Math.floor(rnd() * 14)}, () => 'abc'[Math.floor(rnd() * 3)]);
          const a = A.map(x => x + '\n').join(''), b = B.map(x => x + '\n').join('');
          const out = diff_lines(a, b, {context: 100}).split('\n');
          if (a === b) { if (out.join('') !== '') bad.push('equal ' + a); continue; }
          const body = out.slice(3).filter(l => l !== '');
          const sideA = body.filter(l => l[0] !== '+').map(l => l.slice(1)).join('');
          const sideB = body.filter(l => l[0] !== '-').map(l => l.slice(1)).join('');
          const edits = body.filter(l => l[0] === '+' || l[0] === '-').length;
          if (sideA !== A.join('') || sideB !== B.join('')) bad.push('sides ' + a + '|' + b);
          if (edits !== A.length + B.length - 2 * lcs(A, B)) bad.push('not minimal ' + a + '|' + b);
        }
        bad.length ? bad.join(' / ') : 'ok'"##;
    expect_both(code, "ok").await;
}

#[tokio::test]
async fn diff_lines_rejects_bad_input_and_is_bounded() {
    let code = r##"
        const name = (f) => { try { f(); return 'no' } catch (e) { return e.name + ':' + e.message.slice(0, 44) } };
        const big = Array.from({length: 20000}, (_, i) => 'line' + i);
        const out = [
          name(() => diff_lines(1, 'a')), name(() => diff_lines('a', 'b', {context: -1})),
          name(() => diff_lines('a', 'b', {context: 1.5})), name(() => diff_lines('a', 'b', {a_name: 'x\ny'})),
          name(() => diff_lines('a', 'b', 5)),
          name(() => diff_lines('x\n'.repeat(20001), '')),
          name(() => diff_lines('x'.repeat(9 * 1024 * 1024), '')),
          name(() => diff_lines(big.join('\n'), big.map(x => x + 'z').join('\n'))),
          diff_lines(big.join('\n'), big.map((x, i) => i === 10000 ? 'changed' : x).join('\n')).split('\n').length,
          diff_lines(big.join('\n'), big.map((x, i) => i % 20 === 0 ? 'c' + i : x).join('\n')).split('\n').length > 1000,
          diff_lines('x'.repeat(5000000), 'x'.repeat(4999999) + 'y').length > 0,
          diff_lines('\n'.repeat(20000), '\n'.repeat(19999)).split('\n').length,
          diff_lines('ab\n'.repeat(10000), 'ab\n'.repeat(9999) + 'ba\n') === '' ? 'eq' : 'neq',
        ];
        JSON.stringify(out)"##;
    let started = std::time::Instant::now();
    let tool = tool_for(ModelEffectProfile::Full);
    let got = run(&tool, code).await;
    assert!(
        got.contains("RangeError:diff_lines: input exceeds 20000 lines"),
        "{got}"
    );
    assert!(got.contains("more than 2000 lines differ"), "{got}");
    assert!(got.contains("TypeError:diff_lines: a and b"), "{got}");
    assert!(got.ends_with("\"neq\"]"), "{got}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn dockerfile_parse_handles_continuations_heredocs_stages_and_json_form() {
    let code = r##"
        const df = [
          '# syntax=docker/dockerfile:1', '# comment', 'FROM --platform=linux/amd64 node:20-alpine AS build',
          'ARG VERSION=1.2 NAME', 'ENV A=1 B="two words" C=x\\ y', 'ENV LEGACY some legacy value',
          'RUN apt-get update && \\', '    # inner comment', '    apt-get install -y \\', '', '    curl',
          'COPY --from=build /app /app', 'RUN <<EOF', 'echo hi', 'exit 0', 'EOF',
          "RUN <<-'X' cat <<Y", '\t\tbody', 'X', 'y body', 'Y',
          'CMD ["node", "server.js"]', "ENTRYPOINT [ 'a' ]", 'FROM scratch', 'USER 1000',
        ].join('\r\n') + '\r\n';
        const r = dockerfile_parse(df);
        const bad = [];
        const eq = (label, got, want) => { const g = JSON.stringify(got); if (g !== JSON.stringify(want)) bad.push(label + ': ' + g); };
        eq('ops', r.map(e => e.op + '@' + e.line).join(' '),
           'DIRECTIVE@1 FROM@3 ARG@4 ENV@5 ENV@6 RUN@7 COPY@12 RUN@13 RUN@17 CMD@22 ENTRYPOINT@23 FROM@24 USER@25');
        eq('directive', r[0].args, 'syntax=docker/dockerfile:1');
        eq('from', [r[1].image, r[1].as, r[1].platform, r[1].stage], ['node:20-alpine', 'build', 'linux/amd64', 0]);
        eq('from2', [r[11].image, r[11].as, r[11].stage], ['scratch', null, 1]);
        eq('arg', r[2].pairs, [['VERSION', '1.2'], ['NAME', null]]);
        eq('env', r[3].pairs, [['A', '1'], ['B', 'two words'], ['C', 'x y']]);
        eq('legacy', r[4].pairs, [['LEGACY', 'some legacy value']]);
        eq('cont', r[5].args, 'apt-get update &&     apt-get install -y     curl');
        eq('heredoc', r[7].heredocs, [{name: 'EOF', body: 'echo hi\nexit 0\n'}]);
        eq('heredocs2', r[8].heredocs, [{name: 'X', body: 'body\n'}, {name: 'Y', body: 'y body\n'}]);
        eq('json', r[9].args, ['node', 'server.js']);
        eq('not json', r[10].args, "[ 'a' ]");
        eq('backtick', dockerfile_parse('# escape=`\nRUN a `\n b').map(e => e.args), ['escape=`', 'a  b']);
        const err = (t) => { try { dockerfile_parse(t); return 'no' } catch (e) { return e.name + ':' + e.message } };
        eq('unknown', err('\nFOO bar'), 'SyntaxError:dockerfile_parse: line 2: unknown instruction \'FOO\'');
        eq('heredoc open', err('RUN <<EOF\nabc'), "SyntaxError:dockerfile_parse: line 1: unterminated heredoc 'EOF'");
        eq('quote', err('ENV A="x'), 'SyntaxError:dockerfile_parse: line 1: unterminated " quote in ENV');
        eq('from none', err('FROM'), 'SyntaxError:dockerfile_parse: line 1: FROM needs an image');
        eq('type', err(5), 'TypeError:dockerfile_parse: text must be a string');
        eq('big', err('x'.repeat(1024 * 1024 + 1)).slice(0, 10), 'RangeError');
        bad.length ? bad.join(' / ') : 'ok'"##;
    expect_both(code, "ok").await;
}

#[tokio::test]
async fn dockerfile_parse_is_bounded_on_adversarial_input() {
    let code = r##"
        const t0 = Date.now();
        const tries = [
          () => dockerfile_parse('RUN ' + 'a'.repeat(900000)).length,
          () => dockerfile_parse('RUN a \\\n'.repeat(100000)).length,
          () => dockerfile_parse('RUN ' + '<<A'.repeat(100000)),
          () => dockerfile_parse('RUN <<A\n' + 'x\n'.repeat(400000)),
          () => dockerfile_parse('ENV A=' + '"b'.repeat(400000) + '"'),
          () => dockerfile_parse('#'.repeat(100) + ' '.repeat(900000) + '\nFROM x').length,
          () => dockerfile_parse('# a=b\n'.repeat(100000) + 'FROM x').length,
          () => dockerfile_parse('[' .repeat(900000)),
          () => dockerfile_parse('RUN ' + '[' .repeat(900000)).length,
        ];
        const out = tries.map(f => { try { return String(f()) } catch (e) { return e.name } });
        out.join(',') + ':' + (Date.now() - t0 < 20000)"##;
    let tool = tool_for(ModelEffectProfile::Full);
    let started = std::time::Instant::now();
    let got = run(&tool, code).await;
    assert_eq!(
        got,
        "1,1,SyntaxError,SyntaxError,SyntaxError,1,1,SyntaxError,1:true"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[tokio::test]
async fn hcl_blocks_reads_terraform_blocks_attributes_and_literals() {
    let code = r##"
        const tf = [
          '# top comment', 'terraform {', '  required_version = ">= 1.5.0" // inline', '  required_providers {',
          '    aws = {', '      source = "hashicorp/aws"', '    }', '  }', '}', '',
          'provider "aws" {', '  region = var.region', '}', '',
          'variable "n" { default = 3 }', '',
          'resource "aws_instance" "web" {', '  ami = "ami-123"', '  instance_type = var.big ? "m5.large" : "t3.micro"',
          '  enabled = true', '  ratio = 1.5e2', '  none = null',
          '  tags = {', '    Name = "web-${count.index}"', '    Env = "prod # not a comment"', '  }',
          '  user_data = <<-EOT', '    #!/bin/bash', '      echo hi', '    }', '  EOT',
          '  note = <<EOT', 'cost ${var.x}', 'EOT',
          '  /* block', '     comment { */',
          '  ebs_block_device {', '    size = 8', '  }',
          '  lifecycle { prevent_destroy = true }',
          '  cidrs = [', '    "10.0.0.0/8", # comment ]', '    "192.168.0.0/16",', '  ]',
          '  q = "a \\"q\\" \\u00e9 $${x}"', '  __proto__ = "p"',
          '}', '', 'output "id" { value = aws_instance.web.id }',
        ].join('\n') + '\n';
        const r = hcl_blocks(tf);
        const bad = [];
        const eq = (label, got, want) => { const g = JSON.stringify(got); if (g !== JSON.stringify(want)) bad.push(label + ': ' + g); };
        eq('shape', r.map(b => [b.type, b.labels, b.line, b.end]),
           [['terraform', [], 2, 9], ['provider', ['aws'], 11, 13], ['variable', ['n'], 15, 15],
            ['resource', ['aws_instance', 'web'], 17, 47], ['output', ['id'], 49, 49]]);
        eq('nested', r[0].blocks.map(b => [b.type, b.line, b.end]), [['required_providers', 4, 8]]);
        eq('req', r[0].attrs.required_version, {raw: '">= 1.5.0"', kind: 'string', value: '>= 1.5.0', line: 3});
        const a = r[3].attrs;
        eq('expr', [a.instance_type.kind, a.instance_type.raw, 'value' in a.instance_type], ['expr', 'var.big ? "m5.large" : "t3.micro"', false]);
        eq('lits', [a.enabled.value, a.ratio.value, a.none.kind, a.none.value, r[2].attrs.default.value], [true, 150, 'null', null, 3]);
        eq('tags', [a.tags.kind, a.tags.raw.includes('prod # not a comment')], ['expr', true]);
        eq('heredoc dash', a.user_data.value, '#!/bin/bash\n  echo hi\n}\n');
        eq('heredoc template', [a.note.kind, 'value' in a.note], ['heredoc', false]);
        eq('template', r[3].attrs.tags.kind, 'expr');
        eq('string', a.q.value, 'a "q" é ${x}');
        eq('proto key', [Object.keys(a).includes('__proto__'), a['__proto__'].value], [true, 'p']);
        eq('blocks', r[3].blocks.map(b => [b.type, b.line, b.end, Object.keys(b.attrs)]),
           [['ebs_block_device', 37, 39, ['size']], ['lifecycle', 40, 40, ['prevent_destroy']]]);
        eq('list', a.cidrs.raw.split('\n').length, 4);
        eq('tpl', hcl_blocks('x {\n a = "p-${f("q}", {b = 1})}"\n}')[0].attrs.a.kind, 'template');
        eq('tfvars', hcl_blocks('a = 1\nb = "x"\n').map(b => [b.type, Object.keys(b.attrs), b.line, b.end]), [['(top-level)', ['a', 'b'], 1, 2]]);
        eq('empty', hcl_blocks('  # only a comment\n'), []);
        eq('crlf', hcl_blocks('x "l" {\r\n a = 1\r\n}\r\n')[0].attrs.a.value, 1);
        const err = (t) => { try { hcl_blocks(t); return 'no' } catch (e) { return e.name + ':' + e.message } };
        const L = 'SyntaxError:hcl_blocks: line ';
        eq('unterminated block', err('x "a" {'), L + "1: unclosed block (missing '}')");
        eq('unterminated string', err('x {\n a = "abc\n}'), L + '2: unterminated string');
        eq('interp', err('x { a = "${" }'), L + '1: unterminated string');
        eq('heredoc open', err('x {\n a = <<EOT\nfoo\n}'), L + "2: unterminated heredoc 'EOT'");
        eq('comment', err('x {\n\n /* }'), L + '3: unterminated /* comment');
        eq('single quote', err("x { a = 'q' }"), L + '1: unexpected single quote (HCL strings use double quotes)');
        eq('mismatch', err('x { a = [1, 2 }'), L + "1: mismatched '}'");
        eq('dup', err('x {\n a = 1\n a = 2\n}'), L + "3: duplicate attribute 'a'");
        eq('missing', err('x { a = }'), L + "1: missing expression for 'a'");
        eq('label', err('x "a${b}" { }'), L + '1: a block label must be a plain string');
        eq('stray', err('}'), L + "1: unexpected '}'");
        eq('after brace', err('x { } y'), L + '1: expected a newline after the closing brace');
        eq('type', err(1), 'TypeError:hcl_blocks: text must be a string');
        bad.length ? bad.join(' / ') : 'ok'"##;
    expect_both(code, "ok").await;
}

#[tokio::test]
async fn hcl_blocks_is_bounded_on_adversarial_input() {
    let code = r##"
        const t0 = Date.now();
        const tries = [
          () => hcl_blocks('x { a = ' + '['.repeat(100000)),
          () => hcl_blocks('x { a = "' + '${"'.repeat(100000)),
          () => hcl_blocks('x {'.repeat(100000)),
          () => hcl_blocks('x { a = ' + '"a'.repeat(100000)),
          () => hcl_blocks('/*'.repeat(100000)),
          () => hcl_blocks('x { a = <<A\n' + 'l\n'.repeat(200000)),
          () => hcl_blocks('a = "' + 'x'.repeat(900000) + '"').length,
          () => hcl_blocks('x {\n a = 1\n}\n'.repeat(30000)).length,
          () => hcl_blocks('x { a = {'.repeat(500)),
          () => hcl_blocks('# c\n'.repeat(200000)).length,
          () => hcl_blocks('x { a = "${'.repeat(1000)),
          () => hcl_blocks('x'.repeat(900000)),
        ];
        // Stack-depth exhaustion (RangeError) is an equally bounded failure; the stack
        // limit differs per platform (macOS threads are smaller), so fold it into SyntaxError.
        const out = tries.map(f => { try { return String(f()) } catch (e) { return e.name === 'RangeError' ? 'SyntaxError' : e.name } });
        out.join(',') + ':' + (Date.now() - t0 < 20000)"##;
    let tool = tool_for(ModelEffectProfile::Full);
    let started = std::time::Instant::now();
    let got = run(&tool, code).await;
    assert_eq!(
        got,
        "SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,1,30000,SyntaxError,0,SyntaxError,SyntaxError:true"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[tokio::test]
async fn cidr_and_ip_in_do_exact_ipv4_and_ipv6_arithmetic() {
    let code = r##"
        const bad = [];
        const eq = (label, got, want) => { const g = JSON.stringify(got); if (g !== JSON.stringify(want)) bad.push(label + ': ' + g); };
        eq('v4', cidr('10.0.0.0/24'), {version: 4, address: '10.0.0.0', prefix: 24, cidr: '10.0.0.0/24', network: '10.0.0.0',
           broadcast: '10.0.0.255', size: 256, netmask: '255.255.255.0'});
        const host = cidr('10.1.2.3/16');
        eq('hostbits', [host.address, host.cidr, host.network, host.broadcast, host.size], ['10.1.2.3', '10.1.0.0/16', '10.1.0.0', '10.1.255.255', 65536]);
        eq('all', [cidr('0.0.0.0/0').size, cidr('0.0.0.0/0').broadcast, cidr('0.0.0.0/0').netmask], [4294967296, '255.255.255.255', '0.0.0.0']);
        eq('single', [cidr('10.0.0.1').prefix, cidr('10.0.0.1').size, cidr('10.0.0.1/31').size, cidr('10.0.0.1/31').netmask], [32, 1, 2, '255.255.255.254']);
        const v6 = cidr('2001:db8::/32');
        eq('v6', [v6.network, v6.broadcast, v6.size, v6.version, 'netmask' in v6], ['2001:db8::', '2001:db8:ffff:ffff:ffff:ffff:ffff:ffff', '79228162514264337593543950336', 6, false]);
        eq('v6 all', [cidr('::/0').size, cidr('::/0').broadcast], ['340282366920938463463374607431768211456', 'ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff']);
        eq('v6 small', [cidr('2001:db8::/120').size, cidr('::1').size, cidr('::1').address], [256, 1, '::1']);
        eq('v6 text', [cidr('2001:0DB8:0000:0000:0000:0000:0000:0001').address, cidr('::ffff:1.2.3.4').address,
                       cidr('1:0:0:2:0:0:0:3').address, cidr('1:0:0:2:0:0:3:4').address, cidr('1:2:3:4:5:6:7:8').address,
                       cidr('0:0:0:0:0:0:0:0').address, cidr('1::').address],
           ['2001:db8::1', '::ffff:102:304', '1:0:0:2::3', '1::2:0:0:3:4', '1:2:3:4:5:6:7:8', '::', '1::']);
        const cmp = (a, b) => { const c = cidr(a, b); return [c.overlaps, c.contains, c.within].map(Number).join(''); };
        eq('cmp', [cmp('10.0.0.0/8', '10.255.0.0/16'), cmp('10.0.0.0/24', '10.0.0.0/16'), cmp('10.0.0.0/24', '10.0.1.0/24'),
                   cmp('10.0.0.0/25', '10.0.0.128/25'), cmp('10.0.0.0/24', '10.0.0.255'), cmp('10.0.0.0/8', '::/0'),
                   cmp('10.0.0.0/22', '10.0.2.0/24'), cmp('10.0.0.0/24', '10.0.0.0/24'), cmp('fd00::/8', 'fd12:3456::/32')],
           ['110', '101', '000', '000', '110', '000', '110', '111', '110']);
        const inn = (c, ip) => ip_in(c, ip);
        eq('in', [inn('10.0.0.0/8', '10.255.255.255'), inn('10.0.0.0/8', '11.0.0.0'), inn('10.0.0.0/8', '9.255.255.255'),
                  inn('2001:db8::/32', '2001:db8:ffff::1'), inn('2001:db8::/32', '2001:db9::'), inn('0.0.0.0/0', '255.255.255.255'),
                  inn('1.2.3.0/24', '::ffff:1.2.3.4'), inn('10.0.0.0/8', '10.0.0.0/7'), inn('10.0.0.0/8', '10.1.0.0/16'), inn('10.0.0.5', '10.0.0.5')],
           [true, false, false, true, false, true, false, false, true, true]);
        // random IPv4 membership against plain 32-bit arithmetic
        let seed = 11;
        const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
        const u32 = () => Math.floor(rnd() * 4294967296) >>> 0;
        const dotted = (x) => [x >>> 24, (x >>> 16) & 255, (x >>> 8) & 255, x & 255].join('.');
        for (let t = 0; t < 400; t++) {
          const p = Math.floor(rnd() * 33), base = u32();
          const ip = rnd() < 0.5 ? ((base ^ Math.floor(rnd() * 2 ** Math.max(0, 32 - p))) >>> 0) : u32();
          const mask = p === 0 ? 0 : (0xffffffff << (32 - p)) >>> 0;
          const want = ((ip & mask) >>> 0) === ((base & mask) >>> 0);
          if (ip_in(dotted(base) + '/' + p, dotted(ip)) !== want) bad.push('member ' + dotted(base) + '/' + p + ' ' + dotted(ip));
          const c = cidr(dotted(base) + '/' + p);
          if (c.size !== 2 ** (32 - p) || c.network !== dotted((base & mask) >>> 0)) bad.push('info ' + dotted(base) + '/' + p);
        }
        const err = (f) => { try { f(); return 'no' } catch (e) { return e.name } };
        const invalid = ['10.0.0', '10.0.0.256', '010.0.0.1', '10.0.0.0/33', '10.0.0.0/ 8', '10.0.0.0/08', ' 10.0.0.0', '1::2::3', ':::',
                         '1:2:3:4:5:6:7:8:9', 'fe80::1%eth0', '10.0.0.0/', '/8', '', 'g::1', '::ffff:1.2.3', '2001:db8::/129',
                         '1.2.3.4/', '1.2.3.4.5', '1.2.3.4:80', '0x10.0.0.1', '1:2:3:4:5:6:7::8:9', '10.0.0.0/+8', '::1/-1'];
        eq('invalid', invalid.filter(s => err(() => cidr(s)) !== 'SyntaxError'), []);
        eq('types', [err(() => cidr(5)), err(() => ip_in('10.0.0.0/8', null)), err(() => cidr('1'.repeat(101))), err(() => ip_in('1.2.3', '1.2.3.4'))],
           ['TypeError', 'TypeError', 'RangeError', 'SyntaxError']);
        bad.length ? bad.join(' / ') : 'ok'"##;
    expect_both(code, "ok").await;
}

#[tokio::test]
async fn scan_secrets_reports_masked_previews_and_never_a_full_secret() {
    // Fake secrets are assembled from pieces so this file does not trip secret scanners.
    let code = r##"
        const lines = [];
        const expected = [];
        const secrets = [];
        const plant = (rule, text, secret, preview) => {
          lines.push(text);
          secrets.push(secret);
          expected.push([rule, lines.length, preview]);
        };
        const noise = (text) => lines.push(text);
        const aws = 'AK' + 'IA' + 'IOSFODNN7EXAMPLE';
        const gh = 'gh' + 'p_' + 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8';
        const pat = 'github' + '_pat_' + '11ABCDEFG0abcdefghijklmnopqrstuvwxyz0123456789';
        const gl = 'gl' + 'pat-' + 'abcdefghij0123456789';
        const slack = 'xo' + 'xb-' + '123456789012-1234567890123-abcdefghijklmnopqrstuvwx';
        const jwt = 'ey' + 'JhbGciOiJIUzI1NiJ9' + '.' + 'ey' + 'JzdWIiOiIxMjM0NTY3ODkwIn0' + '.' + 'SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV';
        const gkey = 'AI' + 'za' + 'SyA-9876543210abcdefghijklmnopqrstu';
        const stripe = 'sk' + '_li' + 've_4eC39HqLyjWDarjtT1zdp7dc';
        const npm = 'np' + 'm_' + 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8';
        const ant = 'sk-' + 'ant-' + 'api03-abcdefghijklmnopqrstuvwxyz0123456789';
        const oai = 'sk-' + 'proj-' + 'abcdefghijklmnopqrstuvwxyz0123456789';
        const pypi = 'py' + 'pi-' + 'AgEIcHlwaS5vcmcCJGU3abcdefghijklmnopqrstuvwxyz0123456789ABCD';
        const pw = 'Sup3r' + 'S3cretPw';
        const pem = '-----BEGIN ' + 'RSA PRIVATE KEY' + '-----';
        noise('# config');
        plant('aws_access_key_id', 'aws_access_key_id = ' + aws, aws, 'AKIA... (20 chars)');
        noise('plain line');
        plant('github_token', 'export GH=' + gh, gh, 'ghp_... (40 chars)');
        plant('github_fine_grained_token', 'pat: "' + pat + '"', pat, 'gith... (' + pat.length + ' chars)');
        plant('gitlab_token', 'GL=' + gl, gl, 'glpa... (26 chars)');
        plant('slack_token', 'hook ' + slack, slack, 'xoxb... (' + slack.length + ' chars)');
        plant('jwt', 'Authorization: Bearer ' + jwt, jwt, 'eyJh... (' + jwt.length + ' chars)');
        plant('google_api_key', '{"key":"' + gkey + '"}', gkey, 'AIza... (39 chars)');
        plant('stripe_secret_key', 'STRIPE ' + stripe, stripe, 'sk_l... (' + stripe.length + ' chars)');
        plant('npm_token', '//registry.npmjs.org/:_authToken=' + npm, npm, 'npm_... (40 chars)');
        plant('anthropic_api_key', 'k=' + ant, ant, 'sk-a... (' + ant.length + ' chars)');
        plant('openai_api_key', 'k=' + oai, oai, 'sk-p... (' + oai.length + ' chars)');
        plant('pypi_token', 'password = ' + pypi, pypi, 'pypi... (' + pypi.length + ' chars)');
        plant('url_credentials', 'DATABASE_URL=postgres://app:' + pw + '@db.internal:5432/x', pw, 'Sup3... (13 chars)');
        plant('pem_private_key', pem, pem, '----... (31 chars)');
        noise('MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj');
        plant('generic_secret_assignment', 'db_password = "hunter2hunter2"', 'hunter2hunter2', 'hunt... (14 chars)');
        plant('generic_secret_assignment', 'API_KEY: abcd1234efgh5678', 'abcd1234efgh5678', 'abcd... (16 chars)');
        plant('generic_secret_assignment', '{"client_secret": "zxcvbnm12345"}', 'zxcvbnm12345', 'zxcv... (12 chars)');
        plant('generic_secret_assignment', 'SECRET=abcdefgh', 'abcdefgh', 'ab... (8 chars)');
        for (const n of ['password = var.db_password', 'token: ${TOKEN}', 'password = "short"', 'secret = <redacted>',
                         'AKIA1234', 'passenger: Johnathan', 'token_url = https://example.com/oauth/token',
                         'eyJhbGci.eyJ.x', 'password = xxxxxxxxxxxx', 'api_key = process.env.KEY_VALUE', 'DB=postgres://app:${PW}@host/x',
                         'url http://example.com:8080/path@x', 'secret_name = lookup(a)']) noise(n);
        const out = scan_secrets(lines.join('\n') + '\n');
        const bad = [];
        const got = out.map(f => [f.rule, f.line, f.preview]);
        if (JSON.stringify(got) !== JSON.stringify(expected)) bad.push('findings: ' + JSON.stringify(got));
        if (out.some(f => Object.keys(f).join() !== 'rule,line,preview')) bad.push('keys');
        const dump = JSON.stringify(out);
        for (const s of secrets) {
          if (dump.includes(s)) bad.push('full secret leaked');
          for (let k = 0; k + 8 <= s.length; k++) if (dump.includes(s.slice(k, k + 8))) { bad.push('window leaked ' + s.length); break; }
        }
        const two = scan_secrets('a=' + aws + ' b=' + gh + '\r\nnext');
        if (JSON.stringify(two.map(f => [f.rule, f.line])) !== JSON.stringify([['aws_access_key_id', 1], ['github_token', 1]])) bad.push('two');
        if (scan_secrets('').length !== 0) bad.push('empty');
        const err = (f) => { try { f(); return 'no' } catch (e) { return e.name } };
        if (err(() => scan_secrets(5)) !== 'TypeError' || err(() => scan_secrets('x'.repeat(4 * 1024 * 1024 + 1))) !== 'RangeError') bad.push('errors');
        bad.length ? bad.join(' / ') : 'ok'"##;
    expect_both(code, "ok").await;
}

#[tokio::test]
async fn scan_secrets_is_bounded_on_adversarial_input() {
    let code = r##"
        const t0 = Date.now();
        const aws = 'AK' + 'IA' + 'A'.repeat(16);
        const tries = [
          () => scan_secrets('AKIA'.repeat(400000)),
          () => scan_secrets('ey' + 'J'.repeat(300000)),
          () => scan_secrets('a.'.repeat(400000)),
          () => scan_secrets('.'.repeat(1000000)),
          () => scan_secrets('password='.repeat(100000)),
          () => scan_secrets('secret: '.repeat(100000)),
          () => scan_secrets((aws + ' ').repeat(40000)),
          () => scan_secrets('password=abcdefgh12\n'.repeat(40000)),
          () => scan_secrets('-----BEGIN '.repeat(100000)),
          () => scan_secrets('://a:'.repeat(100000)),
          () => scan_secrets('x'.repeat(2000000)),
          () => scan_secrets('\n'.repeat(1000000)),
          () => scan_secrets('token'.repeat(150000)),
          () => scan_secrets('password' + ' '.repeat(1000000) + 'x'),
        ];
        const out = tries.map(f => { try { return f().length } catch (e) { return e.name } });
        out.join(',') + ':' + (Date.now() - t0 < 20000)"##;
    let tool = tool_for(ModelEffectProfile::Full);
    let started = std::time::Instant::now();
    let got = run(&tool, code).await;
    assert_eq!(got, "0,0,0,0,1,0,1001,1001,0,0,0,0,0,0:true");
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

#[tokio::test]
async fn catalogue_lists_every_segment_helper_and_each_is_installed() {
    let names: &[&str] = &[
        "diff_lines",
        "dockerfile_parse",
        "hcl_blocks",
        "cidr",
        "ip_in",
        "scan_secrets",
    ];
    for profile in [ModelEffectProfile::Full, ModelEffectProfile::ReadOnly] {
        let tool = tool_for(profile);
        let description = rig::tool::Tool::description(&tool);
        let catalogue = run(&tool, "help()").await;
        for name in names {
            assert!(
                catalogue.contains(&format!("{name}(")),
                "{profile:?} {name}"
            );
            assert!(!description.contains(&format!("`{name}")), "{profile:?}");
            assert_eq!(run(&tool, &format!("typeof {name}")).await, "function");
            assert!(run(&tool, &format!("help('{name}')")).await.contains(name));
        }
    }
}
