// Mirrors the server's FilterQuery.parse (dashboard/filter_query.rs):
// "key:value key2:value2 free text" tokens, where a value may be quoted to
// hold spaces (ua:"Mozilla/5.0 (X11"): from key:" to the next ", or to the
// end without one.
const KEY = /^([a-z_]+):(?=\S)/

export interface ParsedFilter {
  text: string
  fields: Record<string, string>
}

export function parseFilter(query: string): ParsedFilter {
  const text: string[] = []
  const fields: Record<string, string> = {}
  let rest = query
  for (;;) {
    rest = rest.replace(/^\s+/, "")
    if (!rest) break
    const end = rest.search(/\s/)
    const word = end < 0 ? rest : rest.slice(0, end)
    const m = KEY.exec(word)
    if (m) {
      const after = rest.slice(m[0].length)
      if (after.startsWith('"')) {
        const close = after.indexOf('"', 1)
        const value = close < 0 ? after.slice(1) : after.slice(1, close)
        if (value) fields[m[1]] = value
        rest = close < 0 ? "" : after.slice(close + 1)
        continue
      }
      fields[m[1]] = word.slice(m[0].length)
    } else {
      text.push(word)
    }
    rest = end < 0 ? "" : rest.slice(end)
  }
  return { text: text.join(" "), fields }
}

// A value holding whitespace is quoted (one with a " in it is cut there,
// as the grammar cannot write it).
function token(key: string, value: string): string {
  return /\s/.test(value) ? `${key}:"${value.split('"')[0]}"` : `${key}:${value}`
}

export function serializeFilter(parsed: ParsedFilter): string {
  const tokens = Object.entries(parsed.fields).map(([k, v]) => token(k, v))
  if (parsed.text) tokens.push(parsed.text)
  return tokens.join(" ")
}
