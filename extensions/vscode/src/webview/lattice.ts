// The lattice view's script: the lattice's hexes, grouped by where they are.

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
  const stat = (label: string, value: string) => stats.append(el("div", { className: "stat" }, el("div", { className: "stat-value" }, value), el("div", { className: "stat-label" }, label)))
  stat("Hexes", String(l.hexes.length))
  stat("Regions", String(l.regions.length))
  stat("Overseer", overseer ? overseer.name : "None")
  stat("Answering", l.answeredBy)
  stat("Version", l.version)
  app.append(stats)

  // One column per region, the Overseer's first; hexes without a region last.
  const groups = new Map<string, HexView[]>()
  for (const h of l.hexes) {
    const key = h.region || ""
    groups.set(key, [...(groups.get(key) ?? []), h])
  }
  const order = [...groups.keys()].sort((a, b) => {
    const lead = (k: string) => (groups.get(k)!.some((h) => h.role === "Overseer") ? 0 : k ? 1 : 2)
    return lead(a) - lead(b) || a.localeCompare(b)
  })
  const regions = el("div", { className: "regions" })
  for (const code of order) {
    const hexes = groups.get(code)!.sort((a, b) => rank(a.role) - rank(b.role) || a.name.localeCompare(b.name))
    const sample = hexes[0]
    const where = code ? (sample.city ? `${sample.city}` : code) : "No region"
    const column = el(
      "section",
      { className: "region" },
      el("div", { className: "region-head" }, el("span", { className: "pin" }, "◆"), el("span", { className: "region-city" }, where), code ? el("span", { className: "region-code" }, code) : null),
      sample.country ? el("div", { className: "region-country" }, sample.country) : null,
    )
    for (const h of hexes) column.append(card(h))
    regions.append(column)
  }
  app.append(regions)
  app.append(el("div", { className: "foot" }, `Updated ${new Date(l.fetchedAt).toLocaleTimeString()}. Roles: the Overseer takes writes; Harvesters and Replicants serve reads and keep copies (Replicants never lead).`))
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
  return el(
    "div",
    { className: `hex role-${h.role.toLowerCase()}${up ? "" : " down"}` },
    el(
      "div",
      { className: "hex-head" },
      el("span", { className: "hex-shape", ariaHidden: "true" }, "⬢"),
      el("span", { className: "hex-name" }, h.name),
      h.isSelf ? el("span", { className: "badge", title: "This hex answered the connection" }, "answering") : null,
    ),
    el("div", { className: "hex-role" }, h.role, up ? "" : ` · ${h.status}`),
    el("dl", {}, ...rows.flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)])),
  )
}

function rank(role: string): number {
  return role === "Overseer" ? 0 : role === "Harvester" ? 1 : 2
}

vscode.postMessage({ type: "ready" })
