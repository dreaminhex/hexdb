import { buildClientSchema, getIntrospectionQuery, type GraphQLSchema, type IntrospectionQuery } from "graphql"

/** The GraphQL endpoint, served by the HexDB API at the site root. */
export const GRAPHQL_ENDPOINT = "/graphql"

export interface GraphQLError {
  message: string
  path?: (string | number)[]
  locations?: { line: number; column: number }[]
  extensions?: { code?: string; [key: string]: unknown }
}

export interface GraphQLResult {
  data?: unknown
  errors?: GraphQLError[]
  /** HTTP status of the response (0 if the request failed). */
  status: number
  /** Round-trip time in milliseconds. */
  durationMs: number
}

/** Execute a GraphQL operation against the HexDB API. */
export async function executeGraphQL(
  query: string,
  variables?: Record<string, unknown>,
  operationName?: string,
): Promise<GraphQLResult> {
  const started = performance.now()
  try {
    const response = await fetch(GRAPHQL_ENDPOINT, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify({ query, variables: variables ?? {}, operationName }),
    })
    const durationMs = Math.round(performance.now() - started)
    const text = await response.text()
    let body: { data?: unknown; errors?: GraphQLError[]; error?: { message: string; code?: string } } = {}
    try {
      body = text ? JSON.parse(text) : {}
    } catch {
      return { status: response.status, durationMs, errors: [{ message: text || response.statusText }] }
    }
    // Non-GraphQL errors (e.g. malformed JSON) use the REST error shape.
    if (body.error) {
      return {
        status: response.status,
        durationMs,
        errors: [{ message: body.error.message, extensions: { code: body.error.code } }],
      }
    }
    return { status: response.status, durationMs, data: body.data, errors: body.errors }
  } catch (e) {
    return {
      status: 0,
      durationMs: Math.round(performance.now() - started),
      errors: [{ message: `Could not reach HexDB: ${e instanceof Error ? e.message : String(e)}` }],
    }
  }
}

/** Fetch the schema by introspection, for autocomplete and validation. */
export async function fetchSchema(): Promise<GraphQLSchema> {
  const result = await executeGraphQL(getIntrospectionQuery())
  if (result.errors?.length || !result.data) {
    throw new Error(result.errors?.[0]?.message ?? "Introspection returned no data")
  }
  return buildClientSchema(result.data as IntrospectionQuery)
}
