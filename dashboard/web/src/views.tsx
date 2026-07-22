import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import { ChevronRight, Plus, Loader2, CheckCircle2, Circle, AlertCircle, MoreVertical, Boxes, Rocket, Trash2, Archive, GitPullRequest, RefreshCw, PenLine, TerminalSquare, FolderGit2 } from 'lucide-react'
import { send, urls, useApps, useAppInfo, useArchivedApps, useBotEvents, useContainerHistory, useDeploys, useEvents, useImports, useMetricsHistory, useNodeMetrics, useNodes, useProjects, useWhoami, parseAt, IMPORT_STEPS, RELEASE_STAGES, type ImportStatus, type ReleaseProgress, type Event, type AppSummary } from './api'
import { useApiMutation } from './mutations'
import { ProjectObservability } from './observability'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'
import { ConfirmButton, DeployStatus, Empty, ExtLink, latestEventFor, MetricChart, QueryState, short, StatusBadge } from './ui'

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

/** Step-by-step progress of a release moving through the pipeline (ADR 0022). */
export function ReleaseSteps({ p }: { p: ReleaseProgress }) {
  const idx = RELEASE_STAGES.findIndex((s) => s.key === p.stage)
  // A `done` release has every stage complete (including the last); otherwise
  // the current stage is active (spinning) or failed.
  const current = p.status === 'done' ? RELEASE_STAGES.length : idx
  return (
    <div className="flex flex-col gap-1.5">
      {RELEASE_STAGES.map((s, i) => {
        const done = current > i
        const active = current === i
        const failed = active && p.status === 'failed'
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
      {p.status === 'failed' && (
        <p className="mt-1 rounded-md border border-destructive/40 bg-destructive/10 p-2 font-mono text-xs text-destructive">{p.detail}</p>
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
  const metrics = useNodeMetrics()
  // Live running-container count per project (name is `<project>-<app>-<class>-…`).
  const runningFor = (proj: string) =>
    (metrics.data ?? []).flatMap((n) => n.apps).filter((c) => c.name.startsWith(`${proj}-`)).length

  return (
    <>
      <PageHead title="Projects">
        {me?.admin && <Button asChild><Link to="/new-project"><Plus className="size-4" /> New project</Link></Button>}
      </PageHead>
      <QueryState isLoading={q.isLoading} error={q.error}>
        <div className="grid gap-3.5 [grid-template-columns:repeat(auto-fill,minmax(300px,1fr))]">
          {q.data?.length === 0 && <Empty>No projects registered yet.</Empty>}
          {q.data?.map((p) => {
            const running = runningFor(p.name)
            return p.onboarded ? (
              <Link key={p.org} to="/projects/$org" params={{ org: p.org }}
                className="group rounded-xl border bg-card p-4 transition-all hover:border-primary/50 hover:shadow-md">
                <div className="flex items-center gap-3">
                  <div className="grid size-9 shrink-0 place-items-center rounded-[10px] bg-primary/15 text-sm font-bold text-primary">
                    {p.name.slice(0, 1).toUpperCase()}
                  </div>
                  <div className="min-w-0">
                    <div className="truncate font-semibold tracking-tight">{p.name}</div>
                    <div className="truncate font-mono text-xs text-muted-foreground">{p.org}</div>
                  </div>
                  <StatusBadge tone="success" dot title="listed in registry + App installed">live</StatusBadge>
                </div>
                <div className="mt-3.5 flex items-center gap-2.5 border-t pt-3 text-xs text-muted-foreground">
                  <span><b className="text-foreground">{p.apps}</b> app{p.apps === 1 ? '' : 's'}</span>
                  {running > 0 && <><span>·</span><span><b className="text-foreground">{running}</b> running</span></>}
                  <ChevronRight className="ml-auto size-4 text-muted-foreground/40 transition-transform group-hover:translate-x-0.5" />
                </div>
              </Link>
            ) : (
              <div key={p.org} className="rounded-xl border border-dashed bg-card p-4">
                <div className="flex items-center gap-3">
                  <div className="grid size-9 shrink-0 place-items-center rounded-[10px] bg-muted text-sm font-bold text-muted-foreground">
                    {p.name.slice(0, 1).toUpperCase()}
                  </div>
                  <div className="min-w-0">
                    <div className="truncate font-semibold tracking-tight">{p.name}</div>
                    <div className="truncate font-mono text-xs text-muted-foreground">{p.org}</div>
                  </div>
                </div>
                <div className="mt-3.5 border-t pt-3"><StatusBadge tone="muted">registered · App not installed</StatusBadge></div>
              </div>
            )
          })}
        </div>
        <p className="mt-6 rounded-lg border border-dashed bg-muted/40 p-3.5 text-xs text-muted-foreground">
          Projects map 1:1 to GitHub orgs. A project is live only when it is listed in <code className="font-mono">projects.yaml</code> <b>and</b> the App
          is installed on the org. “New project” registers the org; org creation stays on GitHub.
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

// A controlled confirm dialog (for the destructive/one-shot project actions).
function ConfirmDialog({ open, onOpenChange, title, description, confirmText, destructive, disabled, onConfirm }: {
  open: boolean; onOpenChange: (o: boolean) => void; title: string; description?: string
  confirmText: string; destructive?: boolean; disabled?: boolean; onConfirm: () => void
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader><DialogTitle>{title}</DialogTitle></DialogHeader>
        {description && <p className="text-sm text-muted-foreground">{description}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button variant={destructive ? 'destructive' : 'default'} disabled={disabled} onClick={onConfirm}>{confirmText}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// Admin actions for a project, collapsed into a ⋯ overflow: rename (dialog),
// sync repo templates, and the archive→delete lifecycle (archive while apps are
// active; permanent delete once archived).
function ProjectAdminMenu({ org, name, activeApps }: { org: string; name: string; activeApps: number }) {
  const navigate = useNavigate()
  const archived = useArchivedApps(org)
  const [dialog, setDialog] = useState<null | 'rename' | 'sync' | 'archive' | 'delete'>(null)
  const [newName, setNewName] = useState('')
  const close = () => setDialog(null)
  const rename = useApiMutation({ invalidate: [['projects'], ['apps', org], ['events']], onDone: () => { close(); navigate({ to: '/projects/$org', params: { org } }) } })
  const sync = useApiMutation({ invalidate: [['events']], onDone: close })
  const archive = useApiMutation({ invalidate: [['projects'], ['apps', org], ['archived', org], ['events']], onDone: close })
  const del = useApiMutation({ invalidate: [['projects'], ['events']], onDone: () => { close(); navigate({ to: '/' }) } })
  const canDelete = activeApps === 0 && (archived.data?.length ?? 0) > 0
  const validName = /^[a-z0-9-]+$/.test(newName) && newName !== name
  return (
    <>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="outline" size="icon" className="size-9" aria-label="Project actions"><MoreVertical className="size-4" /></Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={() => { setNewName(''); setDialog('rename') }}>Rename project…</DropdownMenuItem>
          <DropdownMenuItem onSelect={() => setDialog('sync')}>Sync repo templates</DropdownMenuItem>
          {(activeApps > 0 || canDelete) && <DropdownMenuSeparator />}
          {activeApps > 0 && <DropdownMenuItem variant="destructive" onSelect={() => setDialog('archive')}>Archive project</DropdownMenuItem>}
          {canDelete && <DropdownMenuItem variant="destructive" onSelect={() => setDialog('delete')}>Delete project…</DropdownMenuItem>}
        </DropdownMenuContent>
      </DropdownMenu>

      <Dialog open={dialog === 'rename'} onOpenChange={(o) => !o && close()}>
        <DialogContent>
          <DialogHeader><DialogTitle>Rename project {name}</DialogTitle></DialogHeader>
          <p className="text-sm text-muted-foreground">The project name prefixes every app’s containers, volumes and databases — each app’s data is migrated to the new prefix with a brief per-app cutover.</p>
          <Input value={newName} onChange={(e) => setNewName(e.target.value)} placeholder="new-project-name" aria-label="new project name" className="font-mono" />
          <DialogFooter>
            <Button variant="outline" onClick={close}>Cancel</Button>
            <Button disabled={!validName || rename.isPending} onClick={() => rename.mutate(() => send(urls.projectRename(org), { json: { new: newName } }))}>Rename project</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog open={dialog === 'sync'} onOpenChange={(o) => !o && close()}
        title="Sync repo templates?" confirmText="Sync templates" disabled={sync.isPending}
        description="Opens a template-sync PR on each app repo whose platform-managed CI files (.github/) have drifted from the current template, and seeds a build-CI matrix caller into any monorepo repo that lacks one. Your source, Dockerfile and existing build files are never touched."
        onConfirm={() => sync.mutate(() => send(urls.templateSync(org)))} />
      <ConfirmDialog open={dialog === 'archive'} onOpenChange={(o) => !o && close()} destructive
        title="Archive this project?" confirmText="Archive project" disabled={archive.isPending}
        description="Takes every app down and archives its source repos. Volumes, databases, the ops repo and the registry entry are kept — permanently delete afterward to reclaim storage."
        onConfirm={() => archive.mutate(() => send(urls.projectArchive(org)))} />
      <ConfirmDialog open={dialog === 'delete'} onOpenChange={(o) => !o && close()} destructive
        title="Permanently delete this project?" confirmText="Delete project forever" disabled={del.isPending}
        description="Irreversible. Purges every app’s volumes and databases + the per-project network, ingress and DB role, deletes all app repos and the ops repo, and removes the project from the registry."
        onConfirm={() => del.mutate(() => send(urls.projectDelete(org)))} />
    </>
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

  const containersFor = (app: string, cls: string) =>
    (metrics.data ?? []).flatMap((n) => n.apps).filter((c) => c.name.startsWith(`${name}-${app}-${cls}-`))
  const runningCount = (metrics.data ?? []).flatMap((n) => n.apps).filter((c) => c.name.startsWith(`${name}-`)).length
  const anyFailed = (apps.data ?? []).some((a) => latestEventFor(events.data, name, a.name)?.result.startsWith('FAILED'))
  const hasApps = (apps.data?.length ?? 0) > 0

  // A single app row — reused for solo apps and for monorepo group members. In
  // a group `label` is the repo-stripped name (`zpevnik-api` → `api`), since the
  // group header already names the repo; routing/aria still use the full name.
  const appRow = (a: AppSummary, label?: string) => {
    const meta = [short(a.image), a.database].filter(Boolean).join('  ·  ')
    return (
      <div key={a.name}
        className="relative flex items-center gap-3.5 rounded-xl border bg-card px-4 py-4 transition-colors hover:border-primary/50">
        {/* stretched link makes the whole row clickable; inner links opt back in */}
        <Link to="/projects/$org/apps/$app" params={{ org, app: a.name }} aria-label={`Open ${a.name}`} className="absolute inset-0 rounded-xl" />
        <div className="grid size-9 shrink-0 place-items-center rounded-[10px] bg-muted text-muted-foreground"><Boxes className="size-4" /></div>
        <div className="pointer-events-none relative flex min-w-0 flex-1 flex-col gap-1">
          <div className="flex flex-wrap items-center gap-2 font-semibold">
            {label ?? a.name}
            {a.service
              ? <StatusBadge tone="accent">service · {a.service}</StatusBadge>
              : <AppEnvBadges org={org} app={a.name} classes={a.classes} digestFor={(c) => runningDigest(a.name, c)} />}
          </div>
          <div className="truncate font-mono text-xs text-muted-foreground">
            {meta || '—'}
            {a.host && <> · <ExtLink to={a.host} className="pointer-events-auto relative">{a.host}</ExtLink></>}
          </div>
        </div>
        <DeployStatus ev={latestEventFor(events.data, name, a.name)} />
        <ChevronRight className="relative size-4 text-muted-foreground/50" />
      </div>
    )
  }

  // Group monorepo members under their shared repo; solo apps stay loose. Order
  // follows the (name-sorted) apps list by each repo's first appearance.
  type Row = { kind: 'app'; app: AppSummary } | { kind: 'repo'; repo: string; apps: AppSummary[] }
  const rows: Row[] = []
  const repoAt = new Map<string, number>()
  for (const a of apps.data ?? []) {
    if (a.repo) {
      const at = repoAt.get(a.repo)
      if (at === undefined) { repoAt.set(a.repo, rows.length); rows.push({ kind: 'repo', repo: a.repo, apps: [a] }) }
      else (rows[at] as Extract<Row, { kind: 'repo' }>).apps.push(a)
    } else {
      rows.push({ kind: 'app', app: a })
    }
  }

  return (
    <>
      <Crumbs><Link to="/projects">Projects</Link> / {name}</Crumbs>
      <PageHead title={name} sub={org}>
        <Button asChild variant="outline" size="sm"><a href={`https://github.com/${org}`} target="_blank" rel="noreferrer">GitHub ↗</a></Button>
        <Button asChild variant="outline" size="sm"><a href={`https://github.com/${org}/ops`} target="_blank" rel="noreferrer">ops repo ↗</a></Button>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/deploys" params={{ org }}>Deployments{pending ? ` · ${pending}` : ''}</Link></Button>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/members" params={{ org }}>Members</Link></Button>
        <Button asChild variant="outline" size="sm"><Link to="/projects/$org/new-service" params={{ org }}><Plus className="size-4" /> New service</Link></Button>
        <Button asChild size="sm"><Link to="/projects/$org/new-app" params={{ org }}><Plus className="size-4" /> New app</Link></Button>
        {isAdmin && <ProjectAdminMenu org={org} name={name} activeApps={apps.data?.length ?? 0} />}
      </PageHead>

      {hasApps && (
        <div className="mb-6 grid gap-3 [grid-template-columns:repeat(auto-fit,minmax(150px,1fr))]">
          <ProjStat n={String(apps.data?.length ?? 0)} l="Apps" />
          <ProjStat n={String(runningCount)} l="Running containers" />
          <ProjStat n={String(pending)} l="Open deployments" />
          <div className="rounded-xl border bg-card p-4">
            <StatusBadge tone={anyFailed ? 'danger' : 'success'} dot>{anyFailed ? 'attention' : 'healthy'}</StatusBadge>
            <div className="mt-2 text-[11px] uppercase tracking-wide text-muted-foreground">Status</div>
          </div>
        </div>
      )}

      <h2 className="mb-3 text-sm font-semibold">Apps</h2>
      <QueryState isLoading={apps.isLoading} error={apps.error}>
        <div className="flex flex-col gap-2.5">
          {apps.data?.length === 0 && importing.length === 0 && <Empty>No apps yet — create one.</Empty>}
          {importing.map((imp) => (
            <Link key={imp.app} to="/projects/$org/apps/$app" params={{ org, app: imp.app }}
              className="flex items-center gap-3.5 rounded-xl border border-dashed bg-card/50 px-4 py-4 transition-colors hover:border-primary/50">
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
          {rows.map((r) => r.kind === 'app' ? appRow(r.app) : (
            <div key={`repo:${r.repo}`} className="rounded-2xl border border-dashed bg-muted/30 p-2">
              <div className="flex items-center gap-2 px-2 py-1.5 text-sm">
                <FolderGit2 className="size-4 text-muted-foreground" />
                <span className="font-semibold">{r.repo}</span>
                <Badge variant="outline" className="text-muted-foreground">monorepo · {r.apps.length} app{r.apps.length === 1 ? '' : 's'}</Badge>
                <a href={`https://github.com/${org}/${r.repo}`} target="_blank" rel="noreferrer"
                  className="ml-auto text-xs text-primary hover:underline">GitHub ↗</a>
              </div>
              <div className="flex flex-col gap-2.5">
                {r.apps.map((a) => appRow(a, a.name.startsWith(`${r.repo}-`) ? a.name.slice(r.repo.length + 1) : a.name))}
              </div>
            </div>
          ))}
        </div>
      </QueryState>

      {(apps.data ?? []).some((a) => a.otel) && (
        <ProjectObservability org={org} apps={apps.data ?? []} containersFor={containersFor} />
      )}

      <ArchivedApps org={org} />
    </>
  )
}

function ProjStat({ n, l }: { n: string; l: string }) {
  return (
    <div className="rounded-xl border bg-card p-4">
      <div className="text-2xl font-semibold tracking-tight tabular-nums">{n}</div>
      <div className="mt-1 text-[11px] uppercase tracking-wide text-muted-foreground">{l}</div>
    </div>
  )
}

// ── Nodes ────────────────────────────────────────────────────────────────────
const ZONE: Record<string, string> = { main: 'control plane', prod: 'public', private: 'internal' }
const gb = (b: number) => `${(b / 1e9).toFixed(1)} GB`
const mb = (b: number) => `${Math.round(b / 1e6)} MB`
function Stat({ label, value }: { label: string; value: string }) {
  return <div><div className="text-muted-foreground">{label}</div><div className="font-mono">{value || '—'}</div></div>
}

const RANGES: { label: string; secs: number | 'live' }[] = [
  { label: 'Live', secs: 'live' },
  { label: '1h', secs: 3600 },
  { label: '6h', secs: 21600 },
  { label: '24h', secs: 86400 },
  { label: '7d', secs: 604800 },
  { label: '30d', secs: 2592000 },
]

// Compact per-container CPU sparkline from persisted history (ADR 0017).
export function ContainerSpark({ container, range, byApp }: { container: string; range: number; byApp?: boolean }) {
  const q = useContainerHistory(range, container, true, byApp)
  const v = (q.data ?? []).map((p) => p.cpu_pct)
  if (v.length < 2) return <span className="text-muted-foreground">—</span>
  const W = 90, H = 18, pad = 2
  const max = Math.max(10, ...v) // scale to the data (CPU% can be small), floor at 10%
  const x = (i: number) => (i / (v.length - 1)) * W
  const y = (val: number) => pad + (1 - Math.min(val, max) / max) * (H - pad * 2)
  const line = v.map((val, i) => `${i ? 'L' : 'M'}${x(i).toFixed(1)} ${y(val).toFixed(1)}`).join(' ')
  const last = v[v.length - 1] ?? 0
  return (
    <span className="inline-flex items-center gap-1.5">
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="h-4 w-[90px]">
        <path d={`${line} L${W} ${H} L0 ${H} Z`} className="fill-primary/15" />
        <path d={line} className="fill-none stroke-primary" strokeWidth={1.5} strokeLinejoin="round" vectorEffect="non-scaling-stroke" />
      </svg>
      <span className="text-muted-foreground">{last.toFixed(0)}%</span>
    </span>
  )
}

export function Nodes() {
  const q = useNodes()
  const m = useNodeMetrics()
  const me = useWhoami().data
  const [range, setRange] = useState<number | 'live'>('live')
  // Live: accumulate a rolling ~10-min window client-side. A chosen range:
  // pull persisted history (ADR 0017) instead.
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

  const isLive = range === 'live'
  const histQ = useMetricsHistory(isLive ? 0 : range, !isLive)
  // Per-node series for the charts, from live accumulation or persisted history.
  const seriesFor = (name: string): { cpu: number[]; mem: number[]; sampleSecs: number } => {
    if (isLive) return { ...(hist[name] ?? { cpu: [], mem: [] }), sampleSecs: 10 }
    const pts = (histQ.data ?? []).filter((p) => p.node === name)
    return {
      cpu: pts.map((p) => p.cpu_pct),
      mem: pts.map((p) => (p.mem_total ? (p.mem_used / p.mem_total) * 100 : 0)),
      sampleSecs: pts.length > 1 ? Math.round((range as number) / (pts.length - 1)) : 60,
    }
  }

  return (
    <>
      <PageHead title="Nodes">
        <div className="inline-flex overflow-hidden rounded-md border text-xs">
          {RANGES.map((r) => (
            <button key={r.label} onClick={() => setRange(r.secs)}
              className={`px-2.5 py-1 font-medium ${range === r.secs ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-muted'}`}>
              {r.label}
            </button>
          ))}
        </div>
      </PageHead>
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
                  {me?.admin && enrolled && (
                    <Link to="/terminal" search={{ mode: 'host', node: n.name }}
                      className="inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground">
                      <TerminalSquare className="size-3.5" /> Open shell
                    </Link>
                  )}
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
                      {(() => {
                        const s = seriesFor(n.name)
                        return <>
                          <MetricChart label="CPU" values={s.cpu} sampleSecs={s.sampleSecs}
                            format={(p) => `${(p / 100 * mm.cpus).toFixed(1)} of ${mm.cpus} cores`} />
                          <MetricChart label="Memory" values={s.mem} sampleSecs={s.sampleSecs}
                            format={(p) => `${gb((p / 100) * mm.mem_total)} of ${gb(mm.mem_total)}`} />
                        </>
                      })()}
                    </div>
                    {mm.apps.length > 0 && (
                      <div className="mt-3 overflow-x-auto">
                        <table className="w-full text-xs">
                          <thead><tr className="text-left text-muted-foreground"><th className="py-1 pr-3 font-medium">container</th><th className="py-1 pr-3 font-medium">state</th><th className="py-1 pr-3 font-medium">cpu</th><th className="py-1 pr-3 font-medium">mem</th><th className="py-1 font-medium">{isLive ? '' : 'cpu trend'}</th></tr></thead>
                          <tbody className="font-mono">
                            {mm.apps.map((a) => (
                              <tr key={a.name} className="border-t">
                                <td className="py-1 pr-3">{a.name}</td>
                                <td className="py-1 pr-3">{a.state}</td>
                                <td className="py-1 pr-3">{a.cpu_pct.toFixed(1)}%</td>
                                <td className="py-1 pr-3">
                                  {mb(a.mem_used)}{a.mem_limit ? ` / ${gb(a.mem_limit)}` : ''}
                                  {a.mem_limit > 0 && (
                                    <div className="mt-0.5 h-1 w-24 overflow-hidden rounded-full bg-muted">
                                      <div className="h-full rounded-full bg-primary" style={{ width: `${Math.min(100, Math.round((a.mem_used / a.mem_limit) * 100))}%` }} />
                                    </div>
                                  )}
                                </td>
                                <td className="py-1">{!isLive && <ContainerSpark container={a.name} range={range as number} />}</td>
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
// ── Activity feed ─────────────────────────────────────────────────────────────
type EvKind = 'deploy' | 'remove' | 'config'
export type EvTone = 'ok' | 'bad' | 'mut' | 'info'
const KIND_LABELS: { k: 'all' | EvKind; label: string }[] = [
  { k: 'all', label: 'All' }, { k: 'deploy', label: 'Deploys' },
  { k: 'remove', label: 'Removals' }, { k: 'config', label: 'Config & repos' },
]

function EvPill({ tone, children }: { tone: EvTone; children: React.ReactNode }) {
  const c = { ok: 'bg-success/15 text-success', bad: 'bg-destructive/15 text-destructive', mut: 'bg-muted text-muted-foreground', info: 'bg-accent text-accent-foreground' }[tone]
  return <span className={`inline-flex items-center rounded-full px-2 py-0.5 align-middle font-mono text-[11.5px] font-medium ${c}`}>{children}</span>
}

// The image/version deployed, from a "deployed <ref> (…)" result: the version
// (vX.Y.Z) if the ref is one, else the short digest.
function deployedRef(result: string): string {
  const ref = result.match(/^deployed\s+(\S+)/)?.[1]
  if (!ref) return ''
  return ref.match(/@sha256:([0-9a-f]{12})/)?.[1] ?? ref
}
const firstToken = (s: string) => s.split(/\s+/)[0] ?? ''

// Map a stored (action, result) event to a feed item. Always renders something
// sensible; unknown kinds fall back to the raw action/result.
export function classify(e: Event): { kind: EvKind; tone: EvTone; Icon: React.ComponentType<{ className?: string }>; line: React.ReactNode; detail: string } {
  const parts = e.action.trim().split(/\s+/)
  const verb = parts[0] ?? ''
  const target = parts.slice(1).join(' ')
  const detail = [e.project, e.node || null, e.commit.slice(0, 7)].filter(Boolean).join(' · ')

  if (e.result.startsWith('FAILED')) {
    const reason = e.result.replace(/^FAILED:?\s*/, '')
    return { kind: verb === 'converge' ? 'deploy' : 'config', tone: 'bad', Icon: AlertCircle,
      line: <><b>{target || e.project}</b> {verb} <EvPill tone="bad">failed</EvPill></>,
      detail: [detail, reason].filter(Boolean).join(' · ') }
  }
  if (verb === 'converge') {
    if (e.result.startsWith('deployed')) return { kind: 'deploy', tone: 'ok', Icon: Rocket, line: <><b>{target}</b> deployed <EvPill tone="ok">{deployedRef(e.result)}</EvPill></>, detail }
    if (e.result === 'in sync') return { kind: 'deploy', tone: 'ok', Icon: CheckCircle2, line: <><b>{target}</b> <EvPill tone="ok">in sync</EvPill></>, detail }
    return { kind: 'deploy', tone: 'mut', Icon: Rocket, line: <><b>{target}</b> {e.result}</>, detail }
  }
  if (verb === 'gc') return { kind: 'remove', tone: 'mut', Icon: Trash2, line: <><b>{target || short(e.result)}</b> <EvPill tone="mut">removed</EvPill></>, detail }

  switch (e.action) {
    case 'app-archived': case 'project-archived': case 'repo-archived':
      return { kind: 'config', tone: 'mut', Icon: Archive, line: <><b>{firstToken(e.result)}</b> <EvPill tone="mut">archived</EvPill></>, detail }
    case 'app-deleted': case 'project-deleted':
      return { kind: 'config', tone: 'bad', Icon: Trash2, line: <><b>{firstToken(e.result)}</b> <EvPill tone="bad">deleted</EvPill></>, detail }
    case 'app-renamed': case 'project-renamed':
      return { kind: 'config', tone: 'info', Icon: PenLine, line: <>Renamed <span className="font-mono">{e.result}</span></>, detail }
    case 'render-pr':
      return { kind: 'config', tone: 'info', Icon: GitPullRequest, line: <>Render PR <span className="font-mono">{e.result}</span></>, detail }
    case 'template-sync':
      return { kind: 'config', tone: 'info', Icon: RefreshCw, line: <>Template sync — {e.result}</>, detail }
    case 'org-sync':
      return { kind: 'config', tone: 'info', Icon: RefreshCw, line: <>Org sync — {e.result}</>, detail }
    case 'repo-unarchived':
      return { kind: 'config', tone: 'info', Icon: Archive, line: <><b>{e.result}</b> unarchived</>, detail }
  }
  return { kind: 'config', tone: 'mut', Icon: Circle, line: <>{e.action}{e.result ? <> — {e.result}</> : null}</>, detail }
}

export function relTime(at: string): string {
  const t = parseAt(at)
  if (Number.isNaN(t)) return at
  const s = Math.max(0, (Date.now() - t) / 1000)
  if (s < 60) return 'just now'
  if (s < 3600) return `${Math.floor(s / 60)} min ago`
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`
  const d = Math.floor(s / 86400)
  return d === 1 ? 'yesterday' : `${d} d ago`
}
function dayLabel(at: string): string {
  const t = parseAt(at)
  if (Number.isNaN(t)) return 'Earlier'
  const d = new Date(t), now = new Date()
  const same = (a: Date, b: Date) => a.toDateString() === b.toDateString()
  if (same(d, now)) return 'Today'
  const y = new Date(now); y.setDate(now.getDate() - 1)
  if (same(d, y)) return 'Yesterday'
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', ...(d.getFullYear() !== now.getFullYear() ? { year: 'numeric' } : {}) })
}
export const DOT_TONE: Record<EvTone, string> = {
  ok: 'border-success/60 text-success', bad: 'border-destructive/60 text-destructive',
  info: 'border-primary/55 text-primary', mut: 'border-border text-muted-foreground',
}

export function Activity() {
  const recon = useEvents(100)
  const bot = useBotEvents()
  const [kind, setKind] = useState<'all' | EvKind>('all')
  const [proj, setProj] = useState('all')
  // Merge the reconciler (converge/gc/purge) + bot (archive/rename/render-PR/
  // template-sync/org-sync) event streams into one time-ordered feed.
  const events = [...(recon.data ?? []), ...(bot.data ?? [])].sort((a, b) => parseAt(b.at) - parseAt(a.at))
  const projectList = [...new Set(events.map((e) => e.project).filter(Boolean))].sort()
  // Prefer the stored `kind` (set at write time); fall back to the classifier.
  const kindOf = (e: Event): EvKind => (e.kind as EvKind) || classify(e).kind
  const filtered = events.filter((e) => (proj === 'all' || e.project === proj) && (kind === 'all' || kindOf(e) === kind))

  const groups: { day: string; items: Event[] }[] = []
  for (const e of filtered) {
    const day = dayLabel(e.at)
    const last = groups[groups.length - 1]
    if (last && last.day === day) last.items.push(e)
    else groups.push({ day, items: [e] })
  }

  return (
    <>
      <PageHead title="Activity">
        {projectList.length > 1 && (
          <select value={proj} onChange={(e) => setProj(e.target.value)}
            className="h-8 rounded-md border bg-card px-2.5 text-xs">
            <option value="all">All projects</option>
            {projectList.map((p) => <option key={p} value={p}>{p}</option>)}
          </select>
        )}
        <div className="inline-flex gap-0.5 rounded-lg bg-muted p-0.5">
          {KIND_LABELS.map(({ k, label }) => (
            <button key={k} onClick={() => setKind(k)}
              className={`h-7 rounded-md px-2.5 text-xs font-medium transition-colors ${kind === k ? 'bg-card text-foreground shadow-sm' : 'text-muted-foreground hover:text-foreground'}`}>
              {label}
            </button>
          ))}
        </div>
      </PageHead>

      <QueryState isLoading={recon.isLoading && bot.isLoading} error={recon.error && bot.error ? recon.error : null}>
        {filtered.length === 0 && <Empty>No matching activity.</Empty>}
        {groups.map((g, gi) => (
          <div key={gi}>
            <div className="mb-1.5 ml-11 mt-5 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground first:mt-1">{g.day}</div>
            <div className="relative pl-11 before:absolute before:left-[17px] before:top-2 before:bottom-2 before:w-px before:bg-border">
              {g.items.map((e, i) => {
                const c = classify(e)
                return (
                  <div key={i} className="relative flex items-baseline gap-3 rounded-lg px-3 py-2 hover:bg-muted/50">
                    <div className={`absolute -left-[34px] top-1.5 grid size-7 place-items-center rounded-full border-2 bg-card ${DOT_TONE[c.tone]}`}><c.Icon className="size-3.5" /></div>
                    <div className="min-w-0 flex-1">
                      <div className="leading-snug">{c.line}</div>
                      <div className="mt-0.5 truncate font-mono text-xs text-muted-foreground">{c.detail}</div>
                    </div>
                    <div className="shrink-0 whitespace-nowrap text-xs text-muted-foreground tabular-nums" title={e.at}>{relTime(e.at)}</div>
                  </div>
                )
              })}
            </div>
          </div>
        ))}
      </QueryState>
    </>
  )
}
