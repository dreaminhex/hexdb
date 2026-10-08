// AI keywords: Claude reads each changed document's text and returns search
// keywords (including synonyms and named entities); they are patched into the
// document. A hash of the text is stored next to them, so a document whose
// text hasn't changed (including this plugin's own update) is skipped.
import { createHash } from "node:crypto"

import { api, log, main, payloads, warn } from "../../sdk/hexdb-plugin.mjs"

const env = process.env
const fields = (env.TEXT_FIELDS || "title,body").split(",").map((f) => f.trim()).filter(Boolean)
const keywordsField = env.KEYWORDS_FIELD || "keywords"
const hashField = `${keywordsField}_hash`
const model = env.MODEL || "claude-haiku-4-5"
const maxKeywords = Number(env.MAX_KEYWORDS || 12)

function textOf(doc) {
  return fields
    .map((f) => f.split(".").reduce((v, k) => (v == null ? undefined : v[k]), doc))
    .filter((v) => typeof v === "string" && v.trim())
    .join("\n\n")
    .slice(0, 20_000)
}

async function keywords(text) {
  const response = await fetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: { "x-api-key": env.ANTHROPIC_API_KEY, "anthropic-version": "2023-06-01", "content-type": "application/json" },
    body: JSON.stringify({
      model,
      max_tokens: 400,
      system:
        "You index documents for keyword search. Reply with only a JSON array of lowercase strings: " +
        `at most ${maxKeywords} search terms for the text, including key topics, named entities, and common synonyms ` +
        "a person might search for that the text doesn't literally use. No explanations.",
      messages: [{ role: "user", content: text }],
    }),
  })
  if (!response.ok) throw new Error(`Claude API ${response.status}: ${await response.text()}`)
  const reply = (await response.json()).content?.find((c) => c.type === "text")?.text ?? "[]"
  const parsed = JSON.parse(reply.slice(reply.indexOf("["), reply.lastIndexOf("]") + 1))
  return [...new Set(parsed.filter((k) => typeof k === "string").map((k) => k.toLowerCase().trim()).filter(Boolean))].slice(0, maxKeywords)
}

await main(async () => {
  if (!env.ANTHROPIC_API_KEY) throw new Error("Set ANTHROPIC_API_KEY in the server's environment (passed through by pass_env).")
  const hexdb = api()
  log(`adding ${keywordsField} from ${fields.join(", ")} with ${model}`)

  for await (const change of payloads()) {
    if (change.op !== "put" || !change.document) continue
    const text = textOf(change.document)
    if (!text) continue
    const hash = createHash("sha256").update(text).digest("hex").slice(0, 16)
    if (change.document[hashField] === hash) continue
    try {
      const found = await keywords(text)
      await hexdb.patch(`/${encodeURIComponent(change.tessellation)}/${encodeURIComponent(change.id)}`, { [keywordsField]: found, [hashField]: hash })
    } catch (e) {
      // A deleted document, or a failed call: log and go on with the next change.
      warn(`${change.tessellation}/${change.id}: ${e.message}`)
    }
  }
})
