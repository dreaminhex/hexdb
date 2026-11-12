import { useCallback, useEffect, useState } from "react"
import { IconBell, IconKey, IconLoader2 } from "@tabler/icons-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Textarea } from "@/components/ui/textarea"
import { api, errorMessage, type AlertDelivery, type SecretInfo } from "@/lib/api"

type Field = { key: string; label: string; secret?: boolean; placeholder?: string; multiline?: boolean }
type Channel = { id: string; name: string; help: string; fields: Field[] }

const CHANNELS: Channel[] = [
  {
    id: "pagerduty",
    name: "PagerDuty",
    help: "An Events API v2 integration key: flagged reviews trigger an incident.",
    fields: [{ key: "alerts.pagerduty.routing_key", label: "Integration key", secret: true }],
  },
  {
    id: "sms",
    name: "SMS (Twilio)",
    help: "A short text to each number, in international format.",
    fields: [
      { key: "alerts.twilio.account_sid", label: "Account SID", placeholder: "AC…" },
      { key: "alerts.twilio.auth_token", label: "Auth token", secret: true },
      { key: "alerts.twilio.from", label: "From", placeholder: "+15551234567 or MG…" },
      { key: "alerts.twilio.to", label: "To", placeholder: "+15551234567, +447700900123" },
    ],
  },
  {
    id: "chat",
    name: "Slack, Teams or Discord",
    help: "Incoming webhook URLs, one per line; each gets its service's format (others get Slack's).",
    fields: [{ key: "alerts.chat.webhook_urls", label: "Webhook URLs", secret: true, multiline: true }],
  },
  {
    id: "webhook",
    name: "Webhook",
    help: "The review as JSON, signed with HMAC-SHA256 (X-HexDB-Signature) when a signing secret is set.",
    fields: [
      { key: "alerts.webhook.url", label: "URL", placeholder: "https://hooks.example.com/hexdb" },
      { key: "alerts.webhook.signing_secret", label: "Signing secret", secret: true },
    ],
  },
]

const SEVERITIES = [
  { value: "low", label: "Low or worse" },
  { value: "medium", label: "Medium or worse" },
  { value: "high", label: "High only" },
]

function isSecretInfo(v: unknown): v is SecretInfo {
  return typeof v === "object" && v !== null && (v as SecretInfo).set === true
}

/** Where flagged security reviews go besides email (alerts.* in the lattice configuration). */
export function AlertChannelsCard({ configured, onSaved }: { configured: string[]; onSaved: () => void }) {
  const [values, setValues] = useState<Record<string, unknown>>({})
  // Text settings as edited; secrets: a new value, null to clear, absent to keep.
  const [draft, setDraft] = useState<Record<string, string | null>>({})
  const [loaded, setLoaded] = useState(false)
  const [saving, setSaving] = useState(false)
  const [testing, setTesting] = useState<string | null>(null)

  const load = useCallback(async () => {
    const config = await api.config()
    setValues(config.lattice)
    setDraft({})
    setLoaded(true)
  }, [])

  useEffect(() => {
    load().catch((e) => toast.error(errorMessage(e)))
  }, [load])

  const text = (key: string, fallback = "") => {
    if (key in draft) return draft[key] ?? ""
    const v = values[key]
    return typeof v === "string" ? v : fallback
  }
  const set = (key: string, value: string | null | undefined) =>
    setDraft((d) => {
      const next = { ...d }
      if (value === undefined) delete next[key]
      else next[key] = value
      return next
    })

  const changed = Object.keys(draft).length > 0
  const save = async () => {
    const changes: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(draft)) {
      changes[key] = typeof value === "string" ? value.trim() : value
    }
    setSaving(true)
    try {
      await api.saveConfig(changes, { comment: "Alert channels" })
      toast.success("Alert channels saved.")
      await load()
      onSaved()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  const test = async (channel?: string) => {
    setTesting(channel ?? "all")
    try {
      const { deliveries } = await api.testAlert(channel)
      report(deliveries)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setTesting(null)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Alert channels</CardTitle>
        <CardDescription>
          Besides email, flagged reviews go to every channel set up here. Keys and tokens are write-only: leave a field empty to keep what's saved.
        </CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-5">
        <div className="flex flex-wrap items-center gap-2">
          <Label>Notify for reviews of</Label>
          <Select value={text("security.review.notify_severity", "medium")} onValueChange={(v) => set("security.review.notify_severity", v)} disabled={!loaded}>
            <SelectTrigger className="h-8 w-44" aria-label="Notify severity">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SEVERITIES.map((s) => (
                <SelectItem key={s.value} value={s.value}>
                  {s.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <span className="text-muted-foreground text-xs">(email too)</span>
        </div>
        {CHANNELS.map((channel) => (
          <div key={channel.id} className="flex flex-col gap-2 border-t pt-4">
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-sm font-medium">{channel.name}</span>
              {configured.includes(channel.id) ? <Badge variant="outline" className="text-status-good">on</Badge> : <Badge variant="outline" className="text-muted-foreground">off</Badge>}
              <Button
                size="sm"
                variant="ghost"
                className="ml-auto h-7"
                onClick={() => void test(channel.id)}
                disabled={!configured.includes(channel.id) || changed || testing !== null}
                title={changed ? "Save first" : undefined}
              >
                {testing === channel.id ? <IconLoader2 className="animate-spin" /> : <IconBell />} Test
              </Button>
            </div>
            <p className="text-muted-foreground text-xs">{channel.help}</p>
            <div className="grid gap-2 sm:grid-cols-2">
              {channel.fields.map((field) => (
                <div key={field.key} className={field.multiline ? "flex flex-col gap-1 sm:col-span-2" : "flex flex-col gap-1"}>
                  <Label htmlFor={field.key} className="text-xs">
                    {field.label}
                    {field.secret && <SecretState info={values[field.key]} cleared={draft[field.key] === null} />}
                  </Label>
                  <div className="flex gap-1.5">
                    {field.multiline ? (
                      <Textarea
                        id={field.key}
                        className="min-h-16 text-xs"
                        placeholder={isSecretInfo(values[field.key]) ? "Replace…" : "https://hooks.slack.com/services/…"}
                        value={typeof draft[field.key] === "string" ? (draft[field.key] as string) : ""}
                        onChange={(e) => set(field.key, e.target.value === "" ? undefined : e.target.value)}
                        disabled={!loaded}
                      />
                    ) : (
                      <Input
                        id={field.key}
                        type={field.secret ? "password" : "text"}
                        autoComplete={field.secret ? "new-password" : "off"}
                        className="h-8 text-xs"
                        placeholder={field.secret ? (isSecretInfo(values[field.key]) ? "Replace…" : "Enter…") : field.placeholder}
                        value={field.secret ? (typeof draft[field.key] === "string" ? (draft[field.key] as string) : "") : text(field.key)}
                        onChange={(e) => set(field.key, field.secret && e.target.value === "" ? undefined : e.target.value)}
                        disabled={!loaded}
                      />
                    )}
                    {field.secret && isSecretInfo(values[field.key]) && draft[field.key] !== null && (
                      <Button variant="ghost" size="sm" className="h-8" onClick={() => set(field.key, null)}>
                        Clear
                      </Button>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        ))}
        <div className="flex flex-wrap gap-2 border-t pt-4">
          <Button size="sm" onClick={() => void save()} disabled={!loaded || saving || !changed}>
            {saving && <IconLoader2 className="animate-spin" />} Save
          </Button>
          <Button size="sm" variant="outline" onClick={() => void test()} disabled={configured.length === 0 || changed || testing !== null}>
            {testing === "all" ? <IconLoader2 className="animate-spin" /> : <IconBell />} Send a test alert
          </Button>
        </div>
      </CardContent>
    </Card>
  )
}

function SecretState({ info, cleared }: { info: unknown; cleared: boolean }) {
  if (cleared) return <span className="text-status-warning ml-2 font-normal">will be cleared</span>
  if (!isSecretInfo(info)) return <span className="text-muted-foreground ml-2 font-normal">not set</span>
  return (
    <span className="text-muted-foreground ml-2 inline-flex items-center gap-1 font-normal" title={`Set by ${info.updated_by}`}>
      <IconKey className="size-3" /> set
    </span>
  )
}

/** One toast for the whole test. */
function report(deliveries: AlertDelivery[]) {
  const failed = deliveries.filter((d) => !d.ok)
  if (failed.length === 0) {
    toast.success(`Test alert sent to ${deliveries.map((d) => d.channel).join(", ")}.`)
  } else {
    toast.error(failed.map((d) => `${d.channel}: ${d.error ?? "failed"}`).join("\n"))
  }
}
