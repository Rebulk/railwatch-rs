import { describe, expect, it } from "vitest"

import { truncationNotice } from "@/lib/truncation"

describe("truncationNotice", () => {
  it("states what was recorded, what was dropped, and why", () => {
    expect(truncationNotice(2478, 3324, 26_214_400)).toBe(
      "2,478 of 5,802 child records recorded; 3,324 dropped (25.0 MB) because this execution's tree outgrew the per-execution limit (10,000 records or execution_buffer_bytes). What is below is the first 42.7% of it.",
    )
  })

  it("omits the byte figure when there is none", () => {
    expect(truncationNotice(10, 5, null)).toBe(
      "10 of 15 child records recorded; 5 dropped because this execution's tree outgrew the per-execution limit (10,000 records or execution_buffer_bytes). What is below is the first 66.7% of it.",
    )
  })
})
