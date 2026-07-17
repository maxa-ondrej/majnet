// Node terminal page (#13, ADR 0016) — a live shell over a WebSocket to the
// reconciler. Two modes: host shell (pick a node) and container exec (deep-linked
// from an app's Exec button). Platform-admin only; every session is recorded.
import { useEffect, useMemo, useRef, useState } from 'react'
import { Terminal as Xterm } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { Cpu, Info, ServerCog, TerminalSquare, TriangleAlert } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useNodes, terminalWsUrl } from './api'
import { PageHead } from './views'
import { StatusBadge } from './ui'

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
                <div className="flex items-center gap-2 text-[13px] text-muted-foreground">
                  <Cpu className="size-4" /> Open a container shell from an app’s page — its <b className="text-foreground">Exec</b> button targets that app + environment.
                </div>
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
      </div>
    </>
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
