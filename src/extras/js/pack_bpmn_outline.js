// Lazily evaluated BPMN outline-to-XML segment of the helper pack (opt-in, kept apart from the
// read-side BPMN helpers); see pack.rs and pack_boot.js. Uses xml_build and bpmn_validate when it runs.
// One catalogue line per helper (`//@ name | signature | purpose | usage`); keep lines free of `|`.
//@ bpmn_from_outline | bpmn_from_outline(steps, opts?) | build BPMN 2.0 XML with diagram interchange from a flat outline (opt-in; complements, never replaces, the mcp-bpmn server and the bpmn-modeler skill) | steps is an array of {id, type, name?, lane?, pool?, next?, cond?, msg?, attachedTo?, trigger?, steps?}: types start end task userTask serviceTask xor and or boundary subprocess call. next is an id or an array of ids (same pool); on xor/or, cond is a string for the single next, an array aligned with next, or a map id to text, and the one next without cond becomes the default flow. boundary needs attachedTo (an activity id) and takes trigger timer, message, error or signal; start and end also take trigger (end: terminate). subprocess with steps is expanded. msg (id or array) draws a message flow into another pool. ids must be XML NCNames, unique over the whole outline; at most 500 steps. Layout is left to right by longest path, lanes and pools as bands (back edges run below the nodes). The result is checked with bpmn_validate and any error throws. Prefer the MCP tools or the skill when they are available; use this for a headless first draft. opts: name (pool or process name), indent. Example: bpmn_from_outline([{id: 's', type: 'start', next: 't'}, {id: 't', type: 'task', name: 'Do', next: 'e'}, {id: 'e', type: 'end'}])
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const MAX_STEPS = 500;
  const NCNAME = /^[A-Za-z_][A-Za-z0-9_.-]*$/;
  const TYPES = {
    start: "startEvent", end: "endEvent", task: "task", userTask: "userTask", serviceTask: "serviceTask",
    xor: "exclusiveGateway", and: "parallelGateway", or: "inclusiveGateway", boundary: "boundaryEvent",
    subprocess: "subProcess", call: "callActivity",
  };
  const TRIGGERS = { timer: "timerEventDefinition", message: "messageEventDefinition", error: "errorEventDefinition", signal: "signalEventDefinition", terminate: "terminateEventDefinition" };
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const fail = (msg) => {
    throw new TypeError("bpmn_from_outline: " + msg);
  };
  const el = (name, attrs, children, text) => ({ name, attrs: attrs || {}, children: children || [], text: text || "" });
  const B = (l) => "bpmn:" + l;
  const sizeOf = (type) => (type === "start" || type === "end" || type === "boundary" ? [36, 36] : type === "xor" || type === "and" || type === "or" ? [50, 50] : [100, 80]);

  // Lay out one scope (a process or an expanded subprocess). nodes: {id,type,out:[node],host,lane,w,h}.
  // Returns positions relative to the scope origin plus its extent and lane band heights.
  const layoutScope = (nodes, nLanes) => {
    const flowsOf = (n) => n.out;
    const preds = new Map(nodes.map((n) => [n, []]));
    for (const n of nodes) {
      for (const t of flowsOf(n)) preds.get(t).push([n, 1]);
      if (n.host) preds.get(n).push([n.host, 0]);
    }
    const succ = (n) => {
      const s = flowsOf(n).map((t) => [t, 1]);
      for (const b of nodes) if (b.host === n) s.push([b, 0]);
      return s;
    };
    // Depth-first search for back edges, then longest-path ranks over the remaining DAG.
    const state = new Map();
    const back = new Set();
    const order = [];
    const roots = nodes.filter((n) => !preds.get(n).length).concat(nodes);
    for (const r of roots) {
      if (state.has(r)) continue;
      const stack = [[r, succ(r), 0]];
      state.set(r, 1);
      while (stack.length) {
        const top = stack[stack.length - 1];
        if (top[2] < top[1].length) {
          const [t] = top[1][top[2]++];
          if (!state.has(t)) {
            state.set(t, 1);
            stack.push([t, succ(t), 0]);
          } else if (state.get(t) === 1) back.add(top[0].id + ">" + t.id);
        } else {
          state.set(top[0], 2);
          order.push(top[0]);
          stack.pop();
        }
      }
    }
    const rank = new Map();
    for (const n of order.reverse()) {
      let r = 0;
      for (const [p, w] of preds.get(n)) if (!back.has(p.id + ">" + n.id)) r = Math.max(r, rank.get(p) + w);
      rank.set(n, r);
    }
    // Rows: first free row of the node's (lane, rank) slot; the target of a boundary event starts below its host.
    const row = new Map();
    const used = new Set();
    const placed = nodes.filter((n) => n.type !== "boundary").sort((a, b) => rank.get(a) - rank.get(b) || a.idx - b.idx);
    for (const n of placed) {
      let r = 0;
      for (const [p, w] of preds.get(n)) {
        if (p.type === "boundary" && !back.has(p.id + ">" + n.id)) r = Math.max(r, row.get(p.host) + 1);
      }
      while (used.has(n.lane + "|" + rank.get(n) + "|" + r)) r++;
      used.add(n.lane + "|" + rank.get(n) + "|" + r);
      row.set(n, r);
    }
    const ranks = Math.max(0, ...placed.map((n) => rank.get(n))) + 1;
    const colW = new Array(ranks).fill(0);
    for (const n of placed) colW[rank.get(n)] = Math.max(colW[rank.get(n)], n.w);
    const colX = [];
    let x = 0;
    for (let r = 0; r < ranks; r++) {
      colX.push(x);
      x += colW[r] + 60;
    }
    const rowH = Array.from({ length: nLanes }, () => []);
    for (const n of placed) rowH[n.lane][row.get(n)] = Math.max(rowH[n.lane][row.get(n)] || 0, n.h + 40);
    const laneTop = [];
    const laneH = [];
    let y = 0;
    for (let l = 0; l < nLanes; l++) {
      const rows = rowH[l].length || 1;
      for (let k = 0; k < rows; k++) rowH[l][k] = rowH[l][k] || 80;
      laneTop.push(y);
      laneH.push(rowH[l].reduce((a, b) => a + b, 0));
      y += laneH[l];
    }
    const pos = new Map();
    for (const n of placed) {
      const l = n.lane;
      let top = laneTop[l];
      for (let k = 0; k < row.get(n); k++) top += rowH[l][k];
      pos.set(n, { x: colX[rank.get(n)] + (colW[rank.get(n)] - n.w) / 2, y: top + (rowH[l][row.get(n)] - n.h) / 2, w: n.w, h: n.h });
    }
    for (const n of nodes) {
      if (n.type === "boundary") {
        const h = pos.get(n.host);
        pos.set(n, { x: h.x + h.w / 2 - 18, y: h.y + h.h - 18, w: 36, h: 36 });
      }
    }
    return { pos, width: Math.max(0, x - 60), height: y, laneTop, laneH, rank, back };
  };

  const route = (s, t, a, b, isBack, bottom) => {
    const out = (a.x + a.w) | 0;
    if (s.type === "boundary") {
      const px = a.x + 18;
      const py = a.y + 36;
      return [[px, py], [px, b.y + b.h / 2], [b.x, b.y + b.h / 2]];
    }
    const sy = a.y + a.h / 2;
    const ty = b.y + b.h / 2;
    if (isBack) {
      const by = Math.max(a.y + a.h, b.y + b.h, bottom) + 10;
      return [[out, sy], [out + 15, sy], [out + 15, by], [b.x - 15, by], [b.x - 15, ty], [b.x, ty]];
    }
    if (sy === ty) return [[out, sy], [b.x, ty]];
    const mx = out + 30;
    return [[out, sy], [mx, sy], [mx, ty], [b.x, ty]];
  };

  define("bpmn_from_outline", function bpmn_from_outline(steps, opts) {
    const o = opts === undefined ? {} : opts;
    if (o === null || typeof o !== "object") fail("opts must be an object");
    if (!Array.isArray(steps) || !steps.length) fail("steps must be a non-empty array");
    const used = new Set();
    const byId = new Map();
    let count = 0;
    const clean = (list, parent) =>
      list.map((s) => {
        if (s === null || typeof s !== "object" || Array.isArray(s)) fail("every step must be an object");
        if (++count > MAX_STEPS) fail("more than " + MAX_STEPS + " steps");
        if (typeof s.id !== "string" || !NCNAME.test(s.id) || s.id.length > 100) fail("step id must be an XML NCName (letters, digits, _ . -; not starting with a digit)");
        if (used.has(s.id)) fail("duplicate step id");
        used.add(s.id);
        if (!own(TYPES, s.type)) fail("unknown step type (use " + Object.keys(TYPES).join(", ") + ")");
        for (const k of ["name", "lane", "pool", "trigger", "attachedTo"]) {
          if (s[k] !== undefined && (typeof s[k] !== "string" || s[k].length > 200)) fail(k + " must be a string of at most 200 characters");
        }
        if (s.trigger !== undefined && !own(TRIGGERS, s.trigger)) fail("trigger must be one of " + Object.keys(TRIGGERS).join(", "));
        if (s.steps !== undefined && (s.type !== "subprocess" || !Array.isArray(s.steps) || !s.steps.length)) fail("steps is only for a subprocess and must be a non-empty array");
        if (s.type === "boundary" && !s.attachedTo) fail("a boundary step needs attachedTo");
        const [w, h] = sizeOf(s.type);
        const node = { src: s, id: s.id, type: s.type, idx: count, w, h, out: [], parent, lane: 0, labels: [] };
        byId.set(s.id, node);
        return node;
      });
    const top = clean(steps, null);
    const subs = [];
    const collect = (list) => {
      for (const n of list) {
        if (n.src.steps) {
          n.kids = clean(n.src.steps, n);
          subs.push(n);
          collect(n.kids);
        }
      }
    };
    collect(top);
    const everyNode = [...byId.values()];
    const asList = (v, what) => (v === undefined ? [] : typeof v === "string" ? [v] : Array.isArray(v) && v.every((x) => typeof x === "string") ? v : fail(what + " must be an id or an array of ids"));
    for (const n of everyNode) {
      const s = n.src;
      if (s.attachedTo) {
        const host = byId.get(s.attachedTo);
        if (!host || !["task", "userTask", "serviceTask", "subprocess", "call"].includes(host.type) || host.parent !== n.parent || (host.src.pool || "") !== (s.pool || "")) fail("attachedTo must name an activity in the same scope and pool");
        n.host = host;
      } else if (s.type === "boundary") fail("a boundary step needs attachedTo");
      const nexts = asList(s.next, "next");
      let conds = [];
      if (s.cond !== undefined) {
        if (s.type !== "xor" && s.type !== "or") fail("cond is only allowed on xor and or steps");
        if (typeof s.cond === "string") {
          if (nexts.length !== 1) fail("a string cond needs exactly one next; use an array or a map");
          conds = [s.cond];
        } else if (Array.isArray(s.cond)) {
          if (s.cond.length !== nexts.length) fail("cond array must align with next");
          conds = s.cond.map((c) => (c === null || c === undefined ? "" : c));
        } else if (s.cond && typeof s.cond === "object") conds = nexts.map((t) => (own(s.cond, t) ? s.cond[t] : ""));
        else fail("cond must be a string, an array or a map");
        if (conds.some((c) => typeof c !== "string" || c.length > 200)) fail("every cond must be a string of at most 200 characters");
      } else conds = nexts.map(() => "");
      if (s.type === "end" && nexts.length) fail("an end step cannot have next");
      nexts.forEach((id, k) => {
        const t = byId.get(id);
        if (!t) fail("next refers to an unknown step");
        if (t.parent !== n.parent) fail("next must stay inside one subprocess scope");
        if (t.type === "start" || t.type === "boundary") fail("next cannot point at a start or boundary step");
        if ((s.pool || "") !== (t.src.pool || "") && !n.parent) fail("next must stay inside one pool; use msg for a message flow");
        n.out.push(t);
        n.labels.push(conds[k]);
      });
      if (s.type === "xor" || s.type === "or") {
        const bare = n.labels.filter((c) => c === "").length;
        if (n.out.length > 1 && (bare > 1 || bare === n.labels.length)) fail(s.type + " with several next needs a cond on all but at most one (the default)");
      }
    }
    // Pools -> processes; lanes per process.
    const poolNames = [];
    for (const n of top) {
      const p = n.src.pool || "";
      if (!poolNames.includes(p)) poolNames.push(p);
    }
    const procs = poolNames.map((name) => ({ name, nodes: top.filter((n) => (n.src.pool || "") === name), lanes: [] }));
    for (const p of procs) {
      for (const n of p.nodes) {
        const ln = n.src.lane;
        if (ln !== undefined && !p.lanes.includes(ln)) p.lanes.push(ln);
      }
      if (p.lanes.length && p.nodes.some((n) => n.src.lane === undefined)) p.lanes.unshift("Unassigned");
      for (const n of p.nodes) n.lane = Math.max(0, p.lanes.indexOf(n.src.lane === undefined ? "Unassigned" : n.src.lane));
    }
    const collab = procs.length > 1 || poolNames[0] !== "" || procs[0].lanes.length > 0;
    // Unique generated ids.
    const fresh = (base) => {
      let id = base;
      for (let k = 2; used.has(id); k++) id = base + "_" + k;
      used.add(id);
      return id;
    };
    // Layout every expanded subprocess (innermost first), then each process.
    const layouts = new Map();
    for (const sp of subs.slice().reverse()) {
      const lay = layoutScope(sp.kids, 1);
      layouts.set(sp, lay);
      sp.w = Math.max(200, lay.width + 60);
      sp.h = Math.max(120, lay.height + 60);
    }
    const shapes = [];
    const edges = [];
    const abs = new Map();
    const placeScope = (list, lay, ox, oy) => {
      for (const n of list) {
        const p = lay.pos.get(n);
        const a = { x: Math.round(ox + p.x), y: Math.round(oy + p.y), w: n.w, h: n.h };
        abs.set(n, a);
        shapes.push([n, a]);
        if (n.kids) placeScope(n.kids, layouts.get(n), a.x + 30, a.y + 30);
      }
    };
    let offY = 20;
    const procEls = [];
    const partEls = [];
    const poolShapes = [];
    const poolOf = new Map();
    const scopeFlows = [];
    // Flows are created before any node is emitted so every node knows its incoming/outgoing ids.
    const emitFlows = (list) => {
      for (const n of list) {
        n.flowIds = [];
        n.inFlows = [];
      }
      const flows = [];
      for (const n of list) {
        n.out.forEach((t, k) => {
          const id = fresh("Flow_" + n.id + "_" + t.id);
          n.flowIds.push(id);
          t.inFlows.push(id);
          const attrs = { id, sourceRef: n.id, targetRef: t.id };
          const kids = [];
          if (n.labels[k]) {
            attrs.name = n.labels[k];
            kids.push(el(B("conditionExpression"), { "xsi:type": "bpmn:tFormalExpression" }, [], n.labels[k]));
          }
          flows.push({ n, t, id, xml: el(B("sequenceFlow"), attrs, kids) });
        });
      }
      scopeFlows.push(...flows);
      return flows;
    };
    // Subprocess scopes first so nested nodes know their flow ids when the parent is emitted.
    const flowsOfScope = new Map();
    for (const sp of subs.slice().reverse()) flowsOfScope.set(sp, emitFlows(sp.kids));
    for (const p of procs) flowsOfScope.set(p, emitFlows(p.nodes));
    const buildScope = (list, flows) => [...list.map(emitNode), ...flows.map((f) => f.xml)];
    const emitNode = (n) => {
      const s = n.src;
      const attrs = { id: n.id };
      if (s.name) attrs.name = s.name;
      const kids = [];
      if (n.type === "call" && s.ref) attrs.calledElement = s.ref;
      if (n.host) attrs.attachedToRef = n.host.id;
      if (n.type === "boundary") attrs.cancelActivity = "true";
      if ((s.type === "xor" || s.type === "or") && n.out.length > 1) {
        const dk = n.labels.findIndex((c) => c === "");
        if (dk >= 0) attrs.default = n.flowIds[dk];
      }
      for (const f of n.inFlows) kids.push(el(B("incoming"), {}, [], f));
      for (const f of n.flowIds) kids.push(el(B("outgoing"), {}, [], f));
      if (s.trigger) kids.push(el(B(TRIGGERS[s.trigger]), { id: fresh(n.id + "_def") }));
      else if (n.type === "boundary") kids.push(el(B("timerEventDefinition"), { id: fresh(n.id + "_def") }));
      if (n.kids) kids.push(...buildScope(n.kids, flowsOfScope.get(n)));
      if (n.type === "subprocess" && !n.kids) attrs.isExpanded = "false";
      return el(B(TYPES[n.type]), attrs, kids);
    };
    // Processes, lanes, participants and the diagram.
    const laneIds = new Map();
    procs.forEach((p, k) => {
      p.id = fresh("Process_" + (k + 1));
      const lay = layoutScope(p.nodes, Math.max(1, p.lanes.length));
      p.lay = lay;
      const kids = [];
      if (p.lanes.length) {
        const lanes = p.lanes.map((ln, li) => {
          const id = fresh("Lane_" + (k + 1) + "_" + (li + 1));
          laneIds.set(p.id + "|" + li, id);
          const refs = p.nodes.filter((n) => n.lane === li).map((n) => el(B("flowNodeRef"), {}, [], n.id));
          return el(B("lane"), { id, name: ln }, refs);
        });
        kids.push(el(B("laneSet"), { id: fresh("LaneSet_" + (k + 1)) }, lanes));
      }
      kids.push(...buildScope(p.nodes, flowsOfScope.get(p)));
      procEls.push(el(B("process"), { id: p.id, name: p.name || o.name || "", isExecutable: "false" }, kids));
      for (const n of p.nodes) poolOf.set(n, p);
    });
    if (procEls.length === 1 && !procEls[0].attrs.name) delete procEls[0].attrs.name;
    const collabId = collab ? fresh("Collaboration_1") : null;
    procs.forEach((p) => {
      const lay = p.lay;
      const headX = collab ? 60 : 0;
      const ox = headX + 20;
      placeScope(p.nodes, lay, ox, offY);
      if (collab) {
        p.partId = fresh("Participant_" + (procs.indexOf(p) + 1));
        partEls.push(el(B("participant"), { id: p.partId, name: p.name || o.name || "Process", processRef: p.id }));
        const w = headX + 40 + lay.width;
        poolShapes.push([p.partId, 0, offY, w, lay.height, true]);
        for (let li = 0; li < p.lanes.length; li++) poolShapes.push([laneIds.get(p.id + "|" + li), 30, offY + lay.laneTop[li], w - 30, lay.laneH[li], true]);
        p.top = offY;
        p.bottom = offY + lay.height;
      }
      offY += lay.height + 60;
    });
    for (const f of scopeFlows) {
      const a = abs.get(f.n);
      const b = abs.get(f.t);
      const lay = f.n.parent ? layouts.get(f.n.parent) : poolOf.get(f.n).lay;
      const isBack = lay.back.has(f.n.id + ">" + f.t.id);
      const scopeBottom = f.n.parent ? abs.get(f.n.parent).y + abs.get(f.n.parent).h - 40 : 0;
      edges.push([f.id, route(f.n, f.t, a, b, isBack, scopeBottom).map(([x, y]) => [Math.round(x), Math.round(y)])]);
    }
    // Message flows between pools.
    const msgEls = [];
    for (const n of top) {
      for (const tid of asList(n.src.msg, "msg")) {
        const t = byId.get(tid);
        if (!t || t.parent) fail("msg must name a top-level step");
        const ps = poolOf.get(n);
        const pt = poolOf.get(t);
        if (ps === pt) fail("msg must target a step in another pool");
        const id = fresh("Msg_" + n.id + "_" + t.id);
        msgEls.push(el(B("messageFlow"), { id, sourceRef: n.id, targetRef: t.id }));
        const a = abs.get(n);
        const b = abs.get(t);
        const down = ps.top < pt.top;
        const sy = down ? a.y + a.h : a.y;
        const ty = down ? b.y : b.y + b.h;
        const my = Math.round((sy + ty) / 2);
        edges.push([id, [[a.x + a.w / 2, sy], [a.x + a.w / 2, my], [b.x + b.w / 2, my], [b.x + b.w / 2, ty]].map(([x, y]) => [Math.round(x), Math.round(y)])]);
      }
    }
    if (msgEls.length && !collab) fail("internal: message flow without a collaboration");
    // Diagram interchange.
    const di = [];
    for (const [id, x, y, w, h] of poolShapes) {
      di.push(el("bpmndi:BPMNShape", { id: fresh(id + "_di"), bpmnElement: id, isHorizontal: "true" }, [el("dc:Bounds", { x: String(x), y: String(y), width: String(w), height: String(h) })]));
    }
    for (const [n, a] of shapes) {
      const attrs = { id: fresh(n.id + "_di"), bpmnElement: n.id };
      if (n.type === "subprocess") attrs.isExpanded = n.kids ? "true" : "false";
      di.push(el("bpmndi:BPMNShape", attrs, [el("dc:Bounds", { x: String(a.x), y: String(a.y), width: String(a.w), height: String(a.h) })]));
    }
    for (const [id, pts] of edges) {
      di.push(el("bpmndi:BPMNEdge", { id: fresh(id + "_di"), bpmnElement: id }, pts.map(([x, y]) => el("di:waypoint", { x: String(x), y: String(y) }))));
    }
    const rootRef = collab ? collabId : procs[0].id;
    const plane = el("bpmndi:BPMNPlane", { id: fresh("BPMNPlane_1"), bpmnElement: rootRef }, di);
    const kids = [];
    if (collab) kids.push(el(B("collaboration"), { id: collabId }, [...partEls, ...msgEls]));
    kids.push(...procEls, el("bpmndi:BPMNDiagram", { id: fresh("BPMNDiagram_1") }, [plane]));
    const root = el(B("definitions"), {
      "xmlns:bpmn": "http://www.omg.org/spec/BPMN/20100524/MODEL",
      "xmlns:bpmndi": "http://www.omg.org/spec/BPMN/20100524/DI",
      "xmlns:dc": "http://www.omg.org/spec/DD/20100524/DC",
      "xmlns:di": "http://www.omg.org/spec/DD/20100524/DI",
      "xmlns:xsi": "http://www.w3.org/2001/XMLSchema-instance",
      id: fresh("Definitions_1"),
      targetNamespace: "http://bpmn.io/schema/bpmn",
    }, kids);
    const xml = g.xml_build(root, { indent: o.indent === undefined ? 2 : o.indent, declaration: true });
    const bad = g.bpmn_validate(xml).filter((f) => f.level === "error");
    if (bad.length) {
      throw new Error("bpmn_from_outline: the outline gives an invalid diagram (" + bad.slice(0, 3).map((f) => f.rule + " " + f.id).join("; ") + ")");
    }
    return xml;
  });
})(globalThis);
