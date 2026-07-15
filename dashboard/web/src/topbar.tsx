import { useEffect, useState, type ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { useQueries } from '@tanstack/react-query'
import { Bell, GitPullRequest, Loader2 } from 'lucide-react'
import { getJSON, parseAt, urls, useEvents, useProjects, type DeployPr, type Event } from './api'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover'

function Count({ children }: { children: ReactNode }) {
  return (
    <span className="absolute -right-0.5 -top-0.5 grid min-w-4 place-items-center rounded-full bg-primary px-1 text-[10px] font-semibold text-primary-foreground">
      {children}
    </span>
  )
}

// ── deployments (in-flight converges + open render PRs, all projects) ─────────
// The deployment lifecycle lives here, out of Notifications: "deploying now"
// (an active converge), and pending render PRs — flagged "reconciling" until
// GitHub finishes computing mergeability (merge is held until then).
function Deployments() {
  const projects = useProjects()
  const onboarded = (projects.data ?? []).filter((p) => p.onboarded)
  const results = useQueries({
    queries: onboarded.map((p) => ({
      queryKey: ['deploys', p.org],
      queryFn: () => getJSON<DeployPr[]>(urls.deploys(p.org)),
      refetchInterval: 15_000,
    })),
  })
  const pending = onboarded.flatMap((p, i) => (results[i]?.data ?? []).map((pr) => ({ p, pr })))
  const events = useEvents()
  const now = Date.now()
  const deploying = (events.data ?? []).filter((e) => e.result.startsWith('deployed') && now - parseAt(e.at) < 90_000)
  const [open, setOpen] = useState(false)
  const count = pending.length + deploying.length

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="icon" className="relative" title="Deployments" aria-label="Deployments">
          <GitPullRequest className="size-5" />
          {deploying.length > 0 && <span className="absolute right-1 top-1 size-2 animate-pulse rounded-full bg-warning" />}
          {count > 0 && <Count>{count}</Count>}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 p-2">
        <div className="px-2 py-1.5 text-sm font-semibold">Deployments</div>
        {deploying.length > 0 && (
          <div className="mb-1 rounded-md bg-warning/10 px-2 py-1.5">
            <div className="flex items-center gap-1.5 text-xs font-medium text-warning"><Loader2 className="size-3 animate-spin" /> Deploying now</div>
            {deploying.map((e, i) => (
              <div key={i} className="pl-4 text-xs text-muted-foreground">{e.project} · {e.action.replace('converge ', '')}</div>
            ))}
          </div>
        )}
        {count === 0 && <div className="px-2 py-3 text-xs text-muted-foreground">No active or pending deployments.</div>}
        {pending.map(({ p, pr }) => (
          <Link key={`${p.org}-${pr.number}`} to="/projects/$org/deploys" params={{ org: p.org }} onClick={() => setOpen(false)}
            className="block rounded-md px-2 py-1.5 hover:bg-accent">
            <div className="flex flex-wrap items-center gap-2 text-sm">
              <Badge variant="secondary" className="bg-accent text-primary">{pr.class}</Badge>
              {p.name} · #{pr.number}
              {pr.mergeable !== true && <Badge variant="outline" className="text-warning">reconciling</Badge>}
            </div>
            <div className="mt-0.5 truncate text-xs text-muted-foreground">{pr.title}</div>
          </Link>
        ))}
      </PopoverContent>
    </Popover>
  )
}

// ── notifications (events feed + in-progress deploys) ─────────────────────────
const SEEN = 'majnet-notifs-seen'
const evKey = (e: Event) => `${e.at}|${e.action}|${e.project}|${e.commit}`

function NotifRow({ e, fresh }: { e: Event; fresh: boolean }) {
  const bad = e.result.startsWith('FAILED')
  const dep = e.result.startsWith('deployed')
  const dot = bad ? 'bg-destructive' : dep ? 'bg-success' : e.action.startsWith('gc') ? 'bg-muted-foreground' : 'bg-primary'
  return (
    <div className={`flex items-start gap-2 rounded-md px-2 py-1.5 ${fresh ? 'bg-accent/40' : ''}`}>
      <span className={`mt-1.5 size-1.5 shrink-0 rounded-full ${dot}`} />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm"><span className="font-medium">{e.project}</span> {e.action}</div>
        <div className={`truncate text-xs ${bad ? 'text-destructive' : 'text-muted-foreground'}`}>{e.result} · {e.at}</div>
      </div>
    </div>
  )
}

function Notifications() {
  const events = useEvents()
  const evs = events.data ?? []
  const [seen, setSeen] = useState(() => localStorage.getItem(SEEN) ?? '')
  const [open, setOpen] = useState(false)

  // Baseline on first load so a fresh session doesn't show everything as unread.
  useEffect(() => {
    if (!seen && evs[0]) {
      const k = evKey(evs[0])
      localStorage.setItem(SEEN, k)
      setSeen(k)
    }
  }, [seen, evs])

  const seenIdx = seen ? evs.findIndex((e) => evKey(e) === seen) : 0
  const unread = seenIdx === -1 ? evs.length : seenIdx

  const onOpenChange = (o: boolean) => {
    setOpen(o)
    if (o && evs[0]) {
      const k = evKey(evs[0])
      localStorage.setItem(SEEN, k)
      setSeen(k)
    }
  }

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="icon" className="relative" title="Notifications" aria-label="Notifications">
          <Bell className="size-5" />
          {unread > 0 && <Count>{unread > 9 ? '9+' : unread}</Count>}
        </Button>
      </PopoverTrigger>
      <PopoverContent align="end" className="w-96 p-2">
        <div className="flex items-center px-2 py-1.5">
          <span className="text-sm font-semibold">Notifications</span>
          <Link to="/activity" onClick={() => setOpen(false)} className="ml-auto text-xs text-primary hover:underline">All activity</Link>
        </div>
        <div className="max-h-96 overflow-auto">
          {evs.length === 0 && <div className="px-2 py-3 text-xs text-muted-foreground">Nothing yet.</div>}
          {evs.slice(0, 15).map((e, i) => <NotifRow key={i} e={e} fresh={i < unread} />)}
        </div>
      </PopoverContent>
    </Popover>
  )
}

export function TopBar() {
  return (
    <header className="sticky top-0 z-20 flex h-12 items-center gap-1 border-b bg-background/80 px-4 backdrop-blur md:px-6">
      <div className="flex-1" />
      <Deployments />
      <Notifications />
    </header>
  )
}
