import { bytes, pct } from "@/lib/format"

// The gem keeps an execution's earliest child records up to its per-execution
// limits (10,000 records, execution_buffer_bytes) and drops the rest, so a
// truncated trace looks complete unless the page says otherwise. `recorded`
// is every child record the server kept; the dropped ones never reached it.
export function truncationNotice(
  recorded: number,
  dropped: number,
  droppedBytes: number | null | undefined,
): string {
  const total = recorded + dropped
  return `${recorded.toLocaleString()} of ${total.toLocaleString()} child records recorded; ${dropped.toLocaleString()} dropped${droppedBytes ? ` (${bytes(droppedBytes)})` : ""} because this execution's tree outgrew the per-execution limit (10,000 records or execution_buffer_bytes). What is below is the first ${pct(recorded, total)} of it.`
}
