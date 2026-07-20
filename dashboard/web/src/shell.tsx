import { useEffect, useState } from 'react'
import { Link, Outlet } from '@tanstack/react-router'
import { Activity, ArrowUp, Boxes, Cpu, Home, Loader2, Server, Settings, TerminalSquare, X } from 'lucide-react'
import { useControlPlane, useWhoami } from './api'
import { TopBar } from './topbar'
import { MajnetMark } from './components/majnet-mark'

// This bundle's build commit (CI-baked; empty in dev).
const BUNDLE_COMMIT = import.meta.env.VITE_BUILD_COMMIT ?? ''

const NAV = [
  { to: '/', label: 'Overview', icon: Home, exact: true, admin: false },
  { to: '/projects', label: 'Projects', icon: Boxes, exact: false, admin: false },
  { to: '/activity', label: 'Activity', icon: Activity, exact: false, admin: false },
  { to: '/nodes', label: 'Nodes', icon: Server, exact: false, admin: false },
  { to: '/control-plane', label: 'Control plane', icon: Cpu, exact: false, admin: true },
  { to: '/terminal', label: 'Terminal', icon: TerminalSquare, exact: false, admin: true },
  { to: '/settings', label: 'Settings', icon: Settings, exact: false, admin: false },
] as const

const base = 'flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm font-medium transition-colors'

// Global nudge (admins only) — a slim strip when the control plane is updating
// or a new build is available. Mounted only for admins (the endpoint is gated).
function ControlPlaneBanner() {
  const { data: cp } = useControlPlane()
  const [dismissed, setDismissed] = useState<string | null>(null)
  // Once a control-plane update has converged, if this tab is still running an
  // older bundle, hard-reload once to pick up the new dashboard. Guarded per-ref
  // via sessionStorage so a mid-rollout timing race can't loop.
  useEffect(() => {
    if (!cp || cp.converged !== true || !BUNDLE_COMMIT) return
    if (cp.current.ref.startsWith(BUNDLE_COMMIT.slice(0, 7))) return // already on this build
    const KEY = 'majnet-cp-reloaded'
    if (sessionStorage.getItem(KEY) === cp.current.ref) return // already reloaded for this ref
    sessionStorage.setItem(KEY, cp.current.ref)
    const t = setTimeout(() => location.reload(), 1500)
    return () => clearTimeout(t)
  }, [cp])
  if (!cp) return null
  if (cp.converged === false) {
    return (
      <div className="flex items-center gap-2 border-b bg-accent px-6 py-2 text-[13px] text-accent-foreground md:px-8">
        <Loader2 className="size-4 animate-spin" />
        <span>Control plane is updating…</span>
        <Link to="/control-plane" className="ml-auto font-medium underline-offset-2 hover:underline">View progress →</Link>
      </div>
    )
  }
  if (cp.latest && !cp.up_to_date && cp.latest.ref !== dismissed) {
    return (
      <div className="flex items-center gap-2 border-b border-warning/30 bg-warning/10 px-6 py-2 text-[13px] md:px-8">
        <ArrowUp className="size-4 shrink-0 text-warning" />
        <span>A control-plane update is available <span className="font-mono text-muted-foreground">({cp.latest.ref.slice(0, 7)})</span>.</span>
        <Link to="/control-plane" className="font-medium text-primary underline-offset-2 hover:underline">Review &amp; update →</Link>
        <button onClick={() => setDismissed(cp.latest!.ref)} aria-label="Dismiss"
          className="ml-auto rounded p-1 text-muted-foreground transition-colors hover:bg-warning/15 hover:text-foreground">
          <X className="size-3.5" />
        </button>
      </div>
    )
  }
  return null
}

export function Shell() {
  const { data: me } = useWhoami()
  const initials = (me?.login || 'infra').slice(0, 1).toUpperCase()
  return (
    <div className="grid h-screen grid-cols-[240px_1fr] overflow-hidden max-md:grid-cols-1 max-md:grid-rows-[auto_1fr]">
      <aside className="flex flex-col gap-1 overflow-y-auto border-r bg-sidebar p-3 text-sidebar-foreground max-md:flex-row max-md:items-center max-md:gap-2 max-md:overflow-x-auto max-md:overflow-y-hidden">
        <div className="flex items-center gap-2.5 px-2 py-3 max-md:py-0">
          <MajnetMark className="size-8" />
          <span className="font-semibold tracking-tight">MajNet</span>
        </div>
        <nav className="flex flex-col gap-0.5 max-md:flex-row">
          {NAV.filter((n) => !n.admin || me?.admin).map((n) => (
            <Link
              key={n.to}
              to={n.to}
              activeOptions={{ exact: n.exact }}
              activeProps={{ className: `${base} bg-sidebar-accent text-sidebar-accent-foreground` }}
              inactiveProps={{ className: `${base} text-muted-foreground hover:bg-sidebar-accent hover:text-foreground` }}
            >
              <n.icon className="size-4" /> {n.label}
            </Link>
          ))}
        </nav>
        <div className="mt-auto flex items-center gap-2.5 border-t pt-3 max-md:mt-0 max-md:border-0 max-md:pt-0">
          <div className="grid size-7 place-items-center rounded-full bg-primary text-xs font-semibold text-primary-foreground">{initials}</div>
          <div className="text-xs leading-tight max-md:hidden">
            <div className="text-foreground">{me?.login || 'infra'}</div>
            <div className="text-[10px] font-semibold uppercase tracking-wide text-success">{me?.admin ? 'admin' : 'member'}</div>
          </div>
        </div>
      </aside>
      <div className="flex min-h-0 min-w-0 flex-col overflow-hidden">
        <TopBar />
        {me?.admin && <ControlPlaneBanner />}
        <main className="min-h-0 flex-1 overflow-y-auto">
          <div className="w-full max-w-[1400px] p-6 md:px-8 md:py-7">
            <Outlet />
          </div>
        </main>
      </div>
    </div>
  )
}
