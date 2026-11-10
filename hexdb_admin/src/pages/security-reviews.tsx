import { useEffect, useState } from "react"
import { IconAlertTriangle, IconLoader2, IconMail, IconPlayerPlay, IconShieldCheck } from "@tabler/icons-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type SecurityReview } from "@/lib/api"
import { cn } from "@/lib/utils"

const SEVERITY_STYLE: Record<string, string> = {
  none: "text-status-good",
  low: "text-muted-foreground",
  medium: "text-status-warning",
  high: "text-destructive",
  error: "text-destructive",
}

function when(ms: number): string {
  return new Date(ms).toLocaleString()
}

/** The AI's reviews of the audit trail, and their settings (administrators). */
export function SecurityReviewsPage() {
  const reviews = usePoll(() => api.securityReviews(), 30_000, [])
  const [enabled, setEnabled] = useState(false)
  const [hours, setHours] = useState(24)
  const [recipients, setRecipients] = useState("")
  const [loaded, setLoaded] = useState(false)
  const [saving, setSaving] = useState(false)
  const [running, setRunning] = useState(false)
  const [testing, setTesting] = useState(false)
  const [open, setOpen] = useState<string | null>(null)

  useEffect(() => {
    api.config().then(
      (config) => {
        const value = (key: string) => config.lattice[key]
        setEnabled(value("security.review.enabled") === true)
        setHours(typeof value("security.review.interval_hours") === "number" ? (value("security.review.interval_hours") as number) : 24)
        setRecipients(typeof value("security.review.recipients") === "string" ? (value("security.review.recipients") as string) : "")
        setLoaded(true)
      },
      (e) => toast.error(errorMessage(e)),
    )
  }, [])

  const save = async () => {
    if (!(hours >= 1 && hours <= 24)) {
      toast.error("Review every 1 to 24 hours.")
      return
    }
    setSaving(true)
    try {
      await api.saveConfig(
        { "security.review.enabled": enabled, "security.review.interval_hours": hours, "security.review.recipients": recipients.trim() },
        { comment: "Security review settings" },
      )
      toast.success(enabled ? `Reviews every ${hours} hour${hours === 1 ? "" : "s"}.` : "Security reviews are off.")
      await reviews.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  const runNow = async () => {
    setRunning(true)
    try {
      const review = await api.runSecurityReview()
      toast.success(review.error ? "The review couldn't finish; see below." : `Reviewed ${review.events} events: ${review.severity}.`)
      setOpen(review.id)
      await reviews.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setRunning(false)
    }
  }

  const testEmail = async () => {
    setTesting(true)
    try {
      const sent = await api.testEmail()
      toast.success(`Sent a test email to ${sent.sent_to}.`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setTesting(false)
    }
  }

  const data = reviews.data
  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        The AI reads a summary of the audit trail (failed sign-ins, refused requests, users on new addresses, activity by hour, administrative changes) and
        says what looks like an attack or misuse, with the evidence. Findings of medium severity or worse are emailed. It sees counts, logins and addresses,
        never your documents, and the review runs on the Oracle when the lattice has one.
      </p>

      <Card>
        <CardHeader>
          <CardTitle>Settings</CardTitle>
          <CardDescription>
            {data && !data.ai && "Set up an AI provider on the Configuration page first. "}
            {data && !data.email && "Email isn't set up (email.smtp_url and email.from on the Configuration page): reviews are kept here but not sent."}
          </CardDescription>
        </CardHeader>
        <CardContent className="flex flex-col gap-4">
          <div className="flex items-center gap-2">
            <Checkbox id="review-enabled" checked={enabled} onCheckedChange={(v) => setEnabled(v === true)} disabled={!loaded} />
            <Label htmlFor="review-enabled">Review the audit trail on a schedule</Label>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Label htmlFor="review-hours">Every</Label>
            <Input
              id="review-hours"
              type="number"
              min={1}
              max={24}
              step={1}
              className="h-8 w-20"
              value={hours}
              onChange={(e) => setHours(Math.round(Number(e.target.value)))}
              disabled={!loaded}
            />
            <span className="text-sm">hour{hours === 1 ? "" : "s"} (1 to 24)</span>
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="review-recipients">Email findings to</Label>
            <Input
              id="review-recipients"
              className="h-8 max-w-xl text-xs"
              placeholder="Every administrator with an email address"
              value={recipients}
              onChange={(e) => setRecipients(e.target.value)}
              disabled={!loaded}
            />
          </div>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" onClick={() => void save()} disabled={!loaded || saving}>
              {saving && <IconLoader2 className="animate-spin" />} Save
            </Button>
            <Button size="sm" variant="outline" onClick={() => void runNow()} disabled={running || data?.ai === false}>
              {running ? <IconLoader2 className="animate-spin" /> : <IconPlayerPlay />} Review now
            </Button>
            <Button size="sm" variant="outline" onClick={() => void testEmail()} disabled={testing || !data?.email}>
              {testing ? <IconLoader2 className="animate-spin" /> : <IconMail />} Send me a test email
            </Button>
          </div>
          {data?.next_due && <p className="text-muted-foreground text-xs">Next review around {when(data.next_due)}.</p>}
        </CardContent>
      </Card>

      {reviews.error && <p className="text-destructive text-sm">{reviews.error.message}</p>}
      {data?.reviews.length === 0 && (
        <p className="text-muted-foreground inline-flex items-center gap-2 text-sm">
          <IconShieldCheck className="size-4" /> No reviews yet.
        </p>
      )}
      <div className="flex flex-col gap-2">
        {data?.reviews.map((r: SecurityReview) => (
          <Card key={r.id} className="gap-2 py-3">
            <button type="button" className="flex w-full flex-wrap items-center gap-x-3 gap-y-1 px-4 text-left" onClick={() => setOpen(open === r.id ? null : r.id)} aria-expanded={open === r.id}>
              <Badge variant="outline" className={cn("gap-1 uppercase", SEVERITY_STYLE[r.severity])}>
                {r.severity !== "none" && <IconAlertTriangle className="size-3" />}
                {r.severity}
              </Badge>
              <span className="text-sm font-medium">{when(r.started)}</span>
              <span className="text-muted-foreground text-xs">
                {r.events} events since {when(r.window_from)} · {r.findings.length} finding{r.findings.length === 1 ? "" : "s"} · {r.trigger === "schedule" ? "scheduled" : `by ${r.trigger}`} · on {r.hex}
                {r.emailed_to.length > 0 && ` · emailed to ${r.emailed_to.length}`}
              </span>
            </button>
            {open === r.id && (
              <div className="flex flex-col gap-2 px-4 text-sm">
                {r.error && <p className="text-destructive">{r.error}</p>}
                {r.summary && <p>{r.summary}</p>}
                {r.findings.map((f, i) => (
                  <div key={i} className="rounded-md border p-3">
                    <div className="flex items-center gap-2 font-medium">
                      <Badge variant="outline" className={cn("uppercase", SEVERITY_STYLE[f.severity])}>
                        {f.severity}
                      </Badge>
                      {f.title}
                    </div>
                    <p className="text-muted-foreground mt-1 text-xs">Evidence: {f.evidence}</p>
                    <p className="mt-1 text-xs">What to do: {f.recommendation}</p>
                  </div>
                ))}
                {r.email_error && <p className="text-status-warning text-xs">Not emailed: {r.email_error}</p>}
                <p className="text-muted-foreground text-xs">
                  {r.model && `${r.model} · ${r.tokens} tokens · `}Check the evidence in the Audit Trail before acting on it.
                </p>
              </div>
            )}
          </Card>
        ))}
      </div>
    </div>
  )
}
