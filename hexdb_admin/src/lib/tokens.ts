/** Tokens of tokenized fields: `tok_` + 26 base32 characters, and `_` + the value's last 4 when it had at least 10. */
const TOKEN = /^tok_[a-z2-7]{26}(_.{4})?$/u

export function isToken(value: unknown): value is string {
  return typeof value === "string" && TOKEN.test(value)
}

/** Every token in a document, with its field path (dotted; array items by index). */
export function tokensIn(value: unknown, path = ""): { path: string; token: string }[] {
  if (isToken(value)) return [{ path, token: value }]
  if (Array.isArray(value)) return value.flatMap((item, i) => tokensIn(item, path ? `${path}.${i}` : String(i)))
  if (value !== null && typeof value === "object") {
    return Object.entries(value).flatMap(([key, item]) => tokensIn(item, path ? `${path}.${key}` : key))
  }
  return []
}
