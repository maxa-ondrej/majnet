import { useEffect, useState } from 'react'
import { Link, useParams } from '@tanstack/react-router'
import { ChevronRight, Plus, Loader2, CheckCircle2, Circle, AlertCircle } from 'lucide-react'
import { send, urls, useApps, useAppInfo, useArchivedApps, useDeploys, useEvents, useImports, useNodeMetrics, useNodes, useProjects, useWhoami, IMPORT_STEPS, type ImportStatus } from './api'
import { useApiMutation } from './mutations'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { ConfirmButton, DeployStatus, Empty, ExtLink, latestEventFor, QueryState, short, Sparkline, StatusBadge } from './ui'

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
function ArchivedApps({ org }: { org: string }) {
  const q = useArchivedApps(org)
  const m = useApiMutation({ invalidate: [['archived', org], ['apps', org], ['events']] })
  const apps = q.data ?? []
  if (apps.length === 0) return null
  return (
    <div className="mt-6">
      <h2 className="mb-2.5 text-sm font-semibold text-muted-foreground">Archived</h2>
      <div className="flex flex-col gap-2">
        {apps.map((name) => (
          <div key={name} className="flex items-center gap-3 rounded-lg border border-dashed bg-card/50 px-4 py-3">
            <span className="min-w-0 flex-1 font-mono text-sm text-muted-foreground">{name}</span>
            <Badge variant="outline" className="text-muted-foreground">archived</Badge>
            <ConfirmButton variant="outline" size="sm" className="text-destructive" disabled={m.isPending}
              title={`Permanently delete ${name}?`}
              description="Irreversible: purges its containers, volumes and databases, and deletes the GitHub source repo. There is no undo."
              confirmText="Delete permanently" onConfirm={() => m.mutate(() => send(urls.appDelete(org, name)))}>
              Delete
            </ConfirmButton>
          </div>
        ))}
      </div>
    </div>
  )
}

function RenameProjectControl({ org, current }: { org: string; current: string }) {
  const [name, setName] = useState('')
  const m = useApiMutation({ invalidate: [['projects'], ['apps', org], ['events']] })
  const valid = /^[a-z0-9-]+$/.test(name) && name !== current
  return (
    <div className="flex items-center gap-2">
      <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="new-project-name" className="h-8 w-44" aria-label="new project name" />
      <ConfirmButton variant="outline" size="sm" disabled={!valid || m.isPending}
        title={`Rename project ${current} → ${name}?`}
        description="The project name prefixes every app’s containers, volumes and databases — each app’s data is migrated to the new prefix with a brief per-app cutover."
        confirmText="Rename project"
        onConfirm={() => m.mutate(() => send(urls.projectRename(org), { json: { new: name } }))}>
        Rename
      </ConfirmButton>
    </div>
  )
}

// Sync platform-managed template files (.github/ CI) into the project's app
// repos — opens a template-sync PR per repo that has drifted (admin-only).
function SyncTemplateControl({ org }: { org: string }) {
  const m = useApiMutation({ invalidate: [['events']] })
  return (
    <ConfirmButton variant="outline" size="sm" disabled={m.isPending}
      title="Sync repo templates?"
      description="Opens a template-sync PR on each app repo whose platform-managed CI files (.github/) have drifted from the current template. Your source, Dockerfile and other files are never touched."
      confirmText="Sync templates"
      onConfirm={() => m.mutate(() => send(urls.templateSync(org)))}>
      Sync templates
    </ConfirmButton>
  )
}

// Per-env badges showing the running version the app reports at /info (scraped
// at deploy time), falling back to the image digest when /info has no version.
function AppEnvBadges({ org, app, classes, digestFor }: {
  org: string; app: string; classes: string[]; digestFor: (cls: string) => string | null
}) {
  const info = useAppInfo(org, app)
  const versionFor = (cls: string): string | null => {
    const v = info.data?.find((r) => r.class === cls)?.info?.version
    return typeof v === 'string' ? v : null
  }
  return (
    <>
      {classes.map((c) => {
        const ver = versionFor(c)
        const d = digestFor(c)
        const label = ver ?? d
        const title = label ? `running ${label}` : 'not running in this env'
        return (
          <Badge key={c} variant="secondary" className="bg-accent font-mono text-primary" title={title}>
            {c}{label ? ` · ${label}` : ''}
          </Badge>
        )
      })}
    </>
  )
}

export function ProjectDetail() {
  const { org } = useParams({ from: '/projects/$org' })
  const projects = useProjects()
  const name = projects.data?.find((x) => x.org === org)?.name ?? org
  const isAdmin = useWhoami().data?.admin ?? false
  const apps = useApps(org)
  const imports = useImports(org)
  const events = useEvents()
  const deploys = useDeploys(org)
  const pending = deploys.data?.length ?? 0
  // Importing apps not yet declared in the manifest — shown as skeletons.
  const importing = (imports.data ?? []).filter((i) => !apps.data?.some((a) => a.name === i.app))
  // Running image digest per (app, class), from live container names
  // `<project>-<app>-<class>-<hash>` — the version actually deployed per env.
  const metrics = useNodeMetrics()
  const runningDigest = (app: string, cls: string): string | null => {
    const prefix = `${name}-${app}-${cls}-`
    const c = (metrics.data ?? []).flatMap((n) => n.apps).find((x) => x.name.startsWith(prefix))
    return c?.image.split('@sha256:')[1]?.slice(0, 7) ?? null
  }

  return (
    <>
      <Crumbs><Link to="/">Projects</Link> / {name}</Crumbs>
      <PageHead title={name} sub={org}>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/deploys" params={{ org }}>Deployments{pending ? ` · ${pending}` : ''}</Link></Button>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/members" params={{ org }}>Members</Link></Button>
        <Button asChild size="sm"><Link to="/projects/$org/new-app" params={{ org }}><Plus className="size-4" /> New app</Link></Button>
        {isAdmin && <SyncTemplateControl org={org} />}
        {isAdmin && <RenameProjectControl org={org} current={name} />}
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
              <div key={a.name}
                className="flex items-center gap-3 rounded-lg border bg-card px-4 py-3 transition-colors hover:border-primary">
                <Link to="/projects/$org/apps/$app" params={{ org, app: a.name }} className="flex min-w-0 flex-1 flex-col">
                  <div className="flex flex-wrap items-center gap-2 font-semibold">
                    {a.name}
                    <AppEnvBadges org={org} app={a.name} classes={a.classes} digestFor={(c) => runningDigest(a.name, c)} />
                  </div>
                  <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">{dm || '—'}</div>
                </Link>
                {a.host && <ExtLink to={a.host} className="font-mono text-xs max-sm:hidden">{a.host}</ExtLink>}
                <DeployStatus ev={latestEventFor(events.data, name, a.name)} />
                <Link to="/projects/$org/apps/$app" params={{ org, app: a.name }} aria-label={`Open ${a.name}`}>
                  <ChevronRight className="size-4 text-muted-foreground/50" />
                </Link>
              </div>
            )
          })}
        </div>
      </QueryState>

      <ArchivedApps org={org} />
    </>
  )
}

// ── Nodes ────────────────────────────────────────────────────────────────────
const ZONE: Record<string, string> = { main: 'control plane', prod: 'public', private: 'internal' }
const gb = (b: number) => `${(b / 1e9).toFixed(1)} GB`
const mb = (b: number) => `${Math.round(b / 1e6)} MB`
function Stat({ label, value }: { label: string; value: string }) {
  return <div><div className="text-muted-foreground">{label}</div><div className="font-mono">{value || '—'}</div></div>
}

export function Nodes() {
  const q = useNodes()
  const m = useNodeMetrics()
  // Accumulate a rolling window (~last 60 samples ≈ 10 min at 10s) client-side
  // so the charts build up live while the page is open.
  const [hist, setHist] = useState<Record<string, { cpu: number[]; mem: number[] }>>({})
  useEffect(() => {
    if (!m.data) return
    setHist((h) => {
      const next = { ...h }
      for (const nd of m.data!) {
        if (!nd.reachable) continue
        const cur = next[nd.name] ?? { cpu: [], mem: [] }
        const memPct = nd.mem_total ? (nd.mem_used / nd.mem_total) * 100 : 0
        next[nd.name] = { cpu: [...cur.cpu, nd.host_cpu_pct].slice(-60), mem: [...cur.mem, memPct].slice(-60) }
      }
      return next
    })
  }, [m.data])
  return (
    <>
      <PageHead title="Nodes" />
      <QueryState isLoading={q.isLoading} error={q.error}>
        <div className="flex flex-col gap-3">
          {q.data?.length === 0 && <Empty>No nodes enrolled.</Empty>}
          {q.data?.map((n) => {
            const enrolled = !!n.wireguard_pubkey
            const ep = [n.wireguard_ip, n.public_endpoint].filter(Boolean).join(' · ')
            const mm = m.data?.find((x) => x.name === n.name)
            return (
              <div key={n.role} className={`rounded-lg border bg-card px-4 py-3 ${enrolled ? '' : 'opacity-60'}`}>
                <div className="flex items-center gap-3">
                  <div className="flex-1">
                    <div className="flex items-center gap-2 font-semibold">{n.name} <Badge variant="secondary">{ZONE[n.role] ?? n.role}</Badge></div>
                    <div className="mt-0.5 font-mono text-xs text-muted-foreground">{ep || '—'}</div>
                  </div>
                  {!enrolled ? <StatusBadge tone="muted">pending</StatusBadge>
                    : mm?.reachable ? <StatusBadge tone="success" dot>online</StatusBadge>
                    : <StatusBadge tone="danger" dot>unreachable</StatusBadge>}
                </div>
                {mm?.reachable && (
                  <>
                    <div className="mt-3 grid grid-cols-2 gap-x-6 gap-y-2 text-xs sm:grid-cols-4">
                      <Stat label="CPU" value={`${mm.host_cpu_pct.toFixed(0)}% of ${mm.cpus}`} />
                      <Stat label="Memory" value={`${gb(mm.mem_used)} / ${gb(mm.mem_total)}${mm.mem_total ? ` (${Math.round((mm.mem_used / mm.mem_total) * 100)}%)` : ''}`} />
                      <Stat label="Image disk" value={gb(mm.disk_images)} />
                      <Stat label="Containers" value={`${mm.containers_running}/${mm.containers}`} />
                      <Stat label="Docker" value={mm.server_version} />
                      <Stat label="OS" value={mm.os} />
                    </div>
                    <div className="mt-3 grid grid-cols-2 gap-3">
                      <div className="rounded-md border p-2">
                        <div className="mb-1 flex items-center justify-between text-xs"><span className="text-muted-foreground">CPU</span><span className="font-mono">{mm.host_cpu_pct.toFixed(0)}%</span></div>
                        <Sparkline values={hist[n.name]?.cpu ?? []} />
                      </div>
                      <div className="rounded-md border p-2">
                        <div className="mb-1 flex items-center justify-between text-xs"><span className="text-muted-foreground">Memory</span><span className="font-mono">{mm.mem_total ? Math.round((mm.mem_used / mm.mem_total) * 100) : 0}%</span></div>
                        <Sparkline values={hist[n.name]?.mem ?? []} />
                      </div>
                    </div>
                    {mm.apps.length > 0 && (
                      <div className="mt-3 overflow-x-auto">
                        <table className="w-full text-xs">
                          <thead><tr className="text-left text-muted-foreground"><th className="py-1 pr-3 font-medium">container</th><th className="py-1 pr-3 font-medium">state</th><th className="py-1 pr-3 font-medium">cpu</th><th className="py-1 font-medium">mem</th></tr></thead>
                          <tbody className="font-mono">
                            {mm.apps.map((a) => (
                              <tr key={a.name} className="border-t">
                                <td className="py-1 pr-3">{a.name}</td>
                                <td className="py-1 pr-3">{a.state}</td>
                                <td className="py-1 pr-3">{a.cpu_pct.toFixed(1)}%</td>
                                <td className="py-1">{mb(a.mem_used)}{a.mem_limit ? ` / ${gb(a.mem_limit)}` : ''}</td>
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </div>
                    )}
                  </>
                )}
                {enrolled && mm && !mm.reachable && <div className="mt-2 text-xs text-destructive">{mm.error ?? 'unreachable'}</div>}
              </div>
            )
          })}
        </div>
        <p className="mt-4 text-xs text-muted-foreground">Metrics read live over each node's Docker API (no agents). Enroll a pending node from <Link to="/settings" className="text-primary hover:underline">Settings → Nodes</Link>.</p>
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
