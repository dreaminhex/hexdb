// The lattice view's script: a card per hex (Overseer first), each saying where it runs.

import type { FromLattice, HexView, LatticeView, ToLattice } from "../shared/messages"

declare function acquireVsCodeApi(): { postMessage(message: FromLattice): void }

const vscode = acquireVsCodeApi()
const app = document.getElementById("app") as HTMLElement

window.addEventListener("message", (event: MessageEvent<ToLattice>) => render(event.data.lattice))

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> & { className?: string } = {}, ...children: (Node | string | null | undefined)[]): HTMLElementTagNameMap[K] {
  const node = Object.assign(document.createElement(tag), props)
  for (const child of children) if (child !== null && child !== undefined) node.append(child)
  return node
}

function render(l: LatticeView): void {
  app.replaceChildren()
  const refresh = el("button", { title: "Refresh now" }, "Refresh")
  refresh.onclick = () => vscode.postMessage({ type: "refresh" })
  const overseer = l.hexes.find((h) => h.role === "Overseer")
  app.append(
    el(
      "header",
      { className: "lattice-head" },
      el("div", {}, el("h1", {}, l.name || "Lattice"), el("div", { className: "sub" }, `${l.connection} · ${l.url}`)),
      refresh,
    ),
  )
  if (l.error) app.append(el("div", { className: "error" }, l.error))

  const stats = el("div", { className: "stats" })
  const stat = (label: string, value: string, title = "") =>
    stats.append(el("div", { className: "stat", title }, el("div", { className: "stat-value" }, value), el("div", { className: "stat-label" }, label)))
  stat("Hexes", String(l.hexes.length))
  stat("Regions", String(l.regions.length), l.regions.map((r) => (r.city ? `${r.city} (${r.code})` : r.code)).join(", "))
  stat("Overseer", overseer ? overseer.name : "None")
  stat("Connected to", l.answeredBy, "The hex that answered this connection. With an address in front of several hexes (such as a Fly.io app), it's usually the nearest one.")
  stat("Version", l.version)
  app.append(stats)

  const grid = el("div", { className: "hexes" })
  for (const h of [...l.hexes].sort((a, b) => rank(a.role) - rank(b.role) || a.name.localeCompare(b.name))) grid.append(card(h))
  app.append(grid)
  app.append(el("div", { className: "foot" }, `Updated ${new Date(l.fetchedAt).toLocaleTimeString()}. The Overseer takes writes; Harvesters and Replicants serve reads and keep copies, and Replicants never lead.`))
}

function card(h: HexView): HTMLElement {
  const up = h.status === "active"
  const rows: [string, string][] = []
  if (h.replicationState) rows.push(["Replication", h.replicationState])
  if (h.lag !== null && h.lag !== undefined) rows.push(["Lag", `${h.lag} ${h.lag === 1 ? "change" : "changes"}`])
  if (h.slicing && h.slicesHeld !== null && h.slicesHeld !== undefined) rows.push(["Slices", `${h.slicesHeld} of 64`])
  if (h.version) rows.push(["Version", h.version])
  if (h.preference && h.preference !== "auto") rows.push(["Prefers", h.preference])
  if (!up && h.lastSeen) rows.push(["Last seen", new Date(h.lastSeen).toLocaleString()])
  const where = h.region ? (h.city ? `${h.city}${h.country ? `, ${h.country}` : ""}` : h.region) : "No region set"
  return el(
    "div",
    { className: `hex role-${h.role.toLowerCase()}${up ? "" : " down"}` },
    el(
      "div",
      { className: "hex-head" },
      el("span", { className: "hex-shape", ariaHidden: "true" }, "⬢"),
      el("span", { className: "hex-name" }, h.name),
      h.isSelf ? el("span", { className: "badge", title: "This hex answered this connection" }, "connected") : null,
    ),
    el("div", { className: "hex-role" }, h.role, up ? "" : ` · ${h.status}`),
    el("div", { className: "hex-where" }, el("span", { className: "pin", ariaHidden: "true" }, "◆"), el("span", {}, where), h.region && h.city ? el("span", { className: "region-code" }, h.region) : null),
    el("dl", {}, ...rows.flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)])),
  )
}

function rank(role: string): number {
  return role === "Overseer" ? 0 : role === "Harvester" ? 1 : 2
}

vscode.postMessage({ type: "ready" })
