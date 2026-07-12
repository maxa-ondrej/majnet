import { Link, useParams } from '@tanstack/react-router'
import { ChevronRight, Plus, Loader2, CheckCircle2, Circle, AlertCircle } from 'lucide-react'
import { useApps, useDeploys, useEvents, useImports, useNodes, useProjects, useWhoami, IMPORT_STEPS, type ImportStatus } from './api'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { DeployStatus, Empty, latestEventFor, QueryState, short, StatusBadge } from './ui'

/** Step-by-step progress of an in-flight (or failed) app import. */
export function ImportSteps({ status }: { status: ImportStatus }) {
  const current = IMPORT_STEPS.findIndex((s) => s.key === status.step)
  return (
    <div className="flex flex-col gap-1.5">
      {IMPORT_STEPS.map((s, i) => {
        const done = current > i
        const active = current === i
        const failed = active && status.status === 'failed'
        return (
          <div key={s.key} className={`flex items-center gap-2 text-sm ${done ? 'text-muted-foreground' : active ? 'font-medium' : 'text-muted-foreground/50'}`}>
            {failed ? <AlertCircle className="size-4 text-destructive" />
              : done ? <CheckCircle2 className="size-4 text-primary" />
              : active ? <Loader2 className="size-4 animate-spin text-primary" />
              : <Circle className="size-4" />}
            {s.label}
          </div>
        )
      })}
      {status.status === 'failed' && (
        <p className="mt-1 rounded-md border border-destructive/40 bg-destructive/10 p-2 font-mono text-xs text-destructive">{status.detail}</p>
      )}
    </div>
  )
}

export function Crumbs({ children }: { children: React.ReactNode }) {
  return <div className="mb-1.5 text-xs text-muted-foreground [&_a]:text-primary [&_a]:hover:underline">{children}</div>
}
export function PageHead({ title, sub, children }: { title: string; sub?: string; children?: React.ReactNode }) {
  return (
    <div className="mb-6 flex flex-wrap items-center gap-3">
      <h1 className="text-2xl font-bold tracking-tight">{title}</h1>
      {sub && <span className="font-mono text-sm text-muted-foreground">{sub}</span>}
      <div className="flex-1" />
      {children}
    </div>
  )
}

// ── Projects ─────────────────────────────────────────────────────────────────
export function Projects() {
  const q = useProjects()
  const { data: me } = useWhoami()
  return (
    <>
      <PageHead title="Projects">
        {me?.admin && <Button asChild><Link to="/new-project"><Plus className="size-4" /> New project</Link></Button>}
      </PageHead>
      <QueryState isLoading={q.isLoading} error={q.error}>
        <div className="grid gap-3.5 [grid-template-columns:repeat(auto-fill,minmax(280px,1fr))]">
          {q.data?.length === 0 && <Empty>No projects registered yet.</Empty>}
          {q.data?.map((p) => p.onboarded ? (
            <Link key={p.org} to="/projects/$org" params={{ org: p.org }}
              className="rounded-xl border bg-card p-4 shadow-sm transition-colors hover:border-primary">
              <div className="font-semibold">{p.name}</div>
              <div className="font-mono text-xs text-muted-foreground">{p.org}</div>
              <div className="mt-3 flex items-center gap-3 text-xs text-muted-foreground">
                <span><b className="text-foreground">{p.apps}</b> app{p.apps === 1 ? '' : 's'}</span>
                <StatusBadge tone="success" dot>onboarded</StatusBadge>
              </div>
            </Link>
          ) : (
            <div key={p.org} className="rounded-xl border border-dashed bg-card p-4 opacity-60">
              <div className="font-semibold">{p.name}</div>
              <div className="font-mono text-xs text-muted-foreground">{p.org}</div>
              <div className="mt-3"><StatusBadge tone="muted">registered · App not installed</StatusBadge></div>
            </div>
          ))}
        </div>
        <p className="mt-6 rounded-lg border border-dashed bg-muted/40 p-3.5 text-xs text-muted-foreground">
          Projects map 1:1 to GitHub orgs. A project is live only when it is listed in <code>projects.yaml</code> <b>and</b> the App
          is installed on the org. "New project" registers the org; org creation stays on GitHub.
        </p>
      </QueryState>
    </>
  )
}

// ── Project detail ───────────────────────────────────────────────────────────
export function ProjectDetail() {
  const { org } = useParams({ from: '/projects/$org' })
  const projects = useProjects()
  const name = projects.data?.find((x) => x.org === org)?.name ?? org
  const apps = useApps(org)
  const imports = useImports(org)
  const events = useEvents()
  const deploys = useDeploys(org)
  const pending = deploys.data?.length ?? 0
  // Importing apps not yet declared in the manifest — shown as skeletons.
  const importing = (imports.data ?? []).filter((i) => !apps.data?.some((a) => a.name === i.app))

  return (
    <>
      <Crumbs><Link to="/">Projects</Link> / {name}</Crumbs>
      <PageHead title={name} sub={org}>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/deploys" params={{ org }}>Deployments{pending ? ` · ${pending}` : ''}</Link></Button>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/members" params={{ org }}>Members</Link></Button>
        <Button asChild size="sm"><Link to="/projects/$org/new-app" params={{ org }}><Plus className="size-4" /> New app</Link></Button>
      </PageHead>

      <h2 className="mb-2.5 text-sm font-semibold">Apps</h2>
      <QueryState isLoading={apps.isLoading} error={apps.error}>
        <div className="flex flex-col gap-2">
          {apps.data?.length === 0 && importing.length === 0 && <Empty>No apps yet — create one.</Empty>}
          {importing.map((imp) => (
            <Link key={imp.app} to="/projects/$org/apps/$app" params={{ org, app: imp.app }}
              className="flex items-center gap-3 rounded-lg border border-dashed bg-card/50 px-4 py-3 transition-colors hover:border-primary">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2 font-semibold">
                  {imp.app}
                  <Badge variant="outline" className={imp.status === 'failed' ? 'text-destructive' : 'text-muted-foreground'}>
                    {imp.status === 'failed' ? 'import failed' : 'importing…'}
                  </Badge>
                </div>
                <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">
                  {IMPORT_STEPS.find((s) => s.key === imp.step)?.label ?? imp.step} · {short(imp.detail)}
                </div>
              </div>
              {imp.status === 'failed'
                ? <AlertCircle className="size-4 text-destructive" />
                : <Loader2 className="size-4 animate-spin text-muted-foreground" />}
              <ChevronRight className="size-4 text-muted-foreground/50" />
            </Link>
          ))}
          {apps.data?.map((a) => {
            const dm = [short(a.image), a.database].filter(Boolean).join('  ·  ')
            return (
              <Link key={a.name} to="/projects/$org/apps/$app" params={{ org, app: a.name }}
                className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3 transition-colors hover:border-primary">
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2 font-semibold">
                    {a.name}
                    {a.classes.map((c) => <Badge key={c} variant="secondary" className="text-primary bg-accent">{c}</Badge>)}
                  </div>
                  <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">{dm || '—'}</div>
                </div>
                {a.host && <span className="font-mono text-xs text-muted-foreground max-sm:hidden">{a.host}</span>}
                <DeployStatus ev={latestEventFor(events.data, name, a.name)} />
                <ChevronRight className="size-4 text-muted-foreground/50" />
              </Link>
            )
          })}
        </div>
      </QueryState>
    </>
  )
}

// ── Nodes ────────────────────────────────────────────────────────────────────
const ZONE: Record<string, string> = { main: 'control plane', prod: 'public', private: 'internal' }
export function Nodes() {
  const q = useNodes()
  return (
    <>
      <PageHead title="Nodes" />
      <QueryState isLoading={q.isLoading} error={q.error}>
        <div className="flex flex-col gap-2">
          {q.data?.length === 0 && <Empty>No nodes enrolled.</Empty>}
          {q.data?.map((n) => {
            const enrolled = !!n.wireguard_pubkey
            const ep = [n.wireguard_ip, n.public_endpoint].filter(Boolean).join(' · ')
            return (
              <div key={n.role} className={`flex items-center gap-3 rounded-lg border bg-card px-4 py-3 ${enrolled ? '' : 'opacity-60'}`}>
                <div className="flex-1">
                  <div className="flex items-center gap-2 font-semibold">{n.name} <Badge variant="secondary">{ZONE[n.role] ?? n.role}</Badge></div>
                  <div className="mt-0.5 font-mono text-xs text-muted-foreground">{ep || '—'}</div>
                </div>
                {enrolled ? <StatusBadge tone="success" dot>enrolled</StatusBadge> : <StatusBadge tone="muted">pending</StatusBadge>}
              </div>
            )
          })}
        </div>
        <p className="mt-4 text-xs text-muted-foreground">Enroll a pending node from <Link to="/settings" className="text-primary hover:underline">Settings → Nodes</Link>.</p>
      </QueryState>
    </>
  )
}

// ── Activity ─────────────────────────────────────────────────────────────────
export function Activity() {
  const q = useEvents(100)
  return (
    <>
      <PageHead title="Activity" />
      <QueryState isLoading={q.isLoading} error={q.error}>
        <div className="rounded-xl border bg-card">
          <Table>
            <TableHeader>
              <TableRow><TableHead>time</TableHead><TableHead>project</TableHead><TableHead>node</TableHead><TableHead>action</TableHead><TableHead>result</TableHead><TableHead>commit</TableHead></TableRow>
            </TableHeader>
            <TableBody className="font-mono text-xs">
              {q.data?.length === 0 && <TableRow><TableCell colSpan={6} className="text-muted-foreground">No events yet.</TableCell></TableRow>}
              {q.data?.map((e, i) => (
                <TableRow key={i}>
                  <TableCell>{e.at}</TableCell><TableCell>{e.project}</TableCell><TableCell>{e.node}</TableCell><TableCell>{e.action}</TableCell>
                  <TableCell className={e.result.startsWith('FAILED') ? 'text-destructive' : ''}>{e.result}</TableCell>
                  <TableCell>{e.commit.slice(0, 12)}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </QueryState>
    </>
  )
}
