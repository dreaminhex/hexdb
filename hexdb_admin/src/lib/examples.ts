// Ready-to-run examples for using streams and queues from your own code (see components/code-snippets.tsx).

export type Snippet = { id: string; label: string; note: string; code: string }

/** This hex's address, for examples: where the admin UI was loaded from. */
export function origin(): string {
  return window.location.origin
}

/** How every example signs in: an API key from the Account page. */
export const KEY_NOTE = "Create an API key on the Account page and put it in HEXDB_KEY; it acts with your roles."

const lines = (...l: string[]) => l.join("\n")

/** Ways to read a stream: live (SSE, WebSocket), in order, as a consumer group, or by webhook; and to publish. */
export function streamSnippets(name: string): Snippet[] {
  const base = `${origin()}/streams/${encodeURIComponent(name)}`
  const auth = `-H "Authorization: Bearer $HEXDB_KEY"`
  return [
    {
      id: "sse",
      label: "Live (SSE)",
      note: `Server-Sent Events: the connection stays open and each new message arrives as an event. Add ?group=<name> to start where your group left off, or ?after=<offset>. ${KEY_NOTE}`,
      code: lines(
        `curl -N ${auth} \\`,
        `  "${base}/subscribe"`,
        ``,
        `# In the browser, signed in to this hex:`,
        `const events = new EventSource("${base}/subscribe", { withCredentials: true })`,
        `events.onmessage = (e) => console.log(e.lastEventId, JSON.parse(e.data))`,
      ),
    },
    {
      id: "ws",
      label: "WebSocket",
      note: `The same messages as {"event": "message", "id": <offset>, "data": ...} frames. ${KEY_NOTE}`,
      code: lines(`websocat ${auth} \\`, `  "${base.replace(/^http/, "ws")}/ws"`),
    },
    {
      id: "poll",
      label: "Read in order",
      note: "Read a page of messages, then ask for the ones after the last offset you saw. wait=20 holds the request open until something arrives (long polling).",
      code: lines(
        `curl ${auth} "${base}/messages?limit=100"`,
        `# -> { "messages": [{ "offset", "time", "key", "payload", ... }], "next": "<offset>" }`,
        ``,
        `curl ${auth} "${base}/messages?after=<next>&limit=100&wait=20"`,
      ),
    },
    {
      id: "group",
      label: "Consumer group",
      note: "HexDB remembers a group's position: its consumers read from the last committed offset, so a restarted worker carries on where it stopped.",
      code: lines(
        `curl ${auth} "${base}/messages?group=billing&limit=100"`,
        ``,
        `# Done with them: commit the last offset you handled`,
        `curl -X POST ${auth} -H "Content-Type: application/json" \\`,
        `  -d '{ "offset": "<last offset>" }' \\`,
        `  "${base}/groups/billing/commit"`,
      ),
    },
    {
      id: "webhook",
      label: "Webhook",
      note: "No code to run: a destination makes HexDB POST every message, in batches, to your URL, retrying until it answers 2xx. Add one with Edit (it needs manage on the stream).",
      code: lines(`"destinations": [`, `  { "url": "https://example.com/hooks/${name}", "headers": { "Authorization": "Bearer ..." }, "batch_size": 100 }`, `]`),
    },
    {
      id: "publish",
      label: "Publish",
      note: "Producers publish one message or an array; each gets an offset. A key groups related messages.",
      code: lines(
        `curl -X POST ${auth} -H "Content-Type: application/json" \\`,
        `  -d '{ "payload": { "order": "A-1001", "status": "paid" }, "key": "customer-17" }' \\`,
        `  "${base}/messages"`,
      ),
    },
  ]
}

/** How to work a queue: receive (a lease), then acknowledge or release; and send. */
export function queueSnippets(name: string, visibility: number): Snippet[] {
  const base = `${origin()}/queues/${encodeURIComponent(name)}`
  const auth = `-H "Authorization: Bearer $HEXDB_KEY" -H "Content-Type: application/json"`
  return [
    {
      id: "worker",
      label: "Worker",
      note: `A worker receives a few messages, does the work, and acknowledges each. If it stops first, the message comes back after ${visibility} s for another worker. ${KEY_NOTE}`,
      code: lines(
        `# 1. Receive up to 10, waiting up to 20 s for some to arrive`,
        `curl -X POST ${auth} -d '{ "max": 10, "wait_seconds": 20 }' "${base}/receive"`,
        `# -> { "messages": [{ "id", "receipt", "body", "receives", "sent_at" }] }`,
        ``,
        `# 2. Done: acknowledge with the receipts (the messages are deleted)`,
        `curl -X POST ${auth} -d '{ "receipts": ["<receipt>"] }' "${base}/ack"`,
      ),
    },
    {
      id: "node",
      label: "Node.js",
      note: "The same worker as a loop in JavaScript (Node 18 or later).",
      code: lines(
        `const base = "${base}"`,
        "const headers = { authorization: `Bearer ${process.env.HEXDB_KEY}`, \"content-type\": \"application/json\" }",
        ``,
        `for (;;) {`,
        "  const r = await fetch(`${base}/receive`, { method: \"POST\", headers, body: JSON.stringify({ max: 10, wait_seconds: 20 }) })",
        `  const { messages } = await r.json()`,
        `  for (const m of messages) {`,
        `    console.log("working on", m.body)`,
        "    await fetch(`${base}/ack`, { method: \"POST\", headers, body: JSON.stringify({ receipts: [m.receipt] }) })",
        `  }`,
        `}`,
      ),
    },
    {
      id: "send",
      label: "Send",
      note: "Send up to 100 at once. delay_seconds holds one back; a dedupe_key sent again within 5 minutes is skipped.",
      code: lines(
        `curl -X POST ${auth} \\`,
        `  -d '{ "messages": [{ "body": { "task": "resize", "file": "cover.jpg" } }, { "body": "later", "delay_seconds": 60 }] }' \\`,
        `  "${base}/messages"`,
      ),
    },
    {
      id: "release",
      label: "Release or extend",
      note: "Can't finish? Release a message so another worker gets it (optionally after a delay), or extend your lease if the work runs long.",
      code: lines(
        `curl -X POST ${auth} -d '{ "receipts": ["<receipt>"], "delay_seconds": 30 }' "${base}/nack"`,
        `curl -X POST ${auth} -d '{ "receipts": ["<receipt>"], "visibility_seconds": 120 }' "${base}/extend"`,
      ),
    },
  ]
}
