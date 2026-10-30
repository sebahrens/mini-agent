//! Tests for the XML and BPMN pack segments (`pack_xml.js`, `pack_bpmn.js`,
//! `pack_bpmn_outline.js`), run in-process against the real segment list.

use rquickjs::{Context, Ctx, Runtime};
use std::time::{Duration, Instant};

/// A minimal valid collaboration with full diagram interchange; broken variants are derived by
/// string replacement inside the scripts.
const PRELUDE: &str = r####"
const MINI = String.raw`<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" xmlns:bpmndi="http://www.omg.org/spec/BPMN/20100524/DI" xmlns:dc="http://www.omg.org/spec/DD/20100524/DC" xmlns:di="http://www.omg.org/spec/DD/20100524/DI" id="D">
  <bpmn:collaboration id="C">
    <bpmn:participant id="PA" name="Customer" processRef="PrA"/>
    <bpmn:participant id="PB" name="Shop" processRef="PrB"/>
    <bpmn:messageFlow id="M1" sourceRef="T1" targetRef="R1"/>
  </bpmn:collaboration>
  <bpmn:process id="PrA">
    <bpmn:laneSet id="LS"><bpmn:lane id="L1" name="Clerk"><bpmn:flowNodeRef>S1</bpmn:flowNodeRef><bpmn:flowNodeRef>T1</bpmn:flowNodeRef><bpmn:flowNodeRef>E1</bpmn:flowNodeRef></bpmn:lane></bpmn:laneSet>
    <bpmn:startEvent id="S1"><bpmn:outgoing>F1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:userTask id="T1" name="Order"><bpmn:incoming>F1</bpmn:incoming><bpmn:outgoing>F2</bpmn:outgoing></bpmn:userTask>
    <bpmn:endEvent id="E1"><bpmn:incoming>F2</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="F1" sourceRef="S1" targetRef="T1"/>
    <bpmn:sequenceFlow id="F2" sourceRef="T1" targetRef="E1"/>
  </bpmn:process>
  <bpmn:process id="PrB">
    <bpmn:startEvent id="S2"/>
    <bpmn:receiveTask id="R1"/>
    <bpmn:endEvent id="E2"/>
    <bpmn:sequenceFlow id="F3" sourceRef="S2" targetRef="R1"/>
    <bpmn:sequenceFlow id="F4" sourceRef="R1" targetRef="E2"/>
  </bpmn:process>
  <bpmndi:BPMNDiagram id="DG"><bpmndi:BPMNPlane id="PL" bpmnElement="C">
    <bpmndi:BPMNShape id="PA_di" bpmnElement="PA"><dc:Bounds x="0" y="0" width="500" height="150"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="PB_di" bpmnElement="PB"><dc:Bounds x="0" y="200" width="500" height="150"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="L1_di" bpmnElement="L1"><dc:Bounds x="30" y="0" width="470" height="150"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="S1_di" bpmnElement="S1"><dc:Bounds x="50" y="50" width="36" height="36"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="T1_di" bpmnElement="T1"><dc:Bounds x="150" y="30" width="100" height="80"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="E1_di" bpmnElement="E1"><dc:Bounds x="300" y="50" width="36" height="36"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="S2_di" bpmnElement="S2"><dc:Bounds x="50" y="250" width="36" height="36"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="R1_di" bpmnElement="R1"><dc:Bounds x="150" y="230" width="100" height="80"/></bpmndi:BPMNShape>
    <bpmndi:BPMNShape id="E2_di" bpmnElement="E2"><dc:Bounds x="300" y="250" width="36" height="36"/></bpmndi:BPMNShape>
    <bpmndi:BPMNEdge id="F1_di" bpmnElement="F1"><di:waypoint x="86" y="68"/><di:waypoint x="150" y="68"/></bpmndi:BPMNEdge>
    <bpmndi:BPMNEdge id="F2_di" bpmnElement="F2"><di:waypoint x="250" y="68"/><di:waypoint x="300" y="68"/></bpmndi:BPMNEdge>
    <bpmndi:BPMNEdge id="F3_di" bpmnElement="F3"><di:waypoint x="86" y="268"/><di:waypoint x="150" y="268"/></bpmndi:BPMNEdge>
    <bpmndi:BPMNEdge id="F4_di" bpmnElement="F4"><di:waypoint x="250" y="270"/><di:waypoint x="300" y="268"/></bpmndi:BPMNEdge>
    <bpmndi:BPMNEdge id="M1_di" bpmnElement="M1"><di:waypoint x="200" y="110"/><di:waypoint x="200" y="230"/></bpmndi:BPMNEdge>
  </bpmndi:BPMNPlane></bpmndi:BPMNDiagram>
</bpmn:definitions>`;
const rules = (x) => bpmn_validate(x).filter((f) => f.level === 'error').map((f) => f.rule + ':' + f.id).sort().join(',');
"####;

fn with_ctx(body: impl FnOnce(&Ctx<'_>)) {
    let runtime = Runtime::new().unwrap();
    let context = Context::full(&runtime).unwrap();
    context.with(|ctx| {
        super::install(&ctx).unwrap();
        ctx.eval::<(), _>(PRELUDE).unwrap();
        body(&ctx);
    });
}

fn run(code: &str) -> String {
    let mut out = String::new();
    with_ctx(|ctx| {
        out = ctx
            .eval::<String, _>(code)
            .unwrap_or_else(|e| format!("EVAL ERROR {e:?}: {:?}", ctx.catch()));
    });
    out
}

/// Evaluate `code` and require it to finish within `secs` seconds.
fn run_bounded(code: &str, secs: u64) -> String {
    let started = Instant::now();
    let out = run(code);
    assert!(
        started.elapsed() < Duration::from_secs(secs),
        "took {:?}",
        started.elapsed()
    );
    out
}

#[test]
fn xml_parse_builds_a_plain_tree_and_keeps_prefixes() {
    let code = r##"
        const t = xml_parse('<?xml version="1.0"?>\n<!-- c --><r xmlns:p="urn:x" a=\'1&amp;2\' b="x&#10;y&#x41;"><p:c>t&lt;<![CDATA[<raw>]]>&gt;</p:c>\n <e/><?pi data?>  <e2></e2></r>');
        JSON.stringify(t)"##;
    assert_eq!(
        run(code),
        r#"{"name":"r","attrs":{"xmlns:p":"urn:x","a":"1&2","b":"x\nyA"},"children":[{"name":"p:c","attrs":{},"children":[],"text":"t<<raw>>"},{"name":"e","attrs":{},"children":[],"text":""},{"name":"e2","attrs":{},"children":[],"text":""}],"text":""}"#
    );
    // Whitespace-only text beside children is dropped; a leaf keeps its exact text; CRLF is LF;
    // attribute whitespace characters are normalised to spaces but references are preserved.
    let code2 = r##"
        const a = xml_parse('<a>\r\n  <b> x </b>\r\n</a>');
        const b = xml_parse('<a t="1\t2\n3&#9;4"/>');
        const c = xml_parse('<__proto__ __proto__="p" constructor="c"/>');
        JSON.stringify([a.text, a.children[0].text, b.attrs.t, c.name, Object.getPrototypeOf(c.attrs) === Object.prototype, c.attrs['__proto__'], c.attrs.constructor])"##;
    assert_eq!(
        run(code2),
        r#"[""," x ","1 2 3\t4","__proto__",true,"p","c"]"#
    );
}

#[test]
fn xml_parse_rejects_dtds_entities_and_malformed_input_with_line_and_column() {
    let code = r##"
        const msg = (s) => { try { xml_parse(s); return 'OK'; } catch (e) { return e.name + ': ' + e.message; } };
        JSON.stringify([
          msg('<!DOCTYPE a [<!ENTITY x "y">]><a>&x;</a>'),
          msg('<?xml version="1.0"?>\n<!DOCTYPE lolz [<!ENTITY lol "lol"><!ENTITY lol2 "&lol;&lol;">]>\n<lolz>&lol2;</lolz>'),
          msg('<!DOCTYPE a SYSTEM "file:///etc/passwd"><a/>'),
          msg('<a>&xxe;</a>'),
          msg('<a>&#0;</a>'), msg('<a>&#x110000;</a>'), msg('<a>&#xD800;</a>'), msg('<a>&amp</a>'), msg('<a>&#12x;</a>'),
          msg('<a>\n  <b>\n </a>'),
          msg('<a x="1" x="2"/>'), msg('<a x=1/>'), msg('<a x="<"/>'), msg('<a>]]></a>'),
          msg('<a/><b/>'), msg('text<a/>'), msg('<a/>text'), msg(''), msg('   '), msg('<a'), msg('<a><!-- -- --></a>'),
          msg('<a>\u0001</a>'), msg('<a><?xml version="1.0"?></a>'), msg('<![CDATA[x]]><a/>'), msg('<a:b:c d:e="1"/>'),
          msg(1),
        ])"##;
    let out = run(code);
    let rows: Vec<String> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let expect = [
        "SyntaxError: xml_parse: DOCTYPE/DTD declarations are not allowed (line 1, column 1)",
        "SyntaxError: xml_parse: DOCTYPE/DTD declarations are not allowed (line 2, column 1)",
        "SyntaxError: xml_parse: DOCTYPE/DTD declarations are not allowed (line 1, column 1)",
        "SyntaxError: xml_parse: undefined entity reference (only the five predefined entities are supported) (line 1, column 4)",
        "SyntaxError: xml_parse: character reference to a forbidden character (line 1, column 4)",
        "SyntaxError: xml_parse: character reference to a forbidden character (line 1, column 4)",
        "SyntaxError: xml_parse: character reference to a forbidden character (line 1, column 4)",
        "SyntaxError: xml_parse: malformed entity reference (line 1, column 4)",
        "SyntaxError: xml_parse: malformed character reference (line 1, column 4)",
        "SyntaxError: xml_parse: mismatched end tag (line 3, column 2)",
        "SyntaxError: xml_parse: duplicate attribute (line 1, column 10)",
        "SyntaxError: xml_parse: attribute value must be quoted (line 1, column 6)",
        "SyntaxError: xml_parse: '<' is not allowed in an attribute value (line 1, column 7)",
        "SyntaxError: xml_parse: ']]>' is not allowed in character data (line 1, column 4)",
        "SyntaxError: xml_parse: more than one root element (line 1, column 5)",
        "SyntaxError: xml_parse: text before the root element (line 1, column 1)",
        "SyntaxError: xml_parse: text after the root element (line 1, column 5)",
        "SyntaxError: xml_parse: no root element (line 1, column 1)",
        "SyntaxError: xml_parse: no root element (line 1, column 4)",
        "SyntaxError: xml_parse: unterminated start tag (line 1, column 1)",
        "SyntaxError: xml_parse: '--' is not allowed inside a comment (line 1, column 4)",
        "SyntaxError: xml_parse: illegal character U+0001 (line 1, column 4)",
        "SyntaxError: xml_parse: the XML declaration is only allowed at the very start (line 1, column 4)",
        "SyntaxError: xml_parse: CDATA outside the root element (line 1, column 1)",
        "OK",
        "TypeError: xml_parse: text must be a string",
    ];
    assert_eq!(rows.len(), expect.len(), "{out}");
    for (got, want) in rows.iter().zip(expect) {
        assert_eq!(got, want);
    }
}

#[test]
fn xml_parse_is_bounded_on_adversarial_input() {
    // Entity-expansion bomb (rejected at the DOCTYPE), deep nesting, huge attribute and node
    // counts and an oversized input must all fail fast with a closed error.
    let code = r##"
        const msg = (f) => { try { f(); return 'OK'; } catch (e) { return e.name + ': ' + e.message.replace(/ \(line.*$/, ''); } };
        let lol = '<!DOCTYPE lolz [<!ENTITY lol "lol">';
        for (let i = 2; i <= 9; i++) lol += '<!ENTITY lol' + i + ' "' + ('&lol' + (i - 1) + ';').repeat(10) + '">';
        lol += ']><lolz>&lol9;</lolz>';
        const attrs = Array.from({ length: 5000 }, (_, i) => 'a' + i + '="x"').join(' ');
        JSON.stringify([
          msg(() => xml_parse(lol)),
          msg(() => xml_parse('<a>'.repeat(300) + '</a>'.repeat(300))),
          msg(() => xml_parse('<a>'.repeat(256) + '</a>'.repeat(256))),
          msg(() => xml_parse('<a ' + attrs + '/>')),
          msg(() => xml_parse('<a>' + '<b/>'.repeat(600000) + '</a>')),
          msg(() => xml_parse('<a>' + 'x'.repeat(4 * 1024 * 1024) + '</a>')),
          msg(() => xml_parse('<a>' + '&amp;'.repeat(700000) + '</a>')),
          msg(() => xml_parse('<a b="' + '&#65;'.repeat(700000) + '"/>')),
          msg(() => xml_parse('<' + 'a'.repeat(1000000) + '>')),
          msg(() => xml_parse('<!--' + '-'.repeat(1000000))),
          msg(() => xml_parse('<a>' + '<![CDATA['.repeat(100000))),
        ])"##;
    let out = run_bounded(code, 20);
    let rows: Vec<String> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        rows[0],
        "SyntaxError: xml_parse: DOCTYPE/DTD declarations are not allowed"
    );
    assert_eq!(
        rows[1],
        "RangeError: xml_parse: nesting deeper than 256 elements"
    );
    assert_eq!(rows[2], "OK");
    assert_eq!(
        rows[3],
        "RangeError: xml_parse: more than 1000 attributes on one element"
    );
    assert_eq!(rows[4], "RangeError: xml_parse: more than 500000 nodes");
    assert_eq!(
        rows[5],
        "RangeError: xml_parse: input exceeds 4194304 characters"
    );
    assert_eq!(rows[6], "OK");
    assert_eq!(rows[7], "OK");
    assert!(rows[8].starts_with("SyntaxError"), "{}", rows[8]);
    assert!(rows[9].starts_with("SyntaxError"), "{}", rows[9]);
    assert!(rows[10].starts_with("SyntaxError"), "{}", rows[10]);
}

#[test]
fn xml_build_escapes_and_round_trips_through_xml_parse() {
    let code = r##"
        const nodes = [
          {name: 'a', attrs: {}, children: [], text: ''},
          {name: 'a', attrs: {x: '1'}, children: [], text: 'hi'},
          {name: 'p:a', attrs: {'xmlns:p': 'urn:x', q: 'a<b>&"\'\t\n\r end'}, children: [
            {name: 'p:b', attrs: {}, children: [], text: 'x ]]> y & < > \r é中😀'},
            {name: 'c', attrs: {}, children: [{name: 'd', attrs: {k: ''}, children: [], text: ' '}], text: ''},
          ], text: 'lead '},
          {name: '__proto__', attrs: {'__proto__': 'v', constructor: 'c'}, children: [], text: ''},
        ];
        const bad = nodes.filter((n) => JSON.stringify(xml_parse(xml_build(n))) !== JSON.stringify(n)).length;
        const pretty = xml_build(nodes[2], {indent: 2, declaration: true});
        const prettyRound = JSON.stringify(xml_parse(pretty)) === JSON.stringify(nodes[2]);
        const msg = (f) => { try { f(); return 'OK'; } catch (e) { return e.name; } };
        JSON.stringify([bad, prettyRound, xml_build({name: 'a', attrs: {x: '1'}, text: 'hi'}),
          xml_build({name: 'r', children: [{name: 'k'}]}, {indent: 2}),
          msg(() => xml_build({name: '1a'})), msg(() => xml_build({name: 'a', attrs: {'b c': 'x'}})),
          msg(() => xml_build({name: 'a', attrs: {b: {}}})), msg(() => xml_build({name: 'a', text: 'x\u0001'})),
          msg(() => xml_build({name: 'a', children: 'x'})), msg(() => xml_build(null)), msg(() => xml_build({name: 'a'}, {indent: 9})),
          xml_build({name: 'a', attrs: {n: 1, t: true}}),
        ])"##;
    let out = run(code);
    let rows: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(rows[0], 0, "{out}");
    assert_eq!(rows[1], true, "{out}");
    assert_eq!(rows[2], "<a x=\"1\">hi</a>");
    assert_eq!(rows[3], "<r>\n  <k/>\n</r>");
    for i in 4..11 {
        assert!(rows[i] != "OK", "case {i}: {out}");
    }
    assert_eq!(rows[11], "<a n=\"1\" t=\"true\"/>");
}

#[test]
fn xml_build_is_bounded_and_canonicalises() {
    let code = r##"
        const msg = (f) => { try { f(); return 'OK'; } catch (e) { return e.name + ': ' + e.message; } };
        let deep = {name: 'x'};
        for (let i = 0; i < 300; i++) deep = {name: 'x', children: [deep]};
        const cyc = {name: 'x', children: []}; cyc.children.push(cyc);
        const wide = {name: 'r', children: Array.from({length: 600000}, () => ({name: 'k'}))};
        const big = {name: 'r', text: 'x'.repeat(17 * 1024 * 1024)};
        // Canonical form: attribute order, quote style, entity spelling, CDATA and comments do not matter.
        const a = xml_build(xml_parse('<r b="2" a=\'1\'><!-- note --><c>a&#38;b</c><d><![CDATA[x<y]]></d></r>'), {sort: true});
        const b = xml_build(xml_parse('<?xml version="1.0"?><r a="1"   b=\'2\'><c>a&amp;b</c><d>x&lt;y</d></r>'), {sort: true});
        JSON.stringify([msg(() => xml_build(deep)), msg(() => xml_build(cyc)), msg(() => xml_build(wide)),
          msg(() => xml_build(big)), msg(() => xml_build({name: 'a', attrs: Object.fromEntries(Array.from({length: 1001}, (_, i) => ['a' + i, 'v']))})),
          a === b, a])"##;
    let out = run_bounded(code, 20);
    let rows: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        rows[0],
        "RangeError: xml_build: nesting deeper than 256 elements"
    );
    assert_eq!(
        rows[1],
        "RangeError: xml_build: nesting deeper than 256 elements"
    );
    assert_eq!(rows[2], "RangeError: xml_build: more than 500000 nodes");
    assert_eq!(
        rows[3],
        "RangeError: xml_build: output exceeds 16777216 characters"
    );
    assert!(
        rows[4]
            .as_str()
            .unwrap()
            .contains("more than 1000 attributes")
    );
    assert_eq!(rows[5], true, "{out}");
    assert_eq!(
        rows[6],
        "<r a=\"1\" b=\"2\"><c>a&amp;b</c><d>x&lt;y</d></r>"
    );
}

#[test]
fn bpmn_validate_accepts_a_valid_diagram_and_flags_each_injected_defect() {
    let code = r##"
        const rep = (a, b) => MINI.split(a).join(b);
        JSON.stringify({
          ok: rules(MINI),
          dangling: rules(rep('targetRef="E1"', 'targetRef="Nope"')),
          dup: rules(rep('<bpmn:endEvent id="E2"/>', '<bpmn:endEvent id="E2"/><bpmn:endEvent id="E1"/>')),
          cross: rules(rep('<bpmn:sequenceFlow id="F4" sourceRef="R1" targetRef="E2"/>', '<bpmn:sequenceFlow id="F4" sourceRef="R1" targetRef="E1"/>')),
          msg: rules(rep('<bpmn:messageFlow id="M1" sourceRef="T1" targetRef="R1"/>', '<bpmn:messageFlow id="M1" sourceRef="T1" targetRef="E1"/>')),
          shape: rules(rep('<bpmndi:BPMNShape id="R1_di" bpmnElement="R1"><dc:Bounds x="150" y="230" width="100" height="80"/></bpmndi:BPMNShape>', '')),
          edge: rules(rep('<bpmndi:BPMNEdge id="M1_di" bpmnElement="M1"><di:waypoint x="200" y="110"/><di:waypoint x="200" y="230"/></bpmndi:BPMNEdge>', '')),
          diref: rules(rep('bpmnElement="F3"', 'bpmnElement="Ghost"')),
          lane: rules(rep('<bpmn:flowNodeRef>E1</bpmn:flowNodeRef>', '<bpmn:flowNodeRef>E2</bpmn:flowNodeRef>')),
          noend: rules(rep('<bpmn:endEvent id="E2"/>', '').split('<bpmn:sequenceFlow id="F4" sourceRef="R1" targetRef="E2"/>').join('')),
          nostart: rules(rep('<bpmn:startEvent id="S2"/>', '<bpmn:task id="S2"/>')),
          startin: rules(rep('<bpmn:sequenceFlow id="F4"', '<bpmn:sequenceFlow id="F9" sourceRef="R1" targetRef="S2"/><bpmn:sequenceFlow id="F4"')),
          boundsbad: rules(rep('width="100" height="80"/></bpmndi:BPMNShape>\n    <bpmndi:BPMNShape id="E1_di"', 'width="0" height="80"/></bpmndi:BPMNShape>\n    <bpmndi:BPMNShape id="E1_di"')),
          wp: rules(rep('<di:waypoint x="86" y="68"/>', '')),
          mismatch: rules(rep('<bpmn:outgoing>F2</bpmn:outgoing>', '<bpmn:outgoing>F1</bpmn:outgoing>')),
          root: rules('<a/>'),
          notxml: bpmn_validate('<a>').map((f) => f.rule + ':' + f.level).join(),
          doctype: bpmn_validate('<!DOCTYPE a><a/>')[0].rule,
        })"##;
    let out = run(code);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let get = |k: &str| v[k].as_str().unwrap().to_string();
    assert_eq!(get("ok"), "", "{out}");
    assert_eq!(
        get("dangling"),
        "flow-ref-mismatch:E1,unresolved-ref:F2",
        "{out}"
    );
    assert_eq!(get("dup"), "duplicate-id:E1", "{out}");
    assert!(
        get("cross").contains("sequence-flow-same-process:F4"),
        "{out}"
    );
    assert_eq!(get("msg"), "message-flow-same-participant:M1", "{out}");
    assert_eq!(get("shape"), "di-missing-shape:R1", "{out}");
    assert_eq!(get("edge"), "di-missing-edge:M1", "{out}");
    assert_eq!(
        get("diref"),
        "di-missing-edge:F3,di-unresolved-ref:F3_di",
        "{out}"
    );
    assert!(get("lane").contains("lane-ref-process:L1"), "{out}");
    assert!(get("noend").contains("end-event-required:PrB"), "{out}");
    assert!(get("nostart").contains("start-event-required:PrB"), "{out}");
    assert!(get("startin").contains("start-event-incoming:S2"), "{out}");
    assert_eq!(get("boundsbad"), "di-shape-bounds:T1_di", "{out}");
    assert_eq!(get("wp"), "di-edge-waypoints:F1_di", "{out}");
    assert!(get("mismatch").contains("flow-ref-mismatch:T1"), "{out}");
    assert_eq!(get("root"), "root-element:", "{out}");
    assert_eq!(get("notxml"), "xml-parse:error", "{out}");
    assert_eq!(get("doctype"), "xml-parse", "{out}");
}

#[test]
fn bpmn_validate_checks_gateway_rules_and_namespaces() {
    let code = r##"
        const wrap = (body, ns) => `<definitions xmlns="${ns || 'http://www.omg.org/spec/BPMN/20100524/MODEL'}" id="D"><process id="P">${body}</process></definitions>`;
        const base = (gw, flows) => wrap(`<startEvent id="s"/><task id="a"/><task id="b"/><endEvent id="e"/>${gw}${flows}`);
        const xor = (c1, c2, dflt) => base(`<exclusiveGateway id="g" ${dflt}/>`,
          `<sequenceFlow id="f0" sourceRef="s" targetRef="g"/><sequenceFlow id="f1" sourceRef="g" targetRef="a">${c1}</sequenceFlow><sequenceFlow id="f2" sourceRef="g" targetRef="b">${c2}</sequenceFlow>` +
          `<sequenceFlow id="f3" sourceRef="a" targetRef="e"/><sequenceFlow id="f4" sourceRef="b" targetRef="e"/>`);
        const cond = '<conditionExpression>x</conditionExpression>';
        const errs = (x) => bpmn_validate(x).filter((f) => f.level === 'error').map((f) => f.rule + ':' + f.id).join();
        JSON.stringify([
          errs(xor(cond, cond, '')),
          errs(xor(cond, '', 'default="f2"')),
          errs(xor('', '', '')),
          errs(xor(cond, '', 'default="f1"')),
          errs(xor(cond, '', 'default="nope"')),
          // a different namespace is not BPMN
          bpmn_validate(wrap('', 'urn:other'))[0].rule,
          // prefix may be anything
          errs(MINI.split('bpmn:').join('b2:').split('xmlns:bpmn').join('xmlns:b2')),
          // no diagram at all is a warning, not an error
          bpmn_validate(wrap('<startEvent id="s"/><endEvent id="e"/><sequenceFlow id="f" sourceRef="s" targetRef="e"/>')).map((f) => f.level + ':' + f.rule).join(),
          // an already parsed tree is accepted
          bpmn_validate(xml_parse(MINI)).length,
        ])"##;
    let out = run(code);
    let v: Vec<serde_json::Value> =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v[0], "", "{out}");
    assert_eq!(v[1], "", "{out}");
    assert!(
        v[2].as_str().unwrap().contains("gateway-flow-condition:f1"),
        "{out}"
    );
    assert!(
        v[2].as_str().unwrap().contains("gateway-flow-condition:f2"),
        "{out}"
    );
    assert!(
        v[3].as_str().unwrap().contains("gateway-flow-condition:f2"),
        "{out}"
    );
    assert!(
        v[4].as_str().unwrap().contains("default-flow-invalid:g"),
        "{out}"
    );
    assert_eq!(v[5], "root-element", "{out}");
    assert_eq!(v[6], "", "{out}");
    assert_eq!(v[7], "warning:di-missing", "{out}");
    assert_eq!(v[8], 0, "{out}");
}

#[test]
fn bpmn_summary_is_compact_and_reports_structure() {
    let code = r##"
        const s = bpmn_summary(MINI);
        const broken = bpmn_summary(MINI.split('bpmnElement="F3"').join('bpmnElement="Ghost"').replace('<bpmn:endEvent id="E2"/>', '<bpmn:endEvent id="E2"/><bpmn:task id="Lonely"/>'));
        // a large generated diagram: 400 tasks, 40 lanes, 30 pools worth of message flows
        let big = MINI;
        let tasks = '', refs = '', flows = '', sh = '';
        for (let i = 0; i < 400; i++) {
          tasks += `<bpmn:task id="X${i}"/>`;
          if (i) flows += `<bpmn:sequenceFlow id="XF${i}" sourceRef="X${i - 1}" targetRef="X${i}"/>`;
          sh += `<bpmndi:BPMNShape id="X${i}_di" bpmnElement="X${i}"><dc:Bounds x="${i}" y="0" width="10" height="10"/></bpmndi:BPMNShape>`;
          if (i) sh += `<bpmndi:BPMNEdge id="XF${i}_di" bpmnElement="XF${i}"><di:waypoint x="0" y="0"/><di:waypoint x="1" y="1"/></bpmndi:BPMNEdge>`;
        }
        big = big.replace('</bpmn:process>\n  <bpmn:process id="PrB">', tasks + flows + '</bpmn:process>\n  <bpmn:process id="PrB">').replace('</bpmndi:BPMNPlane>', sh + '</bpmndi:BPMNPlane>');
        const bs = bpmn_summary(big);
        JSON.stringify({s, broken, bigLen: JSON.stringify(bs).length, bigTasks: bs.counts.task, bigFlows: bs.flows, bigErr: bs.findings.errors,
          capped: bpmn_summary(big, {max: 1}).participants.length, trunc: bpmn_summary(big, {max: 1}).truncated, bigOrphans: bs.orphans,
          threw: (() => { try { bpmn_summary('<a/>'); } catch (e) { return e.name; } })(),
          threw2: (() => { try { bpmn_summary('<a'); } catch (e) { return e.name; } })() })"##;
    let out = run(code);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let s = &v["s"];
    assert_eq!(
        s["processes"],
        serde_json::json!([
            {"id": "PrA", "participant": "PA", "nodes": 3, "flows": 2, "lanes": 1},
            {"id": "PrB", "participant": "PB", "nodes": 3, "flows": 2, "lanes": 0}
        ])
    );
    assert_eq!(
        s["participants"],
        serde_json::json!([
            {"id": "PA", "name": "Customer", "processRef": "PrA"},
            {"id": "PB", "name": "Shop", "processRef": "PrB"}
        ])
    );
    assert_eq!(
        s["lanes"],
        serde_json::json!([{"id": "L1", "name": "Clerk", "process": "PrA", "nodes": 3}])
    );
    assert_eq!(
        s["counts"],
        serde_json::json!({"endEvent": 2, "receiveTask": 1, "startEvent": 2, "userTask": 1})
    );
    assert_eq!(s["flows"], 4);
    assert_eq!(
        s["messageFlows"],
        serde_json::json!([{"id": "M1", "from": "PA", "to": "PB"}])
    );
    assert_eq!(s["orphans"], serde_json::json!([]));
    assert_eq!(s["diErrors"], serde_json::json!([]));
    assert_eq!(
        s["findings"],
        serde_json::json!({"errors": 0, "warnings": 0})
    );
    let b = &v["broken"];
    assert_eq!(b["orphans"], serde_json::json!(["Lonely"]));
    assert!(
        b["diErrors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "di-unresolved-ref:F3_di"),
        "{out}"
    );
    assert!(
        b["diErrors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x == "di-missing-shape:Lonely"),
        "{out}"
    );
    assert!(v["bigLen"].as_u64().unwrap() < 1500, "{out}");
    assert_eq!(v["bigTasks"], 400);
    assert_eq!(v["bigFlows"], 403);
    assert_eq!(v["bigErr"], 0, "{out}");
    assert_eq!(v["capped"], 1);
    assert_eq!(
        v["trunc"],
        serde_json::json!({"participants": 2, "processes": 2})
    );
    assert_eq!(v["threw"], "SyntaxError");
    assert_eq!(v["threw2"], "SyntaxError");
}

#[test]
fn bpmn_validate_and_summary_survive_hostile_diagrams_quickly() {
    let code = r##"
        const msg = (f) => { try { const r = f(); return 'OK'; } catch (e) { return e.name; } };
        const defs = (body) => `<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="d">${body}</bpmn:definitions>`;
        const self = defs('<bpmn:process id="p"><bpmn:task id="t"/><bpmn:sequenceFlow id="f" sourceRef="t" targetRef="t"/></bpmn:process>');
        const lots = defs('<bpmn:process id="p">' + Array.from({length: 20000}, (_, i) => `<bpmn:sequenceFlow id="f${i}" sourceRef="nope" targetRef="nope"/>`).join('') + '</bpmn:process>');
        const dupIds = defs('<bpmn:process id="p">' + '<bpmn:task id="same"/>'.repeat(20000) + '</bpmn:process>');
        const proto = defs('<bpmn:process id="__proto__"><bpmn:task id="constructor" default="toString"/></bpmn:process>');
        JSON.stringify([msg(() => bpmn_validate(self)), msg(() => bpmn_validate(lots)), msg(() => bpmn_summary(lots)), msg(() => bpmn_validate(dupIds)),
          msg(() => bpmn_validate(proto)), msg(() => bpmn_validate(null)), msg(() => bpmn_validate(5)), bpmn_validate(lots).length, bpmn_validate(lots).at(-1).rule])"##;
    let out = run_bounded(code, 20);
    let v: Vec<serde_json::Value> =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v[0], "OK");
    assert_eq!(v[1], "OK");
    assert_eq!(v[2], "OK");
    assert_eq!(v[3], "OK");
    assert_eq!(v[4], "OK");
    assert_eq!(v[5], "TypeError");
    assert_eq!(v[6], "TypeError");
    assert_eq!(v[7], 1001, "{out}");
    assert_eq!(v[8], "truncated");
}

#[test]
fn bpmn_from_outline_produces_valid_non_overlapping_diagrams() {
    let code = r##"
        const outlines = {
          linear: [{id: 's', type: 'start', next: 'a'}, {id: 'a', type: 'userTask', name: 'Review & <sign>', next: 'b'}, {id: 'b', type: 'serviceTask', next: 'e'}, {id: 'e', type: 'end'}],
          xor: [{id: 's', type: 'start', next: 'a'}, {id: 'a', type: 'task', next: 'g'}, {id: 'g', type: 'xor', next: ['x', 'y'], cond: ['yes', '']},
                {id: 'x', type: 'task', next: 'm'}, {id: 'y', type: 'call', next: 'm'}, {id: 'm', type: 'xor', next: 'e'}, {id: 'e', type: 'end'}],
          par: [{id: 's', type: 'start', next: 'f'}, {id: 'f', type: 'and', next: ['a', 'b', 'c']}, {id: 'a', type: 'task', next: 'j'}, {id: 'b', type: 'task', next: 'j'}, {id: 'c', type: 'task', next: 'j'},
                {id: 'j', type: 'and', next: 'e'}, {id: 'e', type: 'end', trigger: 'terminate'}],
          lanes: [{id: 's', type: 'start', lane: 'Clerk', next: 'a'}, {id: 'a', type: 'userTask', lane: 'Clerk', next: 'b'}, {id: 'b', type: 'task', lane: 'Manager', next: 'c'},
                  {id: 'c', type: 'task', lane: 'IT', next: 'e'}, {id: 'e', type: 'end', lane: 'IT'}],
          pools: [{id: 's', type: 'start', pool: 'Customer', next: 'a'}, {id: 'a', type: 'task', pool: 'Customer', msg: 'r', next: 'e'}, {id: 'e', type: 'end', pool: 'Customer'},
                  {id: 'r', type: 'start', trigger: 'message', pool: 'Shop', next: 'p'}, {id: 'p', type: 'task', pool: 'Shop', next: 'f'}, {id: 'f', type: 'end', pool: 'Shop'}],
          boundary: [{id: 's', type: 'start', next: 'a'}, {id: 'a', type: 'task', next: 'e'}, {id: 'bt', type: 'boundary', attachedTo: 'a', trigger: 'timer', next: 'esc'},
                     {id: 'esc', type: 'task', next: 'e2'}, {id: 'e2', type: 'end'}, {id: 'e', type: 'end'}],
          sub: [{id: 's', type: 'start', next: 'sp'}, {id: 'sp', type: 'subprocess', next: 'e', steps: [{id: 'is', type: 'start', next: 'it'}, {id: 'it', type: 'task', next: 'ie'}, {id: 'ie', type: 'end'}]}, {id: 'e', type: 'end'}],
          loop: [{id: 's', type: 'start', next: 'a'}, {id: 'a', type: 'task', next: 'g'}, {id: 'g', type: 'xor', next: ['a', 'e'], cond: {a: 'retry'}}, {id: 'e', type: 'end'}],
        };
        const report = {};
        for (const [name, steps] of Object.entries(outlines)) {
          const xml = bpmn_from_outline(steps);
          const again = bpmn_from_outline(steps);
          const tree = xml_parse(xml);
          const all = [];
          const walk = (n) => { all.push(n); n.children.forEach(walk); };
          walk(tree);
          const local = (n) => n.name.replace(/^[a-z]+:/, '');
          const boxes = all.filter((n) => local(n) === 'BPMNShape').map((n) => {
            const b = n.children.find((c) => c.name === 'dc:Bounds').attrs;
            return {id: n.attrs.bpmnElement, x: +b.x, y: +b.y, w: +b.width, h: +b.height};
          });
          const kind = {};
          all.forEach((n) => { if (n.attrs.id) kind[n.attrs.id] = local(n); });
          const container = (a, b) => b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w && b.y + b.h <= a.y + a.h;
          let overlaps = 0;
          for (let i = 0; i < boxes.length; i++) for (let j = i + 1; j < boxes.length; j++) {
            const a = boxes[i], b = boxes[j];
            if (kind[a.id] === 'boundaryEvent' || kind[b.id] === 'boundaryEvent') continue;
            if (container(a, b) || container(b, a)) continue; // pools, lanes and expanded subprocesses contain shapes
            if (a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h) overlaps++;
          }
          const edges = all.filter((n) => local(n) === 'BPMNEdge');
          const shortEdges = edges.filter((e) => e.children.length < 2).length;
          const flows = all.filter((n) => ['sequenceFlow', 'messageFlow'].includes(local(n))).length;
          report[name] = {same: xml === again, errors: bpmn_validate(xml).filter((f) => f.level === 'error').length, overlaps, shortEdges, edgesOk: edges.length === flows,
                          negative: boxes.filter((b) => b.x < 0 || b.y < 0).length, summary: bpmn_summary(xml).counts};
        }
        JSON.stringify(report)"##;
    let out = run(code);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    for (name, r) in v.as_object().unwrap() {
        assert_eq!(r["same"], true, "{name}: {out}");
        assert_eq!(r["errors"], 0, "{name}: {out}");
        assert_eq!(r["overlaps"], 0, "{name}: {out}");
        assert_eq!(r["shortEdges"], 0, "{name}: {out}");
        assert_eq!(r["edgesOk"], true, "{name}: {out}");
        assert_eq!(r["negative"], 0, "{name}: {out}");
    }
    assert_eq!(
        v["linear"]["summary"],
        serde_json::json!({"endEvent": 1, "serviceTask": 1, "startEvent": 1, "userTask": 1})
    );
    assert_eq!(v["boundary"]["summary"]["boundaryEvent"], 1);
    assert_eq!(v["sub"]["summary"]["subProcess"], 1);
}

#[test]
fn bpmn_from_outline_writes_conditions_defaults_lanes_and_escapes() {
    let code = r##"
        const xml = bpmn_from_outline([{id: 's', type: 'start', next: 'g'}, {id: 'g', type: 'xor', name: 'a<b', next: ['x', 'y'], cond: {x: '${n > 1 && ok}'}},
          {id: 'x', type: 'task', lane: 'Ops', next: 'e'}, {id: 'y', type: 'task', lane: 'Ops', next: 'e'}, {id: 'e', type: 'end'}], {name: 'Flow & Co'});
        const t = xml_parse(xml);
        const g = t.children.find((c) => c.name === 'bpmn:process').children.find((c) => c.attrs.id === 'g');
        JSON.stringify({xml: xml.includes('${n &gt; 1 &amp;&amp; ok}'), dflt: g.attrs.default, name: g.attrs.name,
          lane: xml.includes('<bpmn:lane id="Lane_1_2" name="Ops">') && xml.includes('name="Unassigned"'),
          part: xml.includes('name="Flow &amp; Co"'), decl: xml.startsWith('<?xml version="1.0" encoding="UTF-8"?>\n')})"##;
    let out = run(code);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(v["xml"], true, "{out}");
    assert_eq!(v["dflt"], "Flow_g_y", "{out}");
    assert_eq!(v["name"], "a<b", "{out}");
    assert_eq!(v["lane"], true, "{out}");
    assert_eq!(v["part"], true, "{out}");
    assert_eq!(v["decl"], true, "{out}");
}

#[test]
fn bpmn_from_outline_fails_loudly_on_bad_outlines() {
    let code = r##"
        const msg = (steps, o) => { try { bpmn_from_outline(steps, o); return 'OK'; } catch (e) { return e.name + ': ' + e.message.replace('bpmn_from_outline: ', ''); } };
        const ok = [{id: 's', type: 'start', next: 'e'}, {id: 'e', type: 'end'}];
        const mod = (patch) => ok.map((x, i) => (i === 0 ? Object.assign({}, x, patch) : x));
        JSON.stringify([
          msg([]), msg('x'), msg([1]), msg(mod({id: '1x'})), msg(mod({id: 'a b'})), msg([...ok, ok[0]]), msg(mod({type: 'wat'})), msg(mod({next: 'zz'})),
          msg(mod({next: 5})), msg(mod({cond: 'x'})), msg(mod({name: 5})), msg(mod({trigger: 'bogus'})), msg(mod({steps: [{id: 'q', type: 'task'}]})),
          msg([{id: 's', type: 'start', next: 'g'}, {id: 'g', type: 'xor', next: ['a', 'b']}, {id: 'a', type: 'end'}, {id: 'b', type: 'end'}]),
          msg([{id: 's', type: 'start', next: 'g'}, {id: 'g', type: 'xor', next: ['a', 'b'], cond: ['p', 'q']}, {id: 'a', type: 'end'}, {id: 'b', type: 'end'}]),
          msg([{id: 'a', type: 'task'}]), msg([{id: 'b', type: 'boundary'}]), msg([{id: 's', type: 'start', next: 'e', pool: 'A'}, {id: 'e', type: 'end'}]),
          msg(ok.map((x) => ({...x, msg: 's'}))), msg(ok, 5),
          msg(Array.from({length: 501}, (_, i) => ({id: 'n' + i, type: 'task'}))),
        ])"##;
    let out = run_bounded(code, 20);
    let rows: Vec<String> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(rows.len(), 21);
    for (i, row) in rows.iter().enumerate() {
        // Case 14 (a cond on every branch, no default) is legal.
        if i == 14 {
            assert_eq!(row, "OK");
            continue;
        }
        assert!(
            row.starts_with("TypeError") || row.starts_with("Error"),
            "case {i}: {row}"
        );
    }
    assert!(rows[13].contains("needs a cond"), "{}", rows[13]);
    assert!(rows[15].contains("start-event-required"), "{}", rows[15]);
    assert!(rows[20].contains("more than 500 steps"), "{}", rows[20]);
}

#[test]
fn bpmn_from_outline_handles_500_steps_quickly() {
    let code = r##"
        const steps = [{id: 's', type: 'start', next: 't0'}];
        for (let i = 0; i < 497; i++) steps.push({id: 't' + i, type: i % 3 === 0 ? 'userTask' : 'task', lane: 'L' + (i % 4), next: i === 496 ? 'e' : 't' + (i + 1)});
        steps.push({id: 'e', type: 'end'});
        const xml = bpmn_from_outline(steps);
        JSON.stringify([steps.length, bpmn_summary(xml).lanes.length, xml.length > 50000])"##;
    assert_eq!(run_bounded(code, 20), "[499,5,true]");
}

#[test]
fn segments_stay_lazy_and_are_independent() {
    with_ctx(|ctx| {
        let ev = |code: &str| ctx.eval::<String, _>(code).unwrap();
        let desc = "(n) => typeof Object.getOwnPropertyDescriptor(globalThis, n).get";
        // Nothing is loaded after install and the prelude: all names are still getters.
        assert_eq!(
            ev(&format!(
                "const d = {desc}; ['xml_parse','xml_build','bpmn_validate','bpmn_summary','bpmn_from_outline'].map(d).join()"
            )),
            "function,function,function,function,function"
        );
        // Touching a BPMN name loads only the BPMN segment (xml_parse is looked up at call time).
        assert_eq!(ev("typeof bpmn_validate"), "function");
        assert_eq!(
            ev(
                "['xml_parse','xml_build','bpmn_validate','bpmn_summary','bpmn_from_outline'].map(n => typeof Object.getOwnPropertyDescriptor(globalThis, n).get).join()"
            ),
            "function,function,undefined,undefined,function"
        );
        assert_eq!(ev("bpmn_validate('<a/>')[0].rule"), "root-element");
        // ... and that call loaded the xml segment on demand.
        assert_eq!(
            ev("typeof Object.getOwnPropertyDescriptor(globalThis, 'xml_build').get"),
            "undefined"
        );
        let help = ev("help()");
        for name in [
            "xml_parse(",
            "xml_build(",
            "bpmn_validate(",
            "bpmn_summary(",
            "bpmn_from_outline(",
        ] {
            assert!(help.contains(name), "{name}: {help}");
        }
        assert!(ev("help('bpmn_from_outline')").contains("mcp-bpmn"));
    });
}

#[test]
fn catalogue_entries_are_short_and_report_their_size() {
    // Documented budget: each entry shows up in help() only, never in the always-sent description.
    for (name, source) in [
        ("xml", include_str!("pack_xml.js")),
        ("bpmn", include_str!("pack_bpmn.js")),
        ("outline", include_str!("pack_bpmn_outline.js")),
    ] {
        for line in source.lines().filter(|l| l.starts_with("//@ ")) {
            assert!(
                line.len() < 2400,
                "{name}: catalogue line of {} bytes",
                line.len()
            );
        }
    }
}
