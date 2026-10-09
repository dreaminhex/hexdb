import { useCallback, useEffect, useMemo, useState } from "react"
import { json } from "@codemirror/lang-json"
import { sql, StandardSQL, type SQLNamespace } from "@codemirror/lang-sql"
import { type Extension } from "@codemirror/state"
import {
  IconAlertTriangle,
  IconBraces,
  IconBrandGraphql,
  IconChevronDown,
  IconExternalLink,
  IconHistory,
  IconLoader2,
  IconPlayerPlayFilled,
  IconSparkles,
  IconSql,
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
import { DocumentTable } from "@/components/document-table"
import { toTable, type TableData } from "@/lib/doc-table"
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { executeGraphQL, fetchSchema, GRAPHQL_ENDPOINT, type GraphQLResult } from "@/lib/graphql"
import { QUERY_EXAMPLES, queryExamples, SQL_EXAMPLES, sqlExamples } from "@/lib/query-examples"
import { api, ApiError, type SqlResult } from "@/lib/api"

/** The two languages the console speaks. SQL is read-only; GraphQL also writes. */
type Lang = "graphql" | "sql"

interface Doc {
  query: string
  /** GraphQL variables (an object) or SQL parameters (an array), as JSON text. */
  variables: string
}

interface Draft {
  lang: Lang
  docs: Record<Lang, Doc>
}

const DRAFT_KEY = "hexdb.query.draft"
const HISTORY_KEY = "hexdb.query.history"
const HISTORY_LIMIT = 25

interface HistoryEntry {
  /** Missing in entries saved before the console spoke SQL. */
  lang?: Lang
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

const DEFAULT_DOCS: Record<Lang, Doc> = {
  graphql: { query: QUERY_EXAMPLES[0].query, variables: "{}" },
  sql: { query: SQL_EXAMPLES[0].sql, variables: JSON.stringify(SQL_EXAMPLES[0].params ?? [], null, 2) },
}

/** The saved draft, whichever shape it was saved in. */
function loadDraft(): Draft {
  const raw = load<Partial<Draft> | { query?: string; variables?: string } | null>(DRAFT_KEY, null)
  if (raw && "docs" in raw && raw.docs) {
    return {
      lang: raw.lang === "sql" ? "sql" : "graphql",
      docs: { graphql: raw.docs.graphql ?? DEFAULT_DOCS.graphql, sql: raw.docs.sql ?? DEFAULT_DOCS.sql },
    }
  }
  // The older, GraphQL-only draft.
  if (raw && "query" in raw && typeof raw.query === "string") {
    return { lang: "graphql", docs: { ...DEFAULT_DOCS, graphql: { query: raw.query, variables: raw.variables ?? "{}" } } }
  }
  return { lang: "graphql", docs: DEFAULT_DOCS }
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

/** Run one SQL statement and shape the answer like a GraphQL result, so the result panel is shared. */
async function executeSql(statement: string, params: unknown[]): Promise<GraphQLResult> {
  const started = performance.now()
  try {
    const result = await api.sql({ sql: statement, params })
    return { status: 200, durationMs: Math.round(performance.now() - started), data: result }
  } catch (e) {
    const durationMs = Math.round(performance.now() - started)
    if (e instanceof ApiError) return { status: e.status, durationMs, errors: [{ message: e.message, extensions: { code: e.code } }] }
    return { status: 0, durationMs, errors: [{ message: e instanceof Error ? e.message : String(e) }] }
  }
}

/** SQL rows arrive as arrays in column order; the table wants objects. */
function sqlTable(result: SqlResult): TableData | null {
  if (!Array.isArray(result.columns) || !Array.isArray(result.rows) || result.rows.length === 0) return null
  const columns = result.columns.map((c) => c.name)
  const rows = result.rows.map((row) => Object.fromEntries(columns.map((name, i) => [name, row[i]])))
  return { path: "rows", columns, rows }
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function QueriesPage() {
  const [draft] = useState(loadDraft)
  const [lang, setLang] = useState<Lang>(draft.lang)
  const [docs, setDocs] = useState<Record<Lang, Doc>>(draft.docs)
  const { query, variables } = docs[lang]
  const setQuery = useCallback((value: string) => setDocs((d) => ({ ...d, [lang]: { ...d[lang], query: value } })), [lang])
  const setVariables = useCallback((value: string) => setDocs((d) => ({ ...d, [lang]: { ...d[lang], variables: value } })), [lang])

  const [inputTab, setInputTab] = useState<"query" | "variables">("query")
  const [resultTab, setResultTab] = useState<"json" | "table">("json")
  const [history, setHistory] = useState<HistoryEntry[]>(() => load(HISTORY_KEY, []))
  const [schema, setSchema] = useState<GraphQLSchema | null>(null)
  const [schemaError, setSchemaError] = useState<string | null>(null)
  const [running, setRunning] = useState(false)
  const [result, setResult] = useState<{ lang: Lang; response: GraphQLResult } | null>(null)
  const [inputError, setInputError] = useState<string | null>(null)
  // The examples are written against the tessellations this hex has.
  const [tessellations, setTessellations] = useState<string[]>([])
  const graphqlExamples = useMemo(() => queryExamples(tessellations), [tessellations])
  const sqlExampleList = useMemo(() => sqlExamples(tessellations), [tessellations])
  // Tables and their columns, for SQL completion. Loaded the first time SQL is chosen.
  const [sqlSchema, setSqlSchema] = useState<SQLNamespace | null>(null)

  useEffect(() => {
    fetchSchema()
      .then(setSchema)
      .catch((e: Error) => setSchemaError(e.message))
    api
      .tessellations()
      .then((list) => setTessellations(list.filter((t) => t.kind !== "system").map((t) => t.name)))
      .catch(() => undefined)
  }, [])

  useEffect(() => {
    if (lang !== "sql" || sqlSchema) return
    let alive = true
    api
      .sqlTables()
      .then(async (tables) => {
        const entries = await Promise.all(
          tables.slice(0, 25).map(async (table) => [table, (await api.sqlColumns(table).catch(() => [])).map((c) => c.name)] as const),
        )
        if (alive) setSqlSchema(Object.fromEntries(entries))
      })
      .catch(() => {
        if (alive) setSqlSchema({})
      })
    return () => {
      alive = false
    }
  }, [lang, sqlSchema])

  useEffect(() => save(DRAFT_KEY, { lang, docs } satisfies Draft), [lang, docs])

  const queryExtensions = useMemo<Extension[]>(
    () => (lang === "sql" ? [sql({ dialect: StandardSQL, schema: sqlSchema ?? {}, upperCaseKeywords: true })] : [graphqlLanguage(schema ?? undefined)]),
    [lang, schema, sqlSchema],
  )
  const jsonExtensions = useMemo<Extension[]>(() => [json()], [])

  const run = useCallback(async () => {
    let parsedVariables: unknown = lang === "sql" ? [] : {}
    if (variables.trim()) {
      try {
        parsedVariables = JSON.parse(variables)
      } catch (e) {
        setInputError(`${lang === "sql" ? "Parameters are" : "Variables are"} not valid JSON: ${e instanceof Error ? e.message : String(e)}`)
        setInputTab("variables")
        return
      }
    }
    if (lang === "sql" && !Array.isArray(parsedVariables)) {
      setInputError('Parameters are a JSON array, one value per placeholder: ["paid", 100]')
      setInputTab("variables")
      return
    }
    if (lang === "graphql" && (parsedVariables === null || typeof parsedVariables !== "object" || Array.isArray(parsedVariables))) {
      setInputError("Variables are a JSON object.")
      setInputTab("variables")
      return
    }
    setInputError(null)
    setRunning(true)
    let response: GraphQLResult
    if (lang === "sql") {
      response = await executeSql(query, parsedVariables as unknown[])
    } else {
      const names = operationNames(query)
      response = await executeGraphQL(query, parsedVariables as Record<string, unknown>, names.length > 1 ? names[0] || undefined : undefined)
    }
    setRunning(false)
    setResult({ lang, response })

    const entry: HistoryEntry = { lang, query, variables, at: Date.now() }
    setHistory((previous) => {
      const next = [entry, ...previous.filter((h) => h.query !== query || h.variables !== variables)].slice(0, HISTORY_LIMIT)
      save(HISTORY_KEY, next)
      return next
    })
  }, [lang, query, variables])

  const prettify = () => {
    try {
      if (lang === "graphql") setQuery(print(parse(query)))
      if (variables.trim()) setVariables(JSON.stringify(JSON.parse(variables), null, 2))
      setInputError(null)
    } catch (e) {
      setInputError(`Can't format: ${e instanceof Error ? e.message : String(e)}`)
    }
  }

  const switchLang = (next: Lang) => {
    if (next === lang) return
    setLang(next)
    setInputTab("query")
    setInputError(null)
  }

  const loadGraphqlExample = (index: number) => {
    const example = graphqlExamples[index]
    setDocs((d) => ({ ...d, graphql: { query: example.query, variables: example.variables ? JSON.stringify(example.variables, null, 2) : "{}" } }))
    setInputTab("query")
  }

  const loadSqlExample = (index: number) => {
    const example = sqlExampleList[index]
    setDocs((d) => ({ ...d, sql: { query: example.sql, variables: JSON.stringify(example.params ?? [], null, 2) } }))
    setInputTab("query")
  }

  const loadHistory = (entry: HistoryEntry) => {
    const target: Lang = entry.lang ?? "graphql"
    setDocs((d) => ({ ...d, [target]: { query: entry.query, variables: entry.variables } }))
    switchLang(target)
  }

  const response = result?.response ?? null
  const resultJson = useMemo(() => {
    if (!response) return ""
    const body: Record<string, unknown> = {}
    if (response.data !== undefined) body.data = response.data
    if (response.errors?.length) body.errors = response.errors
    return JSON.stringify(body, null, 2)
  }, [response])

  const table = useMemo(() => {
    if (!result?.response.data) return null
    return result.lang === "sql" ? sqlTable(result.response.data as SqlResult) : toTable(result.response.data)
  }, [result])
  const errors = response?.errors ?? []
  const multipleOperations = lang === "graphql" && operationNames(query).length > 1
  const sqlTranslated = result?.lang === "sql" ? (result.response.data as SqlResult | undefined)?.translated : undefined
  const sqlNext = result?.lang === "sql" ? (result.response.data as SqlResult | undefined)?.next : undefined

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 p-4 lg:p-6">
      {/* Toolbar */}
      <div className="flex flex-wrap items-center gap-2">
        <Tabs value={lang} onValueChange={(v) => switchLang(v as Lang)}>
          <TabsList aria-label="Query language">
            <TabsTrigger value="graphql">
              <IconBrandGraphql />
              GraphQL
            </TabsTrigger>
            <TabsTrigger value="sql">
              <IconSql />
              SQL
            </TabsTrigger>
          </TabsList>
        </Tabs>

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
            {lang === "sql"
              ? sqlExampleList.map((example, i) => (
                  <DropdownMenuItem key={example.name} onSelect={() => loadSqlExample(i)} className="flex-col items-start gap-0">
                    <span>{example.name}</span>
                    <span className="text-muted-foreground text-xs">{example.description}</span>
                  </DropdownMenuItem>
                ))
              : graphqlExamples.map((example, i) => (
                  <DropdownMenuItem key={example.name} onSelect={() => loadGraphqlExample(i)} className="flex-col items-start gap-0">
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
              <DropdownMenuItem key={entry.at} onSelect={() => loadHistory(entry)} className="flex-col items-start gap-0">
                <span className="flex w-full items-center gap-2">
                  <Badge variant="outline" className="h-4 shrink-0 px-1 font-mono text-[10px] uppercase">
                    {entry.lang ?? "graphql"}
                  </Badge>
                  <span className="min-w-0 flex-1 truncate font-mono text-xs">
                    {entry.query.replace(/^\s*(#|--).*$/gm, "").replace(/\s+/g, " ").trim()}
                  </span>
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
          {lang === "sql" ? (
            <span className="text-muted-foreground hidden text-xs md:inline" title="SQL is read-only: SELECT over one tessellation, translated onto the same queries REST runs.">
              Read-only SELECT
            </span>
          ) : (
            <>
              <span className="text-muted-foreground hidden text-xs md:inline" title={schemaError ?? undefined}>
                {schema ? "Schema loaded" : schemaError ? "Schema unavailable" : "Loading schema…"}
              </span>
              <Button variant="ghost" size="sm" asChild>
                <a href={GRAPHQL_ENDPOINT} target="_blank" rel="noopener noreferrer">
                  GraphiQL
                  <IconExternalLink className="opacity-60" />
                </a>
              </Button>
            </>
          )}
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
                <TabsTrigger value="query">{lang === "sql" ? "SQL" : "Query"}</TabsTrigger>
                <TabsTrigger value="variables">{lang === "sql" ? "Parameters" : "Variables"}</TabsTrigger>
              </TabsList>
            </Tabs>
            {multipleOperations && <span className="text-muted-foreground text-xs">Runs the first operation</span>}
          </CardHeader>
          <CardContent className="min-h-0 flex-1 px-0">
            <div className={inputTab === "query" ? "h-full" : "hidden"}>
              <CodeEditor
                value={query}
                onChange={setQuery}
                extensions={queryExtensions}
                onRun={run}
                aria-label={lang === "sql" ? "SQL statement" : "GraphQL query"}
                placeholder={lang === "sql" ? "SELECT * FROM orders LIMIT 25" : "Write a GraphQL query…"}
              />
            </div>
            <div className={inputTab === "variables" ? "h-full" : "hidden"}>
              <CodeEditor
                value={variables}
                onChange={setVariables}
                extensions={jsonExtensions}
                onRun={run}
                aria-label={lang === "sql" ? "SQL parameters (JSON array)" : "Query variables (JSON)"}
                placeholder={lang === "sql" ? '[ "paid", 100 ]' : "{ }"}
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
            {response && (
              <div className="flex items-center gap-2">
                {errors.length > 0 && (
                  <Badge variant="destructive">
                    {errors.length} error{errors.length === 1 ? "" : "s"}
                  </Badge>
                )}
                {sqlTranslated && typeof sqlTranslated.tessellation === "string" && (
                  <Badge variant="outline" className="hidden font-mono sm:inline-flex" title={JSON.stringify(sqlTranslated, null, 2)}>
                    {sqlTranslated.aggregate ? "aggregate" : "query"} on {sqlTranslated.tessellation}
                  </Badge>
                )}
                <Badge variant="outline" className="font-mono">
                  {response.status || "—"} · {response.durationMs} ms
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
            {!response ? (
              <div className="text-muted-foreground flex h-full flex-col items-center justify-center gap-2 p-8 text-center text-sm">
                <IconPlayerPlayFilled className="text-primary/60 size-6" />
                <p>
                  Run a query to see results here. Press <kbd className="bg-muted rounded px-1.5 font-mono text-xs">{RUN_SHORTCUT}</kbd> in the
                  editor, or start from an example.
                </p>
                <p className="text-xs">
                  {lang === "sql"
                    ? "SQL is one read-only SELECT over one tessellation, with ? or $1 placeholders filled from the Parameters tab."
                    : "GraphQL reads and writes, with variables from the Variables tab."}
                </p>
              </div>
            ) : resultTab === "table" && table ? (
              <div className="flex h-full flex-col">
                <div className="text-muted-foreground border-b px-4 py-1.5 font-mono text-xs">
                  {table.path || "data"} · {table.rows.length} row{table.rows.length === 1 ? "" : "s"}
                  {sqlNext ? " · more pages (send next as cursor)" : ""}
                </div>
                <div className="min-h-0 flex-1">
                  <DocumentTable columns={table.columns} rows={table.rows} />
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
