import { useMemo } from "react"
import { json } from "@codemirror/lang-json"
import { type Extension } from "@codemirror/state"

import { CodeEditor } from "@/components/code-editor"
import { cn } from "@/lib/utils"

/** A bordered CodeMirror editor; JSON syntax unless `plain`. */
export function JsonEditor({
  value,
  onChange,
  label,
  plain = false,
  className,
  readOnly = false,
}: {
  value: string
  onChange?: (value: string) => void
  label: string
  plain?: boolean
  className?: string
  readOnly?: boolean
}) {
  const extensions = useMemo<Extension[]>(() => (plain ? [] : [json()]), [plain])
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
    throw new Error(`${what} isn't valid JSON: ${e instanceof Error ? e.message : String(e)}`)
  }
}
