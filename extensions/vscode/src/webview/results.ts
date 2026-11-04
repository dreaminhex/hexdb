// The results panel's script: a tab per run; each shows a table (or JSON) with filtering, paging and export.

import type { FromResults, ResultTab, ToResults } from "../shared/messages"

declare function acquireVsCodeApi(): { postMessage(message: FromResults): void; getState(): unknown; setState(state: unknown): void }

const vscode = acquireVsCodeApi()
const app = document.getElementById("app") as HTMLElement

interface TabState {
  tab: ResultTab
  view: "table" | "json"
  filter: string
  loading: boolean
}

const tabs: TabState[] = []
let active: string | undefined

function send(message: FromResults): void {
  vscode.postMessage(message)
}

window.addEventListener("message", (event: MessageEvent<ToResults>) => {
  const message = event.data
  const existing = tabs.find((t) => t.tab.id === message.tab.id)
  if (existing) {
    existing.tab = message.tab
    existing.loading = false
  } else {
    tabs.push({ tab: message.tab, view: message.tab.columns ? "table" : "json", filter: "", loading: false })
    // Keep the panel tidy: at most 20 runs.
    while (tabs.length > 20) send({ type: "close", id: tabs.shift()!.tab.id })
  }
  if (message.type === "show") active = message.tab.id
  render()
})

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> & { className?: string } = {}, ...children: (Node | string | null | undefined)[]): HTMLElementTagNameMap[K] {
  const node = Object.assign(document.createElement(tag), props)
  for (const child of children) if (child !== null && child !== undefined) node.append(child)
  return node
}

function render(): void {
  app.replaceChildren()
  if (tabs.length === 0) {
    app.append(el("div", { className: "empty" }, "Run a query (Ctrl+Enter in a SQL or GraphQL editor) or browse a tessellation to see results here."))
    return
  }
  const strip = el("div", { className: "tabs", role: "tablist" })
  for (const t of tabs) {
    const tab = el(
      "div",
      { className: `tab${t.tab.id === active ? " active" : ""}${t.tab.error ? " failed" : ""}`, role: "tab", title: t.tab.source },
      el("span", { className: "tab-title" }, t.tab.title),
    )
    tab.onclick = () => {
      active = t.tab.id
      render()
    }
    const close = el("button", { className: "tab-close", title: "Close", ariaLabel: "Close" }, "×")
    close.onclick = (e) => {
      e.stopPropagation()
      const i = tabs.indexOf(t)
      tabs.splice(i, 1)
      send({ type: "close", id: t.tab.id })
      if (active === t.tab.id) active = tabs[Math.min(i, tabs.length - 1)]?.tab.id
      render()
    }
    tab.append(close)
    strip.append(tab)
  }
  app.append(strip)
  const current = tabs.find((t) => t.tab.id === active) ?? tabs[tabs.length - 1]
  app.append(renderTab(current))
}

function renderTab(state: TabState): HTMLElement {
  const { tab } = state
  const body = el("div", { className: "tab-body" })

  const meta = el("div", { className: "meta" })
  meta.append(el("span", { className: "conn" }, tab.connection))
  if (tab.ms !== undefined) meta.append(el("span", {}, `${tab.ms} ms`))
  if (tab.rows) meta.append(el("span", {}, `${tab.rows.length}${tab.hasMore ? "+" : ""} ${tab.rows.length === 1 ? "row" : "rows"}`))
  if (tab.tessellation) meta.append(el("span", {}, tab.tessellation))
  body.append(meta)

  const source = el("details", { className: "source" }, el("summary", {}, firstLine(tab.source)), el("pre", {}, tab.source))
  body.append(source)

  if (tab.error) {
    body.append(el("div", { className: "error" }, tab.error))
    return body
  }
  if (tab.rows === undefined && tab.json === undefined) {
    body.append(el("div", { className: "running" }, "Running…"))
    return body
  }
  for (const note of tab.notes ?? []) body.append(el("div", { className: "note" }, note))

  const toolbar = el("div", { className: "toolbar" })
  if (tab.columns) {
    const seg = el("div", { className: "segmented" })
    for (const view of ["table", "json"] as const) {
      const b = el("button", { className: state.view === view ? "on" : "" }, view === "table" ? "Table" : "JSON")
      b.onclick = () => {
        state.view = view
        render()
      }
      seg.append(b)
    }
    toolbar.append(seg)
  }
  let filterInput: HTMLInputElement | undefined
  if (state.view === "table" && tab.columns) {
    filterInput = el("input", { type: "search", placeholder: "Filter rows", value: state.filter, className: "filter" })
    toolbar.append(filterInput)
  }
  toolbar.append(el("span", { className: "spacer" }))
  const copy = el("button", { title: "Copy as JSON" }, "Copy")
  copy.onclick = () => {
    void navigator.clipboard.writeText(JSON.stringify(asJson(tab), null, 2))
    copy.textContent = "Copied"
    setTimeout(() => (copy.textContent = "Copy"), 1200)
  }
  toolbar.append(copy)
  if (tab.columns) {
    const csv = el("button", { title: "Save as CSV" }, "CSV")
    csv.onclick = () => send({ type: "export", id: tab.id, format: "csv" })
    toolbar.append(csv)
  }
  const json = el("button", { title: "Save as JSON" }, "Save JSON")
  json.onclick = () => send({ type: "export", id: tab.id, format: "json" })
  toolbar.append(json)
  body.append(toolbar)

  if (state.view === "json" || !tab.columns || !tab.rows) {
    body.append(el("pre", { className: "json" }, JSON.stringify(asJson(tab), null, 2)))
  } else {
    const wrap = el("div", { className: "grid-wrap" })
    const draw = () => wrap.replaceChildren(table(state))
    draw()
    if (filterInput) {
      filterInput.oninput = () => {
        state.filter = filterInput!.value
        draw()
      }
    }
    body.append(wrap)
  }

  if (tab.hasMore) {
    const more = el("button", { className: "more", disabled: state.loading }, state.loading ? "Loading…" : "Load more")
    more.onclick = () => {
      state.loading = true
      render()
      send({ type: "loadMore", id: tab.id })
    }
    body.append(more)
  }
  return body
}

function table(state: TabState): HTMLElement {
  const { tab } = state
  const columns = tab.columns ?? []
  const idColumn = tab.tessellation ? columns.findIndex((c) => c.name === "id") : -1
  const needle = state.filter.trim().toLowerCase()
  const rows = (tab.rows ?? []).filter((r) => !needle || r.some((v) => cellText(v).toLowerCase().includes(needle)))

  const head = el("tr", {}, el("th", { className: "rownum" }, "#"))
  for (const c of columns) head.append(el("th", { title: c.type ?? "" }, c.name, c.type ? el("span", { className: "type" }, c.type) : null))
  const tbody = el("tbody")
  rows.forEach((row, i) => {
    const tr = el("tr", {}, el("td", { className: "rownum" }, String(i + 1)))
    row.forEach((value, c) => {
      const td = el("td", { className: cellClass(value) })
      const text = cellText(value)
      if (c === idColumn && typeof value === "string") {
        const a = el("a", { href: "#", title: "Open this document" }, text)
        a.onclick = (e) => {
          e.preventDefault()
          send({ type: "openDocument", id: tab.id, docId: value })
        }
        td.append(a)
      } else {
        td.textContent = text.length > 300 ? `${text.slice(0, 300)}…` : text
        if (text.length > 60) td.title = text.slice(0, 4000)
      }
      tr.append(td)
    })
    tbody.append(tr)
  })
  const t = el("table", { className: "grid" }, el("thead", {}, head), tbody)
  if (rows.length === 0) return el("div", { className: "empty" }, needle ? "No rows match the filter." : "No rows.")
  return t
}

function cellText(value: unknown): string {
  if (value === null || value === undefined) return "null"
  if (typeof value === "object") return JSON.stringify(value)
  return String(value)
}

function cellClass(value: unknown): string {
  if (value === null || value === undefined) return "null"
  if (typeof value === "number") return "num"
  if (typeof value === "boolean") return "bool"
  if (typeof value === "object") return "obj"
  return ""
}

function asJson(tab: ResultTab): unknown {
  if (tab.json !== undefined) return tab.json
  if (tab.columns && tab.rows) {
    const names = tab.columns.map((c) => c.name)
    return tab.rows.map((r) => Object.fromEntries(names.map((n, i) => [n, r[i]])))
  }
  return tab.json ?? null
}

function firstLine(text: string): string {
  const line = text.trim().split("\n")[0] ?? ""
  return line.length > 120 ? `${line.slice(0, 120)}…` : line
}

render()
send({ type: "ready" })
