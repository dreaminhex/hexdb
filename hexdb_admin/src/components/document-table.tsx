import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { cn } from "@/lib/utils"

export type { TableData } from "@/lib/doc-table"

function formatCell(value: unknown): string {
  if (value === undefined) return ""
  if (value === null) return "null"
  if (typeof value === "string") return value
  const text = JSON.stringify(value)
  return text.length > 80 ? `${text.slice(0, 77)}…` : text
}

export interface DocumentTableProps {
  columns: string[]
  rows: Record<string, unknown>[]
  /** Makes rows clickable. */
  onRowClick?: (row: Record<string, unknown>) => void
  /** Highlights the row whose `id` matches. */
  selectedId?: string | null
  className?: string
}

/** A compact, monospace table of JSON rows. */
export function DocumentTable({ columns, rows, onRowClick, selectedId, className }: DocumentTableProps) {
  return (
    <div className={cn("h-full overflow-auto", className)}>
      <Table>
        <TableHeader className="bg-muted/60 sticky top-0 z-10">
          <TableRow>
            {columns.map((column) => (
              <TableHead key={column} className="font-mono text-xs">
                {column}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row, i) => {
            const id = typeof row.id === "string" ? row.id : undefined
            return (
              <TableRow
                key={id ?? i}
                data-state={id && id === selectedId ? "selected" : undefined}
                className={onRowClick ? "cursor-pointer" : undefined}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
                tabIndex={onRowClick ? 0 : undefined}
                onKeyDown={
                  onRowClick
                    ? (e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault()
                          onRowClick(row)
                        }
                      }
                    : undefined
                }
              >
                {columns.map((column) => {
                  const value = row[column]
                  return (
                    <TableCell
                      key={column}
                      className={cn(
                        "max-w-72 truncate font-mono text-xs",
                        (value === null || value === undefined) && "text-muted-foreground",
                      )}
                      title={typeof value === "object" && value !== null ? JSON.stringify(value, null, 2) : undefined}
                    >
                      {formatCell(value)}
                    </TableCell>
                  )
                })}
              </TableRow>
            )
          })}
        </TableBody>
      </Table>
    </div>
  )
}
