import 'react-grid-layout/css/styles.css'
import 'react-resizable/css/styles.css'
import { useEffect, useRef, useState } from 'react'
import { Responsive, WidthProvider, type Layout, type Layouts } from 'react-grid-layout'
import { useQueries } from '@tanstack/react-query'
import { Link } from '@tanstack/react-router'
import { Boxes, Check, Cpu, EyeOff, GripVertical, Layers, Pencil, Plus, RotateCcw, Server } from 'lucide-react'
import {
  getJSON, parseAt, urls,
  useAlertSettings, useBotEvents, useControlPlane, useDashboardLayout, useEvents, useMetricsHistory, useNodeMetrics, useProjects, useWhoami,
  saveDashboardLayout, type DashboardLayout, type DeployPr, type Event, type NodeMetrics,
} from './api'
import { classify, DOT_TONE, PageHead, relTime } from './views'
import { StatusBadge } from './ui'
import { Button } from '@/components/ui/button'

const Grid = WidthProvider(Responsive)

// ── widget bodies (each owns its own hooks; only mounted while visible) ────────

function StatTile({ icon: Icon, label, value, unit, foot }: {
  icon: React.ComponentType<{ className?: string }>; label: string
  value: React.ReactNode; unit?: string; foot?: React.ReactNode
}) {
  return (
    <div className="flex h-full flex-col justify-center">
      <div className="flex items-center gap-2 text-xs font-medium text-muted-foreground"><Icon className="size-3.5 opacity-70" /> {label}</div>
      <div className="mt-1 text-3xl font-bold tracking-tight tabular-nums">{value}{unit && <span className="ml-1 text-base font-medium text-muted-foreground">{unit}</span>}</div>
      {foot && <div className="mt-0.5 text-xs text-muted-foreground">{foot}</div>}
    </div>
  )
}

function ProjectsTile() {
  const p = useProjects()
  const names = (p.data ?? []).map((x) => x.name).slice(0, 3).join(' · ')
  return <StatTile icon={Boxes} label="Projects" value={p.data?.length ?? '…'} foot={names || ' '} />
}
function AppsTile() {
  const p = useProjects()
  const total = (p.data ?? []).reduce((n, x) => n + (x.apps ?? 0), 0)
  return <StatTile icon={Layers} label="Apps" value={p.isLoading ? '…' : total} foot={`across ${p.data?.length ?? 0} projects`} />
}
function ContainersTile() {
  const m = useNodeMetrics()
  const running = (m.data ?? []).reduce((n, x) => n + (x.containers_running ?? 0), 0)
  const nodes = (m.data ?? []).length
  return <StatTile icon={Cpu} label="Containers" value={m.isLoading ? '…' : running} unit="running" foot={`across ${nodes} node${nodes === 1 ? '' : 's'}`} />
}
function NodesTile() {
  const m = useNodeMetrics()
  const total = (m.data ?? []).length
  const online = (m.data ?? []).filter((n) => n.reachable).length
  const allUp = total > 0 && online === total
  return (
    <StatTile icon={Server} label="Nodes" value={m.isLoading ? '…' : online} unit={total ? `/${total} online` : ''}
      foot={total ? <StatusBadge tone={allUp ? 'success' : 'warn'} dot>{allUp ? 'all reachable' : `${total - online} unreachable`}</StatusBadge> : ' '} />
  )
}

// Compact sparkline (2px line + area + endpoint), colored by the current value
// against the 80% threshold. Single-series magnitude → one hue.
function Spark({ label, values, cur }: { label: string; values: number[]; cur: number }) {
  const tone = cur >= 90 ? 'var(--destructive)' : cur >= 80 ? 'var(--warning)' : 'var(--success)'
  const W = 120, H = 24, pad = 2
  const n = values.length
  const x = (i: number) => (i / Math.max(1, n - 1)) * W
  const y = (v: number) => pad + (1 - Math.min(Math.max(v, 0), 100) / 100) * (H - pad * 2)
  const line = n >= 2 ? values.map((v, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)} ${y(v).toFixed(1)}`).join(' ') : ''
  return (
    <div className="grid grid-cols-[34px_1fr_38px] items-center gap-2.5">
      <span className="text-[10.5px] font-medium uppercase tracking-wide text-muted-foreground">{label}</span>
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="block h-6 w-full">
        <line x1={0} y1={y(80)} x2={W} y2={y(80)} stroke="var(--border)" strokeWidth={1} strokeDasharray="2 2" vectorEffect="non-scaling-stroke" />
        {n >= 2 && <>
          <path d={`${line} L${W} ${H} L0 ${H} Z`} fill={tone} fillOpacity={0.12} />
          <path d={line} fill="none" stroke={tone} strokeWidth={2} strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
          <circle cx={x(n - 1)} cy={y(values[n - 1] ?? 0)} r={2.4} fill={tone} />
        </>}
      </svg>
      <span className="text-right text-xs tabular-nums" style={{ color: cur >= 80 ? tone : 'var(--muted-foreground)' }}>{Math.round(cur)}%</span>
    </div>
  )
}

function FleetWidget() {
  const m = useNodeMetrics()
  const hist = useMetricsHistory(21600) // 6h sparklines
  const nodes = m.data ?? []
  if (m.isLoading) return <Muted>Loading…</Muted>
  if (!nodes.length) return <Muted>No nodes reporting metrics.</Muted>
  return (
    <div className="flex flex-col gap-3">
      {nodes.map((n: NodeMetrics) => {
        const memPct = n.mem_total ? (n.mem_used / n.mem_total) * 100 : 0
        const pts = (hist.data ?? []).filter((p) => p.node === n.name)
        const cpuSeries = pts.map((p) => p.cpu_pct)
        const memSeries = pts.map((p) => (p.mem_total ? (p.mem_used / p.mem_total) * 100 : 0))
        return (
          <div key={n.name} className="border-t pt-3 first:border-0 first:pt-0">
            <div className="mb-1.5 flex items-center gap-2">
              <span className="text-sm font-semibold">{n.name}</span>
              <span className="text-[11px] text-muted-foreground">{ZONE[n.role] ?? n.role}</span>
              <span className="ml-auto">{n.reachable ? <StatusBadge tone="success" dot>online</StatusBadge> : <StatusBadge tone="danger" dot>unreachable</StatusBadge>}</span>
            </div>
            {n.reachable && <div className="flex flex-col gap-1.5">
              <Spark label="CPU" values={cpuSeries} cur={n.host_cpu_pct} />
              <Spark label="MEM" values={memSeries} cur={memPct} />
            </div>}
          </div>
        )
      })}
    </div>
  )
}

function DeploymentsWidget() {
  const projects = useProjects()
  const onboarded = (projects.data ?? []).filter((p) => p.onboarded)
  const results = useQueries({
    queries: onboarded.map((p) => ({ queryKey: ['deploys', p.org], queryFn: () => getJSON<DeployPr[]>(urls.deploys(p.org)) })),
  })
  const pending = onboarded.flatMap((p, i) => (results[i]?.data ?? []).map((pr) => ({ org: p.org, name: p.name, pr })))
  const events = useEvents()
  const [, tick] = useState(0)
  useEffect(() => { const id = setInterval(() => tick((n) => n + 1), 10_000); return () => clearInterval(id) }, [])
  const now = Date.now()
  const deploying = (events.data ?? []).filter((e) => e.result.startsWith('deployed') && now - parseAt(e.at) < 90_000)

  if (!deploying.length && !pending.length) return <Empty>Everything is in sync.</Empty>
  return (
    <div className="flex flex-col gap-3 text-sm">
      {deploying.length > 0 && <div>
        <Label>Deploying now</Label>
        {deploying.map((e, i) => <Line key={i} dot="var(--primary)" title={e.project} sub={`${e.node} · ${e.result}`} badge={<StatusBadge tone="accent">deploying</StatusBadge>} />)}
      </div>}
      {pending.length > 0 && <div>
        <Label>Pending review</Label>
        {pending.map(({ org, name, pr }) => (
          <Link key={`${org}-${pr.number}`} to="/projects/$org/deploys" params={{ org }} className="block">
            <Line dot="var(--warning)" title={name} sub={`${pr.class} · render PR #${pr.number}`} badge={<StatusBadge tone="warn">review</StatusBadge>} />
          </Link>
        ))}
      </div>}
    </div>
  )
}

function ControlPlaneWidget() {
  const cp = useControlPlane()
  const d = cp.data
  const status = !d ? null
    : !d.up_to_date ? <StatusBadge tone="warn" dot>update available</StatusBadge>
    : d.converged === false ? <StatusBadge tone="accent" dot>rolling out</StatusBadge>
    : <StatusBadge tone="success" dot>up to date</StatusBadge>
  return (
    <div className="flex flex-col gap-2 text-sm">
      <Row k="Status">{cp.isLoading ? '…' : status}</Row>
      <Row k="Pinned"><code className="font-mono text-xs">{d?.current.ref?.slice(0, 7) ?? '—'}</code></Row>
      <Row k="Update"><Link to="/control-plane" className="text-xs text-primary hover:underline">Open control plane →</Link></Row>
    </div>
  )
}

function AlertsWidget() {
  const a = useAlertSettings()
  const m = useNodeMetrics()
  const cpuThr = a.data?.cpu_pct ?? 90
  const memThr = a.data?.mem_pct ?? 90
  const nodes = (m.data ?? []).filter((n) => n.reachable)
  const over = nodes.filter(
    (n) => n.host_cpu_pct >= cpuThr || (n.mem_total > 0 && (n.mem_used / n.mem_total) * 100 >= memThr),
  )
  return (
    <div className="flex flex-col gap-2 text-sm">
      <Row k="Status">
        {m.isLoading ? '…' : over.length
          ? <StatusBadge tone="warn" dot>{over.length} node{over.length === 1 ? '' : 's'} over threshold</StatusBadge>
          : <StatusBadge tone="success" dot>all nominal</StatusBadge>}
      </Row>
      <Row k="Discord">
        {a.isLoading ? '…' : a.data?.webhook_set
          ? <StatusBadge tone="success">configured</StatusBadge>
          : <span className="text-muted-foreground">not set</span>}
      </Row>
      <Row k="Thresholds"><span className="text-muted-foreground tabular-nums">CPU {cpuThr}% · MEM {memThr}%</span></Row>
      {over.length > 0 && <div className="text-xs text-muted-foreground">Over: {over.map((n) => n.name).join(', ')}</div>}
    </div>
  )
}

function ActivityWidget() {
  const recon = useEvents(60)
  const bot = useBotEvents()
  const events = [...(recon.data ?? []), ...(bot.data ?? [])].sort((a, b) => parseAt(b.at) - parseAt(a.at)).slice(0, 8)
  if (!events.length) return <Muted>No recent activity.</Muted>
  return (
    <div className="flex flex-col">
      {events.map((e: Event, i) => {
        const c = classify(e)
        return (
          <div key={i} className="flex items-center gap-2.5 border-t py-2 text-[13px] first:border-0">
            <span className={`size-1.5 shrink-0 rounded-full border ${DOT_TONE[c.tone]}`} style={{ background: 'currentColor' }} />
            <span className="min-w-0 flex-1 truncate">{c.line}</span>
            <span className="shrink-0 text-[11px] text-muted-foreground">{relTime(e.at)}</span>
          </div>
        )
      })}
    </div>
  )
}

// small shared bits
const ZONE: Record<string, string> = { main: 'control plane', prod: 'public', private: 'internal' }
const Muted = ({ children }: { children: React.ReactNode }) => <div className="grid h-full place-items-center text-sm text-muted-foreground">{children}</div>
const Empty = ({ children }: { children: React.ReactNode }) => <div className="flex items-center gap-2 text-sm text-muted-foreground"><Check className="size-4 text-success" />{children}</div>
const Label = ({ children }: { children: React.ReactNode }) => <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{children}</div>
const Row = ({ k, children }: { k: string; children: React.ReactNode }) => <div className="flex items-baseline gap-3"><span className="min-w-20 text-muted-foreground">{k}</span><span>{children}</span></div>
function Line({ dot, title, sub, badge }: { dot: string; title: string; sub: string; badge: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2.5 border-t py-2 first:border-0">
      <span className="size-1.5 shrink-0 rounded-full" style={{ background: dot }} />
      <div className="min-w-0 flex-1"><span className="font-medium">{title}</span><div className="truncate text-[11.5px] text-muted-foreground">{sub}</div></div>
      {badge}
    </div>
  )
}

// ── widget registry + default layout ──────────────────────────────────────────

type WidgetDef = { id: string; title: string; Body: React.FC; adminOnly?: boolean; noPad?: boolean }
const WIDGETS: WidgetDef[] = [
  { id: 'projects', title: 'Projects', Body: ProjectsTile },
  { id: 'apps', title: 'Apps', Body: AppsTile },
  { id: 'containers', title: 'Containers', Body: ContainersTile },
  { id: 'nodes', title: 'Nodes', Body: NodesTile },
  { id: 'fleet', title: 'Fleet health', Body: FleetWidget },
  { id: 'deployments', title: 'Deployments', Body: DeploymentsWidget },
  { id: 'controlplane', title: 'Control plane', Body: ControlPlaneWidget, adminOnly: true },
  { id: 'alerts', title: 'Alerts', Body: AlertsWidget },
  { id: 'activity', title: 'Recent activity', Body: ActivityWidget },
]
// Two balanced columns that fill the viewport: a tall Fleet + Activity carry the
// content, Deployments/Control-plane/Alerts fill the gaps. Both columns bottom
// out around the same row so there's no dead space below.
const DEFAULT_LG: Layout[] = [
  { i: 'projects', x: 0, y: 0, w: 3, h: 1, minW: 2, minH: 1 },
  { i: 'apps', x: 3, y: 0, w: 3, h: 1, minW: 2, minH: 1 },
  { i: 'containers', x: 6, y: 0, w: 3, h: 1, minW: 2, minH: 1 },
  { i: 'nodes', x: 9, y: 0, w: 3, h: 1, minW: 2, minH: 1 },
  { i: 'fleet', x: 0, y: 1, w: 6, h: 4, minW: 3, minH: 2 },
  { i: 'deployments', x: 6, y: 1, w: 6, h: 2, minW: 3, minH: 2 },
  { i: 'activity', x: 6, y: 3, w: 6, h: 4, minW: 3, minH: 2 },
  { i: 'controlplane', x: 0, y: 5, w: 3, h: 2, minW: 3, minH: 2 },
  { i: 'alerts', x: 3, y: 5, w: 3, h: 2, minW: 3, minH: 2 },
]
const DEFAULT_BY_ID = Object.fromEntries(DEFAULT_LG.map((l) => [l.i, l]))

function WidgetShell({ title, editing, onHide, noPad, children }: {
  title: string; editing: boolean; onHide: () => void; noPad?: boolean; children: React.ReactNode
}) {
  return (
    <div className="relative flex h-full flex-col overflow-hidden rounded-xl border bg-card">
      <div className={`min-h-0 flex-1 overflow-auto ${noPad ? 'px-4 py-3' : 'p-4'}`}>{children}</div>
      {/* In edit mode the drag bar floats OVER the content (absolute) rather than
          stealing height, so widgets don't lose a row while you're arranging. */}
      {editing && (
        <div className="drag-handle absolute inset-x-0 top-0 z-10 flex cursor-move items-center gap-2 border-b bg-card/80 px-3 py-1.5 backdrop-blur-sm">
          <GripVertical className="size-3.5 text-muted-foreground" />
          <span className="text-xs font-medium">{title}</span>
          <button onClick={onHide} title="Hide" className="ml-auto rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground"><EyeOff className="size-3.5" /></button>
        </div>
      )}
    </div>
  )
}

export function Overview() {
  const admin = useWhoami().data?.admin ?? false
  const saved = useDashboardLayout()
  const widgets = WIDGETS.filter((w) => admin || !w.adminOnly)

  const [editing, setEditing] = useState(false)
  const [layouts, setLayouts] = useState<Layouts | null>(null)
  const [hidden, setHidden] = useState<string[]>([])
  const seeded = useRef(false)
  const saveTimer = useRef<ReturnType<typeof setTimeout>>(undefined)

  useEffect(() => {
    if (seeded.current || saved.isLoading) return
    seeded.current = true
    const s = saved.data
    setLayouts((s?.layouts as unknown as Layouts) ?? { lg: DEFAULT_LG })
    setHidden(s?.hidden ?? [])
  }, [saved.isLoading, saved.data])

  const persist = (l: Layouts, h: string[]) => {
    saveDashboardLayout({ layouts: l as unknown as DashboardLayout['layouts'], hidden: h }).catch(() => {})
  }
  const onLayoutChange = (_: Layout[], all: Layouts) => {
    setLayouts(all)
    if (!editing) return
    clearTimeout(saveTimer.current)
    saveTimer.current = setTimeout(() => persist(all, hidden), 700)
  }
  const hide = (id: string) => { const h = [...hidden, id]; setHidden(h); if (layouts) persist(layouts, h) }
  const show = (id: string) => { const h = hidden.filter((x) => x !== id); setHidden(h); if (layouts) persist(layouts, h) }
  const reset = () => { setLayouts({ lg: DEFAULT_LG }); setHidden([]); persist({ lg: DEFAULT_LG }, []) }
  const toggleEdit = () => { if (editing && layouts) persist(layouts, hidden); setEditing((e) => !e) }

  if (!layouts) return <PageHead title="Overview" />

  const visible = widgets.filter((w) => !hidden.includes(w.id))
  const hiddenWidgets = widgets.filter((w) => hidden.includes(w.id))

  return (
    <>
      <style>{`
        .react-grid-item.react-grid-placeholder { background: var(--primary); opacity: 0.14; border-radius: 12px; }
        .react-grid-item > .react-resizable-handle::after { border-color: var(--muted-foreground); opacity: 0.5; }
        /* Make the edge (width/height) handles findable, not just the corner. */
        .react-grid-item > .react-resizable-handle-e,
        .react-grid-item > .react-resizable-handle-w,
        .react-grid-item > .react-resizable-handle-s { background-image: none; }
        .react-grid-item > .react-resizable-handle-e::before,
        .react-grid-item > .react-resizable-handle-w::before,
        .react-grid-item > .react-resizable-handle-s::before {
          content: ''; position: absolute; border-radius: 999px;
          background: var(--muted-foreground); opacity: 0; transition: opacity .12s;
        }
        .react-grid-item > .react-resizable-handle-e::before { top: 50%; right: 2px; width: 3px; height: 26px; transform: translateY(-50%); }
        .react-grid-item > .react-resizable-handle-w::before { top: 50%; left: 2px; width: 3px; height: 26px; transform: translateY(-50%); }
        .react-grid-item > .react-resizable-handle-s::before { left: 50%; bottom: 2px; height: 3px; width: 26px; transform: translateX(-50%); }
        .react-grid-item:hover > .react-resizable-handle-e::before,
        .react-grid-item:hover > .react-resizable-handle-w::before,
        .react-grid-item:hover > .react-resizable-handle-s::before { opacity: 0.45; }
      `}</style>
      <PageHead title="Overview" sub={editing ? 'Drag to reorder · drag a corner to resize · hide with the eye' : 'Your at-a-glance view of the platform'}>
        {editing && <Button variant="ghost" size="sm" onClick={reset}><RotateCcw className="size-4" /> Reset</Button>}
        <Button variant={editing ? 'default' : 'outline'} size="sm" onClick={toggleEdit}>
          {editing ? <><Check className="size-4" /> Done</> : <><Pencil className="size-4" /> Customize</>}
        </Button>
      </PageHead>

      <Grid
        className={editing ? 'rounded-lg outline-dashed outline-1 outline-border' : ''}
        layouts={layouts}
        breakpoints={{ lg: 1100, md: 800, sm: 500, xs: 0 }}
        cols={{ lg: 12, md: 12, sm: 6, xs: 2 }}
        rowHeight={94}
        margin={[14, 14]}
        containerPadding={[0, 0]}
        isDraggable={editing}
        isResizable={editing}
        // East + west (width) and south (height) handles alongside the corner, so
        // horizontal resize is its own gesture in EITHER direction — a widget at
        // the right grid edge grows leftward via its west handle. preventCollision
        // =false lets a widening widget push its neighbour aside instead of block.
        resizeHandles={['e', 'w', 's', 'se']}
        preventCollision={false}
        compactType="vertical"
        draggableHandle=".drag-handle"
        onLayoutChange={onLayoutChange}
      >
        {visible.map((w) => {
          // `data-grid` overrides the controlled `layouts` prop in RGL's
          // synchronizeLayoutWithChildren — set it ONLY for widgets missing from
          // the current layout (else every drag/resize snaps back to it).
          const known = (layouts.lg ?? []).some((l) => l.i === w.id)
          return (
            <div key={w.id} {...(known ? {} : { 'data-grid': DEFAULT_BY_ID[w.id] })}>
              <WidgetShell title={w.title} editing={editing} onHide={() => hide(w.id)}><w.Body /></WidgetShell>
            </div>
          )
        })}
      </Grid>

      {editing && hiddenWidgets.length > 0 && (
        <div className="mt-4 rounded-xl border border-dashed p-3">
          <div className="mb-2 text-xs font-medium text-muted-foreground">Hidden widgets</div>
          <div className="flex flex-wrap gap-2">
            {hiddenWidgets.map((w) => <Button key={w.id} variant="outline" size="sm" onClick={() => show(w.id)}><Plus className="size-4" /> {w.title}</Button>)}
          </div>
        </div>
      )}
    </>
  )
}
