// Figure toolkit for the how-it-works page: DOM/SVG element builders, glyph
// geometry helpers that consume the inspector's output directly, a slider
// factory, captions, and the collapsible "gory details" block. Everything a
// section draws should come through here so the figures read as one system.
//
// Conventions: inspector coordinates are em units, y-up, baseline origin. SVG
// is y-down, so `emSpace` hands back a <g> whose transform does the flip —
// draw in raw em coordinates inside it and set `vector-effect:
// non-scaling-stroke` (svgEl does this for you) so stroke widths stay in
// screen pixels.

/// Tokyo Night, the only colors a figure should use. Backgrounds stay
/// transparent — the page supplies them.
export const PALETTE = {
  bg: "#1a1b26",
  bgDark: "#15161e",
  border: "#2a2c3a",
  text: "#c0caf5",
  textDim: "#a9b1d6",
  muted: "#565f89",
  blue: "#7aa2f7",
  green: "#9ece6a",
  red: "#f7768e",
  orange: "#e0af68",
  purple: "#bb9af7",
  teal: "#7dcfff",
};

const SVG_NS = "http://www.w3.org/2000/svg";

/// An HTML element: el("div", { class: "x" }, child, "text", ...).
export function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v);
  }
  node.append(...children);
  return node;
}

/// An SVG element in the SVG namespace. Stroked shapes get
/// `vector-effect: non-scaling-stroke` by default so strokes are screen-px
/// even inside the em-space flip; pass vectorEffect: null to opt out.
export function svgEl(tag, attrs = {}, ...children) {
  const node = document.createElementNS(SVG_NS, tag);
  if (!("vector-effect" in attrs) && tag !== "svg" && tag !== "g") {
    node.setAttribute("vector-effect", "non-scaling-stroke");
  }
  for (const [k, v] of Object.entries(attrs)) {
    if (v != null) node.setAttribute(k, v);
  }
  node.append(...children);
  return node;
}

/// A responsive <svg> sized in CSS px with a matching viewBox.
export function svg(width, height, attrs = {}) {
  return svgEl("svg", {
    viewBox: `0 0 ${width} ${height}`,
    width,
    height,
    style: "max-width: 100%; height: auto;",
    ...attrs,
  });
}

/// An em-space group over an inspection bbox: everything drawn inside it uses
/// raw em coordinates, y-up, and lands in a `width × height` px box with
/// `pad` px of margin. Returns { g, width, height, scale, toPx } — append `g`
/// to an svg(width, height), and use toPx([ex, ey]) when something (a text
/// label) must be placed in pixel space instead.
export function emSpace(bbox, { width = 420, pad = 24 } = {}) {
  const [minX, minY, maxX, maxY] = bbox;
  const spanX = Math.max(maxX - minX, 1e-6);
  const spanY = Math.max(maxY - minY, 1e-6);
  const scale = (width - 2 * pad) / spanX;
  const height = Math.ceil(spanY * scale + 2 * pad);
  const tx = pad - minX * scale;
  const ty = pad + maxY * scale;
  const g = svgEl("g", { transform: `translate(${tx} ${ty}) scale(${scale} ${-scale})` });
  return {
    g,
    width,
    height,
    scale,
    toPx: ([x, y]) => [tx + x * scale, ty - y * scale],
  };
}

/// SVG path data for a glyph's stored quadratics: one `M … Q … Z` run per
/// contour, in em coordinates. Feed it `inspection.curves` and
/// `inspection.contours` (or a COLR layer's, or `master_b.curves`).
export function glyphPathD(curves, contours) {
  let d = "";
  let i = 0;
  for (const n of contours) {
    if (n === 0) continue;
    const start = curves[i].p0;
    d += `M ${start[0]} ${start[1]} `;
    for (let j = 0; j < n; j++, i++) {
      const { p1, p2 } = curves[i];
      d += `Q ${p1[0]} ${p1[1]} ${p2[0]} ${p2[1]} `;
    }
    d += "Z ";
  }
  return d.trim();
}

/// The octagon corner clipping cuts a glyph's bbox down to, as em-space
/// points: the bbox corners with `clips` legs removed, in drawing order.
/// Corners with a zero leg stay square (the octagon degenerates there).
export function clipOctagon(bbox, clips) {
  const [minX, minY, maxX, maxY] = bbox;
  // clips[] is in the unit quad's corner order (0,0) (1,0) (1,1) (0,1) =
  // em (minX,maxY) (maxX,maxY) (maxX,minY) (minX,minY).
  const [tl, tr, br, bl] = clips;
  const pts = [];
  const corner = (x, y, leg, dxa, dya, dxb, dyb) => {
    if (leg > 0) pts.push([x + dxa * leg, y + dya * leg], [x + dxb * leg, y + dyb * leg]);
    else pts.push([x, y]);
  };
  corner(minX, maxY, tl, 0, -1, 1, 0); // top-left, walking clockwise in em
  corner(maxX, maxY, tr, -1, 0, 0, -1); // top-right
  corner(maxX, minY, br, 0, 1, -1, 0); // bottom-right
  corner(minX, minY, bl, 1, 0, 0, 1); // bottom-left
  return pts;
}

/// A labeled range input: slider({ label, min, max, step, value, format,
/// oninput }) → { root, input, set }. `oninput(number)` fires on drag;
/// `format(number)` renders the readout (defaults to the raw value). `set(v)`
/// moves the slider programmatically (and fires oninput).
export function slider({
  label,
  min = 0,
  max = 1,
  step = 0.01,
  value = min,
  format = (v) => `${v}`,
  oninput = () => {},
} = {}) {
  const input = el("input", { type: "range", min, max, step, value });
  const readout = el("output", {}, format(Number(value)));
  const fire = (v) => {
    readout.textContent = format(v);
    oninput(v);
  };
  input.addEventListener("input", () => fire(Number(input.value)));
  const root = el("div", { class: "hiw-slider" }, el("label", {}, label), input, readout);
  return {
    root,
    input,
    set: (v) => {
      input.value = `${v}`;
      fire(Number(v));
    },
  };
}

/// A figure caption. Give figures one — they are the page's captions layer,
/// prose stays in the section html.
export function caption(text) {
  return el("p", { class: "hiw-caption" }, text);
}

/// A wrapper for one figure: children (svg, canvas, controls) plus an
/// optional trailing caption string.
export function figure(children, captionText = null) {
  const root = el("div", { class: "hiw-figure" }, ...[].concat(children));
  if (captionText) root.append(caption(captionText));
  return root;
}

/// A `.hiw-controls` row for sliders and toggles under a figure.
export function controls(...children) {
  return el("div", { class: "hiw-controls" }, ...children);
}

/// The collapsible "the gory details" block: details("summary text", node or
/// html-string, ...). Collapsed by default.
export function details(summary, ...children) {
  const body = el("div", { class: "hiw-details-body" });
  for (const child of children) {
    if (typeof child === "string") body.insertAdjacentHTML("beforeend", child);
    else body.append(child);
  }
  return el("details", { class: "hiw-details" }, el("summary", {}, summary), body);
}
