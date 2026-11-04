// HexDB connection URIs (MANUAL.md, "Connection URIs"):
//
//   hexdb://[login[:password]@]host[:port][/path][?option=value&...]   HTTP, port 7700 by default
//   hexdbs://...                                                       HTTPS, port 443 by default

/** A parsed connection URI. Absent parts are null. */
export interface ConnectionUri {
  /** The base URL requests go to, e.g. https://db.example.com/hexdb (no trailing slash). */
  url: string
  login: string | null
  password: string | null
  apiKey: string | null
  timeoutSeconds: number | null
  pageSize: number | null
  caFile: string | null
}

/** Whether `text` is a hexdb:// or hexdbs:// URI (as opposed to a plain http(s) URL). */
export function isConnectionUri(text: string): boolean {
  return /^hexdbs?:/i.test(text.trim())
}

/** Parse a hexdb:// or hexdbs:// URI; throws an Error naming the problem if it isn't valid. */
export function parseConnectionUri(uri: string): ConnectionUri {
  const text = uri.trim()
  const scheme = /^(hexdbs?):\/\//i.exec(text)
  if (!scheme) {
    throw new Error(/^hexdbs?:/i.test(text) ? "A HexDB URI needs // after the scheme: hexdb://host" : "A HexDB URI starts with hexdb:// or hexdbs://")
  }
  const tls = scheme[1].toLowerCase() === "hexdbs"
  let rest = text.slice(scheme[0].length)
  if (rest.includes("#")) throw new Error("A HexDB URI can't have a fragment (#)")

  let query = ""
  const q = rest.indexOf("?")
  if (q >= 0) {
    query = rest.slice(q + 1)
    rest = rest.slice(0, q)
  }
  const slash = rest.indexOf("/")
  let authority = slash >= 0 ? rest.slice(0, slash) : rest
  const path = slash >= 0 ? rest.slice(slash).replace(/\/+$/, "") : ""

  let login: string | null = null
  let password: string | null = null
  const at = authority.lastIndexOf("@")
  if (at >= 0) {
    const userinfo = authority.slice(0, at)
    authority = authority.slice(at + 1)
    const colon = userinfo.indexOf(":")
    login = decode(colon >= 0 ? userinfo.slice(0, colon) : userinfo, "login")
    password = colon >= 0 ? decode(userinfo.slice(colon + 1), "password") : null
    if (!login) throw new Error("The URI's login is empty")
  }

  if (authority.includes(",")) throw new Error("A HexDB URI names one host; any hex of a lattice serves the whole lattice")
  let host: string
  let portText: string | null = null
  if (authority.startsWith("[")) {
    const close = authority.indexOf("]")
    if (close < 0) throw new Error("An IPv6 address needs a closing ]")
    host = authority.slice(0, close + 1)
    const after = authority.slice(close + 1)
    if (after) {
      if (!after.startsWith(":")) throw new Error(`Unexpected text after the IPv6 address: ${after}`)
      portText = after.slice(1)
    }
  } else {
    const colon = authority.lastIndexOf(":")
    host = colon >= 0 ? authority.slice(0, colon) : authority
    portText = colon >= 0 ? authority.slice(colon + 1) : null
  }
  host = host.toLowerCase()
  if (!host || host === "[]") throw new Error("The URI has no host")

  const defaultPort = tls ? 443 : 7700
  let port = defaultPort
  if (portText !== null) {
    if (!/^\d+$/.test(portText)) throw new Error(`The port isn't a number: ${portText}`)
    port = Number(portText)
    if (port < 1 || port > 65535) throw new Error(`The port must be 1 to 65535: ${portText}`)
  }

  const result: ConnectionUri = {
    url: `${tls ? "https" : "http"}://${host}${port === 443 && tls ? "" : `:${port}`}${path}`,
    login,
    password,
    apiKey: null,
    timeoutSeconds: null,
    pageSize: null,
    caFile: null,
  }

  for (const pair of query.split("&")) {
    if (!pair) continue
    const eq = pair.indexOf("=")
    const name = decode(eq >= 0 ? pair.slice(0, eq) : pair, "option name").toLowerCase()
    const value = decode(eq >= 0 ? pair.slice(eq + 1) : "", name)
    switch (name) {
      case "apikey":
        if (!value) throw new Error("apiKey is empty")
        result.apiKey = value
        break
      case "timeout":
        result.timeoutSeconds = integer(value, "timeout", 1, Number.MAX_SAFE_INTEGER, "a positive number of seconds")
        break
      case "pagesize":
        result.pageSize = integer(value, "pageSize", 1, 10_000, "1 to 10,000")
        break
      case "cafile":
        if (!value) throw new Error("caFile is empty")
        result.caFile = value
        break
      default:
      // Options other drivers use are ignored, so one URI works everywhere.
    }
  }
  if (result.login !== null && result.apiKey !== null) throw new Error("Give a login or an apiKey, not both")
  return result
}

/** The URI with its password and apiKey replaced by `***`, safe to show or log. */
export function redactConnectionUri(uri: string): string {
  return uri
    .replace(/^(hexdbs?:\/\/[^:@/?#]*:)[^@/?#]*@/i, "$1***@")
    .replace(/([?&]apikey=)[^&#]*/gi, "$1***")
}

function decode(text: string, what: string): string {
  try {
    return decodeURIComponent(text.replace(/\+/g, "%2B"))
  } catch {
    throw new Error(`The ${what} has a malformed %-escape`)
  }
}

function integer(value: string, name: string, min: number, max: number, expected: string): number {
  if (!/^\d+$/.test(value)) throw new Error(`${name} must be ${expected}: ${value || "(empty)"}`)
  const n = Number(value)
  if (n < min || n > max) throw new Error(`${name} must be ${expected}: ${value}`)
  return n
}
