// Node terminal page (#13, ADR 0016) — a live shell over a WebSocket to the
// reconciler. Two modes: host shell (pick a node) and container exec (deep-linked
// from an app's Exec button). Platform-admin only; every session is recorded.
import { useEffect, useMemo, useRef, useState } from 'react'
import { Terminal as Xterm } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { History, Info, Loader2, ServerCog, TerminalSquare, TriangleAlert } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Sheet, SheetBody, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet'
import { useApps, useNodeMetrics, useNodes, useProjects, useTerminalSessions, getText, terminalWsUrl, urls, type TerminalSession } from './api'
import { PageHead } from './views'
import { StatusBadge } from './ui'

// Strip ANSI/VT escape sequences so a recorded transcript reads as plain text.
const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, '').replace(/\x1b[=>]/g, '')

// Relative time from a SQLite UTC datetime ("YYYY-MM-DD HH:MM:SS").
function relTime(at: string): string {
  const t = Date.parse(at.replace(' ', 'T') + 'Z')
  if (Number.isNaN(t)) return at
  const s = Math.round((Date.now() - t) / 1000)
  if (s < 60) return 'just now'
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.round(m / 60)
  return h < 24 ? `${h}h ago` : `${Math.round(h / 24)}d ago`
}

type Mode = 'host' | 'container'
interface Target {
  mode: Mode
  node?: string
  project?: string
  app?: string
  class?: string
  label: string
  prod: boolean
  // What the operator must type to confirm a production session.
  confirmWord: string
}

function targetParams(t: Target): Record<string, string | undefined> {
  return t.mode === 'host'
    ? { mode: 'host', node: t.node }
    : { mode: 'container', project: t.project, app: t.app, class: t.class }
}

// ── xterm session ─────────────────────────────────────────────────────────────
function XtermSession({ target }: { target: Target }) {
  const host = useRef<HTMLDivElement>(null)
  const [status, setStatus] = useState<'connecting' | 'open' | 'closed'>('connecting')

  useEffect(() => {
    if (!host.current) return
    const term = new Xterm({
      fontFamily: 'ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      cursorBlink: true,
      theme: { background: '#181a20', foreground: '#d7dae0', cursor: '#8ab4f8' },
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    term.open(host.current)
    fit.fit()

    const ws = new WebSocket(terminalWsUrl(targetParams(target)))
    ws.binaryType = 'arraybuffer'
    const sendResize = () => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ resize: { cols: term.cols, rows: term.rows } }))
    }
    ws.onopen = () => { setStatus('open'); fit.fit(); sendResize(); term.focus() }
    ws.onmessage = (e) => { if (e.data instanceof ArrayBuffer) term.write(new Uint8Array(e.data)) }
    ws.onclose = () => { setStatus('closed'); term.write('\r\n\x1b[90m[session ended]\x1b[0m\r\n') }
    ws.onerror = () => setStatus('closed')

    const onData = term.onData((d) => { if (ws.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(d)) })
    const onResize = term.onResize(() => sendResize())
    const onWin = () => fit.fit()
    window.addEventListener('resize', onWin)

    return () => {
      window.removeEventListener('resize', onWin)
      onData.dispose(); onResize.dispose()
      ws.close(); term.dispose()
    }
  }, [target])

  return (
    <div className={`overflow-hidden rounded-lg border ${target.prod ? 'border-destructive/50 ring-1 ring-destructive/25' : ''}`}>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-b bg-card px-3 py-2">
        <StatusBadge tone="danger" dot>recording</StatusBadge>
        <span className="font-mono text-[13px]">{target.label}</span>
        <span className="ml-auto text-[11.5px] text-muted-foreground">
          {status === 'connecting' ? 'connecting…' : status === 'open' ? 'connected' : 'ended'}
        </span>
      </div>
      <div ref={host} className="h-[420px] w-full bg-[#181a20] p-2" />
    </div>
  )
}

// ── production confirm ────────────────────────────────────────────────────────
function ProdConfirm({ target, onConfirm, onCancel }: { target: Target; onConfirm: () => void; onCancel: () => void }) {
  const [typed, setTyped] = useState('')
  return (
    <div className="rounded-lg border border-destructive/40 bg-destructive/5 p-4">
      <div className="flex items-start gap-2.5">
        <TriangleAlert className="mt-0.5 size-5 shrink-0 text-destructive" />
        <div className="flex-1">
          <div className="text-sm font-semibold text-destructive">Production target</div>
          <p className="mt-1 text-[13px] leading-relaxed text-foreground/80">
            You’re about to open a live root shell on <b className="font-mono">{target.label}</b>. Commands run as
            root on production infrastructure — there is no undo, and the session is recorded. Type{' '}
            <b className="font-mono">{target.confirmWord}</b> to continue.
          </p>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <input
              value={typed} onChange={(e) => setTyped(e.target.value)} autoFocus autoComplete="off"
              placeholder={target.confirmWord}
              className="h-9 w-56 rounded-md border border-input bg-background px-3 font-mono text-sm outline-none focus:ring-2 focus:ring-ring"
            />
            <Button variant="destructive" size="sm" disabled={typed.trim() !== target.confirmWord} onClick={onConfirm}>
              Open root shell
            </Button>
            <Button variant="ghost" size="sm" onClick={onCancel}>Cancel</Button>
          </div>
        </div>
      </div>
    </div>
  )
}

// Cascading picker for container exec: project → app → env class. Classes are
// derived from *running* containers (`<project>-<app>-<class>-<hash>`), so you
// can only pick a target that actually has something to exec into.
const SEL = 'h-9 min-w-52 rounded-md border border-input bg-card px-3 text-[13px] outline-none focus:ring-2 focus:ring-ring disabled:opacity-50'
function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <label className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">{label}</label>
      {children}
    </div>
  )
}
function ContainerPicker({ onConnect }: { onConnect: (t: Target) => void }) {
  const projects = useProjects()
  const onboarded = useMemo(() => (projects.data ?? []).filter((p) => p.onboarded), [projects.data])
  const metrics = useNodeMetrics()
  const [org, setOrg] = useState('')
  const [app, setApp] = useState('')
  const [cls, setCls] = useState('')
  useEffect(() => { if (!org && onboarded[0]) setOrg(onboarded[0].org) }, [onboarded, org])
  useEffect(() => { setApp(''); setCls('') }, [org])

  const apps = useApps(org)
  const appList = apps.data ?? []
  const projName = onboarded.find((p) => p.org === org)?.name ?? org
  // Classes with a running container for the chosen app.
  const classes = useMemo(() => {
    if (!app) return [] as string[]
    const prefix = `${projName}-${app}-`
    const set = new Set<string>()
    for (const n of metrics.data ?? []) for (const c of n.apps) {
      if (c.name.startsWith(prefix)) set.add(c.name.slice(prefix.length).split('-')[0] ?? '')
    }
    return [...set].filter(Boolean).sort()
  }, [app, projName, metrics.data])
  useEffect(() => { setCls((c) => (classes.includes(c) ? c : classes[0] ?? '')) }, [classes])

  const connect = () => {
    if (!org || !app || !cls) return
    onConnect({ mode: 'container', project: org, app, class: cls, label: `${app} · ${cls}`, prod: cls === 'production', confirmWord: app })
  }
  return (
    <>
      <Field label="Project">
        <select value={org} onChange={(e) => setOrg(e.target.value)} className={SEL}>
          {onboarded.map((p) => <option key={p.org} value={p.org}>{p.name}</option>)}
        </select>
      </Field>
      <Field label="App">
        <select value={app} onChange={(e) => setApp(e.target.value)} disabled={!appList.length} className={SEL}>
          <option value="">{appList.length ? 'Select an app…' : 'No apps'}</option>
          {appList.map((a) => <option key={a.name} value={a.name}>{a.name}</option>)}
        </select>
      </Field>
      <Field label="Environment">
        <select value={cls} onChange={(e) => setCls(e.target.value)} disabled={!classes.length} className={SEL}>
          {classes.length ? classes.map((c) => <option key={c} value={c}>{c}</option>) : <option value="">{app ? 'No running container' : '—'}</option>}
        </select>
      </Field>
      <div className="flex-1" />
      <Button onClick={connect} disabled={!org || !app || !cls}><TerminalSquare className="size-4" /> Connect</Button>
    </>
  )
}

// ── page ──────────────────────────────────────────────────────────────────────
export function Terminal() {
  const nodes = useNodes()
  const nodeList = nodes.data ?? []

  // Deep-link target (from a per-node/per-app entry point) parsed once on mount.
  const initial = useMemo<Target | null>(() => buildFromSearch(location.search, nodeList), [nodeList.length])

  const [mode, setMode] = useState<Mode>('host')
  const [nodeName, setNodeName] = useState<string>('')
  const [session, setSession] = useState<Target | null>(null)
  const [pending, setPending] = useState<Target | null>(null) // awaiting production confirm

  // Once nodes load, honor a deep link (or default the host picker to the first node).
  useEffect(() => {
    if (initial && !session && !pending) {
      if (initial.prod) setPending(initial); else setSession(initial)
    }
  }, [initial, session, pending])
  useEffect(() => { if (!nodeName && nodeList[0]) setNodeName(nodeList[0].name) }, [nodeList, nodeName])

  const open = (t: Target) => { if (t.prod) setPending(t); else { setSession(t); setPending(null) } }
  const connectHost = () => {
    const n = nodeList.find((x) => x.name === nodeName)
    if (!n) return
    open({ mode: 'host', node: n.name, label: `root@${n.name}`, prod: n.role === 'prod', confirmWord: n.name })
  }

  return (
    <>
      <PageHead title="Terminal">
        <StatusBadge tone="accent">platform-admin</StatusBadge>
      </PageHead>
      <div className="mx-auto flex max-w-4xl flex-col gap-4">
        <div className="flex items-start gap-2.5 rounded-lg bg-accent px-3.5 py-3 text-[13px] leading-relaxed text-accent-foreground">
          <Info className="mt-0.5 size-4 shrink-0" />
          <div>
            A live shell over the reconciler’s Docker connection — no SSH, no new credentials. Every session is
            recorded and attributed to you. Host shell runs <span className="font-mono">nsenter</span> in the node’s
            host namespaces; container exec targets app containers only.
          </div>
        </div>

        {/* Connect bar (host mode; container sessions arrive via an app's Exec button) */}
        {!session && !pending && (
          <div className="rounded-lg border bg-card px-4 py-4">
            <div className="flex flex-wrap items-end gap-4">
              <div className="flex flex-col gap-1.5">
                <label className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Mode</label>
                <div className="inline-flex rounded-lg border p-0.5">
                  {(['host', 'container'] as Mode[]).map((m) => (
                    <button key={m} onClick={() => setMode(m)}
                      className={`rounded-md px-3 py-1.5 text-[13px] font-medium ${mode === m ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:text-foreground'}`}>
                      {m === 'host' ? 'Host shell' : 'App container'}
                    </button>
                  ))}
                </div>
              </div>

              {mode === 'host' ? (
                <>
                  <div className="flex flex-col gap-1.5">
                    <label className="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">Node</label>
                    <select value={nodeName} onChange={(e) => setNodeName(e.target.value)}
                      className="h-9 min-w-56 rounded-md border border-input bg-card px-3 text-[13px] outline-none focus:ring-2 focus:ring-ring">
                      {nodeList.map((n) => <option key={n.name} value={n.name}>{n.name} · {ZONE[n.role] ?? n.role}</option>)}
                    </select>
                  </div>
                  <div className="flex-1" />
                  <Button onClick={connectHost} disabled={!nodeName}>
                    <TerminalSquare className="size-4" /> Connect
                  </Button>
                </>
              ) : (
                <ContainerPicker onConnect={open} />
              )}
            </div>
          </div>
        )}

        {pending && <ProdConfirm target={pending} onConfirm={() => { setSession(pending); setPending(null) }} onCancel={() => setPending(null)} />}

        {session && (
          <>
            {session.prod && (
              <div className="flex items-start gap-2.5 rounded-lg border border-destructive/40 bg-destructive/10 px-3.5 py-3 text-[12.5px] leading-relaxed">
                <TriangleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
                <div className="text-foreground/80"><b className="text-destructive">Production.</b> Commands run as root on live infrastructure. Recorded in full.</div>
              </div>
            )}
            <XtermSession key={session.label} target={session} />
            <div className="flex items-center gap-3">
              <span className="flex items-center gap-1.5 text-[13px] text-muted-foreground">
                <ServerCog className="size-4" /> {session.mode === 'host' ? 'Host shell' : 'Container exec'} · {session.label}
              </span>
              <Button variant="outline" size="sm" className="ml-auto" onClick={() => setSession(null)}>Close</Button>
            </div>
          </>
        )}

        <Sessions />
      </div>
    </>
  )
}

// ── recorded sessions (audit) ─────────────────────────────────────────────────
function fmtDur(s: TerminalSession): string {
  if (!s.ended_at) return 'open'
  const ms = Date.parse(s.ended_at.replace(' ', 'T') + 'Z') - Date.parse(s.started_at.replace(' ', 'T') + 'Z')
  const sec = Math.max(0, Math.round(ms / 1000))
  return sec < 60 ? `${sec}s` : `${Math.round(sec / 60)}m`
}
const fmtBytes = (b: number | null) => (b == null ? '—' : b < 1024 ? `${b} B` : `${(b / 1024).toFixed(1)} KB`)

function Sessions() {
  const q = useTerminalSessions()
  const [open, setOpen] = useState<TerminalSession | null>(null)
  const [text, setText] = useState('')
  const [loading, setLoading] = useState(false)
  const rows = q.data ?? []

  const view = async (s: TerminalSession) => {
    setOpen(s); setText(''); setLoading(true)
    try { setText(stripAnsi(await getText(urls.terminalTranscript(s.id)))) }
    catch (e) { setText(`(failed to load transcript: ${(e as Error).message})`) }
    finally { setLoading(false) }
  }

  return (
    <div className="rounded-lg border bg-card px-4 py-4">
      <div className="flex items-center gap-2">
        <History className="size-4 text-muted-foreground" />
        <h2 className="text-sm font-semibold">Recent sessions</h2>
        <span className="text-[13px] text-muted-foreground">· every session is recorded</span>
      </div>
      <div className="my-3 h-px bg-border" />
      {rows.length === 0 && <div className="py-2 text-[13px] text-muted-foreground">No sessions recorded yet.</div>}
      <div className="flex flex-col">
        {rows.map((s) => (
          <button key={s.id} onClick={() => view(s)}
            className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t py-2.5 text-left first:border-t-0 hover:bg-accent">
            <StatusBadge tone={s.mode === 'host' ? 'warn' : 'muted'}>{s.mode === 'host' ? 'host' : 'exec'}</StatusBadge>
            <span className="font-mono text-[13px]">{s.target}</span>
            <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">by {s.actor}</span>
            <span className="text-xs text-muted-foreground" title={s.started_at}>{relTime(s.started_at)}</span>
            <span className="w-10 text-right font-mono text-xs text-muted-foreground">{fmtDur(s)}</span>
            <span className="w-16 text-right font-mono text-xs text-muted-foreground">{fmtBytes(s.bytes)}</span>
          </button>
        ))}
      </div>

      <Sheet open={!!open} onOpenChange={(o) => { if (!o) setOpen(null) }}>
        <SheetContent className="w-full sm:max-w-2xl">
          <SheetHeader>
            <SheetTitle>Transcript · <span className="font-mono text-sm">{open?.target}</span></SheetTitle>
          </SheetHeader>
          <SheetBody>
            {loading ? (
              <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground"><Loader2 className="size-4 animate-spin" /> Loading…</div>
            ) : (
              <pre className="max-h-[70vh] overflow-auto rounded-md bg-[#181a20] p-3 font-mono text-xs leading-relaxed text-[#d7dae0] whitespace-pre-wrap break-words">{text || '(empty)'}</pre>
            )}
          </SheetBody>
        </SheetContent>
      </Sheet>
    </div>
  )
}

const ZONE: Record<string, string> = { main: 'control plane', prod: 'production', private: 'private' }

// Build a Target from the URL search (deep link from a per-node/per-app entry).
function buildFromSearch(search: string, nodes: { name: string; role: string }[]): Target | null {
  const p = new URLSearchParams(search)
  const mode = p.get('mode')
  if (mode === 'host') {
    const node = p.get('node') ?? ''
    if (!node) return null
    const role = nodes.find((n) => n.name === node)?.role
    return { mode: 'host', node, label: `root@${node}`, prod: role === 'prod', confirmWord: node }
  }
  if (mode === 'container') {
    const project = p.get('project') ?? ''
    const app = p.get('app') ?? ''
    const cls = p.get('class') ?? ''
    if (!project || !app || !cls) return null
    return {
      mode: 'container', project, app, class: cls,
      label: `${app} · ${cls}`, prod: cls === 'production', confirmWord: app,
    }
  }
  return null
}
