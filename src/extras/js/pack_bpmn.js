// Lazily evaluated BPMN segment of the helper pack; see pack.rs and pack_boot.js.
// Builds on xml_parse (pack_xml.js), which is looked up when a helper runs, never while loading.
// One catalogue line per helper (`//@ name | signature | purpose | usage`); keep lines free of `|`.
//@ bpmn_validate | bpmn_validate(xml) | check BPMN 2.0 XML (string or xml_parse tree) and return findings [{level, id, rule, msg}] | level is 'error' or 'warning'; id is the offending element's id. Rules: xml-parse, root-element, duplicate-id, missing-id, unresolved-ref (sourceRef targetRef attachedToRef processRef bpmnElement flowNodeRef incoming outgoing), ref-type, sequence-flow-same-process, message-flow-same-participant, start-event-required, end-event-required, start-event-incoming, end-event-outgoing, orphan-node, no-incoming, no-outgoing, gateway-flow-condition, default-flow-invalid, gateway-mixed, gateway-pass-through, lane-ref-process, flow-ref-mismatch and DI checks di-missing, di-missing-shape, di-missing-edge, di-duplicate, di-shape-bounds, di-edge-waypoints, di-unresolved-ref, di-ref-type. Read-only; at most 1000 findings (then a 'truncated' entry). Example: bpmn_validate(xml).filter(f => f.level === 'error')
//@ bpmn_summary | bpmn_summary(xml, opts?) | compact overview of BPMN 2.0 XML (about 1 KB for a large diagram) | Returns {processes: [{id, name, participant, nodes, flows, lanes}], participants, lanes, counts (element type to number), flows (sequence flow count), messageFlows [{id, from, to}] (participants), orphans (node ids with no sequence flow), diErrors ('rule:id'), findings {errors, warnings}}. Lists hold at most opts.max entries (default 25, up to 1000) and a truncated map gives the real totals. Throws on malformed XML or a non-BPMN root. Example: bpmn_summary(xml).counts.userTask
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const NS_MODEL = "http://www.omg.org/spec/BPMN/20100524/MODEL";
  const NS_BPMNDI = "http://www.omg.org/spec/BPMN/20100524/DI";
  const NS_DD_DI = "http://www.omg.org/spec/DD/20100524/DI";
  const NS_DC = "http://www.omg.org/spec/DD/20100524/DC";
  const MAX_FINDINGS = 1000;
  const set = (s) => new Set(s.split(" "));
  const TASKS = set("task userTask serviceTask scriptTask businessRuleTask sendTask receiveTask manualTask");
  const SUBPROCS = set("subProcess adHocSubProcess transaction");
  const EVENTS = set("startEvent endEvent intermediateCatchEvent intermediateThrowEvent boundaryEvent implicitThrowEvent");
  const GATEWAYS = set("exclusiveGateway inclusiveGateway parallelGateway eventBasedGateway complexGateway");
  const ARTIFACTS = set("textAnnotation group dataObject dataObjectReference dataStoreReference");
  const EDGES = set("sequenceFlow messageFlow association dataInputAssociation dataOutputAssociation");
  const isActivity = (l) => TASKS.has(l) || SUBPROCS.has(l) || l === "callActivity";
  const isNode = (l) => isActivity(l) || EVENTS.has(l) || GATEWAYS.has(l);
  const isContainer = (l) => l === "process" || SUBPROCS.has(l);
  const clip = (s) => (s.length > 80 ? s.slice(0, 77) + "..." : s);
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);

  // Resolve prefixes to namespaces and index the BPMN elements of a parsed tree.
  const index = (tree) => {
    const m = { els: [], di: [], byId: new Map(), dups: [], root: null, procOf: new Map() };
    const walk = (node, scope, parent, container, proc) => {
      let sc = scope;
      for (const k of Object.keys(node.attrs)) {
        if (k === "xmlns" || k.startsWith("xmlns:")) {
          if (sc === scope) sc = Object.assign(Object.create(null), scope);
          sc[k === "xmlns" ? "" : k.slice(6)] = node.attrs[k];
        }
      }
      const colon = node.name.indexOf(":");
      const prefix = colon < 0 ? "" : node.name.slice(0, colon);
      const local = colon < 0 ? node.name : node.name.slice(colon + 1);
      const uri = sc[prefix];
      const kind = uri === NS_MODEL ? "m" : uri === NS_BPMNDI ? "di" : uri === NS_DC ? "dc" : uri === NS_DD_DI ? "dd" : "x";
      const info = { n: node, local, kind, parent, container, proc, id: own(node.attrs, "id") ? node.attrs.id : "", kids: [] };
      if (parent) parent.kids.push(info);
      if (!parent) m.root = info;
      if (kind === "x" || local === "extensionElements") return;
      if (kind === "dc" || kind === "dd") return;
      if (kind === "m") m.els.push(info);
      else if (kind === "di") m.di.push(info);
      if ((kind === "m" || kind === "di") && info.id !== "") {
        if (m.byId.has(info.id)) m.dups.push(info);
        else m.byId.set(info.id, info);
      }
      const nextContainer = kind === "m" && isContainer(local) ? info : container;
      const nextProc = kind === "m" && local === "process" && parent && parent.local === "definitions" ? info : proc;
      for (const child of node.children) walk(child, sc, info, nextContainer, nextProc);
    };
    walk(tree, Object.create(null), null, null, null);
    return m;
  };
  const text = (info) => info.n.text.trim();
  const attr = (info, k) => (own(info.n.attrs, k) ? info.n.attrs[k] : undefined);
  const kidsOf = (info, local) => info.kids.filter((k) => k.kind === "m" && k.local === local);

  const treeOf = (name, xml) => {
    if (typeof xml === "string") return g.xml_parse(xml);
    if (xml !== null && typeof xml === "object" && typeof xml.name === "string" && Array.isArray(xml.children)) return xml;
    throw new TypeError(name + ": xml must be a string or an xml_parse tree");
  };

  const analyse = (m) => {
    const f = [];
    const add = (level, id, rule, msg) => {
      if (f.length === MAX_FINDINGS) f.push({ level: "warning", id: "", rule: "truncated", msg: "further findings omitted" });
      if (f.length <= MAX_FINDINGS) f.push({ level, id, rule, msg });
    };
    const out = { f, m, flows: [], msgFlows: [], inc: new Map(), outg: new Map(), orphans: [], participantOf: null };
    const root = m.root;
    if (root.kind !== "m" || root.local !== "definitions") {
      add("error", "", "root-element", "root element is not a BPMN 2.0 definitions element");
      out.bad = true;
      return out;
    }
    for (const d of m.dups) add("error", clip(d.id), "duplicate-id", "id is used by more than one element");
    const byProc = new Map();
    for (const e of m.els) if (e.local === "participant" && attr(e, "processRef") !== undefined) byProc.set(attr(e, "processRef"), e);
    const partOf = (e) => (e.local === "participant" ? e : e.proc ? byProc.get(e.proc.id) : undefined);
    out.participantOf = partOf;

    for (const e of m.els) {
      const l = e.local;
      if ((isNode(l) || l === "sequenceFlow" || l === "messageFlow" || l === "process" || l === "participant" || l === "lane") && e.id === "") {
        add("error", "", "missing-id", l + " has no id");
      }
    }
    const find = (id) => (id === undefined ? undefined : m.byId.get(id));
    const need = (e, name, kindCheck, what) => {
      const v = attr(e, name);
      if (v === undefined || v === "") {
        add("error", e.id, "unresolved-ref", e.local + " has no " + name);
        return undefined;
      }
      const t = find(v);
      if (!t || t.kind !== "m") {
        add("error", e.id, "unresolved-ref", name + " '" + clip(v) + "' does not exist");
        return undefined;
      }
      if (kindCheck && !kindCheck(t.local)) {
        add("error", e.id, "ref-type", name + " '" + clip(v) + "' is a " + t.local + ", expected " + what);
        return undefined;
      }
      return t;
    };

    for (const e of m.els) {
      const l = e.local;
      if (l === "sequenceFlow") {
        const s = need(e, "sourceRef", isNode, "a flow node");
        const t = need(e, "targetRef", isNode, "a flow node");
        out.flows.push({ e, s, t });
        if (s && t) {
          if (s.proc !== t.proc) add("error", e.id, "sequence-flow-same-process", "source and target are in different processes");
          else if (s.container !== t.container || s.container !== e.container) {
            add("error", e.id, "sequence-flow-same-process", "source, target and flow are not in the same process or subprocess");
          }
          if (!out.outg.has(s)) out.outg.set(s, []);
          out.outg.get(s).push(e);
          if (!out.inc.has(t)) out.inc.set(t, []);
          out.inc.get(t).push(e);
        }
      } else if (l === "messageFlow") {
        const ok = (x) => isNode(x) || x === "participant";
        const s = need(e, "sourceRef", ok, "a participant or flow node");
        const t = need(e, "targetRef", ok, "a participant or flow node");
        const ps = s && partOf(s);
        const pt = t && partOf(t);
        out.msgFlows.push({ e, s, t, ps, pt });
        if (s && t) {
          if (!ps || !pt) add("warning", e.id, "message-flow-same-participant", "cannot determine the participant of an endpoint");
          else if (ps === pt) add("error", e.id, "message-flow-same-participant", "source and target are in the same participant");
        }
      } else if (l === "association") {
        need(e, "sourceRef");
        need(e, "targetRef");
      } else if (l === "boundaryEvent") {
        need(e, "attachedToRef", isActivity, "an activity");
      } else if (l === "participant") {
        if (attr(e, "processRef") !== undefined) need(e, "processRef", (x) => x === "process", "a process");
      }
      if ((isActivity(l) || GATEWAYS.has(l)) && attr(e, "default") !== undefined) {
        const d = find(attr(e, "default"));
        if (!d || d.local !== "sequenceFlow" || attr(d, "sourceRef") !== e.id) {
          add("error", e.id, "default-flow-invalid", "default flow '" + clip(attr(e, "default")) + "' is not an outgoing sequence flow of this element");
        }
      }
      if (isNode(l)) {
        for (const [tag, end] of [["incoming", "targetRef"], ["outgoing", "sourceRef"]]) {
          for (const k of kidsOf(e, tag)) {
            const t = find(text(k));
            if (!t) add("error", e.id, "unresolved-ref", tag + " '" + clip(text(k)) + "' does not exist");
            else if (t.local !== "sequenceFlow") add("error", e.id, "ref-type", tag + " '" + clip(text(k)) + "' is not a sequenceFlow");
            else if (attr(t, end) !== e.id) add("error", e.id, "flow-ref-mismatch", tag + " '" + clip(text(k)) + "' does not point at this element");
          }
        }
      }
      if (l === "lane") {
        for (const k of kidsOf(e, "flowNodeRef")) {
          const t = find(text(k));
          if (!t) add("error", e.id, "unresolved-ref", "flowNodeRef '" + clip(text(k)) + "' does not exist");
          else if (!isNode(t.local)) add("error", e.id, "ref-type", "flowNodeRef '" + clip(text(k)) + "' is not a flow node");
          else if (t.proc !== e.proc) add("error", e.id, "lane-ref-process", "flowNodeRef '" + clip(text(k)) + "' belongs to another process");
        }
      }
    }

    // Start/end event rules per process or subprocess.
    for (const c of m.els) {
      if (!isContainer(c.local) || c.local === "adHocSubProcess") continue;
      const nodes = c.kids.filter((k) => k.kind === "m" && isNode(k.local));
      if (!nodes.length) continue;
      if (!nodes.some((k) => k.local === "startEvent")) add("error", c.id, "start-event-required", c.local + " has no start event");
      if (!nodes.some((k) => k.local === "endEvent")) add("error", c.id, "end-event-required", c.local + " has no end event");
    }
    const mfNodes = new Set();
    for (const x of out.msgFlows) {
      if (x.s) mfNodes.add(x.s);
      if (x.t) mfNodes.add(x.t);
    }
    for (const e of m.els) {
      const l = e.local;
      if (!isNode(l)) continue;
      const ins = (out.inc.get(e) || []).length;
      const outs = (out.outg.get(e) || []).length;
      if (l === "startEvent" && ins) add("error", e.id, "start-event-incoming", "start event has an incoming sequence flow");
      if (l === "endEvent" && outs) add("error", e.id, "end-event-outgoing", "end event has an outgoing sequence flow");
      const exempt = attr(e, "isForCompensation") === "true" || (e.container && e.container.local === "adHocSubProcess") ||
        e.kids.some((k) => k.kind === "m" && k.local === "linkEventDefinition");
      if (exempt) continue;
      if (!ins && !outs && l !== "boundaryEvent" && !mfNodes.has(e)) {
        out.orphans.push(e);
        add("warning", e.id, "orphan-node", l + " has no sequence flow");
        continue;
      }
      if (!ins && l !== "startEvent" && l !== "boundaryEvent") add("warning", e.id, "no-incoming", l + " has no incoming sequence flow");
      if (!outs && l !== "endEvent") add("warning", e.id, "no-outgoing", l + " has no outgoing sequence flow");
      if (GATEWAYS.has(l)) {
        if (ins > 1 && outs > 1) add("warning", e.id, "gateway-mixed", "gateway both joins and forks");
        else if (ins === 1 && outs === 1) add("warning", e.id, "gateway-pass-through", "gateway has one incoming and one outgoing flow");
        if ((l === "exclusiveGateway" || l === "inclusiveGateway") && outs > 1) {
          for (const fl of out.outg.get(e)) {
            if (!kidsOf(fl, "conditionExpression").length && fl.id !== attr(e, "default")) {
              add("error", fl.id, "gateway-flow-condition", "flow leaves a " + l + " without a condition and is not its default");
            }
          }
        }
      }
    }

    // Diagram interchange.
    const planes = m.di.filter((d) => d.local === "BPMNPlane");
    if (!planes.length) add("warning", "", "di-missing", "no BPMNDiagram");
    else {
      const covered = new Map();
      for (const d of m.di) {
        if (d.local !== "BPMNShape" && d.local !== "BPMNEdge" && d.local !== "BPMNPlane") continue;
        const ref = attr(d, "bpmnElement");
        const t = find(ref);
        if (ref === undefined || !t || t.kind !== "m") {
          add("error", d.id, "di-unresolved-ref", d.local + " bpmnElement '" + clip(ref === undefined ? "" : ref) + "' does not exist");
          continue;
        }
        if (d.local === "BPMNPlane") {
          if (!isContainer(t.local) && t.local !== "collaboration") add("error", d.id, "di-ref-type", "BPMNPlane references a " + t.local);
          continue;
        }
        if ((d.local === "BPMNEdge") !== EDGES.has(t.local)) {
          add("error", d.id, "di-ref-type", d.local + " references a " + t.local);
          continue;
        }
        covered.set(t, (covered.get(t) || 0) + 1);
        if (covered.get(t) === 2) add("error", t.id, "di-duplicate", "more than one " + d.local + " for this element");
        if (d.local === "BPMNShape") {
          const b = d.kids.find((k) => k.kind === "dc" && k.local === "Bounds");
          const w = b && Number(attr(b, "width"));
          const h = b && Number(attr(b, "height"));
          if (!b || !(w > 0) || !(h > 0) || !Number.isFinite(Number(attr(b, "x"))) || !Number.isFinite(Number(attr(b, "y")))) {
            add("error", d.id, "di-shape-bounds", "shape has missing or invalid Bounds");
          }
        } else if (d.kids.filter((k) => k.kind === "dd" && k.local === "waypoint").length < 2) {
          add("error", d.id, "di-edge-waypoints", "edge has fewer than two waypoints");
        }
      }
      for (const e of m.els) {
        const l = e.local;
        if (e.id === "" || m.byId.get(e.id) !== e) continue;
        if ((isNode(l) || l === "participant" || l === "lane") && !covered.has(e)) add("error", e.id, "di-missing-shape", l + " has no BPMNShape");
        else if ((l === "sequenceFlow" || l === "messageFlow") && !covered.has(e)) add("error", e.id, "di-missing-edge", l + " has no BPMNEdge");
      }
    }
    return out;
  };

  define("bpmn_validate", function bpmn_validate(xml) {
    if (typeof xml !== "string" && (xml === null || typeof xml !== "object")) {
      throw new TypeError("bpmn_validate: xml must be a string or an xml_parse tree");
    }
    let tree;
    try {
      tree = treeOf("bpmn_validate", xml);
    } catch (e) {
      if (typeof xml === "string" && (e instanceof SyntaxError || e instanceof RangeError)) {
        return [{ level: "error", id: "", rule: "xml-parse", msg: e.message }];
      }
      throw e;
    }
    return analyse(index(tree)).f;
  });

  define("bpmn_summary", function bpmn_summary(xml, opts) {
    const o = opts === undefined ? {} : opts;
    if (o === null || typeof o !== "object") throw new TypeError("bpmn_summary: opts must be an object");
    const max = o.max === undefined ? 25 : o.max;
    if (!Number.isInteger(max) || max < 1 || max > 1000) throw new RangeError("bpmn_summary: max must be an integer from 1 to 1000");
    const m = index(treeOf("bpmn_summary", xml));
    const a = analyse(m);
    if (a.bad) throw new SyntaxError("bpmn_summary: " + a.f[0].msg);
    const truncated = {};
    const cap = (key, list) => {
      if (list.length > max) truncated[key] = list.length;
      return list.slice(0, max);
    };
    const put = (obj, k, v) => {
      if (v !== undefined && v !== "") obj[k] = v;
      return obj;
    };
    const counts = {};
    const procs = new Map();
    const procList = [];
    const partByProc = new Map();
    for (const e of m.els) if (e.local === "participant" && attr(e, "processRef") !== undefined) partByProc.set(attr(e, "processRef"), e.id);
    for (const e of m.els) {
      if (e.local === "process") {
        const p = put({ id: e.id, name: attr(e, "name") }, "participant", partByProc.get(e.id));
        p.nodes = 0;
        p.flows = 0;
        p.lanes = 0;
        procs.set(e, p);
        procList.push(p);
      }
    }
    const lanes = [];
    const participants = [];
    let flowCount = 0;
    for (const e of m.els) {
      const l = e.local;
      if (isNode(l) || ARTIFACTS.has(l)) counts[l] = (counts[l] || 0) + 1;
      const p = e.proc && procs.get(e.proc);
      if (isNode(l) && p) p.nodes++;
      if (l === "sequenceFlow") {
        flowCount++;
        if (p) p.flows++;
      }
      if (l === "lane") {
        if (p) p.lanes++;
        const lane = put({ id: e.id, name: attr(e, "name") }, "process", e.proc ? e.proc.id : "");
        lane.nodes = kidsOf(e, "flowNodeRef").length;
        lanes.push(lane);
      }
      if (l === "participant") participants.push(put(put({ id: e.id }, "name", attr(e, "name")), "processRef", attr(e, "processRef")));
    }
    const sorted = {};
    for (const k of Object.keys(counts).sort()) sorted[k] = counts[k];
    const messageFlows = a.msgFlows.map((x) => ({
      id: x.e.id,
      from: x.ps ? x.ps.id : x.s ? x.s.id : "",
      to: x.pt ? x.pt.id : x.t ? x.t.id : "",
    }));
    const diErrors = a.f.filter((x) => x.rule.startsWith("di-")).map((x) => x.rule + ":" + x.id);
    return {
      processes: cap("processes", procList),
      participants: cap("participants", participants),
      lanes: cap("lanes", lanes),
      counts: sorted,
      flows: flowCount,
      messageFlows: cap("messageFlows", messageFlows),
      orphans: cap("orphans", a.orphans.map((e) => e.id)),
      diErrors: cap("diErrors", diErrors),
      findings: {
        errors: a.f.filter((x) => x.level === "error").length,
        warnings: a.f.filter((x) => x.level === "warning").length,
      },
      ...(Object.keys(truncated).length ? { truncated } : {}),
    };
  });
})(globalThis);
