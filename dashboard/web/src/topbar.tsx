import { useEffect, useState, type ReactNode } from 'react'
import { Link } from '@tanstack/react-router'
import { useQueries } from '@tanstack/react-query'
import { Bell, GitPullRequest, Loader2 } from 'lucide-react'
import { getJSON, parseAt, urls, useEvents, useProjects, type DeployPr, type Event } from './api'
import { Button } from '@/components/ui/button'
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover'

function Count({ children }: { children: ReactNode }) {
  return (
    <span className="absolute -right-0.5 -top-0.5 grid min-w-4 place-items-center rounded-full bg-primary px-1 text-[10px] font-semibold text-primary-foreground">
      {children}
    </span>
  )
}

const relAge = (at: string): string => {
  const s = Math.max(0, Math.round((Date.now() - parseAt(at)) / 1000))
  if (s < 60) return `${s}s ago`
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.round(m / 60)
  return h < 24 ? `${h}h ago` : `${Math.round(h / 24)}d ago`
}
// The version (or short digest) from a "deployed <ref> (…)" result.
const verOf = (result: string): string => {
  const ref = result.match(/^deployed\s+(\S+)/)?.[1] ?? ''
  return ref.match(/@sha256:([0-9a-f]{12})/)?.[1] ?? ref
}

// ── deployments (all projects) ────────────────────────────────────────────────
// Two surfaces, split: "Deploying now" (live converges, from deploy events) and
// "Pending review" (open render PRs — "reconciling" until GitHub computes
// mergeability, then awaiting an admin merge). The trigger reads as a labeled
// status when active, not just an icon; rows carry inline detail.
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
  // Tick a re-render every 10s so the 90s "deploying" window ages out even when
  // the polled events are unchanged — TanStack structural sharing skips
  // re-renders on identical data, which would otherwise freeze `now` here and
  // leave the indicator stuck on "Deploying" until some other event arrived.
  const [, tick] = useState(0)
  useEffect(() => {
    const id = setInterval(() => tick((t) => t + 1), 10_000)
    return () => clearInterval(id)
  }, [])
  const now = Date.now()
  const deploying = (events.data ?? []).filter((e) => e.result.startsWith('deployed') && now - parseAt(e.at) < 90_000)
  const [open, setOpen] = useState(false)
  const active = deploying.length
  const count = active + pending.length

  const stateChip = 'inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium'

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        {active > 0 ? (
          <Button variant="ghost" size="sm" className="gap-1.5 text-warning hover:text-warning" title="Deployments">
            <Loader2 className="size-4 animate-spin" /> Deploying
            <span className="rounded-full border px-1.5 font-mono text-[11px]">{active}</span>
            {pending.length > 0 && <span className="font-normal text-muted-foreground">· {pending.length} pending</span>}
          </Button>
        ) : (
          <Button variant="ghost" size="sm" className="gap-1.5 text-muted-foreground" title="Deployments">
            <GitPullRequest className="size-4" /> Deployments
            {pending.length > 0 && <span className="rounded-full border px-1.5 font-mono text-[11px]">{pending.length}</span>}
          </Button>
        )}
      </PopoverTrigger>
      <PopoverContent align="end" className="w-96 p-0">
        <div className="flex items-center border-b px-3 py-2.5">
          <span className="text-sm font-semibold">Deployments</span>
          <Link to="/activity" onClick={() => setOpen(false)} className="ml-auto text-xs text-primary hover:underline">All activity →</Link>
        </div>

        {count === 0 && <div className="px-3 py-4 text-center text-xs text-muted-foreground">No active or pending deployments.</div>}

        {active > 0 && (
          <div className="p-1.5">
            <div className="flex items-center gap-1.5 px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
              <Loader2 className="size-3 animate-spin" /> Deploying now <span className="ml-auto font-mono">{active}</span>
            </div>
            {deploying.map((e, i) => {
              const app = e.action.replace(/^converge /, '')
              const ver = verOf(e.result)
              return (
                <div key={i} className="rounded-md px-2 py-1.5">
                  <div className="flex items-center gap-1.5 text-[13px]">
                    <span className="font-medium">{e.project}</span>
                    <span className="font-mono text-muted-foreground">{app}</span>
                    <span className="ml-auto font-mono text-[11px] text-muted-foreground">{relAge(e.at)}</span>
                  </div>
                  <div className="mt-0.5 flex items-center gap-2 text-xs text-muted-foreground">
                    {ver && <span>→ <span className="font-mono text-foreground">{ver}</span></span>}
                    {e.node && <span className="font-mono">{e.node}</span>}
                    <span className={`ml-auto bg-warning/15 text-warning ${stateChip}`}><Loader2 className="size-3 animate-spin" /> converging</span>
                  </div>
                </div>
              )
            })}
          </div>
        )}

        {pending.length > 0 && (
          <div className={`p-1.5 ${active > 0 ? 'border-t' : ''}`}>
            <div className="flex items-center gap-1.5 px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
              <GitPullRequest className="size-3" /> Pending review <span className="ml-auto font-mono">{pending.length}</span>
            </div>
            {pending.map(({ p, pr }) => (
              <Link key={`${p.org}-${pr.number}`} to="/projects/$org/deploys" params={{ org: p.org }} onClick={() => setOpen(false)}
                className="block rounded-md px-2 py-1.5 hover:bg-accent">
                <div className="flex items-center gap-1.5 text-[13px]">
                  <span className="font-medium">{p.name}</span>
                  <span className="text-muted-foreground">#{pr.number}</span>
                  <span className="ml-auto font-mono text-[11px] text-muted-foreground">{relAge(pr.created_at)}</span>
                </div>
                <div className="mt-0.5 flex items-center gap-2 text-xs">
                  <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${pr.class === 'production' ? 'bg-destructive/10 text-destructive' : 'bg-secondary text-secondary-foreground'}`}>{pr.class}</span>
                  <span className={`ml-auto bg-muted text-muted-foreground ${stateChip}`}>
                    {pr.mergeable !== true ? <><Loader2 className="size-3 animate-spin" /> reconciling</> : <>awaiting merge</>}
                  </span>
                </div>
                <div className="mt-0.5 truncate text-xs text-muted-foreground">{pr.title}</div>
              </Link>
            ))}
          </div>
        )}
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
