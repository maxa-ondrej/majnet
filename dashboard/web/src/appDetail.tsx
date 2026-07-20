import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import {
  RotateCw, ScrollText, KeyRound, SlidersHorizontal, ArrowUpFromLine, MoreVertical, TerminalSquare,
} from 'lucide-react'
import {
  send, urls, useApps, useAppContainers, useAppInfo, useAppLogs, useAppSecrets, useEvents, useImports,
  useManifest, useNodeMetrics, useProjects, useReleases, useReleaseConfig, useReleaseDraft, useWhoami,
  type AppInfo, type Autorelease, type ManifestFile, type ReleaseConfig,
} from './api'
import { useApiMutation } from './mutations'
import { ConfirmButton, ExtLink, QueryState, short, StatusBadge } from './ui'
import { Crumbs, ContainerSpark, ImportSteps } from './views'
import { fromData, ManifestForm, toManifest, type ManifestDraft } from './manifestForm'
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

const FILES = ['base.yaml', 'testing.yaml', 'stable.yaml', 'production.yaml', 'ephemeral.yaml']
// Environment order for the selector + strip; filtered to the classes an app has.
const ENV_ORDER = ['production', 'stable', 'testing', 'ephemeral'] as const

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

type Sheeted = null | 'config' | 'logs' | 'secrets'

export function AppDetail() {
  const { org, app } = useParams({ from: '/projects/$org/apps/$app' })
  const apps = useApps(org)
  const a = apps.data?.find((x) => x.name === app)
  const imports = useImports(org)
  const imp = imports.data?.find((x) => x.app === app)
  const manifest = useManifest(org, app)
  const events = useEvents()
  const info = useAppInfo(org, app)
  const metrics = useNodeMetrics()
  const project = useProjects().data?.find((p) => p.org === org)?.name
  const isAdmin = useWhoami().data?.admin ?? false
  const appEvents = (events.data ?? []).filter((e) => e.action.trim().split(/\s+/).pop() === app)
  const imageOf = (f?: ManifestFile) => (f?.data as { image?: string } | null)?.image
  const prodImage = imageOf(manifest.data?.['production.yaml']) ?? imageOf(manifest.data?.['base.yaml'])

  const classes: string[] = ENV_ORDER.filter((c) => a?.classes.includes(c))
  const [env, setEnv] = useState<string>('production')
  // Settle the selection on a class the app actually has, once they load.
  useEffect(() => {
    if (classes.length && !classes.includes(env)) setEnv(classes[0]!)
  }, [classes, env])

  const [sheet, setSheet] = useState<Sheeted>(null)
  const [renameOpen, setRenameOpen] = useState(false)

  const navigate = useNavigate()
  const act = useApiMutation({ invalidate: [['events']] })
  const deploy = useApiMutation({ invalidate: [['deploys', org], ['events']] })
  const retry = useApiMutation({ invalidate: [['imports', org], ['apps', org]] })
  const archive = useApiMutation({
    invalidate: [['apps', org], ['archived', org], ['events']],
    onDone: () => navigate({ to: '/projects/$org', params: { org } }),
  })

  // Live, per-environment state.
  const containersFor = (cls: string) =>
    project ? (metrics.data ?? []).flatMap((n) => n.apps).filter((c) => c.name.startsWith(`${project}-${app}-${cls}-`)) : []
  const versionFor = (cls: string): string | null => {
    const v = info.data?.find((r) => r.class === cls)?.info?.version
    return typeof v === 'string' ? v : null
  }
  const digestShort = (img?: string) => img?.split('@sha256:')[1]?.slice(0, 7) ?? null

  const adminerUrl =
    project && a?.database && a.classes.includes('production')
      ? `https://adminer.prod.majksa.net/?pgsql=majnet-postgres&db=${`${project}_${app}_production`.replace(/-/g, '_')}`
      : null

  return (
    <>
      <Crumbs>
        <Link to="/projects">Projects</Link> / <Link to="/projects/$org" params={{ org }}>{org}</Link> / {app}
      </Crumbs>

      {/* ── app header ─────────────────────────────────────────────────────── */}
      <div className="flex flex-wrap items-start gap-4">
        <div className="min-w-0">
          <h1 className="text-2xl font-semibold tracking-tight">{app}</h1>
          <div className="mt-1 truncate text-sm text-muted-foreground">
            {[short(a?.image), a?.database].filter(Boolean).join('  ·  ') || '—'}
          </div>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <Button asChild variant="outline" size="sm">
            <a href={`https://github.com/${org}/${a?.repo ?? app}`} target="_blank" rel="noreferrer">GitHub ↗</a>
          </Button>
          <Button variant="outline" size="sm" onClick={() => setSheet('config')}>
            <SlidersHorizontal className="size-4" /> Configuration
          </Button>
          <ConfirmButton variant="outline" size="sm" title={`Roll back ${app}?`}
            description={`Revert the last change on ${org}/ops.`} confirmText="Roll back"
            onConfirm={() => deploy.mutate(() => send(urls.rollback(org)))}>
            Roll back
          </ConfirmButton>
          <ConfirmButton size="sm" title={`Promote ${app} to production?`}
            description="An admin still merges the render PR in Deployments." confirmText="Promote"
            onConfirm={() => deploy.mutate(() => send(urls.promote(org, app)))}>
            <ArrowUpFromLine className="size-4" /> Promote
          </ConfirmButton>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="outline" size="icon" className="size-9" aria-label="More actions">
                <MoreVertical className="size-4" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={() => setRenameOpen(true)}>Rename app…</DropdownMenuItem>
              <DropdownMenuItem
                variant="destructive"
                onSelect={() => archive.mutate(() => send(urls.appArchive(org, app)))}>
                Archive app
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

      {/* ── environment selector (the centerpiece) ─────────────────────────── */}
      {classes.length > 0 && (
        <>
          <div className="mt-6 flex flex-wrap items-center gap-x-4 gap-y-2">
            <div className="inline-flex gap-0.5 rounded-xl bg-muted p-1">
              {classes.map((c) => {
                const live = containersFor(c).length > 0
                return (
                  <button key={c} onClick={() => setEnv(c)}
                    className={`inline-flex h-8 items-center gap-2 rounded-lg px-3.5 text-[13px] font-medium transition-colors ${
                      env === c ? 'bg-card text-foreground shadow-sm' : 'text-muted-foreground hover:text-foreground'}`}>
                    <span className={`size-1.5 rounded-full ${live ? 'bg-success' : 'bg-muted-foreground/40'}`} />
                    {c.charAt(0).toUpperCase() + c.slice(1)}
                  </button>
                )
              })}
            </div>
            <span className="text-xs text-muted-foreground">Status, containers, logs & secrets below follow this selection.</span>
          </div>

          <EnvironmentZone
            app={app} env={env} org={org} isAdmin={isAdmin}
            containers={containersFor(env)}
            version={versionFor(env)}
            domains={env === 'production' ? (a?.domains ?? []) : []}
            adminerUrl={env === 'production' ? adminerUrl : null}
            onLogs={() => setSheet('logs')} onSecrets={() => setSheet('secrets')}
            restart={() => act.mutate(() => send(urls.restart(org, env, app)))} busy={act.isPending}
          />
        </>
      )}

      {/* ── all environments (comparison; not filtered) ────────────────────── */}
      {classes.length > 1 && (
        <>
          <SectionHead title="All environments" hint="click to switch · not filtered by the selector" />
          <div className="grid gap-3" style={{ gridTemplateColumns: `repeat(${Math.min(classes.length, 4)}, minmax(0, 1fr))` }}>
            {classes.map((c) => {
              const ver = versionFor(c) ?? digestShort(containersFor(c)[0]?.image) ?? null
              const live = containersFor(c).length > 0
              return (
                <button key={c} onClick={() => setEnv(c)}
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
      )}

      <Releases org={org} app={app} repo={a?.repo} prodImage={prodImage} />

      {appEvents.length > 0 && (
        <>
          <SectionHead title="Recent deploys" />
          <Card><CardContent className="pt-6">
            <Table>
              <TableHeader><TableRow><TableHead>time</TableHead><TableHead>node</TableHead><TableHead>action</TableHead><TableHead>result</TableHead><TableHead>commit</TableHead></TableRow></TableHeader>
              <TableBody className="font-mono text-xs">
                {appEvents.slice(0, 8).map((e, i) => (
                  <TableRow key={i}>
                    <TableCell>{e.at}</TableCell><TableCell>{e.node}</TableCell><TableCell>{e.action}</TableCell>
                    <TableCell className={e.result.startsWith('FAILED') ? 'text-destructive' : ''}>{resultVersioned(e.result, info.data)}</TableCell>
                    <TableCell>{e.commit.slice(0, 12)}</TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardContent></Card>
        </>
      )}

      {/* ── on-demand drawers ──────────────────────────────────────────────── */}
      <Sheet open={sheet === 'config'} onOpenChange={(o) => setSheet(o ? 'config' : null)}>
        <SheetContent>
          <SheetHeader><SheetTitle>Configuration</SheetTitle>
            <span className="text-xs text-muted-foreground">base manifest + per-env overlays</span></SheetHeader>
          <SheetBody>
            <QueryState isLoading={manifest.isLoading} error={imp && !manifest.data ? undefined : manifest.error}>
              {manifest.data && <ManifestEditor org={org} app={app} files={manifest.data} />}
            </QueryState>
          </SheetBody>
        </SheetContent>
      </Sheet>

      <Sheet open={sheet === 'logs'} onOpenChange={(o) => setSheet(o ? 'logs' : null)}>
        <SheetContent>
          <SheetHeader><SheetTitle>Logs</SheetTitle><StatusBadge tone="accent">{env}</StatusBadge></SheetHeader>
          <SheetBody>{sheet === 'logs' && <LogsView org={org} app={app} cls={env} />}</SheetBody>
        </SheetContent>
      </Sheet>

      <Sheet open={sheet === 'secrets'} onOpenChange={(o) => setSheet(o ? 'secrets' : null)}>
        <SheetContent>
          <SheetHeader><SheetTitle>Secrets</SheetTitle><StatusBadge tone="accent">{env}</StatusBadge></SheetHeader>
          <SheetBody>{sheet === 'secrets' && <SecretsView org={org} app={app} cls={env} />}</SheetBody>
        </SheetContent>
      </Sheet>

      <RenameDialog org={org} app={app} stateful={!!a?.database} open={renameOpen} onOpenChange={setRenameOpen} />
    </>
  )
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
  app, env, org, isAdmin, containers, version, domains, adminerUrl, onLogs, onSecrets, restart, busy,
}: {
  app: string; env: string; org: string; isAdmin: boolean
  containers: { name: string; image: string; state: string; cpu_pct: number; mem_used: number; mem_limit: number }[]
  version: string | null; domains: string[]; adminerUrl: string | null
  onLogs: () => void; onSecrets: () => void; restart: () => void; busy: boolean
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
            <Button variant="outline" size="sm" onClick={onSecrets}><KeyRound className="size-4" /> Secrets</Button>
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
                    <td className="py-1"><ContainerSpark container={c.name} range={3600} /></td>
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
function DraftCard({ org, app }: { org: string; app: string }) {
  const q = useReleaseDraft(org, app)
  const m = useApiMutation({ invalidate: [['releaseDraft', org, app], ['releases', org, app], ['deploys', org], ['events']] })
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

// ── secrets (scoped to the selected env) ──────────────────────────────────────
type Row = { key: string; value: string }
function SecretsView({ org, app, cls }: { org: string; app: string; cls: string }) {
  const [mode, setMode] = useState<'fields' | 'bulk'>('fields')
  const [rows, setRows] = useState<Row[]>([{ key: '', value: '' }])
  const [bulk, setBulk] = useState('')
  const q = useAppSecrets(org, cls, app)

  useEffect(() => {
    if (!q.data) return
    const entries = Object.entries(q.data)
    setRows(entries.length ? entries.map(([key, value]) => ({ key, value })) : [{ key: '', value: '' }])
    setBulk(entries.map(([k, v]) => `${k}=${v}`).join('\n'))
  }, [q.data])

  const m = useApiMutation({ invalidate: [['deploys', org], ['manifest', org, app], ['secrets', org, cls, app], ['events']] })
  const envtext = mode === 'bulk'
    ? bulk.trim()
    : rows.filter((r) => r.key.trim()).map((r) => `${r.key.trim()}=${r.value}`).join('\n')
  const setRow = (i: number, patch: Partial<Row>) => setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)))

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center">
        <span className="text-xs text-muted-foreground">Decrypted from SOPS (VPN-only). Saving replaces the whole set for this env.</span>
        <div className="ml-auto inline-flex rounded-md border p-0.5">
          <Button size="sm" variant={mode === 'fields' ? 'secondary' : 'ghost'} className="h-7 px-2.5" onClick={() => setMode('fields')}>Fields</Button>
          <Button size="sm" variant={mode === 'bulk' ? 'secondary' : 'ghost'} className="h-7 px-2.5" onClick={() => setMode('bulk')}>Bulk</Button>
        </div>
      </div>
      <QueryState isLoading={q.isLoading} error={q.error}>
        {mode === 'fields' ? (
          <div className="flex flex-col gap-2">
            {rows.map((r, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input value={r.key} placeholder="SECRET_NAME" className="w-56 font-mono text-xs" onChange={(e) => setRow(i, { key: e.target.value })} />
                <Input value={r.value} placeholder="value" className="flex-1 font-mono text-xs" onChange={(e) => setRow(i, { value: e.target.value })} />
                <Button size="sm" variant="ghost" className="text-destructive" onClick={() => setRows((rs) => { const n = rs.filter((_, j) => j !== i); return n.length ? n : [{ key: '', value: '' }] })}>×</Button>
              </div>
            ))}
            <div><Button size="sm" variant="outline" onClick={() => setRows((rs) => [...rs, { key: '', value: '' }])}>+ Add secret</Button></div>
          </div>
        ) : (
          <Textarea value={bulk} onChange={(e) => setBulk(e.target.value)} className="min-h-40 font-mono text-xs" placeholder={'API_KEY=…\nWEBHOOK_URL=…'} />
        )}
      </QueryState>
      <div>
        <ConfirmButton size="sm" disabled={!envtext || m.isPending}
          title={`Set ${cls} secrets for ${app}?`}
          description={cls === 'production' ? 'Encrypts + commits; a render PR gates the deploy.' : 'Encrypts + commits; auto-deploys on render.'}
          confirmText="Encrypt & save"
          onConfirm={() => m.mutate(() => send(urls.appSecrets(org, app), { json: { class: cls, env: envtext } }))}>
          Save secrets
        </ConfirmButton>
      </div>
    </div>
  )
}

// ── manifest editor: file tabs + Form/YAML (inside the Configuration sheet) ────
function ManifestEditor({ org, app, files }: { org: string; app: string; files: Record<string, ManifestFile> }) {
  const [file, setFile] = useState('base.yaml')
  const [mode, setMode] = useState<'form' | 'yaml'>('form')
  const [draft, setDraft] = useState<ManifestDraft>(() => fromData(files[file]?.data))
  const [yaml, setYaml] = useState(() => files[file]?.yaml ?? '')
  useEffect(() => { setDraft(fromData(files[file]?.data)); setYaml(files[file]?.yaml ?? '') }, [file, files])

  const save = useApiMutation({ invalidate: [['manifest', org, app], ['apps', org], ['deploys', org], ['events']] })
  const onSave = () => {
    if (mode === 'form') save.mutate(() => send(urls.manifestFile(org, app, file), { method: 'PUT', json: toManifest(draft, file, app) }))
    else save.mutate(() => send(urls.manifestFile(org, app, file), { method: 'PUT', body: yaml }))
  }
  return (
    <div className="flex flex-col gap-3.5">
      <div className="flex flex-wrap items-center gap-1 border-b pb-2">
        {/* Existing overlays first (keeping the base→prod gradient), the
            addable "(new)" ones after — so the app's real config isn't buried. */}
        {[...FILES].sort((x, y) => Number(!files[x]) - Number(!files[y])).map((f) => (
          <button key={f} onClick={() => setFile(f)}
            className={`rounded-md px-2.5 py-1.5 text-xs font-medium ${f === file ? 'bg-accent text-accent-foreground' : 'text-muted-foreground hover:text-foreground'}`}>
            {f}{!files[f] && <span className="text-muted-foreground/60"> (new)</span>}
          </button>
        ))}
        <div className="ml-auto flex gap-1 rounded-md bg-muted p-0.5">
          <button onClick={() => setMode('form')} className={`rounded px-2.5 py-1 text-xs font-medium ${mode === 'form' ? 'bg-background shadow-sm' : 'text-muted-foreground'}`}>Form</button>
          <button onClick={() => setMode('yaml')} className={`rounded px-2.5 py-1 text-xs font-medium ${mode === 'yaml' ? 'bg-background shadow-sm' : 'text-muted-foreground'}`}>YAML</button>
        </div>
      </div>
      {mode === 'form'
        ? <ManifestForm file={file} draft={draft} onChange={setDraft} />
        : <Textarea spellCheck={false} value={yaml} onChange={(e) => setYaml(e.target.value)} className="min-h-72 font-mono text-xs" />}
      <div className="flex items-center gap-3">
        <Button size="sm" disabled={save.isPending} onClick={onSave}>Save &amp; commit</Button>
        <span className="text-xs text-muted-foreground">Committed to ops main; a render PR follows. production.yaml requires admin.</span>
      </div>
    </div>
  )
}
