import { useCallback, useEffect, useMemo, useState } from "react"
import { json } from "@codemirror/lang-json"
import { type Extension } from "@codemirror/state"
import {
  IconAlertTriangle,
  IconBraces,
  IconChevronDown,
  IconExternalLink,
  IconHistory,
  IconLoader2,
  IconPlayerPlayFilled,
  IconSparkles,
  IconTable,
} from "@tabler/icons-react"
import { graphql as graphqlLanguage } from "cm6-graphql"
import { type GraphQLSchema, Kind, parse, print } from "graphql"

import { CodeEditor } from "@/components/code-editor"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader } from "@/components/ui/card"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { executeGraphQL, fetchSchema, GRAPHQL_ENDPOINT, type GraphQLResult } from "@/lib/graphql"
import { QUERY_EXAMPLES } from "@/lib/query-examples"

const DRAFT_KEY = "hexdb.query.draft"
const HISTORY_KEY = "hexdb.query.history"
const HISTORY_LIMIT = 25
const MAX_TABLE_COLUMNS = 20
/** Document metadata shown after the document's own fields. */
const META_COLUMNS = ["tessellation", "expiresAt", "_expires_at"]

interface HistoryEntry {
  query: string
  variables: string
  at: number
}

// Browser storage can be unavailable (private windows, blocked site data).
function load<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key)
    return raw ? (JSON.parse(raw) as T) : fallback
  } catch {
    return fallback
  }
}

function save(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    // Not critical.
  }
}

const isMac = typeof navigator !== "undefined" && /mac/i.test(navigator.platform)
const RUN_SHORTCUT = isMac ? "⌘ Enter" : "Ctrl Enter"

/** The first operation name in a document, if it has several. */
function operationNames(query: string): string[] {
  try {
    return parse(query)
      .definitions.filter((d) => d.kind === Kind.OPERATION_DEFINITION)
      .map((d) => ("name" in d && d.name ? d.name.value : ""))
  } catch {
    return []
  }
}

// ---------------------------------------------------------------------------
// Result table
// ---------------------------------------------------------------------------

interface TableData {
  path: string
  columns: string[]
  rows: Record<string, unknown>[]
}

/** Find the first array of objects in the result, depth-first. */
function findRows(value: unknown, path: string[] = []): { path: string[]; items: Record<string, unknown>[] } | null {
  if (Array.isArray(value)) {
    if (value.length > 0 && value.every((v) => v !== null && typeof v === "object" && !Array.isArray(v))) {
      return { path, items: value as Record<string, unknown>[] }
    }
    return null
  }
  if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      const found = findRows(child, [...path, key])
      if (found) return found
    }
  }
  return null
}

/** Flatten document results so each document's fields become columns. */
function toTable(data: unknown): TableData | null {
  const found = findRows(data)
  if (!found) return null

  const rows = found.items.map((item) => {
    const doc = (item.json ?? item.data) as Record<string, unknown> | undefined
    if (doc && typeof doc === "object" && !Array.isArray(doc)) {
      const rest = Object.fromEntries(Object.entries(item).filter(([key]) => key !== "json" && key !== "data"))
      return { ...rest, ...doc }
    }
    return item
  })

  // id first, then document fields, then metadata; skip columns that are empty in every row.
  const sample = rows.slice(0, 200)
  const columns: string[] = []
  for (const row of sample) {
    for (const key of Object.keys(row)) {
      if (!columns.includes(key)) columns.push(key)
    }
  }
  // Scalars read best, so they come before objects and arrays.
  const isComplex = (key: string) => {
    const value = sample.find((row) => row[key] !== null && row[key] !== undefined)?.[key]
    return typeof value === "object"
  }
  const rank = (key: string) => (key === "id" ? 0 : META_COLUMNS.includes(key) ? 3 : isComplex(key) ? 2 : 1)
  const visible = columns
    .filter((key) => sample.some((row) => row[key] !== null && row[key] !== undefined))
    .sort((a, b) => rank(a) - rank(b))
  return { path: found.path.join("."), columns: visible.slice(0, MAX_TABLE_COLUMNS), rows }
}

function formatCell(value: unknown): string {
  if (value === undefined) return ""
  if (value === null) return "null"
  if (typeof value === "string") return value
  const text = JSON.stringify(value)
  return text.length > 80 ? `${text.slice(0, 77)}…` : text
}

function ResultTable({ table }: { table: TableData }) {
  return (
    <div className="h-full overflow-auto">
      <Table>
        <TableHeader className="bg-muted/60 sticky top-0 z-10">
          <TableRow>
            {table.columns.map((column) => (
              <TableHead key={column} className="font-mono text-xs">
                {column}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {table.rows.map((row, i) => (
            <TableRow key={i}>
              {table.columns.map((column) => {
                const value = row[column]
                return (
                  <TableCell
                    key={column}
                    className={
                      "max-w-72 truncate font-mono text-xs " +
                      (value === null || value === undefined ? "text-muted-foreground" : "")
                    }
                    title={typeof value === "object" && value !== null ? JSON.stringify(value, null, 2) : undefined}
                  >
                    {formatCell(value)}
                  </TableCell>
                )
              })}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function QueriesPage() {
  const draft = load<{ query: string; variables: string } | null>(DRAFT_KEY, null)
  const [query, setQuery] = useState(draft?.query ?? QUERY_EXAMPLES[0].query)
  const [variables, setVariables] = useState(draft?.variables ?? "{}")
  const [inputTab, setInputTab] = useState<"query" | "variables">("query")
  const [resultTab, setResultTab] = useState<"json" | "table">("json")
  const [history, setHistory] = useState<HistoryEntry[]>(() => load(HISTORY_KEY, []))
  const [schema, setSchema] = useState<GraphQLSchema | null>(null)
  const [schemaError, setSchemaError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)
  const [result, setResult] = useState<GraphQLResult | null>(null)
  const [inputError, setInputError] = useState<string | null>(null)

  useEffect(() => {
    fetchSchema()
      .then(setSchema)
      .catch((e: Error) => setSchemaError(e.message))
  }, [])

  useEffect(() => save(DRAFT_KEY, { query, variables }), [query, variables])

  const queryExtensions = useMemo<Extension[]>(() => [graphqlLanguage(schema ?? undefined)], [schema])
  const jsonExtensions = useMemo<Extension[]>(() => [json()], [])

  const run = useCallback(async () => {
    let parsedVariables: Record<string, unknown> = {}
    if (variables.trim()) {
      try {
        parsedVariables = JSON.parse(variables)
      } catch (e) {
        setInputError(`Variables are not valid JSON: ${e instanceof Error ? e.message : String(e)}`)
        setInputTab("variables")
        return
      }
    }
    setInputError(null)
    setRunning(true)
    const names = operationNames(query)
    const response = await executeGraphQL(query, parsedVariables, names.length > 1 ? names[0] || undefined : undefined)
    setRunning(false)
    setResult(response)

    const entry: HistoryEntry = { query, variables, at: Date.now() }
    setHistory((previous) => {
      const next = [entry, ...previous.filter((h) => h.query !== query || h.variables !== variables)].slice(0, HISTORY_LIMIT)
      save(HISTORY_KEY, next)
      return next
    })
  }, [query, variables])

  const prettify = () => {
    try {
      setQuery(print(parse(query)))
      if (variables.trim()) setVariables(JSON.stringify(JSON.parse(variables), null, 2))
      setInputError(null)
    } catch (e) {
      setInputError(`Can't format: ${e instanceof Error ? e.message : String(e)}`)
    }
  }

  const loadExample = (index: number) => {
    const example = QUERY_EXAMPLES[index]
    setQuery(example.query)
    setVariables(example.variables ? JSON.stringify(example.variables, null, 2) : "{}")
    setInputTab("query")
  }

  const resultJson = useMemo(() => {
    if (!result) return ""
    const body: Record<string, unknown> = {}
    if (result.data !== undefined) body.data = result.data
    if (result.errors?.length) body.errors = result.errors
    return JSON.stringify(body, null, 2)
  }, [result])

  const table = useMemo(() => (result?.data ? toTable(result.data) : null), [result])
  const errors = result?.errors ?? []
  const multipleOperations = operationNames(query).length > 1

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 p-4 lg:p-6">
      {/* Toolbar */}
      <div className="flex flex-wrap items-center gap-2">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="outline" size="sm">
              <IconSparkles />
              Examples
              <IconChevronDown className="opacity-60" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-72">
            <DropdownMenuLabel>Start from an example</DropdownMenuLabel>
            <DropdownMenuSeparator />
            {QUERY_EXAMPLES.map((example, i) => (
              <DropdownMenuItem key={example.name} onSelect={() => loadExample(i)} className="flex-col items-start gap-0">
                <span>{example.name}</span>
                <span className="text-muted-foreground text-xs">{example.description}</span>
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>

        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="outline" size="sm" disabled={history.length === 0}>
              <IconHistory />
              History
              <IconChevronDown className="opacity-60" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="w-96">
            <DropdownMenuLabel>Recent queries</DropdownMenuLabel>
            <DropdownMenuSeparator />
            {history.map((entry) => (
              <DropdownMenuItem
                key={entry.at}
                onSelect={() => {
                  setQuery(entry.query)
                  setVariables(entry.variables)
                }}
                className="flex-col items-start gap-0"
              >
                <span className="w-full truncate font-mono text-xs">
                  {entry.query.replace(/#.*$/gm, "").replace(/\s+/g, " ").trim()}
                </span>
                <span className="text-muted-foreground text-xs">{new Date(entry.at).toLocaleString()}</span>
              </DropdownMenuItem>
            ))}
            {history.length > 0 && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  onSelect={() => {
                    setHistory([])
                    save(HISTORY_KEY, [])
                  }}
                  className="text-muted-foreground"
                >
                  Clear history
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>

        <Button variant="ghost" size="sm" onClick={prettify}>
          <IconBraces />
          Prettify
        </Button>

        <div className="ml-auto flex items-center gap-3">
          <span className="text-muted-foreground hidden text-xs md:inline" title={schemaError ?? undefined}>
            {schema ? "Schema loaded" : schemaError ? "Schema unavailable" : "Loading schema…"}
          </span>
          <Button variant="ghost" size="sm" asChild>
            <a href={GRAPHQL_ENDPOINT} target="_blank" rel="noopener noreferrer">
              GraphiQL
              <IconExternalLink className="opacity-60" />
            </a>
          </Button>
          <Button size="sm" onClick={run} disabled={running}>
            {running ? <IconLoader2 className="animate-spin" /> : <IconPlayerPlayFilled />}
            Run
            <kbd className="bg-primary-foreground/15 ml-1 hidden rounded px-1.5 font-mono text-[10px] sm:inline">
              {RUN_SHORTCUT}
            </kbd>
          </Button>
        </div>
      </div>

      {/* Editors and results */}
      {/* Panels have a fixed height and scroll inside, so long results never scroll the page. */}
      <div className="grid grid-cols-1 gap-4 xl:h-[calc(100svh-10rem)] xl:min-h-[28rem] xl:grid-cols-2">
        <Card className="flex h-[65svh] min-h-[22rem] flex-col gap-0 overflow-hidden py-0 xl:h-full">
          <CardHeader className="flex flex-row items-center justify-between border-b px-4 py-2 [.border-b]:pb-2">
            <Tabs value={inputTab} onValueChange={(v) => setInputTab(v as "query" | "variables")}>
              <TabsList>
                <TabsTrigger value="query">Query</TabsTrigger>
                <TabsTrigger value="variables">Variables</TabsTrigger>
              </TabsList>
            </Tabs>
            {multipleOperations && (
              <span className="text-muted-foreground text-xs">Runs the first operation</span>
            )}
          </CardHeader>
          <CardContent className="min-h-0 flex-1 px-0">
            <div className={inputTab === "query" ? "h-full" : "hidden"}>
              <CodeEditor
                value={query}
                onChange={setQuery}
                extensions={queryExtensions}
                onRun={run}
                aria-label="GraphQL query"
                placeholder="Write a GraphQL query…"
              />
            </div>
            <div className={inputTab === "variables" ? "h-full" : "hidden"}>
              <CodeEditor
                value={variables}
                onChange={setVariables}
                extensions={jsonExtensions}
                onRun={run}
                aria-label="Query variables (JSON)"
                placeholder="{ }"
              />
            </div>
          </CardContent>
          {inputError && (
            <div className="text-destructive flex items-start gap-2 border-t px-4 py-2 text-sm">
              <IconAlertTriangle className="mt-0.5 size-4 shrink-0" />
              {inputError}
            </div>
          )}
        </Card>

        <Card className="flex h-[65svh] min-h-[22rem] flex-col gap-0 overflow-hidden py-0 xl:h-full">
          <CardHeader className="flex flex-row items-center justify-between border-b px-4 py-2 [.border-b]:pb-2">
            <Tabs value={resultTab} onValueChange={(v) => setResultTab(v as "json" | "table")}>
              <TabsList>
                <TabsTrigger value="json">
                  <IconBraces />
                  JSON
                </TabsTrigger>
                <TabsTrigger value="table" disabled={!table}>
                  <IconTable />
                  Table
                </TabsTrigger>
              </TabsList>
            </Tabs>
            {result && (
              <div className="flex items-center gap-2">
                {errors.length > 0 && (
                  <Badge variant="destructive">
                    {errors.length} error{errors.length === 1 ? "" : "s"}
                  </Badge>
                )}
                <Badge variant="outline" className="font-mono">
                  {result.status || "—"} · {result.durationMs} ms
                </Badge>
              </div>
            )}
          </CardHeader>

          {errors.length > 0 && (
            <div className="border-destructive/30 bg-destructive/10 space-y-1 border-b px-4 py-2 text-sm">
              {errors.map((error, i) => (
                <div key={i} className="flex items-start gap-2">
                  <IconAlertTriangle className="text-destructive mt-0.5 size-4 shrink-0" />
                  <span>
                    {error.extensions?.code && (
                      <span className="text-destructive mr-2 font-mono text-xs">{String(error.extensions.code)}</span>
                    )}
                    {error.message}
                    {error.path && <span className="text-muted-foreground ml-2 font-mono text-xs">at {error.path.join(".")}</span>}
                  </span>
                </div>
              ))}
            </div>
          )}

          <CardContent className="min-h-0 flex-1 px-0">
            {!result ? (
              <div className="text-muted-foreground flex h-full flex-col items-center justify-center gap-2 p-8 text-center text-sm">
                <IconPlayerPlayFilled className="text-primary/60 size-6" />
                <p>
                  Run a query to see results here. Press <kbd className="bg-muted rounded px-1.5 font-mono text-xs">{RUN_SHORTCUT}</kbd> in the
                  editor, or start from an example.
                </p>
              </div>
            ) : resultTab === "table" && table ? (
              <div className="flex h-full flex-col">
                <div className="text-muted-foreground border-b px-4 py-1.5 font-mono text-xs">
                  {table.path || "data"} · {table.rows.length} row{table.rows.length === 1 ? "" : "s"}
                </div>
                <div className="min-h-0 flex-1">
                  <ResultTable table={table} />
                </div>
              </div>
            ) : (
              <CodeEditor value={resultJson} readOnly extensions={jsonExtensions} aria-label="Query result" />
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
