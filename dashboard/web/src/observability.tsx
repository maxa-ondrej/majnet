import { useState } from 'react'
import { Activity, AlertTriangle, Clock, MemoryStick, ListTree, ScrollText, ChevronRight } from 'lucide-react'
import {
  useObsOverview, useObsLogs, useObsTrace,
  type ContainerMetric, type ObsSpan, type ObsTrace,
} from './api'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'

// Grafana lives on the tailnet as an internal service in the majnet project
// (ADR 0023). The public/tailnet host is fixed, like the Adminer deep-link.
const GRAFANA = 'https://grafana.majnet.majksa.net'

const fmtMs = (ms: number) => (ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${Math.round(ms)} ms`)
const fmtWhen = (unixNano: number) => {
  const secs = (Date.now() - unixNano / 1e6) / 1000
  if (secs < 60) return `${Math.max(0, Math.round(secs))}s ago`
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`
  return `${Math.round(secs / 3600)}h ago`
}

// Stable-ish color per service name for waterfall bars + log service labels.
function svcColor(s: string): string {
  let h = 0
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) % 360
  return `hsl(${h} 55% 50%)`
}

function Tile({ icon, label, value, unit, source, tone }: {
  icon: React.ReactNode; label: string; value: string; unit?: string
  source: string; tone?: 'err'
}) {
  return (
    <Card>
      <CardContent className="flex flex-col gap-1 pt-5">
        <div className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">{icon}{label}</div>
        <div className={`text-2xl font-semibold tabular-nums tracking-tight ${tone === 'err' ? 'text-destructive' : ''}`}>
          {value}{unit && <span className="ml-1 text-sm font-medium text-muted-foreground">{unit}</span>}
        </div>
        <span className="mt-0.5 self-start rounded-full bg-muted px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-muted-foreground">
          {source}
        </span>
      </CardContent>
    </Card>
  )
}

/** The per-app Observability tab (ADR 0023 phase 3): RED tiles + traces⇄logs. */
export function Observability({ org, app, cls, containers }: {
  org: string; app: string; cls: string; containers: ContainerMetric[]
}) {
  const [tab, setTab] = useState<'traces' | 'logs'>('traces')
  const ov = useObsOverview(org, cls, app, true)
  const logs = useObsLogs(org, cls, app, tab === 'logs')

  const mem = containers.reduce((s, c) => s + c.mem_used, 0)
  const memLimit = containers[0]?.mem_limit ?? 0
  const red = ov.data?.red

  // The reconciler returns 503 when Tempo/Loki aren't configured yet.
  const unconfigured = (ov.error as { status?: number } | null)?.status === 503
    || (ov.error && String(ov.error).includes('503'))

  return (
    <>
      <div className="mb-3 mt-8 flex items-baseline gap-2.5">
        <h2 className="text-sm font-semibold">Observability</h2>
        <span className="text-xs text-muted-foreground">{cls} · last 15 min · traces &amp; logs from OpenTelemetry</span>
        <Button asChild variant="outline" size="sm" className="ml-auto">
          <a href={GRAFANA} target="_blank" rel="noreferrer">Open in Grafana ↗</a>
        </Button>
      </div>

      {unconfigured ? (
        <Card><CardContent className="py-10 text-center text-sm text-muted-foreground">
          Telemetry backend not reachable yet — the platform has no Tempo/Loki endpoint configured.
        </CardContent></Card>
      ) : (
        <>
          {/* golden signals */}
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <Tile icon={<Activity className="size-3.5" />} label="Request rate"
              value={red ? red.rate_per_min.toFixed(1) : '—'} unit="req/min" source="from traces" />
            <Tile icon={<AlertTriangle className="size-3.5" />} label="Error rate"
              value={red ? red.error_pct.toFixed(1) : '—'} unit="%" source="from traces"
              tone={red && red.error_pct > 0 ? 'err' : undefined} />
            <Tile icon={<Clock className="size-3.5" />} label="p95 latency"
              value={red ? String(red.p95_ms) : '—'} unit="ms" source="from traces" />
            <Tile icon={<MemoryStick className="size-3.5" />} label="Memory"
              value={containers.length ? Math.round(mem / 1e6).toString() : '—'}
              unit={memLimit ? `/ ${Math.round(memLimit / 1e6)} MiB` : 'MiB'} source="native · reconciler" />
          </div>

          {/* traces / logs */}
          <Card className="mt-3.5">
            <div className="flex items-center gap-2 border-b px-4 py-2.5">
              <div className="inline-flex gap-0.5 rounded-lg bg-muted p-0.5">
                <button onClick={() => setTab('traces')}
                  className={`inline-flex items-center gap-1.5 rounded-md px-3 py-1 text-[13px] font-medium ${tab === 'traces' ? 'bg-card shadow-sm' : 'text-muted-foreground'}`}>
                  <ListTree className="size-3.5" /> Traces
                </button>
                <button onClick={() => setTab('logs')}
                  className={`inline-flex items-center gap-1.5 rounded-md px-3 py-1 text-[13px] font-medium ${tab === 'logs' ? 'bg-card shadow-sm' : 'text-muted-foreground'}`}>
                  <ScrollText className="size-3.5" /> Logs
                </button>
              </div>
              {red?.capped && <span className="text-[11px] text-muted-foreground">sampled {red.sampled} — rate is a lower bound</span>}
            </div>
            <CardContent className="p-2">
              {tab === 'traces'
                ? <TracesPanel traces={ov.data?.traces ?? []} loading={ov.isLoading} error={!!ov.error && !unconfigured} />
                : <LogsPanel org={org} app={app} cls={cls} logs={logs} />}
            </CardContent>
          </Card>
        </>
      )}
    </>
  )
}

function TracesPanel({ traces, loading, error }: { traces: ObsTrace[]; loading: boolean; error: boolean }) {
  const [open, setOpen] = useState<string | null>(null)
  if (loading && !traces.length) return <Empty>Loading traces…</Empty>
  if (error) return <Empty>Couldn’t reach Tempo.</Empty>
  if (!traces.length) return <Empty>No traces in the last 15 minutes.</Empty>
  const max = Math.max(1, ...traces.map((t) => t.duration_ms))
  return (
    <div className="flex flex-col">
      {traces.map((t) => (
        <div key={t.trace_id}>
          <button onClick={() => setOpen(open === t.trace_id ? null : t.trace_id)}
            className="grid w-full grid-cols-[16px_1fr_120px_72px_64px] items-center gap-3 rounded-md px-2 py-2 text-left text-[13px] hover:bg-muted/60">
            <ChevronRight className={`size-4 text-muted-foreground transition-transform ${open === t.trace_id ? 'rotate-90' : ''}`} />
            <span className="truncate font-medium">{t.root_name || '(root)'}</span>
            <span className="h-1.5 overflow-hidden rounded bg-muted">
              <span className="block h-full rounded" style={{ width: `${(t.duration_ms / max) * 100}%`, background: t.error ? 'var(--destructive)' : svcColor(t.root_service) }} />
            </span>
            <span className={`text-right font-mono tabular-nums ${t.error ? 'text-destructive' : ''}`}>{fmtMs(t.duration_ms)}</span>
            <span className="text-right text-xs text-muted-foreground tabular-nums">{fmtWhen(t.start_unix_nano)}</span>
          </button>
          {open === t.trace_id && <Waterfall traceId={t.trace_id} />}
        </div>
      ))}
    </div>
  )
}

function Waterfall({ traceId }: { traceId: string }) {
  const q = useObsTrace(traceId)
  if (q.isLoading) return <div className="px-8 py-3 text-xs text-muted-foreground">Loading spans…</div>
  if (q.error || !q.data) return <div className="px-8 py-3 text-xs text-muted-foreground">Couldn’t load the trace.</div>
  const total = Math.max(1, q.data.duration_ms)
  return (
    <div className="mx-2 mb-2 overflow-x-auto rounded-md border bg-muted/30">
      <div className="min-w-[560px] py-1.5">
        {q.data.spans.map((s: ObsSpan, i) => (
          <div key={`${s.span_id}-${i}`} className="grid grid-cols-[240px_1fr] items-center gap-3 px-3 py-1 text-xs hover:bg-muted/50">
            <span className="flex items-center gap-1.5 truncate" style={{ paddingLeft: `${Math.min(s.depth, 8) * 12}px` }}>
              <span className="size-2 shrink-0 rounded-sm" style={{ background: s.error ? 'var(--destructive)' : svcColor(s.service) }} />
              <span className="truncate" title={`${s.service} · ${s.name}`}>{s.name}</span>
            </span>
            <span className="relative h-4">
              <span className="absolute top-0.5 flex h-3 items-center rounded px-1 font-mono text-[10px] text-white"
                style={{ left: `${(s.start_offset_ms / total) * 100}%`, width: `max(3px, ${(s.duration_ms / total) * 100}%)`, background: s.error ? 'var(--destructive)' : svcColor(s.service) }}>
                {s.duration_ms >= total * 0.12 ? fmtMs(s.duration_ms) : ''}
              </span>
            </span>
          </div>
        ))}
        <div className="flex justify-between px-3 pt-1.5 font-mono text-[10px] text-muted-foreground">
          <span>0</span><span>{fmtMs(total)}</span>
        </div>
      </div>
    </div>
  )
}

function LogsPanel({ logs }: {
  org: string; app: string; cls: string
  logs: ReturnType<typeof useObsLogs>
}) {
  if (logs.isLoading && !logs.data) return <Empty>Loading logs…</Empty>
  if (logs.error) return <Empty>Couldn’t reach Loki.</Empty>
  if (!logs.data?.length) return <Empty>No logs in the last 15 minutes.</Empty>
  const lvlColor = (l: string) =>
    l.startsWith('err') ? 'text-destructive' : l.startsWith('warn') ? 'text-amber-600 dark:text-amber-500' : 'text-muted-foreground'
  return (
    <div className="max-h-[420px] overflow-y-auto font-mono text-xs">
      {logs.data.map((r, i) => (
        <div key={i} className="grid grid-cols-[64px_48px_1fr_auto] items-baseline gap-2.5 rounded px-2 py-0.5 hover:bg-muted/60">
          <span className="text-muted-foreground/70 tabular-nums">{new Date(r.ts_unix_nano / 1e6).toLocaleTimeString()}</span>
          <span className={`font-semibold uppercase ${lvlColor(r.level)}`}>{r.level.slice(0, 4)}</span>
          <span className="truncate" title={r.msg}>{r.msg}</span>
          {r.trace_id
            ? <span className="text-[11px] text-primary/80" title={r.trace_id}>trace {r.trace_id.slice(0, 6)}…</span>
            : <span />}
        </div>
      ))}
    </div>
  )
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="px-3 py-10 text-center text-sm text-muted-foreground">{children}</div>
}
