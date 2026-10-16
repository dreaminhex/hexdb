import { Fragment, useEffect, useRef, useState, type ReactNode } from "react"
import { IconLoader2, IconMessageChatbot, IconSend, IconTool, IconTrash } from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { api, errorMessage, type ChatAnswer, type ChatStatus } from "@/lib/api"
import { formatNumber } from "@/lib/format"
import { cn } from "@/lib/utils"

interface Message {
  role: "user" | "assistant"
  content: string
  steps?: ChatAnswer["steps"]
  failed?: boolean
}

const EXAMPLES = ["What's in this database?", "Which five orders have the highest total?", "How many customers are there per country?"]

/** Inline Markdown: **bold** and `code`. Built from React elements, never HTML. */
function inline(text: string): ReactNode[] {
  return text.split(/(\*\*[^*]+\*\*|`[^`]+`)/g).map((part, i) => {
    if (part.startsWith("**") && part.endsWith("**") && part.length > 4) return <strong key={i}>{part.slice(2, -2)}</strong>
    if (part.startsWith("`") && part.endsWith("`") && part.length > 2) return <code key={i} className="bg-muted rounded px-1 font-mono text-[0.9em]">{part.slice(1, -1)}</code>
    return <Fragment key={i}>{part}</Fragment>
  })
}

/** The few Markdown blocks an answer uses: paragraphs, lists and pipe tables. */
function Answer({ text }: { text: string }) {
  const blocks: ReactNode[] = []
  const lines = text.replace(/\r/g, "").split("\n")
  let i = 0
  while (i < lines.length) {
    const line = lines[i]
    if (!line.trim()) {
      i++
      continue
    }
    if (line.trim().startsWith("|")) {
      const rows: string[][] = []
      while (i < lines.length && lines[i].trim().startsWith("|")) {
        const cells = lines[i].trim().replace(/^\||\|$/g, "").split("|").map((c) => c.trim())
        if (!cells.every((c) => /^:?-+:?$/.test(c))) rows.push(cells)
        i++
      }
      const [head, ...body] = rows
      blocks.push(
        <div key={blocks.length} className="overflow-x-auto">
          <table className="my-1 text-sm">
            <thead>
              <tr>{head?.map((c, j) => <th key={j} className="border-b px-2 py-1 text-left font-medium">{inline(c)}</th>)}</tr>
            </thead>
            <tbody>
              {body.map((r, k) => (
                <tr key={k}>{r.map((c, j) => <td key={j} className="border-b px-2 py-1 align-top">{inline(c)}</td>)}</tr>
              ))}
            </tbody>
          </table>
        </div>,
      )
      continue
    }
    const bullet = /^\s*([-*]|\d+\.)\s+/
    if (bullet.test(line)) {
      const ordered = /^\s*\d+\./.test(line)
      const items: string[] = []
      while (i < lines.length && bullet.test(lines[i])) {
        items.push(lines[i].replace(bullet, ""))
        i++
      }
      const List = ordered ? "ol" : "ul"
      blocks.push(
        <List key={blocks.length} className={cn("my-1 ml-5 space-y-0.5", ordered ? "list-decimal" : "list-disc")}>
          {items.map((t, k) => <li key={k}>{inline(t)}</li>)}
        </List>,
      )
      continue
    }
    const para: string[] = []
    while (i < lines.length && lines[i].trim() && !bullet.test(lines[i]) && !lines[i].trim().startsWith("|")) {
      para.push(lines[i].replace(/^#+\s+/, ""))
      i++
    }
    blocks.push(<p key={blocks.length} className="my-1">{inline(para.join(" "))}</p>)
  }
  return <div className="text-sm leading-relaxed">{blocks}</div>
}

/** Ask the AI about the data you can read. It looks things up with your permissions and nothing else. */
export function AskPage() {
  const [status, setStatus] = useState<ChatStatus | null>(null)
  const [statusError, setStatusError] = useState<string | null>(null)
  const [messages, setMessages] = useState<Message[]>([])
  const [question, setQuestion] = useState("")
  const [busy, setBusy] = useState(false)
  const end = useRef<HTMLDivElement>(null)

  useEffect(() => {
    api.chatStatus().then(setStatus, (e) => setStatusError(errorMessage(e)))
  }, [])
  useEffect(() => {
    // Braces: newer browsers return a Promise from scrollIntoView, which React would take for a cleanup function.
    end.current?.scrollIntoView({ behavior: "smooth", block: "end" })
  }, [messages, busy])

  const send = async (text: string) => {
    const q = text.trim()
    if (!q || busy) return
    const history = messages.filter((m) => !m.failed).map((m) => ({ role: m.role, content: m.content }))
    setMessages((m) => [...m, { role: "user", content: q }])
    setQuestion("")
    setBusy(true)
    try {
      const answer = await api.chat(q, history)
      setMessages((m) => [...m, { role: "assistant", content: answer.answer || "(no answer)", steps: answer.steps }])
      setStatus((s) => (s ? { ...s, left: answer.left } : s))
    } catch (e) {
      setMessages((m) => [...m, { role: "assistant", content: errorMessage(e), failed: true }])
    } finally {
      setBusy(false)
    }
  }

  const left = status?.left
  const remaining = [left?.user_today, left?.client_today].filter((n): n is number => typeof n === "number")
  return (
    <div className="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground text-sm">
        Ask about the data in this lattice. The AI looks things up with your permissions, so it sees only what you can, and it answers only
        questions about this data.
      </p>
      {statusError && <p className="text-destructive text-sm">{statusError}</p>}
      {status && !status.enabled && (
        <Card>
          <CardContent className="text-muted-foreground text-sm">
            The AI chat isn't available: an administrator turns it on (<code>ai.chat.enabled</code>) and sets up an AI provider on the
            Configuration page.
          </CardContent>
        </Card>
      )}

      <div className="flex flex-1 flex-col gap-3">
        {messages.length === 0 && status?.enabled && (
          <div className="text-muted-foreground flex flex-col items-center gap-3 py-10 text-sm">
            <IconMessageChatbot className="size-8" />
            <div className="flex flex-wrap justify-center gap-2">
              {EXAMPLES.map((e) => (
                <Button key={e} variant="outline" size="sm" onClick={() => void send(e)}>
                  {e}
                </Button>
              ))}
            </div>
          </div>
        )}
        {messages.map((m, i) => (
          <div key={i} className={cn("flex", m.role === "user" ? "justify-end" : "justify-start")}>
            <div
              className={cn(
                "max-w-[85%] rounded-lg px-3 py-2",
                m.role === "user" ? "bg-primary text-primary-foreground text-sm" : "bg-muted",
                m.failed && "border-destructive/40 bg-destructive/10 text-destructive border text-sm",
              )}
            >
              {m.role === "user" || m.failed ? <span className="whitespace-pre-wrap">{m.content}</span> : <Answer text={m.content} />}
              {m.steps && m.steps.length > 0 && (
                <details className="text-muted-foreground mt-2 text-xs">
                  <summary className="cursor-pointer">
                    <IconTool className="mr-1 inline size-3" />
                    {m.steps.length} lookup{m.steps.length === 1 ? "" : "s"}
                  </summary>
                  <ul className="mt-1 space-y-1">
                    {m.steps.map((s, k) => (
                      <li key={k} className="font-mono break-all">
                        {s.ok ? "" : "✗ "}
                        {s.tool} {JSON.stringify(s.input)}
                      </li>
                    ))}
                  </ul>
                </details>
              )}
            </div>
          </div>
        ))}
        {busy && (
          <div className="text-muted-foreground flex items-center gap-2 text-sm">
            <IconLoader2 className="size-4 animate-spin" /> Looking it up…
          </div>
        )}
        <div ref={end} />
      </div>

      <form
        className="bg-background sticky bottom-0 flex flex-col gap-2 pb-2"
        onSubmit={(e) => {
          e.preventDefault()
          void send(question)
        }}
      >
        <div className="flex gap-2">
          <textarea
            className="border-input bg-background focus-visible:ring-ring min-h-10 flex-1 resize-none rounded-md border px-3 py-2 text-sm focus-visible:ring-2 focus-visible:outline-none"
            rows={2}
            maxLength={status?.max_question_chars ?? 2000}
            placeholder={status?.enabled ? "Ask about the data…" : "The AI chat is off"}
            disabled={!status?.enabled || busy}
            value={question}
            onChange={(e) => setQuestion(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault()
                void send(question)
              }
            }}
            aria-label="Question"
          />
          <div className="flex flex-col gap-2">
            <Button type="submit" disabled={!status?.enabled || busy || !question.trim()} aria-label="Ask">
              <IconSend />
            </Button>
            <Button type="button" variant="ghost" disabled={messages.length === 0 || busy} onClick={() => setMessages([])} aria-label="Clear the conversation" title="Clear the conversation">
              <IconTrash />
            </Button>
          </div>
        </div>
        {remaining.length > 0 && (
          <div className="text-muted-foreground text-xs">
            <Badge variant="outline" className="font-normal">
              {formatNumber(Math.min(...remaining))} tokens left today
            </Badge>
          </div>
        )}
      </form>
    </div>
  )
}
