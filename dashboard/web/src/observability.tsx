import { useEffect, useRef, useState } from 'react'
import { Activity, AlertTriangle, Clock, MemoryStick, ListTree, ScrollText, ChevronRight, Search } from 'lucide-react'
import {
  useObsOverview, useObsTrace, getJSON, urls,
  type AppSummary, type ContainerMetric, type ObsLog, type ObsSpan, type ObsTrace,
  type TraceFilters, type LogFilters,
} from './api'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { ENV_CLASSES, setEnv, useEnv, type EnvClass } from './env'

// Environment order for class pickers; filtered to the classes an app declares.
const orderClasses = (classes: string[]): string[] => ENV_CLASSES.filter((c) => classes.includes(c))

// Look-back windows for the RED tiles + trace/log lists.
const WINDOWS: { label: string; min: number }[] = [
  { label: '5m', min: 5 }, { label: '15m', min: 15 }, { label: '1h', min: 60 },
  { label: '6h', min: 360 }, { label: '24h', min: 1440 },
]
// Per-page sizes (below the reconciler's MAX_LIMIT); a full page implies "load
// more". Traces are heavy rows (each opens a waterfall) so we keep them short;
// logs are dense one-liners, so a fuller page reads better.
const TRACE_PAGE = 15
const LOG_PAGE = 100

/** Debounce a value so text filters don't refetch on every keystroke. */
function useDebounced<T>(value: T, ms = 400): T {
  const [v, setV] = useState(value)
  useEffect(() => {
    const id = setTimeout(() => setV(value), ms)
    return () => clearTimeout(id)
  }, [value, ms])
  return v
}

/** A cursor-paginated, live-refreshing list backed by a `before`-cursor endpoint.
 *  Page 1 auto-refreshes every 15 s until the user loads an older page (then it
 *  pauses so the appended pages aren't clobbered); changing `depsKey` (the filter
 *  identity) resets to a fresh page 1. */
function usePagedObs<T>(
  makeUrl: (before?: number) => string,
  keyOf: (r: T) => string,
  cursorOf: (r: T) => number,
  depsKey: string,
  pageSize: number,
) {
  const [rows, setRows] = useState<T[]>([])
  const [loading, setLoading] = useState(true)
  const [loadingMore, setLoadingMore] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const [hasMore, setHasMore] = useState(false)
  const [paged, setPaged] = useState(false)
  const urlRef = useRef(makeUrl)
  urlRef.current = makeUrl
  const rowsRef = useRef<T[]>([])
  rowsRef.current = rows

  // Fresh page 1 whenever the filters change.
  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    setPaged(false)
    getJSON<T[]>(urlRef.current(undefined))
      .then((d) => { if (!cancelled) { setRows(d); setHasMore(d.length >= pageSize) } })
      .catch((e) => { if (!cancelled) setError(e) })
      .finally(() => { if (!cancelled) setLoading(false) })
    return () => { cancelled = true }
  }, [depsKey])

  // Live refresh of page 1, paused once the user has paged deeper.
  useEffect(() => {
    if (paged) return
    const id = setInterval(() => {
      getJSON<T[]>(urlRef.current(undefined))
        .then((d) => { setRows(d); setHasMore(d.length >= pageSize); setError(null) })
        .catch(() => {})
    }, 15000)
    return () => clearInterval(id)
  }, [depsKey, paged])

  const loadMore = () => {
    const last = rowsRef.current[rowsRef.current.length - 1]
    if (!last || loadingMore) return
    setLoadingMore(true)
    setPaged(true)
    getJSON<T[]>(urlRef.current(cursorOf(last)))
      .then((more) => {
        setRows((prev) => {
          const seen = new Set(prev.map(keyOf))
          return [...prev, ...more.filter((r) => !seen.has(keyOf(r)))]
        })
        setHasMore(more.length >= pageSize)
      })
      .catch(setError)
      .finally(() => setLoadingMore(false))
  }

  return { rows, loading, loadingMore, error, hasMore, loadMore, live: !paged }
}

/** Compact "Load more" / end-of-list footer shared by both panels. */
function LoadMore({ hasMore, loading, onMore }: { hasMore: boolean; loading: boolean; onMore: () => void }) {
  if (!hasMore) return null
  return (
    <div className="flex justify-center py-2">
      <Button variant="outline" size="sm" onClick={onMore} disabled={loading}>
        {loading ? 'Loading…' : 'Load more'}
      </Button>
    </div>
  )
}

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

/** The per-app Observability tab (ADR 0023 phase 3): RED tiles + traces⇄logs.
 *  `control` renders an extra picker in the header (the project view injects an
 *  app switcher there so the whole section keeps a single heading). */
export function Observability({ org, app, cls, containers, control }: {
  org: string; app: string; cls: string; containers: ContainerMetric[]
  control?: React.ReactNode
}) {
  const [tab, setTab] = useState<'traces' | 'logs'>('traces')
  const [windowMin, setWindowMin] = useState(15)
  const ov = useObsOverview(org, cls, app, windowMin, true)

  const mem = containers.reduce((s, c) => s + c.mem_used, 0)
  const memLimit = containers[0]?.mem_limit ?? 0
  const red = ov.data

  // The reconciler returns 503 when Tempo/Loki aren't configured yet.
  const unconfigured = (ov.error as { status?: number } | null)?.status === 503
    || (ov.error && String(ov.error).includes('503'))
  const winLabel = WINDOWS.find((w) => w.min === windowMin)?.label ?? `${windowMin}m`

  return (
    <>
      <div className="mb-3 mt-8 flex flex-wrap items-center gap-2.5">
        <h2 className="text-sm font-semibold">Observability</h2>
        <span className="text-xs text-muted-foreground">{cls} · traces &amp; logs from OpenTelemetry</span>
        <div className="ml-auto flex items-center gap-2.5">
          <Select value={String(windowMin)} onValueChange={(v) => setWindowMin(Number(v))}>
            <SelectTrigger className="h-8 w-24 text-[13px]"><SelectValue /></SelectTrigger>
            <SelectContent>
              {WINDOWS.map((w) => <SelectItem key={w.min} value={String(w.min)}>last {w.label}</SelectItem>)}
            </SelectContent>
          </Select>
          {control}
          <Button asChild variant="outline" size="sm">
            <a href={GRAFANA} target="_blank" rel="noreferrer">Open in Grafana ↗</a>
          </Button>
        </div>
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
              value={red ? red.rate_per_min.toFixed(1) : '—'} unit="req/min" source={`last ${winLabel} · traces`} />
            <Tile icon={<AlertTriangle className="size-3.5" />} label="Error rate"
              value={red ? red.error_pct.toFixed(1) : '—'} unit="%" source={`last ${winLabel} · traces`}
              tone={red && red.error_pct > 0 ? 'err' : undefined} />
            <Tile icon={<Clock className="size-3.5" />} label="p95 latency"
              value={red ? String(red.p95_ms) : '—'} unit="ms" source={`last ${winLabel} · traces`} />
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
              {tab === 'traces' && red?.capped && (
                <span className="text-[11px] text-muted-foreground">RED sampled {red.sampled} — rate is a lower bound</span>
              )}
            </div>
            {tab === 'traces'
              ? <TracesPanel org={org} app={app} cls={cls} windowMin={windowMin} />
              : <LogsPanel org={org} app={app} cls={cls} windowMin={windowMin} />}
          </Card>
        </>
      )}
    </>
  )
}

/** Project-level Observability: the same RED tiles + traces/logs as the per-app
 *  tab, with a header switcher over the project's OTEL-enabled apps (and their
 *  environments). Renders nothing when no app in the project opts into OTEL. */
export function ProjectObservability({ org, apps, containersFor }: {
  org: string; apps: AppSummary[]
  containersFor: (app: string, cls: string) => ContainerMetric[]
}) {
  const otelApps = apps.filter((a) => a.otel)
  const [appName, setAppName] = useState(otelApps[0]?.name ?? '')
  const current = otelApps.find((a) => a.name === appName) ?? otelApps[0]
  const classes = current ? orderClasses(current.classes) : []
  // Class follows the global env store; clamp to a class the selected app has.
  const selected = useEnv()
  const env = classes.includes(selected) ? selected : (classes[0] ?? '')

  if (!current || !env) return null

  const control = (
    <div className="flex items-center gap-2">
      <Select value={current.name} onValueChange={setAppName}>
        <SelectTrigger className="h-8 w-40 text-[13px]"><SelectValue /></SelectTrigger>
        <SelectContent>
          {otelApps.map((a) => <SelectItem key={a.name} value={a.name}>{a.name}</SelectItem>)}
        </SelectContent>
      </Select>
      {classes.length > 1 && (
        <Select value={env} onValueChange={(v) => setEnv(v as EnvClass)}>
          <SelectTrigger className="h-8 w-32 text-[13px]"><SelectValue /></SelectTrigger>
          <SelectContent>
            {classes.map((c) => <SelectItem key={c} value={c}>{c}</SelectItem>)}
          </SelectContent>
        </Select>
      )}
    </div>
  )

  return (
    <Observability
      key={`${current.name}:${env}`}
      org={org} app={current.name} cls={env}
      containers={containersFor(current.name, env)} control={control} />
  )
}

function TracesPanel({ org, app, cls, windowMin }: {
  org: string; app: string; cls: string; windowMin: number
}) {
  const [open, setOpen] = useState<string | null>(null)
  const [status, setStatus] = useState<'all' | 'error' | 'ok'>('all')
  const [text, setText] = useState('')
  const q = useDebounced(text)
  const filters: TraceFilters = { windowMin, status, q: q || undefined, limit: TRACE_PAGE }
  const depsKey = `${org}|${cls}|${app}|${windowMin}|${status}|${q}`
  const { rows, loading, loadingMore, error, hasMore, loadMore } = usePagedObs<ObsTrace>(
    (before) => urls.obsTraces(org, cls, app, filters, before),
    (t) => t.trace_id,
    (t) => t.start_unix_nano,
    depsKey,
    TRACE_PAGE,
  )

  const max = Math.max(1, ...rows.map((t) => t.duration_ms))
  return (
    <>
      <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <Select value={status} onValueChange={(v) => setStatus(v as typeof status)}>
          <SelectTrigger className="h-8 w-32 text-[13px]"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All statuses</SelectItem>
            <SelectItem value="error">Errors only</SelectItem>
            <SelectItem value="ok">Success only</SelectItem>
          </SelectContent>
        </Select>
        <SearchInput value={text} onChange={setText} placeholder="Filter by operation…" />
      </div>
      <CardContent className="p-2">
        {loading && !rows.length ? <Empty>Loading traces…</Empty>
          : error ? <Empty>Couldn’t reach Tempo.</Empty>
          : !rows.length ? <Empty>No matching traces in this window.</Empty>
          : (
            <div className="flex flex-col">
              {rows.map((t) => (
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
              <LoadMore hasMore={hasMore} loading={loadingMore} onMore={loadMore} />
            </div>
          )}
      </CardContent>
    </>
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

function LogsPanel({ org, app, cls, windowMin }: {
  org: string; app: string; cls: string; windowMin: number
}) {
  const [level, setLevel] = useState<'all' | 'warn' | 'error'>('all')
  const [text, setText] = useState('')
  const [trace, setTrace] = useState('')
  const q = useDebounced(text)
  const traceId = useDebounced(trace)
  const filters: LogFilters = { windowMin, level, q: q || undefined, traceId: traceId || undefined, limit: LOG_PAGE }
  const depsKey = `${org}|${cls}|${app}|${windowMin}|${level}|${q}|${traceId}`
  const { rows, loading, loadingMore, error, hasMore, loadMore } = usePagedObs<ObsLog>(
    (before) => urls.obsLogs(org, cls, app, filters, before),
    (r) => `${r.ts_unix_nano}|${r.msg}`,
    (r) => r.ts_unix_nano,
    depsKey,
    LOG_PAGE,
  )

  const lvlColor = (l: string) =>
    l.startsWith('err') ? 'text-destructive' : l.startsWith('warn') ? 'text-amber-600 dark:text-amber-500' : 'text-muted-foreground'
  return (
    <>
      <div className="flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <Select value={level} onValueChange={(v) => setLevel(v as typeof level)}>
          <SelectTrigger className="h-8 w-28 text-[13px]"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="all">All levels</SelectItem>
            <SelectItem value="warn">Warn+</SelectItem>
            <SelectItem value="error">Errors</SelectItem>
          </SelectContent>
        </Select>
        <SearchInput value={text} onChange={setText} placeholder="Filter message…" />
        <Input value={trace} onChange={(e) => setTrace(e.target.value)} placeholder="trace_id"
          className="h-8 w-40 font-mono text-[13px]" />
      </div>
      <CardContent className="p-2">
        {loading && !rows.length ? <Empty>Loading logs…</Empty>
          : error ? <Empty>Couldn’t reach Loki.</Empty>
          : !rows.length ? <Empty>No matching logs in this window.</Empty>
          : (
            <div className="max-h-[460px] overflow-y-auto font-mono text-xs">
              {rows.map((r, i) => (
                <div key={i} className="grid grid-cols-[64px_48px_1fr_auto] items-baseline gap-2.5 rounded px-2 py-0.5 hover:bg-muted/60">
                  <span className="text-muted-foreground/70 tabular-nums">{new Date(r.ts_unix_nano / 1e6).toLocaleTimeString()}</span>
                  <span className={`font-semibold uppercase ${lvlColor(r.level)}`}>{r.level.slice(0, 4)}</span>
                  <span className="truncate" title={r.msg}>{r.msg}</span>
                  {r.trace_id
                    ? <button onClick={() => setTrace(r.trace_id)} className="text-[11px] text-primary/80 hover:underline" title={`Filter to ${r.trace_id}`}>trace {r.trace_id.slice(0, 6)}…</button>
                    : <span />}
                </div>
              ))}
              <LoadMore hasMore={hasMore} loading={loadingMore} onMore={loadMore} />
            </div>
          )}
      </CardContent>
    </>
  )
}

/** Small search input with a leading icon, shared by both filter bars. */
function SearchInput({ value, onChange, placeholder }: {
  value: string; onChange: (v: string) => void; placeholder: string
}) {
  return (
    <div className="relative">
      <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
      <Input value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder}
        className="h-8 w-52 pl-8 text-[13px]" />
    </div>
  )
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="px-3 py-10 text-center text-sm text-muted-foreground">{children}</div>
}
