// The results panel: one webview beside the editor, with a tab per run (SQL, GraphQL, functions, browsing).

import * as vscode from "vscode"

import { errorMessage } from "./connections"
import { toCsv } from "./lib"
import type { FromResults, ResultTab, ToResults } from "./shared/messages"

export interface RunHandlers {
  /** Fetch the next page; returns the tab with the rows appended. */
  loadMore?: (tab: ResultTab) => Promise<ResultTab>
  /** Open a document from a row's id. */
  openDocument?: (docId: string) => void
}

export class ResultsPanel implements vscode.Disposable {
  private panel: vscode.WebviewPanel | undefined
  private ready = false
  private readonly queue: ToResults[] = []
  private readonly tabs = new Map<string, { tab: ResultTab; handlers: RunHandlers }>()
  private counter = 0

  constructor(private readonly context: vscode.ExtensionContext) {}

  dispose(): void {
    this.panel?.dispose()
  }

  newId(): string {
    return `r${++this.counter}`
  }

  /** The tab shown or updated last (for the integration tests). */
  latest: ResultTab | undefined

  /** Show a run as a new tab (or replace the tab with the same id, e.g. once a run finishes). */
  show(tab: ResultTab, handlers: RunHandlers = {}): void {
    this.latest = tab
    const existed = this.tabs.has(tab.id)
    this.tabs.set(tab.id, { tab, handlers })
    this.post({ type: existed ? "update" : "show", tab })
  }

  private post(message: ToResults): void {
    const panel = this.ensurePanel()
    if (this.ready) void panel.webview.postMessage(message)
    else this.queue.push(message)
  }

  private ensurePanel(): vscode.WebviewPanel {
    if (this.panel) {
      if (!this.panel.visible) this.panel.reveal(vscode.ViewColumn.Beside, true)
      return this.panel
    }
    const panel = vscode.window.createWebviewPanel("hexdb.results", "HexDB Results", { viewColumn: vscode.ViewColumn.Beside, preserveFocus: true }, {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, "dist", "webview"), vscode.Uri.joinPath(this.context.extensionUri, "media")],
    })
    panel.iconPath = vscode.Uri.joinPath(this.context.extensionUri, "media", "hexdb.svg")
    panel.webview.html = webviewHtml(panel.webview, this.context.extensionUri, "results", "HexDB Results")
    panel.webview.onDidReceiveMessage((m: FromResults) => this.receive(m))
    panel.onDidDispose(() => {
      this.panel = undefined
      this.ready = false
      this.tabs.clear()
      this.queue.length = 0
    })
    this.panel = panel
    return panel
  }

  private async receive(message: FromResults): Promise<void> {
    switch (message.type) {
      case "ready":
        this.ready = true
        // A reloaded webview gets every tab back.
        if (this.queue.length === 0) for (const { tab } of this.tabs.values()) this.queue.push({ type: "show", tab })
        for (const m of this.queue.splice(0)) void this.panel?.webview.postMessage(m)
        return
      case "close":
        this.tabs.delete(message.id)
        return
      case "loadMore": {
        const entry = this.tabs.get(message.id)
        if (!entry?.handlers.loadMore) return
        try {
          this.show(await entry.handlers.loadMore(entry.tab), entry.handlers)
        } catch (e) {
          vscode.window.showErrorMessage(`Couldn't load more rows: ${errorMessage(e)}`)
          this.show({ ...entry.tab }, entry.handlers)
        }
        return
      }
      case "openDocument":
        this.tabs.get(message.id)?.handlers.openDocument?.(message.docId)
        return
      case "export": {
        const entry = this.tabs.get(message.id)
        if (!entry) return
        await exportTab(entry.tab, message.format)
        return
      }
    }
  }
}

async function exportTab(tab: ResultTab, format: "csv" | "json"): Promise<void> {
  const target = await vscode.window.showSaveDialog({
    title: `Export ${tab.title}`,
    filters: format === "csv" ? { CSV: ["csv"] } : { JSON: ["json"] },
    defaultUri: vscode.Uri.file(`${tab.title.replace(/[^\w.-]+/g, "_")}.${format}`),
  })
  if (!target) return
  let text: string
  if (format === "csv" && tab.columns && tab.rows) {
    text = toCsv(tab.columns, tab.rows)
  } else if (tab.columns && tab.rows) {
    const names = tab.columns.map((c) => c.name)
    text = JSON.stringify(tab.rows.map((r) => Object.fromEntries(names.map((n, i) => [n, r[i]]))), null, 2)
  } else {
    text = JSON.stringify(tab.json ?? null, null, 2)
  }
  await vscode.workspace.fs.writeFile(target, new TextEncoder().encode(text))
  vscode.window.showInformationMessage(`Saved ${vscode.workspace.asRelativePath(target)}`)
}

/** The HTML shell for a webview: the bundled script and the shared stylesheet, under a strict CSP. */
export function webviewHtml(webview: vscode.Webview, extensionUri: vscode.Uri, script: string, title: string): string {
  const nonce = Array.from({ length: 24 }, () => "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"[Math.floor(Math.random() * 62)]).join("")
  const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, "dist", "webview", `${script}.js`))
  const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(extensionUri, "media", "webview.css"))
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${webview.cspSource}; img-src ${webview.cspSource} data:; script-src 'nonce-${nonce}';">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link rel="stylesheet" href="${styleUri}">
<title>${title}</title>
</head>
<body>
<div id="app"></div>
<script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`
}
