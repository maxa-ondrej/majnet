import { Link, Outlet } from '@tanstack/react-router'
import { Activity, Boxes, Server, Settings } from 'lucide-react'
import { useWhoami } from './api'
import { TopBar } from './topbar'

const NAV = [
  { to: '/', label: 'Projects', icon: Boxes, exact: true },
  { to: '/activity', label: 'Activity', icon: Activity, exact: false },
  { to: '/nodes', label: 'Nodes', icon: Server, exact: false },
  { to: '/settings', label: 'Settings', icon: Settings, exact: false },
] as const

const base = 'flex items-center gap-2.5 rounded-md px-2.5 py-2 text-sm font-medium transition-colors'

export function Shell() {
  const { data: me } = useWhoami()
  const initials = (me?.login || 'infra').slice(0, 1).toUpperCase()
  return (
    <div className="grid min-h-screen grid-cols-[240px_1fr] max-md:grid-cols-1">
      <aside className="flex flex-col gap-1 border-r bg-sidebar p-3 text-sidebar-foreground max-md:flex-row max-md:items-center max-md:gap-2 max-md:overflow-x-auto">
        <div className="flex items-center gap-2.5 px-2 py-3 max-md:py-0">
          <div className="grid size-8 place-items-center rounded-lg bg-primary/15 font-bold text-primary">M</div>
          <span className="font-semibold tracking-tight">MajNet</span>
        </div>
        <nav className="flex flex-col gap-0.5 max-md:flex-row">
          {NAV.map((n) => (
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
      <div className="flex min-w-0 flex-col">
        <TopBar />
        <main className="w-full max-w-[1400px] p-6 md:px-8 md:py-7">
          <Outlet />
        </main>
      </div>
    </div>
  )
}
