// Lazily evaluated pack segment: text diff. See pack.rs and pack_boot.js.
// One catalogue line per helper (`//@ name | signature | purpose | usage`); keep lines free of `|`.
//@ diff_lines | diff_lines(a, b, opts?) | unified diff of two texts (line based, minimal), '' when equal | a and b are strings; opts {context: 3, a_name: 'a', b_name: 'b'}. Output is `diff -u` text: ---/+++ headers, @@ -start,count +start,count @@ hunks (a count of 1 is printed bare), and '\ No newline at end of file' when only one side lacks the final newline. Lines compare exactly (CRLF differs from LF). Limits: 20000 lines and 8 MiB per side (RangeError), and at most 2000 differing lines (adds plus deletes; RangeError telling you to diff smaller parts). Example: diff_lines('a\nb\n', 'a\nc\n').split('\n')[3] === '-b'
(function (g) {
  const define = (name, value) =>
    Object.defineProperty(g, name, { value, writable: true, configurable: true, enumerable: false });
  const DIFF_MAX_LINES = 20000;
  const DIFF_MAX_CHARS = 8 * 1024 * 1024;
  const DIFF_MAX_EDITS = 2000;
  const DIFF_MAX_NAME = 200;

  // Split into lines; `eol` is false when the last line has no trailing newline.
  const splitText = (text) => {
    if (text === "") return { lines: [], eol: true };
    const lines = text.split("\n");
    const eol = lines[lines.length - 1] === "";
    if (eol) lines.pop();
    return { lines, eol };
  };

  // Myers O(ND) shortest edit script on integer line ids; returns [type, aIndex, bIndex] runs
  // flattened as an array of ' ' | '-' | '+' per step, or throws when D exceeds the cap.
  const myers = (A, B) => {
    const n = A.length;
    const m = B.length;
    if (n === 0) return Array.from({ length: m }, () => "+");
    if (m === 0) return Array.from({ length: n }, () => "-");
    const maxD = Math.min(n + m, DIFF_MAX_EDITS);
    const off = maxD + 1;
    const V = new Int32Array(2 * maxD + 3);
    const trace = [];
    let found = -1;
    for (let d = 0; d <= maxD && found < 0; d++) {
      for (let k = -d; k <= d; k += 2) {
        let x = k === -d || (k !== d && V[off + k - 1] < V[off + k + 1]) ? V[off + k + 1] : V[off + k - 1] + 1;
        let y = x - k;
        while (x < n && y < m && A[x] === B[y]) {
          x++;
          y++;
        }
        V[off + k] = x;
        if (x >= n && y >= m) {
          found = d;
          break;
        }
      }
      trace.push(V.slice(off - d, off + d + 1));
    }
    if (found < 0) {
      throw new RangeError(
        "diff_lines: more than " + DIFF_MAX_EDITS + " lines differ; diff smaller parts of the inputs"
      );
    }
    // Walk back from (n, m) through the saved V arrays.
    const ops = [];
    let x = n;
    let y = m;
    for (let d = found; d > 0; d--) {
      const prev = trace[d - 1];
      const k = x - y;
      const at = (kk) => prev[kk + d - 1];
      const prevK = k === -d || (k !== d && at(k - 1) < at(k + 1)) ? k + 1 : k - 1;
      const prevX = at(prevK);
      const midX = prevK === k + 1 ? prevX : prevX + 1;
      while (x > midX) {
        ops.push(" ");
        x--;
        y--;
      }
      if (prevK === k + 1) {
        ops.push("+");
        y--;
      } else {
        ops.push("-");
        x--;
      }
    }
    while (x > 0 && y > 0) {
      ops.push(" ");
      x--;
      y--;
    }
    return ops.reverse();
  };

  const range = (start, count) => {
    if (count === 0) return start - 1 + ",0";
    return count === 1 ? String(start) : start + "," + count;
  };

  define("diff_lines", function diff_lines(a, b, opts) {
    if (typeof a !== "string" || typeof b !== "string") throw new TypeError("diff_lines: a and b must be strings");
    if (opts !== undefined && (opts === null || typeof opts !== "object")) {
      throw new TypeError("diff_lines: opts must be an object");
    }
    const o = opts || {};
    const context = o.context === undefined ? 3 : o.context;
    if (!Number.isInteger(context) || context < 0 || context > 1000) {
      throw new RangeError("diff_lines: context must be an integer from 0 to 1000");
    }
    const name = (key, dflt) => {
      const v = o[key] === undefined ? dflt : o[key];
      if (typeof v !== "string" || v.length > DIFF_MAX_NAME || /[\r\n]/.test(v)) {
        throw new TypeError("diff_lines: " + key + " must be a single-line string of at most " + DIFF_MAX_NAME + " characters");
      }
      return v;
    };
    const aName = name("a_name", "a");
    const bName = name("b_name", "b");
    if (a.length > DIFF_MAX_CHARS || b.length > DIFF_MAX_CHARS) {
      throw new RangeError("diff_lines: input exceeds " + DIFF_MAX_CHARS + " characters per side");
    }
    const sa = splitText(a);
    const sb = splitText(b);
    if (sa.lines.length > DIFF_MAX_LINES || sb.lines.length > DIFF_MAX_LINES) {
      throw new RangeError("diff_lines: input exceeds " + DIFF_MAX_LINES + " lines per side");
    }
    // A last line without a newline is a different line from the same text with one.
    const idOf = new Map();
    const ids = (side) =>
      side.lines.map((line, i) => {
        const key = !side.eol && i === side.lines.length - 1 ? "\u0000" + line : line;
        let id = idOf.get(key);
        if (id === undefined) {
          id = idOf.size;
          idOf.set(key, id);
        }
        return id;
      });
    const A = ids(sa);
    const B = ids(sb);
    // Trim the common head and tail, then run Myers on the middle.
    let head = 0;
    while (head < A.length && head < B.length && A[head] === B[head]) head++;
    let tail = 0;
    while (tail < A.length - head && tail < B.length - head && A[A.length - 1 - tail] === B[B.length - 1 - tail]) tail++;
    const middle = myers(A.slice(head, A.length - tail), B.slice(head, B.length - tail));
    const ops = [];
    for (let i = 0; i < head; i++) ops.push(" ");
    for (const op of middle) ops.push(op);
    for (let i = 0; i < tail; i++) ops.push(" ");
    if (!middle.some((op) => op !== " ")) return "";

    // Group into hunks: changes whose gap of unchanged lines is at most 2 * context merge.
    const changes = [];
    for (let i = 0; i < ops.length; i++) if (ops[i] !== " ") changes.push(i);
    const hunks = [];
    let startC = changes[0];
    let endC = changes[0];
    for (let i = 1; i < changes.length; i++) {
      if (changes[i] - endC - 1 > 2 * context) {
        hunks.push([startC, endC]);
        startC = changes[i];
      }
      endC = changes[i];
    }
    hunks.push([startC, endC]);
    // a/b line number reached before each op index
    const aAt = new Int32Array(ops.length + 1);
    const bAt = new Int32Array(ops.length + 1);
    for (let i = 0; i < ops.length; i++) {
      aAt[i + 1] = aAt[i] + (ops[i] === "+" ? 0 : 1);
      bAt[i + 1] = bAt[i] + (ops[i] === "-" ? 0 : 1);
    }
    const out = ["--- " + aName, "+++ " + bName];
    const marker = "\\ No newline at end of file";
    for (const [first, last] of hunks) {
      const from = Math.max(0, first - context);
      const to = Math.min(ops.length, last + 1 + context);
      const aCount = aAt[to] - aAt[from];
      const bCount = bAt[to] - bAt[from];
      out.push("@@ -" + range(aAt[from] + 1, aCount) + " +" + range(bAt[from] + 1, bCount) + " @@");
      for (let i = from; i < to; i++) {
        const op = ops[i];
        if (op === "+") {
          out.push("+" + sb.lines[bAt[i]]);
          if (!sb.eol && bAt[i] === sb.lines.length - 1) out.push(marker);
        } else if (op === "-") {
          out.push("-" + sa.lines[aAt[i]]);
          if (!sa.eol && aAt[i] === sa.lines.length - 1) out.push(marker);
        } else {
          out.push(" " + sa.lines[aAt[i]]);
          if (!sa.eol && aAt[i] === sa.lines.length - 1) out.push(marker);
        }
      }
    }
    return out.join("\n") + "\n";
  });
})(globalThis);
