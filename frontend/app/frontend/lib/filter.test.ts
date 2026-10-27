import { describe, expect, it } from "vitest"

import { parseFilter, serializeFilter } from "@/lib/filter"

describe("parseFilter", () => {
  it("extracts key:value tokens into fields", () => {
    expect(parseFilter("status:500 method:GET").fields).toEqual({
      status: "500",
      method: "GET",
    })
  })

  it("collects non-token words into text, in order", () => {
    expect(parseFilter("hello world").text).toBe("hello world")
  })

  it("mixes field tokens and free text, keeping free text order but not position", () => {
    const parsed = parseFilter("status:500 broken thing method:GET")
    expect(parsed.fields).toEqual({ status: "500", method: "GET" })
    expect(parsed.text).toBe("broken thing")
  })

  it("treats a word with an uppercase key as text, since keys must be lowercase", () => {
    // TOKEN regex requires [a-z_]+ before the colon.
    expect(parseFilter("Status:500").fields).toEqual({})
    expect(parseFilter("Status:500").text).toBe("Status:500")
  })

  it("strips the quotes of a quoted value", () => {
    expect(parseFilter('key:"quoted"').fields).toEqual({ key: "quoted" })
  })

  it("keeps a quoted value's spaces, as the server's parser does", () => {
    const parsed = parseFilter('ua:"Mozilla/5.0 (X11; Linux)" status:401 slow')
    expect(parsed.fields).toEqual({
      ua: "Mozilla/5.0 (X11; Linux)",
      status: "401",
    })
    expect(parsed.text).toBe("slow")
  })

  it("takes an unclosed quote to the end", () => {
    expect(parseFilter('ua:"two words').fields).toEqual({ ua: "two words" })
  })

  it("keeps quotes inside an unquoted value", () => {
    expect(parseFilter('path:/a"b').fields).toEqual({ path: '/a"b' })
  })

  it("treats a colon with no value as text", () => {
    expect(parseFilter("status:").fields).toEqual({})
    expect(parseFilter("status:").text).toBe("status:")
  })

  it("returns empty text and fields for an empty string", () => {
    expect(parseFilter("")).toEqual({ text: "", fields: {} })
  })

  it("returns empty text and fields for a whitespace-only string", () => {
    expect(parseFilter("   ")).toEqual({ text: "", fields: {} })
  })

  it("collapses repeated whitespace between words", () => {
    expect(parseFilter("status:500    method:GET").fields).toEqual({
      status: "500",
      method: "GET",
    })
  })
})

describe("serializeFilter", () => {
  it("joins field tokens as key:value", () => {
    expect(
      serializeFilter({ text: "", fields: { status: "500", method: "GET" } }),
    ).toBe("status:500 method:GET")
  })

  it("appends free text after field tokens", () => {
    expect(
      serializeFilter({ text: "broken thing", fields: { status: "500" } }),
    ).toBe("status:500 broken thing")
  })

  it("omits the text segment entirely when text is empty", () => {
    expect(serializeFilter({ text: "", fields: { status: "500" } })).toBe(
      "status:500",
    )
  })

  it("returns an empty string for an empty parsed filter", () => {
    expect(serializeFilter({ text: "", fields: {} })).toBe("")
  })
})

describe("parseFilter/serializeFilter round trip", () => {
  it("round trips key:value tokens with free text", () => {
    const query = "status:500 method:GET some free text"
    expect(serializeFilter(parseFilter(query))).toBe(
      "status:500 method:GET some free text",
    )
  })

  it("round trips a field-only query", () => {
    const query = "status:500"
    expect(serializeFilter(parseFilter(query))).toBe(query)
  })

  it("round trips free text alone", () => {
    const query = "just some text"
    expect(serializeFilter(parseFilter(query))).toBe(query)
  })

  it("round trips the empty string", () => {
    expect(serializeFilter(parseFilter(""))).toBe("")
  })

  it("round trips a value with spaces by quoting it", () => {
    const query = 'path:/admin* ua:"Mozilla/5.0 (X11; Linux)"'
    expect(serializeFilter(parseFilter(query))).toBe(query)
  })
})
