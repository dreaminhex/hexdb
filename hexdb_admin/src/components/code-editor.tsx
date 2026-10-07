import { useEffect, useRef } from "react"
import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from "@codemirror/autocomplete"
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands"
import { bracketMatching, HighlightStyle, indentOnInput, syntaxHighlighting } from "@codemirror/language"
import { lintGutter, lintKeymap } from "@codemirror/lint"
import { Compartment, EditorState, type Extension } from "@codemirror/state"
import {
  drawSelection,
  EditorView,
  highlightActiveLine,
  highlightActiveLineGutter,
  keymap,
  lineNumbers,
  placeholder as placeholderExtension,
} from "@codemirror/view"
import { tags } from "@lezer/highlight"

import { cn } from "@/lib/utils"

/** Editor chrome built from the UI's CSS variables, so it follows the theme. */
const hexdbTheme = EditorView.theme({
  "&": {
    height: "100%",
    fontSize: "13px",
    color: "var(--foreground)",
    backgroundColor: "transparent",
  },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace",
    lineHeight: "1.6",
  },
  ".cm-content": { caretColor: "var(--primary)", padding: "8px 0" },
  ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--primary)" },
  ".cm-gutters": {
    backgroundColor: "transparent",
    color: "var(--muted-foreground)",
    border: "none",
  },
  ".cm-activeLine": { backgroundColor: "color-mix(in oklch, var(--accent) 45%, transparent)" },
  ".cm-activeLineGutter": { backgroundColor: "transparent", color: "var(--foreground)" },
  "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "color-mix(in oklch, var(--primary) 28%, transparent) !important",
  },
  ".cm-matchingBracket": {
    backgroundColor: "color-mix(in oklch, var(--primary) 20%, transparent)",
    outline: "1px solid color-mix(in oklch, var(--primary) 50%, transparent)",
  },
  ".cm-placeholder": { color: "var(--muted-foreground)" },
  ".cm-tooltip": {
    backgroundColor: "var(--popover)",
    color: "var(--popover-foreground)",
    border: "1px solid var(--border)",
    borderRadius: "var(--radius)",
    boxShadow: "0 8px 24px rgb(0 0 0 / 0.25)",
    overflow: "hidden",
  },
  ".cm-tooltip-autocomplete > ul": { fontFamily: "inherit", maxHeight: "16rem" },
  ".cm-tooltip-autocomplete > ul > li": { padding: "2px 8px" },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": {
    backgroundColor: "var(--accent)",
    color: "var(--accent-foreground)",
  },
  ".cm-completionDetail": { color: "var(--muted-foreground)", fontStyle: "normal", marginLeft: "0.75em" },
  ".cm-completionInfo": { padding: "6px 8px", maxWidth: "24rem" },
  ".cm-diagnostic": { padding: "4px 8px", borderLeftWidth: "3px" },
  ".cm-diagnostic-error": { borderLeftColor: "var(--destructive)" },
  ".cm-lintRange-error": {
    backgroundImage: "none",
    textDecoration: "underline wavy var(--destructive)",
    textUnderlineOffset: "3px",
  },
  ".cm-lint-marker-error": { content: "none" },
})

/** Syntax colors from the chart palette. */
const hexdbHighlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.definitionKeyword], color: "var(--primary)", fontWeight: "500" },
  { tag: [tags.propertyName, tags.attributeName], color: "var(--chart-2)" },
  { tag: [tags.variableName, tags.special(tags.variableName)], color: "var(--chart-4)" },
  { tag: [tags.typeName, tags.className], color: "var(--chart-3)", fontWeight: "500" },
  { tag: [tags.string, tags.special(tags.string)], color: "var(--chart-4)" },
  { tag: [tags.number, tags.bool, tags.null, tags.atom], color: "var(--chart-1)" },
  { tag: tags.comment, color: "var(--muted-foreground)", fontStyle: "italic" },
  { tag: [tags.punctuation, tags.bracket, tags.separator], color: "var(--muted-foreground)" },
  { tag: tags.invalid, color: "var(--destructive)" },
])

export interface CodeEditorProps {
  value: string
  onChange?: (value: string) => void
  /** Language and other mode-specific extensions; may change at runtime. */
  extensions?: Extension[]
  readOnly?: boolean
  placeholder?: string
  /** Called on Ctrl/Cmd+Enter. */
  onRun?: () => void
  className?: string
  "aria-label"?: string
}

/** A CodeMirror 6 editor styled to match the HexDB UI. */
export function CodeEditor({
  value,
  onChange,
  extensions = [],
  readOnly = false,
  placeholder,
  onRun,
  className,
  ...rest
}: CodeEditorProps) {
  const host = useRef<HTMLDivElement>(null)
  const view = useRef<EditorView | null>(null)
  const language = useRef(new Compartment())
  const callbacks = useRef({ onChange, onRun })
  callbacks.current = { onChange, onRun }

  // Create the editor once.
  useEffect(() => {
    if (!host.current) return
    const runKeymap = keymap.of([
      {
        key: "Mod-Enter",
        run: () => {
          callbacks.current.onRun?.()
          return true
        },
      },
    ])
    const state = EditorState.create({
      doc: value,
      extensions: [
        runKeymap,
        lineNumbers(),
        highlightActiveLineGutter(),
        highlightActiveLine(),
        history(),
        drawSelection(),
        indentOnInput(),
        bracketMatching(),
        closeBrackets(),
        autocompletion({ icons: false }),
        lintGutter(),
        keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...historyKeymap, ...completionKeymap, ...lintKeymap, indentWithTab]),
        syntaxHighlighting(hexdbHighlight),
        hexdbTheme,
        EditorState.readOnly.of(readOnly),
        EditorView.editable.of(!readOnly),
        EditorView.lineWrapping,
        placeholder ? placeholderExtension(placeholder) : [],
        EditorView.contentAttributes.of({ "aria-label": rest["aria-label"] ?? "Code editor" }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) callbacks.current.onChange?.(update.state.doc.toString())
        }),
        language.current.of(extensions),
      ],
    })
    view.current = new EditorView({ state, parent: host.current })
    return () => {
      view.current?.destroy()
      view.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // Swap language extensions (e.g. once the schema loads).
  useEffect(() => {
    view.current?.dispatch({ effects: language.current.reconfigure(extensions) })
  }, [extensions])

  // Apply external value changes (examples, history, results).
  useEffect(() => {
    const v = view.current
    if (v && v.state.doc.toString() !== value) {
      v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } })
    }
  }, [value])

  return <div ref={host} className={cn("h-full min-h-0 overflow-hidden", className)} />
}
