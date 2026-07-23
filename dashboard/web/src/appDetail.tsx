import { useEffect, useState } from 'react'
import { Link, Outlet, useNavigate, useParams } from '@tanstack/react-router'
import {
  RotateCw, ScrollText, SlidersHorizontal, ArrowUpFromLine, MoreVertical, TerminalSquare,
  LayoutGrid, Activity as ActivityIcon, Tag, Rocket,
} from 'lucide-react'
import {
  send, urls, useApps, useAppContainers, useAppInfo, useAppLogs, useAppSecrets, useDeployProgress, useEvents, useImports,
  useManifest, useNodeMetrics, useProjects, useReleases, useReleaseConfig, useReleaseDraft, useServiceReleases, useWhoami,
  type AppInfo, type Autorelease, type ManifestFile, type ReleaseConfig,
} from './api'
import { useApiMutation } from './mutations'
import { ConfirmButton, ExtLink, QueryState, short, StatusBadge } from './ui'
import { Crumbs, ContainerSpark, ImportSteps, DeploySteps } from './views'
import { fromData, ManifestForm, OverlayForm, overriddenKeys, toManifest, toOverlay, type ManifestDraft } from './manifestForm'
import { Observability } from './observability'
import { ENV_CLASSES, setEnv, useEnv } from './env'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Sheet, SheetBody, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet'


// Replace a full `ghcr.io/…@sha256:…` image ref in a deploy-event result with the
// version the app reports at /info (else a short digest) — older events predate
// versioned deploy events.
const IMG_REF = /ghcr\.io\/\S+@sha256:[0-9a-f]{64}/
function resultVersioned(result: string, rows?: AppInfo[]): string {
  const m = result.match(IMG_REF)
  if (!m) return result
  const v = rows?.find((r) => r.class === 'production' && typeof r.info?.version === 'string')?.info?.version
    ?? rows?.find((r) => typeof r.info?.version === 'string')?.info?.version
  const repl = (typeof v === 'string' && v) || m[0].split('@sha256:')[1]?.slice(0, 12) || m[0]
  return result.replace(IMG_REF, String(repl))
}

// The next version each bump would create, from the highest recorded release
// (mirrors the bot's next_version; advisory — the server recomputes on cut).
function nextVersions(versions: string[]): { patch: string; minor: string; major: string } {
  const parse = (t: string): [number, number, number] | null => {
    const m = /^v(\d+)\.(\d+)\.(\d+)/.exec(t)
    return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null
  }
  const cmp = (a: number[], b: number[]) => a[0]! - b[0]! || a[1]! - b[1]! || a[2]! - b[2]!
  const [x, y, z] = versions
    .map(parse)
    .filter((v): v is [number, number, number] => v != null)
    .reduce<[number, number, number]>((mx, v) => (cmp(v, mx) > 0 ? v : mx), [0, 0, 0])
  return { patch: `v${x}.${y}.${z + 1}`, minor: `v${x}.${y + 1}.0`, major: `v${x + 1}.0.0` }
}

/** Shared per-app derived state for the layout + every section. React Query
 *  dedupes the underlying fetches, so each section can call this freely. The
 *  selected environment comes from the global, persisted store (`env.ts`). `env`
 *  is the raw global selection (may be a class this app hasn't onboarded — the
 *  Configuration Add-flow needs that); `shownEnv` clamps it to a class the app has
 *  for read-only display sections. */
function useAppView() {
  const { org, app } = useParams({ from: '/projects/$org/apps/$app' })
  const env = useEnv()
  const apps = useApps(org)
  const a = apps.data?.find((x) => x.name === app)
  const info = useAppInfo(org, app)
  const metrics = useNodeMetrics()
  const project = useProjects().data?.find((p) => p.org === org)?.name
  const isAdmin = useWhoami().data?.admin ?? false
  const isService = !!a?.service
  const classes: string[] = ENV_CLASSES.filter((c) => a?.classes.includes(c))
  const shownEnv = classes.includes(env) ? env : (classes[0] ?? 'production')
  const containersFor = (cls: string) =>
    project ? (metrics.data ?? []).flatMap((n) => n.apps).filter((c) => c.name.startsWith(`${project}-${app}-${cls}-`)) : []
  const versionFor = (cls: string): string | null => {
    const v = info.data?.find((r) => r.class === cls)?.info?.version
    return typeof v === 'string' ? v : null
  }
  return { org, app, a, project, isAdmin, isService, classes, env: shownEnv, rawEnv: env, info, containersFor, versionFor }
}

const digestShort = (img?: string) => img?.split('@sha256:')[1]?.slice(0, 7) ?? null

// The app-detail sections, as a tab bar over nested routes. Releases lives in the
// top-bar Releases popover (+ an Overview link), Deployments in the top-bar
// Deployments popover — so neither is a tab here.
const TABS = [
  { to: '/projects/$org/apps/$app', label: 'Overview', icon: LayoutGrid, exact: true },
  { to: '/projects/$org/apps/$app/config', label: 'Configuration', icon: SlidersHorizontal, exact: false },
  { to: '/projects/$org/apps/$app/deploys', label: 'Deployments', icon: Rocket, exact: false },
  { to: '/projects/$org/apps/$app/releases', label: 'Releases', icon: Tag, exact: false },
  { to: '/projects/$org/apps/$app/observability', label: 'Observability', icon: ActivityIcon, exact: false },
] as const

/** App-detail layout: header + actions + tab bar + the active section (Outlet).
 *  The environment selector now lives in the global top bar (`?env=`). */
export function AppDetail() {
  const { org, app } = useParams({ from: '/projects/$org/apps/$app' })
  const apps = useApps(org)
  const a = apps.data?.find((x) => x.name === app)
  const imports = useImports(org)
  const imp = imports.data?.find((x) => x.app === app)
  const isService = !!a?.service
  const [renameOpen, setRenameOpen] = useState(false)
  const [moveOpen, setMoveOpen] = useState(false)
  const navigate = useNavigate()
  const deploy = useApiMutation({ invalidate: [['deploys', org], ['events']] })
  const retry = useApiMutation({ invalidate: [['imports', org], ['apps', org]] })
  const archive = useApiMutation({
    invalidate: [['apps', org], ['archived', org], ['events']],
    onDone: () => navigate({ to: '/projects/$org', params: { org } }),
  })

  return (
    <>
      <Crumbs>
        <Link to="/projects">Projects</Link> / <Link to="/projects/$org" params={{ org }}>{org}</Link> / {app}
      </Crumbs>

      {/* ── app header ─────────────────────────────────────────────────────── */}
      <div className="flex flex-wrap items-start gap-4">
        <div className="min-w-0">
          <h1 className="flex flex-wrap items-center gap-2 text-2xl font-semibold tracking-tight">
            {app}
            {isService && <StatusBadge tone="accent">service · {a?.service}</StatusBadge>}
          </h1>
          <div className="mt-1 truncate text-sm text-muted-foreground">
            {[short(a?.image), a?.database].filter(Boolean).join('  ·  ') || '—'}
          </div>
        </div>
        <div className="ml-auto flex items-center gap-2">
          {!isService && (
            <Button asChild variant="outline" size="sm">
              <a href={`https://github.com/${org}/${a?.repo ?? app}`} target="_blank" rel="noreferrer">GitHub ↗</a>
            </Button>
          )}
          <ConfirmButton variant="outline" size="sm" title={`Roll back ${app}?`}
            description={`Revert the last change on ${org}/ops.`} confirmText="Roll back"
            onConfirm={() => deploy.mutate(() => send(urls.rollback(org)))}>
            Roll back
          </ConfirmButton>
          {!isService && (
            <ConfirmButton size="sm" title={`Promote ${app} to production?`}
              description="An admin still merges the render PR in Deployments." confirmText="Promote"
              onConfirm={() => deploy.mutate(() => send(urls.promote(org, app)))}>
              <ArrowUpFromLine className="size-4" /> Promote
            </ConfirmButton>
          )}
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="outline" size="icon" className="size-9" aria-label="More actions">
                <MoreVertical className="size-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              {/* Rename would move apps/<name>/ but not the services: entry — hide
                  it for services until rename handles them. */}
              {!isService && <DropdownMenuItem onSelect={() => setRenameOpen(true)}>Rename app…</DropdownMenuItem>}
              {!isService && <DropdownMenuItem onSelect={() => setMoveOpen(true)}>Move to project…</DropdownMenuItem>}
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => archive.mutate(() => send(urls.appArchive(org, app)))}>
                {isService ? 'Archive service' : 'Archive app'}
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      {imp && (
        <Card className="mt-5"><CardContent className="pt-6">
          <h2 className="mb-3 text-sm font-semibold">{imp.status === 'failed' ? 'Import failed' : 'Importing…'}</h2>
          <ImportSteps status={imp} />
          {imp.status === 'failed' && (
            <div className="mt-3 flex items-center gap-3">
              <Button size="sm" disabled={retry.isPending}
                onClick={() => retry.mutate(() => send(urls.importRetry(org, app)))}>Retry import</Button>
              <span className="text-xs text-muted-foreground">Re-runs from the stored request; re-enter secrets via the form.</span>
            </div>
          )}
        </CardContent></Card>
      )}

      {/* ── section tab bar ────────────────────────────────────────────────── */}
      <div className="mt-6 flex flex-wrap gap-1 border-b">
        {TABS.map((t) => (
          <Link key={t.to} to={t.to} params={{ org, app }} activeOptions={{ exact: t.exact }}
            className="-mb-px inline-flex items-center gap-2 border-b-2 border-transparent px-3.5 py-2.5 text-[13.5px] font-medium text-muted-foreground hover:text-foreground"
            activeProps={{ className: '-mb-px inline-flex items-center gap-2 border-b-2 border-primary px-3.5 py-2.5 text-[13.5px] font-medium text-foreground' }}>
            <t.icon className="size-4" /> {t.label}
          </Link>
        ))}
      </div>

      <div className="pt-6"><Outlet /></div>

      <RenameDialog org={org} app={app} stateful={!!a?.database} open={renameOpen} onOpenChange={setRenameOpen} />
      <MoveDialog org={org} app={app} stateful={!!a?.database} open={moveOpen} onOpenChange={setMoveOpen} />
    </>
  )
}

/** Overview section: the selected environment's live status + a comparison of
 *  all environments + this app's recent deploys. */
export function AppOverview() {
  const { org, app, a, project, isAdmin, classes, env, containersFor, versionFor } = useAppView()
  const [logsOpen, setLogsOpen] = useState(false)
  const act = useApiMutation({ invalidate: [['events']] })
  const adminerUrl =
    project && a?.database && a.classes.includes('production')
      ? `https://adminer.prod.majksa.net/?pgsql=majnet-postgres&db=${`${project}_${app}_production`.replace(/-/g, '_')}`
      : null

  return (
    <>
      {classes.length > 0 && (
        <EnvironmentZone
          app={app} env={env} org={org} isAdmin={isAdmin}
          containers={containersFor(env)}
          version={versionFor(env)}
          domains={env === 'production' ? (a?.domains ?? []) : []}
          adminerUrl={env === 'production' ? adminerUrl : null}
          onLogs={() => setLogsOpen(true)}
          restart={() => act.mutate(() => send(urls.restart(org, env, app)))} busy={act.isPending}
        />
      )}

      {classes.length > 1 && <AllEnvironments classes={classes} env={env} containersFor={containersFor} versionFor={versionFor} />}

      <Sheet open={logsOpen} onOpenChange={setLogsOpen}>
        <SheetContent>
          <SheetHeader><SheetTitle>Logs</SheetTitle><StatusBadge tone="accent">{env}</StatusBadge></SheetHeader>
          <SheetBody>{logsOpen && <LogsView org={org} app={app} cls={env} />}</SheetBody>
        </SheetContent>
      </Sheet>
    </>
  )
}

/** Deployments section: the live rollout stepper for the selected env (deploy
 *  trackability) + this app's recent deploy events. */
export function AppDeployments() {
  const { app, project, env, info } = useAppView()
  const events = useEvents()
  const appEvents = (events.data ?? []).filter((e) => e.action.trim().split(/\s+/).pop() === app)
  const progress = useDeployProgress()
  const dp = (progress.data ?? []).find((d) => d.project === project && d.app === app && d.class === env)
  const showDeploy = dp && (dp.status === 'active' || Date.now() / 1000 - dp.updated_at < 120)

  return (
    <>
      {showDeploy && dp ? (
        <Card className="mb-4 border-primary/40"><CardContent className="pt-6">
          <div className="mb-3 flex items-center gap-2">
            <h2 className="text-sm font-semibold">
              {dp.status === 'failed' ? 'Deploy failed' : dp.status === 'done' ? 'Deployed' : 'Deploying…'}
            </h2>
            <StatusBadge tone="accent">{env}</StatusBadge>
            {dp.detail && <span className="truncate font-mono text-xs text-muted-foreground">{dp.detail}</span>}
          </div>
          <DeploySteps p={dp} />
        </CardContent></Card>
      ) : (
        <Card className="mb-4"><CardContent className="py-6 text-sm text-muted-foreground">
          No deploy in progress for <StatusBadge tone="accent">{env}</StatusBadge>. A rollout appears here live while it runs.
        </CardContent></Card>
      )}

      <SectionHead title="Recent deploys" />
      {appEvents.length === 0 ? (
        <Card><CardContent className="py-6 text-sm text-muted-foreground">No deploys recorded yet.</CardContent></Card>
      ) : (
        <Card><CardContent className="pt-6">
          <Table>
            <TableHeader><TableRow><TableHead>time</TableHead><TableHead>node</TableHead><TableHead>action</TableHead><TableHead>result</TableHead><TableHead>commit</TableHead></TableRow></TableHeader>
            <TableBody className="font-mono text-xs">
              {appEvents.slice(0, 12).map((e, i) => (
                <TableRow key={i}>
                  <TableCell>{e.at}</TableCell><TableCell>{e.node}</TableCell><TableCell>{e.action}</TableCell>
                  <TableCell className={e.result.startsWith('FAILED') ? 'text-destructive' : ''}>{resultVersioned(e.result, info.data)}</TableCell>
                  <TableCell>{e.commit.slice(0, 12)}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent></Card>
      )}
    </>
  )
}

// The all-environments comparison; clicking a card switches the global env store,
// so the Overview + every section (+ the top-bar selector) follow it.
function AllEnvironments({ classes, env, containersFor, versionFor }: {
  classes: string[]; env: string
  containersFor: (cls: string) => { image: string }[]
  versionFor: (cls: string) => string | null
}) {
  const pick = (c: string) => setEnv(c as (typeof ENV_CLASSES)[number])
  return (
    <>
      <SectionHead title="All environments" hint="click to switch the environment selector" />
      <div className="grid gap-3" style={{ gridTemplateColumns: `repeat(${Math.min(classes.length, 4)}, minmax(0, 1fr))` }}>
        {classes.map((c) => {
          const ver = versionFor(c) ?? digestShort(containersFor(c)[0]?.image) ?? null
          const live = containersFor(c).length > 0
          return (
            <button key={c} onClick={() => pick(c)}
              className={`flex flex-col gap-1.5 rounded-lg border p-3.5 text-left transition-colors hover:border-primary/50 ${
                env === c ? 'border-primary/60 ring-1 ring-primary/40' : ''}`}>
              <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{c}</span>
              <span className="font-mono text-base font-semibold">{ver ?? '—'}</span>
              {live
                ? <StatusBadge tone="success" dot>healthy</StatusBadge>
                : <StatusBadge tone="muted">not deployed</StatusBadge>}
            </button>
          )
        })}
      </div>
    </>
  )
}

/** Configuration section: the manifest editor (base + per-env overlays, inline
 *  secrets) — was a slide-over drawer, now a first-class section. */
const parseDigest = (img?: string) => { const d = img?.split('@')[1]; return d?.startsWith('sha256:') ? d : undefined }
const digestOf = (f?: ManifestFile) => (f?.data as { digest?: string } | null)?.digest

export function AppConfiguration() {
  const { org, app, rawEnv, classes } = useAppView()
  const imports = useImports(org)
  const imp = imports.data?.find((x) => x.app === app)
  const manifest = useManifest(org, app)
  const releases = useReleases(org, app)
  const files = manifest.data
  // Seed digest for the "Add {env}" flow: newest release, else an existing overlay's pin.
  const seedDigest = parseDigest(releases.data?.[0]?.app_image)
    ?? digestOf(files?.['production.yaml']) ?? digestOf(files?.['stable.yaml']) ?? ''
  return (
    <>
      <SectionHead title="Configuration" hint="shared base + per-environment overrides" />
      <QueryState isLoading={manifest.isLoading} error={imp && !manifest.data ? undefined : manifest.error}>
        {files && <ManifestEditor org={org} app={app} files={files} env={rawEnv} classes={classes} seedDigest={seedDigest} />}
      </QueryState>
    </>
  )
}

/** Observability section (ADR 0023): RED + traces + logs for the selected env. */
export function AppObservability() {
  const { org, app, a, classes, env, containersFor } = useAppView()
  if (!a?.otel) {
    return (
      <Card><CardContent className="py-12 text-center text-sm text-muted-foreground">
        This app hasn’t opted into OpenTelemetry. Set <code className="font-mono">otel: true</code> in Configuration to
        emit traces &amp; logs.
      </CardContent></Card>
    )
  }
  if (!classes.includes(env)) return null
  return <Observability org={org} app={app} cls={env} containers={containersFor(env)} />
}

/** Releases section (its own route, reached from the top-bar Releases popover +
 *  the Overview link): cut/draft controls + the release history list. A service
 *  shows its upstream versions instead. */
export function AppReleases() {
  const { org, app, a, isService, versionFor } = useAppView()
  const manifest = useManifest(org, app)
  const imageOf = (f?: ManifestFile) => (f?.data as { image?: string } | null)?.image
  const prodImage = imageOf(manifest.data?.['production.yaml']) ?? imageOf(manifest.data?.['base.yaml'])
  if (isService) return <ServiceVersions org={org} app={app} current={versionFor('production')} />
  return <Releases org={org} app={app} repo={a?.repo} prodImage={prodImage} />
}

function SectionHead({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="mb-3 mt-8 flex items-baseline gap-2.5">
      <h2 className="text-sm font-semibold">{title}</h2>
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// ── the environment section, scoped to the selected class ─────────────────────
function EnvironmentZone({
  app, env, org, isAdmin, containers, version, domains, adminerUrl, onLogs, restart, busy,
}: {
  app: string; env: string; org: string; isAdmin: boolean
  containers: { name: string; image: string; state: string; cpu_pct: number; mem_used: number; mem_limit: number }[]
  version: string | null; domains: string[]; adminerUrl: string | null
  onLogs: () => void; restart: () => void; busy: boolean
}) {
  const running = containers.length > 0
  const cpu = containers.reduce((s, c) => s + c.cpu_pct, 0)
  const mem = containers.reduce((s, c) => s + c.mem_used, 0)

  if (!running) {
    return (
      <>
        <SectionHead title="Environment" hint={env} />
        <Card><CardContent className="flex flex-col items-center gap-2 py-12 text-center">
          <StatusBadge tone="muted" dot>not deployed</StatusBadge>
          <div className="text-sm">{app} isn’t running in <b>{env}</b>{version ? <> — last built <span className="font-mono">{version}</span></> : null}.</div>
          <div className="text-xs text-muted-foreground">
            {env === 'production'
              ? 'Promote a release to deploy it here.'
              : `Add a ${env}.yaml overlay in Configuration to deploy it here.`}
          </div>
        </CardContent></Card>
        <PreviousGenerations org={org} env={env} app={app} isAdmin={isAdmin} />
      </>
    )
  }

  return (
    <>
      <SectionHead title="Environment" hint={`${env}${domains[0] ? ` · ${domains[0]}` : ''}`} />
      <div className="grid gap-3.5 md:grid-cols-[1.3fr_1fr]">
        <Card><CardContent className="pt-6">
          <div className="flex flex-wrap items-start gap-4">
            <div className="min-w-0">
              <div className="text-xs text-muted-foreground">Running version</div>
              <div className="mt-0.5 font-mono text-2xl font-semibold tracking-tight">{version ?? digest(containers[0]?.image)}</div>
            </div>
            <div className="ml-auto flex flex-col items-end gap-2">
              <StatusBadge tone="success" dot>deployed · healthy</StatusBadge>
              {domains.map((d) => <ExtLink key={d} to={d} className="font-mono text-xs" />)}
            </div>
          </div>
          <div className="mt-4 flex flex-wrap gap-2">
            <ConfirmButton variant="outline" size="sm" disabled={busy}
              title={`Restart ${app} in ${env}?`} description="Bounces the container(s) — same digest, brief downtime."
              confirmText="Restart" onConfirm={restart}><RotateCw className="size-4" /> Restart</ConfirmButton>
            <Button variant="outline" size="sm" onClick={onLogs}><ScrollText className="size-4" /> Logs</Button>
            {isAdmin && (
              <Button asChild variant="outline" size="sm">
                <Link to="/terminal" search={{ mode: 'container', project: org, app, class: env }}><TerminalSquare className="size-4" /> Exec</Link>
              </Button>
            )}
            {adminerUrl && <Button asChild variant="outline" size="sm"><a href={adminerUrl} target="_blank" rel="noreferrer">Open in Adminer ↗</a></Button>}
          </div>
        </CardContent></Card>

        <Card><CardContent className="pt-6">
          <div className="flex gap-7">
            <Metric n={String(containers.length)} l="Containers" />
            <Metric n={`${cpu.toFixed(1)}%`} l="CPU" />
            <Metric n={`${Math.round(mem / 1e6)} MB`} l="Memory" />
          </div>
          <div className="mt-3 overflow-x-auto">
            <table className="w-full text-xs">
              <thead><tr className="text-left text-muted-foreground"><th className="py-1 pr-3 font-medium">container</th><th className="py-1 pr-3 font-medium">state</th><th className="py-1 pr-3 font-medium">cpu</th><th className="py-1 pr-3 font-medium">mem</th><th className="py-1 font-medium">cpu · 1h</th></tr></thead>
              <tbody className="font-mono">
                {containers.map((c) => (
                  <tr key={c.name} className="border-t">
                    <td className="py-1 pr-3">{c.name}</td><td className="py-1 pr-3">{c.state}</td>
                    <td className="py-1 pr-3 tabular-nums">{c.cpu_pct.toFixed(1)}%</td>
                    <td className="py-1 pr-3 tabular-nums">{(c.mem_used / 1e6).toFixed(0)} MB{c.mem_limit ? ` / ${(c.mem_limit / 1e9).toFixed(1)} GB` : ''}</td>
                    <td className="py-1"><ContainerSpark container={c.name} range={3600} byApp /></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </CardContent></Card>
      </div>
      <PreviousGenerations org={org} env={env} app={app} isAdmin={isAdmin} />
    </>
  )
}
const digest = (img?: string) => img?.split('@sha256:')[1]?.slice(0, 7) ?? '—'

// Stopped/old containers from earlier deploys (blue-green leaves the previous
// generation behind until the next converge GCs it). Production is admin-gated.
function PreviousGenerations({ org, env, app, isAdmin }: { org: string; env: string; app: string; isAdmin: boolean }) {
  const q = useAppContainers(org, env, app, env !== 'production' || isAdmin)
  const prev = (q.data ?? []).filter((c) => c.state !== 'running')
  if (prev.length === 0) return null
  return (
    <div className="mt-3.5">
      <SectionHead title="Previous generations" hint="stopped containers from earlier deploys" />
      <Card><CardContent className="overflow-x-auto pt-6">
        <table className="w-full text-xs">
          <thead><tr className="text-left text-muted-foreground"><th className="py-1 pr-3 font-medium">container</th><th className="py-1 pr-3 font-medium">state</th><th className="py-1 pr-3 font-medium">image</th><th className="py-1 font-medium">status</th></tr></thead>
          <tbody className="font-mono">
            {prev.map((c) => (
              <tr key={c.name} className="border-t">
                <td className="py-1 pr-3">{c.name}</td>
                <td className="py-1 pr-3"><StatusBadge tone="muted">{c.state}</StatusBadge></td>
                <td className="py-1 pr-3">{digest(c.image)}</td>
                <td className="py-1 text-muted-foreground">{c.status}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </CardContent></Card>
    </div>
  )
}

function Metric({ n, l }: { n: string; l: string }) {
  return <div className="flex flex-col gap-0.5"><span className="text-xl font-semibold tracking-tight tabular-nums">{n}</span><span className="text-[11px] uppercase tracking-wide text-muted-foreground">{l}</span></div>
}

// A bot-prepared draft release: the proposed next version + a generated
// changelog, refreshed on each push to the app repo's main, waiting for an
// operator to submit it. Submitting tags the repo (the cut→CI→record flow).
export function DraftCard({ org, app }: { org: string; app: string }) {
  const q = useReleaseDraft(org, app)
  const m = useApiMutation({ invalidate: [['releaseDraft', org, app], ['releases', org, app], ['releaseDrafts'], ['deploys', org], ['events']] })
  const draft = q.data
  const [notes, setNotes] = useState('')
  const [dirty, setDirty] = useState(false)
  // Resync the editor when the server draft changes, unless there are local edits.
  useEffect(() => {
    if (draft && !dirty) setNotes(draft.notes)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft?.updated_at, draft?.notes])
  if (q.isLoading || q.error) return null
  if (!draft) {
    return (
      <div className="mb-3">
        <Button variant="outline" size="sm" disabled={m.isPending}
          title="Compute a draft release from the commits since the last release"
          onClick={() => m.mutate(() => send(urls.releaseDraftRefresh(org, app)))}>Prepare release draft</Button>
      </div>
    )
  }
  const saveNotes = () => send(urls.releaseDraftNotes(org, app), { method: 'PUT', json: { notes } })
  return (
    <div className="mb-3 rounded-lg border border-primary/40 bg-accent/30 p-4">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <span className="font-medium">Draft release {draft.version}</span>
        <StatusBadge tone="accent">{draft.bump}</StatusBadge>
        <span className="text-xs text-muted-foreground">
          {draft.commit_count > 0
            ? `${draft.commit_count} commit${draft.commit_count === 1 ? '' : 's'}${draft.base ? ` since ${draft.base}` : ''}`
            : 'first release'}
        </span>
        <span className="ml-auto text-xs text-muted-foreground">refreshes on push · waits for you to release</span>
      </div>
      <Textarea value={notes} onChange={(e) => { setNotes(e.target.value); setDirty(true) }}
        className="min-h-40 font-mono text-xs" aria-label="Release notes" />
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <ConfirmButton size="sm" title={`Release ${draft.version}?`}
          description="Tags the repo at main HEAD; CI builds it and it appears in Releases. Promote to production separately."
          confirmText={`Release ${draft.version}`}
          onConfirm={() => m.mutate(async () => {
            if (dirty) await saveNotes()
            setDirty(false)
            return send(urls.releaseDraftSubmit(org, app))
          })}>Release {draft.version} ▶</ConfirmButton>
        <Button variant="outline" size="sm" disabled={m.isPending || !dirty}
          onClick={() => m.mutate(async () => { const r = await saveNotes(); setDirty(false); return r })}>Save notes</Button>
        <Button variant="outline" size="sm" disabled={m.isPending}
          title="Recompute the version + changelog from the latest commits (keeps your edited notes)"
          onClick={() => m.mutate(async () => { const r = await send(urls.releaseDraftRefresh(org, app)); setDirty(false); return r })}>Refresh</Button>
        <Button variant="ghost" size="sm" disabled={m.isPending} className="ml-auto text-muted-foreground"
          title="Discard this draft (it re-prepares on the next push)"
          onClick={() => m.mutate(async () => { const r = await send(urls.releaseDraft(org, app), { method: 'DELETE' }); setDirty(false); return r })}>Discard</Button>
      </div>
    </div>
  )
}

// Per-app release policy (ADR 0020): a scope enables per-app scoped tags
// (`@<scope>/<leaf>@vX.Y.Z`); autorelease auto-cuts on merge for matched paths.
// GitOps — saved into project.yaml via a plain commit (no PR). Admin-gated.
function ReleaseSettings({ org, app, repo }: { org: string; app: string; repo?: string | null }) {
  const q = useReleaseConfig(org, app)
  const isAdmin = useWhoami().data?.admin ?? false
  const m = useApiMutation({ invalidate: [['releaseConfig', org, app], ['releaseDraft', org, app], ['releaseDrafts'], ['events']] })
  const cfg = q.data
  const [scope, setScope] = useState('')
  const [auto, setAuto] = useState<Autorelease>('off')
  const [paths, setPaths] = useState('')
  const [dirty, setDirty] = useState(false)
  // Resync the editor from the server config unless there are local edits.
  useEffect(() => {
    if (dirty) return
    setScope(cfg?.scope ?? '')
    setAuto(cfg?.autorelease ?? 'off')
    setPaths((cfg?.paths ?? []).join('\n'))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cfg?.scope, cfg?.autorelease, (cfg?.paths ?? []).join('\n')])
  if (q.isLoading || q.error) return null
  const save = () => send(urls.releaseConfig(org, app), {
    method: 'PUT',
    json: {
      scope: scope.trim() || null,
      autorelease: auto,
      paths: paths.split('\n').map((s) => s.trim()).filter(Boolean),
      // Preserve any bump-rule override (edited via project.yaml) — this editor
      // doesn't expose it, so pass it through rather than wiping it.
      bumps: cfg?.bumps ?? null,
    } satisfies ReleaseConfig,
  })
  const perApp = !!cfg?.scope
  return (
    <details className="mb-3 rounded-lg border px-4 py-2.5 text-sm">
      <summary className="cursor-pointer font-medium">
        Release settings
        <span className="ml-2 text-xs font-normal text-muted-foreground">
          {perApp ? `per-app · @${cfg!.scope}/<leaf>@vX.Y.Z` : 'repo-wide vX.Y.Z'}
          {cfg && cfg.autorelease !== 'off' ? ` · autorelease ${cfg.autorelease}` : ''}
        </span>
      </summary>
      <div className="mt-3 flex flex-col gap-3">
        <label className="flex flex-col gap-1">
          <span className="text-xs text-muted-foreground">Tag scope — empty = repo-wide <code className="font-mono">vX.Y.Z</code>; set (e.g. <code className="font-mono">{repo ?? 'myscope'}</code>) for per-app <code className="font-mono">@scope/&lt;leaf&gt;@vX.Y.Z</code></span>
          <Input value={scope} disabled={!isAdmin} placeholder={repo ? `${repo} (per-app)` : '(repo-wide)'} onChange={(e) => { setScope(e.target.value); setDirty(true) }} />
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-xs text-muted-foreground">Autorelease on merge to main</span>
          <Select value={auto} onValueChange={(v) => { setAuto(v as Autorelease); setDirty(true) }} disabled={!isAdmin}>
            <SelectTrigger className="w-56"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="off">off — manual cuts only</SelectItem>
              <SelectItem value="patch">patch — always patch</SelectItem>
              <SelectItem value="auto">auto — conventional commits</SelectItem>
            </SelectContent>
          </Select>
        </label>
        <label className="flex flex-col gap-1">
          <span className="text-xs text-muted-foreground">Autorelease paths — one glob per line; a merge touching one cuts this app</span>
          <Textarea className="min-h-24 font-mono text-xs" value={paths} disabled={!isAdmin}
            placeholder={'applications/server/**\npackages/shared/**'}
            onChange={(e) => { setPaths(e.target.value); setDirty(true) }} aria-label="Autorelease paths" />
        </label>
        <div className="flex flex-col gap-1">
          <span className="text-xs text-muted-foreground">Bump rules — commit type → semver bump for auto/changelog (breaking is always major; set in <code className="font-mono">project.yaml</code> <code className="font-mono">release.bumps</code>)</span>
          <span className="font-mono text-xs">
            {cfg?.bumps && Object.keys(cfg.bumps).length > 0
              ? Object.entries(cfg.bumps).map(([t, b]) => `${t}→${b}`).join(', ')
              : 'feat→minor, fix→patch (default)'}
          </span>
        </div>
        <div className="flex justify-end">
          <Button size="sm" disabled={!isAdmin || !dirty || m.isPending}
            onClick={() => m.mutate(async () => { const r = await save(); setDirty(false); return r })}>Save settings</Button>
        </div>
      </div>
    </details>
  )
}

/** Version + upstream releases + promote for a service (ADR 0021). The running
 *  version comes from the reconciler's OCI-label capture (`current`); available
 *  versions from the image repo's registry tags. */
function ServiceVersions({ org, app, current }: { org: string; app: string; current: string | null }) {
  const q = useServiceReleases(org, app)
  const m = useApiMutation({ invalidate: [['deploys'], ['events'], ['serviceReleases', org, app]] })
  const versions = q.data?.versions ?? []
  const currentIdx = current ? versions.indexOf(current) : -1
  return (
    <>
      <SectionHead title="Version" hint="running version + available upstream releases" />
      <Card className="gap-0 py-0">
      <div className="flex flex-wrap items-center gap-2.5 border-b px-4 py-3">
        <span className="text-sm font-medium text-muted-foreground">Running</span>
        {current
          ? <StatusBadge tone="accent">{current}</StatusBadge>
          : <span className="text-xs text-muted-foreground">unknown (set on next deploy)</span>}
        {q.data?.image_repo && <span className="font-mono text-[11px] text-muted-foreground">{q.data.image_repo}</span>}
        <div className="flex-1" />
        {currentIdx === 0 && <span className="text-xs text-muted-foreground">up to date</span>}
      </div>
      <div className="px-4 py-3">
        <QueryState isLoading={q.isLoading} error={q.error}>
          {versions.length === 0 && (
            <span className="text-xs text-muted-foreground">No upstream versions found for this image.</span>
          )}
          {versions.length > 0 && (
            <div className="flex flex-col gap-0.5">
              {versions.slice(0, 15).map((v) => {
                const isCurrent = v === current
                const isNewer = currentIdx >= 0 && versions.indexOf(v) < currentIdx
                return (
                  <div key={v} className="flex items-center gap-3 rounded-md px-2 py-1.5 text-sm hover:bg-accent/40">
                    <span className="font-mono">{v}</span>
                    {isCurrent && <StatusBadge tone="accent">running</StatusBadge>}
                    {isNewer && <span className="rounded-full bg-primary/10 px-2 py-0.5 text-[11px] font-medium text-primary">newer</span>}
                    <div className="flex-1" />
                    {!isCurrent && (
                      <ConfirmButton size="sm" variant={isNewer ? 'default' : 'outline'} disabled={m.isPending}
                        title={`Promote ${app} to ${v}?`}
                        description="Pins this version's digest into the service manifest and opens the env/production render PR — merging it deploys."
                        confirmText={`Promote ${v}`}
                        onConfirm={() => m.mutate(() => send(urls.servicePromote(org, app, v), { method: 'POST' }))}>
                        Promote
                      </ConfirmButton>
                    )}
                  </div>
                )
              })}
            </div>
          )}
        </QueryState>
      </div>
      </Card>
    </>
  )
}

function Releases({ org, app, repo, prodImage }: { org: string; app: string; repo?: string | null; prodImage?: string }) {
  const q = useReleases(org, app)
  const m = useApiMutation({ invalidate: [['deploys', org], ['releases', org, app], ['events']] })
  const releases = q.data ?? []
  const nv = nextVersions(releases.map((r) => r.version))
  const PAGE = 15
  const [page, setPage] = useState(0)
  const pageCount = Math.max(1, Math.ceil(releases.length / PAGE))
  const clamped = Math.min(page, pageCount - 1)
  const shown = releases.slice(clamped * PAGE, clamped * PAGE + PAGE)
  if (q.isLoading || q.error) return null
  return (
    <>
      <SectionHead title="Releases" hint="tagged image publishes" />
      <div className="mb-2 flex justify-end gap-2">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button size="sm" disabled={m.isPending} title="Cut a new release — the bot tags the next semver and CI builds it">Cut release ▾</Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCut(org, app, 'auto')))}>Auto <span className="ml-auto pl-4 text-xs text-muted-foreground">from commits</span></DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCut(org, app, 'patch')))}>Patch <span className="ml-auto pl-4 font-mono text-xs text-muted-foreground">{nv.patch}</span></DropdownMenuItem>
            <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCut(org, app, 'minor')))}>Minor <span className="ml-auto pl-4 font-mono text-xs text-muted-foreground">{nv.minor}</span></DropdownMenuItem>
            <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCut(org, app, 'major')))}>Major <span className="ml-auto pl-4 font-mono text-xs text-muted-foreground">{nv.major}</span></DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
        {repo && (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="outline" size="sm" disabled={m.isPending} title={`Cut a release for every app in the ${repo} monorepo at once`}>Cut all in {repo} ▾</Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCutRepo(org, repo, 'auto')))}>Auto <span className="ml-auto pl-4 text-xs text-muted-foreground">from commits</span></DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCutRepo(org, repo, 'patch')))}>Patch</DropdownMenuItem>
              <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCutRepo(org, repo, 'minor')))}>Minor</DropdownMenuItem>
              <DropdownMenuItem onSelect={() => m.mutate(() => send(urls.releaseCutRepo(org, repo, 'major')))}>Major</DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        )}
        <Button variant="outline" size="sm" disabled={m.isPending}
          title="Reconcile with the registry: record vX.Y.Z publishes the webhook missed, and prune releases whose tag was deleted upstream"
          onClick={() => m.mutate(() => send(urls.releaseBackfill(org, app)))}>Reconcile with registry</Button>
      </div>
      <ReleaseSettings org={org} app={app} repo={repo} />
      <DraftCard org={org} app={app} />
      {releases.length === 0 && (
        <p className="text-sm text-muted-foreground">No releases yet. Tag <code className="font-mono">vX.Y.Z</code> in the app repo, or Reconcile with the registry.</p>
      )}
      <div className="flex flex-col gap-2">
        {shown.map((r) => {
          const onProd = !!prodImage && r.app_image === prodImage
          return (
            <div key={r.version} className="flex items-center gap-3 rounded-lg border px-4 py-2.5">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2 font-medium">{r.version}{onProd && <StatusBadge tone="success" dot>on production</StatusBadge>}</div>
                <div className="truncate font-mono text-xs text-muted-foreground">{short(r.app_image)} · {r.commit.slice(0, 7)} · {r.published_at}</div>
                {r.notes && (
                  <details className="mt-1 text-xs">
                    <summary className="cursor-pointer text-muted-foreground">changelog</summary>
                    <pre className="mt-1 whitespace-pre-wrap font-mono text-muted-foreground">{r.notes}</pre>
                  </details>
                )}
              </div>
              {!onProd && (
                <ConfirmButton size="sm" title={`Promote ${app} ${r.version} to production?`}
                  description="Writes the release into production.yaml; an admin still merges the render PR."
                  confirmText="Promote" onConfirm={() => m.mutate(() => send(urls.releasePromote(org, app, r.version)))}>
                  Promote → production
                </ConfirmButton>
              )}
            </div>
          )
        })}
      </div>
      {pageCount > 1 && (
        <div className="mt-3 flex items-center justify-between text-sm text-muted-foreground">
          <span>{clamped * PAGE + 1}–{clamped * PAGE + shown.length} of {releases.length}</span>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" disabled={clamped === 0} onClick={() => setPage(clamped - 1)}>← Prev</Button>
            <span>Page {clamped + 1} / {pageCount}</span>
            <Button variant="outline" size="sm" disabled={clamped >= pageCount - 1} onClick={() => setPage(clamped + 1)}>Next →</Button>
          </div>
        </div>
      )}
    </>
  )
}

// ── rename dialog (opened from the ⋯ menu) ────────────────────────────────────
function RenameDialog({ org, app, stateful, open, onOpenChange }: {
  org: string; app: string; stateful: boolean; open: boolean; onOpenChange: (o: boolean) => void
}) {
  const [name, setName] = useState('')
  const navigate = useNavigate()
  const m = useApiMutation({
    invalidate: [['apps', org], ['projects'], ['deploys', org], ['events']],
    onDone: () => { onOpenChange(false); navigate({ to: '/projects/$org/apps/$app', params: { org, app: name } }) },
  })
  const valid = /^[a-z0-9-]+$/.test(name) && name !== app
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader><DialogTitle>Rename {app}</DialogTitle></DialogHeader>
        <p className="text-sm text-muted-foreground">
          {stateful
            ? 'Renames the source repo + manifests, then migrates the database (and volumes) to the new names — a brief cutover downtime.'
            : 'Renames the source repo and moves the app’s manifests in one ops commit, then re-renders.'}
        </p>
        <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="new-name" aria-label="new app name" className="font-mono" />
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button disabled={!valid || m.isPending} onClick={() => m.mutate(() => send(urls.appRename(org, app), { json: { new: name } }))}>Rename</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// ── move to another project (ADR 0025) ───────────────────────────────────────
function MoveDialog({ org, app, stateful, open, onOpenChange }: {
  org: string; app: string; stateful: boolean; open: boolean; onOpenChange: (o: boolean) => void
}) {
  const projects = useProjects()
  const dests = (projects.data ?? []).filter((p) => p.onboarded && p.org !== org)
  const [dest, setDest] = useState('')
  const navigate = useNavigate()
  const m = useApiMutation({
    invalidate: [['apps', org], ['projects'], ['deploys', org], ['events']],
    onDone: () => { onOpenChange(false); navigate({ to: '/projects/$org/apps/$app', params: { org: dest, app } }) },
  })
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader><DialogTitle>Move {app} to another project</DialogTitle></DialogHeader>
        <p className="text-sm text-muted-foreground">
          Moves the app’s manifests, source repo{stateful ? ' and data (volumes + database) — a brief cutover downtime' : ''} to the
          destination project (a cross-project rename of the resource prefix). You must be an admin of the destination.
        </p>
        <Select value={dest} onValueChange={setDest}>
          <SelectTrigger><SelectValue placeholder="Destination project…" /></SelectTrigger>
          <SelectContent>
            {dests.map((p) => <SelectItem key={p.org} value={p.org}>{p.name} ({p.org})</SelectItem>)}
          </SelectContent>
        </Select>
        {dests.length === 0 && <p className="text-xs text-muted-foreground">No other onboarded projects to move to.</p>}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button disabled={!dest || m.isPending} onClick={() => m.mutate(() => send(urls.appMove(org, app), { json: { dest_org: dest } }))}>Move</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// ── logs (scoped to the selected env) ─────────────────────────────────────────
function LogsView({ org, app, cls }: { org: string; app: string; cls: string }) {
  const q = useAppLogs(org, cls, app, true)
  return (
    <div className="flex flex-col gap-3">
      <span className="text-xs text-muted-foreground">last 300 lines · live (5s)</span>
      <QueryState isLoading={q.isLoading} error={q.error}>
        <pre className="max-h-[calc(100vh-12rem)] overflow-auto rounded-md border bg-muted/40 p-3 font-mono text-[11px] leading-relaxed whitespace-pre-wrap">
          {q.data?.trim() ? q.data : 'No logs.'}
        </pre>
      </QueryState>
    </div>
  )
}

// ── manifest editor: Base/{env} scope toggle + merged inheritance form + secrets ─
type Row = { key: string; value: string }
function ManifestEditor({ org, app, files, env, classes, seedDigest }: {
  org: string; app: string; files: Record<string, ManifestFile>
  env: string; classes: string[]; seedDigest: string
}) {
  const [scope, setScope] = useState<'base' | 'env'>('env')
  const [mode, setMode] = useState<'form' | 'yaml'>('form')
  const [adding, setAdding] = useState(false)

  const file = scope === 'base' ? 'base.yaml' : `${env}.yaml`
  const onboarded = scope === 'base' || classes.includes(env)
  const baseDraft = fromData(files['base.yaml']?.data)

  const [draft, setDraft] = useState<ManifestDraft>(() => fromData(files[file]?.data))
  const [overridden, setOverridden] = useState<Set<string>>(() => overriddenKeys(files[file]?.data))
  const [yaml, setYaml] = useState(() => files[file]?.yaml ?? '')
  useEffect(() => {
    setDraft(fromData(files[file]?.data))
    setOverridden(overriddenKeys(files[file]?.data))
    setYaml(files[file]?.yaml ?? '')
    setAdding(false)
  }, [file, files])

  // Onboard a new class: a thin overlay pinning this env's digest, nothing else.
  const startAdd = () => {
    setDraft({ ...fromData(undefined), digest: seedDigest })
    setOverridden(new Set(['digest']))
    setAdding(true)
  }

  // Secrets for THIS scope, decrypted for editing (ADR 0024). Stem: base scope →
  // 'base' (secrets apply to every class), else the selected class.
  const stem = scope === 'base' ? 'base' : env
  const secretsQ = useAppSecrets(org, stem, app)
  const [secretRows, setSecretRows] = useState<Row[]>([])
  const [secretsDirty, setSecretsDirty] = useState(false)
  useEffect(() => {
    setSecretRows(Object.entries(secretsQ.data ?? {}).map(([key, value]) => ({ key, value })))
    setSecretsDirty(false)
  }, [secretsQ.data, file])
  const editRows = (fn: (rs: Row[]) => Row[]) => { setSecretRows(fn); setSecretsDirty(true) }
  const existingSecrets = Object.keys(secretsQ.data ?? {}).length

  const save = useApiMutation({ invalidate: [['manifest', org, app], ['apps', org], ['deploys', org], ['secrets', org, stem, app], ['events']] })
  // One Save commits the manifest AND the secrets. Secrets go first (authoritative)
  // so the manifest PUT preserves them server-side; only pushed when actually edited.
  const onSave = () => save.mutate(async () => {
    const msgs: string[] = []
    if (secretsDirty) {
      const envBlob = secretRows.filter((r) => r.key.trim()).map((r) => `${r.key.trim()}=${r.value}`).join('\n')
      const clear = !envBlob && existingSecrets > 0
      if (envBlob || clear) msgs.push(await send(urls.appSecrets(org, app), { json: { file, env: envBlob, clear } }))
    }
    // Base scope emits a full manifest; env scope emits a sparse overlay (only the
    // overridden keys), preserving any unmanaged keys already in the file.
    const json = scope === 'base' ? toManifest(draft, 'base.yaml', app) : toOverlay(draft, overridden, files[file]?.data)
    msgs.push(await send(urls.manifestFile(org, app, file), mode === 'form'
      ? { method: 'PUT', json }
      : { method: 'PUT', body: yaml }))
    setSecretsDirty(false)
    return msgs.join(' · ')
  })

  const seg = (on: boolean) => `rounded px-2.5 py-1 text-xs font-medium ${on ? 'bg-background shadow-sm' : 'text-muted-foreground'}`
  const showAdd = scope === 'env' && !onboarded && !adding && mode === 'form'

  return (
    <div className="flex flex-col gap-3.5">
      <div className="flex flex-wrap items-center gap-2 border-b pb-2">
        <div className="flex gap-1 rounded-md bg-muted p-0.5">
          <button onClick={() => setScope('base')} className={seg(scope === 'base')}>Base defaults</button>
          <button onClick={() => setScope('env')} className={seg(scope === 'env')}>{env} overrides</button>
        </div>
        <div className="ml-auto flex gap-1 rounded-md bg-muted p-0.5">
          <button onClick={() => setMode('form')} className={seg(mode === 'form')}>Form</button>
          <button onClick={() => setMode('yaml')} className={seg(mode === 'yaml')}>YAML</button>
        </div>
      </div>

      {showAdd ? (
        <div className="rounded-lg border border-dashed p-8 text-center">
          <p className="text-sm font-medium"><span className="font-mono">{env}</span> isn’t set up for this app</p>
          <p className="mx-auto mt-1 max-w-md text-xs text-muted-foreground">
            Create a {env} overlay — it inherits everything from <span className="font-mono">base.yaml</span>; you only pin what differs, starting with this env’s image digest.
          </p>
          <Button size="sm" className="mt-4" onClick={startAdd}>Add {env}</Button>
        </div>
      ) : mode === 'yaml' ? (
        <Textarea spellCheck={false} value={yaml} onChange={(e) => setYaml(e.target.value)} className="min-h-72 font-mono text-xs" />
      ) : scope === 'base' ? (
        <ManifestForm file="base.yaml" draft={draft} onChange={setDraft} />
      ) : (
        <OverlayForm cls={env} base={baseDraft} draft={draft} overridden={overridden}
          onChange={(d, o) => { setDraft(d); setOverridden(o) }} />
      )}

      {/* Secrets for this scope, encrypted per key (age) and delivered as tmpfs
          files. Editing routes through the encrypt endpoint (never the committed
          manifest); the single Save below commits manifest + secrets together. */}
      {!showAdd && (
        <div className="border-t pt-4">
          <div className="mb-1.5 flex items-baseline gap-2">
            <h3 className="text-sm font-semibold">Secrets</h3>
            <StatusBadge tone="accent">{file}</StatusBadge>
            {secretsQ.isLoading && <span className="text-xs text-muted-foreground">loading…</span>}
          </div>
          <p className="mb-2.5 text-xs text-muted-foreground">
            Encrypted per key; delivered as tmpfs files, never env vars. {scope === 'base' ? 'base.yaml secrets apply to every environment.' : `Only the ${env} environment.`} Remove a row with × to delete a secret.
          </p>
          {secretsQ.error
            ? <p className="text-xs text-destructive">Couldn’t load secrets (needs VPN + the reconciler).</p>
            : (
              <div className="flex flex-col gap-2">
                {secretRows.map((r, i) => (
                  <div key={i} className="flex items-center gap-2">
                    <Input value={r.key} placeholder="SECRET_NAME" className="w-56 font-mono text-xs" onChange={(e) => editRows((rs) => rs.map((x, j) => (j === i ? { ...x, key: e.target.value } : x)))} />
                    <Input value={r.value} placeholder="value" className="flex-1 font-mono text-xs" onChange={(e) => editRows((rs) => rs.map((x, j) => (j === i ? { ...x, value: e.target.value } : x)))} />
                    <Button size="sm" variant="ghost" className="text-destructive" title="Remove secret" onClick={() => editRows((rs) => rs.filter((_, j) => j !== i))}>×</Button>
                  </div>
                ))}
                <div><Button size="sm" variant="outline" onClick={() => editRows((rs) => [...rs, { key: '', value: '' }])}>+ Add secret</Button></div>
              </div>
            )}
        </div>
      )}

      {!showAdd && (
        <div className="flex items-center gap-3 border-t pt-4">
          <Button size="sm" disabled={save.isPending} onClick={onSave}>Save &amp; commit</Button>
          <span className="text-xs text-muted-foreground">Manifest + secrets committed to ops main; a render PR follows. base.yaml &amp; production.yaml require admin.</span>
        </div>
      )}
    </div>
  )
}
