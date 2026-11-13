import { useState } from "react"
import { IconCheck, IconCopy } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"

import type { Snippet } from "@/lib/examples"

/** Ready-to-run examples, one tab each, with a copy button: how to use a stream or queue from your own code. */
export function CodeSnippets({ snippets }: { snippets: Snippet[] }) {
  const [copied, setCopied] = useState<string | null>(null)
  return (
    <Tabs defaultValue={snippets[0]?.id} className="min-w-0">
      <TabsList className="h-auto flex-wrap justify-start">
        {snippets.map((s) => (
          <TabsTrigger key={s.id} value={s.id} className="text-xs">
            {s.label}
          </TabsTrigger>
        ))}
      </TabsList>
      {snippets.map((s) => (
        <TabsContent key={s.id} value={s.id} className="grid min-w-0 gap-1.5">
          <p className="text-muted-foreground text-xs">{s.note}</p>
          <div className="relative min-w-0">
            <pre className="bg-muted/50 max-h-64 overflow-auto rounded-md border p-3 pr-10 text-xs leading-relaxed">{s.code}</pre>
            <Button
              variant="ghost"
              size="icon"
              className="text-muted-foreground absolute top-1.5 right-1.5 size-7"
              aria-label={`Copy the ${s.label} example`}
              onClick={() =>
                void navigator.clipboard.writeText(s.code).then(() => {
                  setCopied(s.id)
                  setTimeout(() => setCopied(null), 1400)
                })
              }
            >
              {copied === s.id ? <IconCheck className="text-primary" /> : <IconCopy />}
            </Button>
          </div>
        </TabsContent>
      ))}
    </Tabs>
  )
}

