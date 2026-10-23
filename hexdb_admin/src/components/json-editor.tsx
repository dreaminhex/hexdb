import { useMemo } from "react"
import { type Completion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete"
import { json, jsonLanguage, jsonParseLinter } from "@codemirror/lang-json"
import { linter } from "@codemirror/lint"
import { type Extension } from "@codemirror/state"

import { CodeEditor } from "@/components/code-editor"
import { cn } from "@/lib/utils"

/** Completes the quoted word being typed from `keywords` (key names and string values alike). */
function keywordSource(keywords: Completion[]) {
  return (context: CompletionContext): CompletionResult | null => {
    const word = context.matchBefore(/"[\w$.]*/)
    if (!word || (word.from === word.to && !context.explicit)) return null
    return { from: word.from + 1, options: keywords, validFor: /^[\w$.]*$/ }
  }
}

/** A bordered CodeMirror editor; JSON syntax, with errors marked as you type, unless `plain`. */
export function JsonEditor({
  value,
  onChange,
  label,
  plain = false,
  className,
  readOnly = false,
  keywords,
}: {
  value: string
  onChange?: (value: string) => void
  label: string
  plain?: boolean
  className?: string
  readOnly?: boolean
  /** Words to offer while typing inside quotes (keep the array's identity stable). */
  keywords?: Completion[]
}) {
  const extensions = useMemo<Extension[]>(
    () =>
      plain
        ? []
        : [json(), linter(jsonParseLinter(), { delay: 300 }), ...(keywords ? [jsonLanguage.data.of({ autocomplete: keywordSource(keywords) })] : [])],
    [plain, keywords],
  )
  return (
    <div className={cn("bg-background h-40 overflow-hidden rounded-md border", className)}>
      <CodeEditor value={value} onChange={onChange} extensions={extensions} readOnly={readOnly} aria-label={label} />
    </div>
  )
}

/** Parse JSON text, with a readable error naming what it was for. */
// eslint-disable-next-line react-refresh/only-export-components
export function parseJson<T = unknown>(text: string, what: string): T {
  try {
    return JSON.parse(text) as T
  } catch (e) {
    throw new Error(`${what} isn't valid JSON: ${e instanceof Error ? e.message : String(e)}`, { cause: e })
  }
}
