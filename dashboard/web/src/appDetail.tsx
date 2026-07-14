import { useEffect, useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import { send, urls, useApps, useAppLogs, useAppSecrets, useEvents, useImports, useManifest, useProjects, useReleases, type ManifestFile } from './api'
import { useApiMutation } from './mutations'
import { ConfirmButton, DeployStatus, ExtLink, QueryState, short, StatusBadge } from './ui'
import { Crumbs, PageHead, ImportSteps } from './views'
import { fromData, ManifestForm, toManifest, type ManifestDraft } from './manifestForm'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Textarea } from '@/components/ui/textarea'
import { Input } from '@/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'

const FILES = ['base.yaml', 'testing.yaml', 'stable.yaml', 'production.yaml', 'ephemeral.yaml']

function Kv({ k, children }: { k: string; children: React.ReactNode }) {
  return <div className="flex gap-2.5 text-sm"><span className="min-w-28 text-muted-foreground">{k}</span><span className="font-mono text-xs">{children}</span></div>
}

export function AppDetail() {
  const { org, app } = useParams({ from: '/projects/$org/apps/$app' })
  const apps = useApps(org)
  const a = apps.data?.find((x) => x.name === app)
  const imports = useImports(org)
  const imp = imports.data?.find((x) => x.app === app)
  const manifest = useManifest(org, app)
  const events = useEvents()
  const appEvents = (events.data ?? []).filter((e) => e.action.trim().split(/\s+/).pop() === app)
  const imageOf = (f?: ManifestFile) => (f?.data as { image?: string } | null)?.image
  const prodImage = imageOf(manifest.data?.['production.yaml']) ?? imageOf(manifest.data?.['base.yaml'])

  // "Open in Adminer" (ADR 0014): the managed DB name is {project}_{app}_{class}
  // (hyphens → underscores), and the per-project Adminer auto-logs-in scoped to
  // the project. Prod-only for now — that's the only env with an Adminer.
  const project = useProjects().data?.find((p) => p.org === org)?.name
  const adminerUrl =
    project && a?.database && a.classes.includes('production')
      ? `https://adminer.prod.majksa.net/?pgsql=majnet-postgres&db=${`${project}_${app}_production`.replace(/-/g, '_')}`
      : null

  const act = useApiMutation({ invalidate: [['events']] })
  const deploy = useApiMutation({ invalidate: [['deploys', org], ['events']] })
  const retry = useApiMutation({ invalidate: [['imports', org], ['apps', org]] })

  return (
    <>
      <Crumbs><Link to="/">Projects</Link> / <Link to="/projects/$org" params={{ org }}>{org}</Link> / {app}</Crumbs>
      <PageHead title={app}>
        {a && a.classes.length > 0 && <RestartControl org={org} app={app} classes={a.classes} run={act.mutate} busy={act.isPending} />}
        <ConfirmButton variant="outline" size="sm" title={`Roll back ${app}?`} description={`Revert the last change on ${org}/ops.`}
          confirmText="Roll back" onConfirm={() => deploy.mutate(() => send(urls.rollback(org)))}>Roll back</ConfirmButton>
        <ConfirmButton size="sm" title={`Promote ${app} to production?`} description="An admin still merges the render PR in Deployments."
          confirmText="Promote" onConfirm={() => deploy.mutate(() => send(urls.promote(org, app)))}>Promote → production</ConfirmButton>
        <RenameControl org={org} app={app} stateful={!!a?.database} />
      </PageHead>

      {imp && (
        <Card className="mb-4"><CardContent className="pt-6">
          <h2 className="mb-3 text-sm font-semibold">
            {imp.status === 'failed' ? 'Import failed' : 'Importing…'}
          </h2>
          <ImportSteps status={imp} />
          {imp.status === 'failed' && (
            <div className="mt-3 flex items-center gap-3">
              <Button size="sm" disabled={retry.isPending}
                onClick={() => retry.mutate(() => send(urls.importRetry(org, app)))}>Retry import</Button>
              <span className="text-xs text-muted-foreground">Re-runs from the stored request; re-enter a private-repo token or env secrets via the form.</span>
            </div>
          )}
        </CardContent></Card>
      )}

      {a && (
        <Card className="mb-4"><CardContent className="flex flex-col gap-2.5 pt-6">
          <Kv k="Deploy status"><span className="inline-flex items-center gap-2"><DeployStatus ev={appEvents[0]} />{appEvents[0] && <span className="text-muted-foreground">{appEvents[0].result} · {appEvents[0].at}</span>}</span></Kv>
          <Kv k="Classes">{a.classes.join(', ') || '—'}</Kv>
          <Kv k="Domains">
            {a.domains.length
              ? a.domains.map((d, i) => <span key={d}>{i > 0 && ', '}<ExtLink to={d} /></span>)
              : '—'}
          </Kv>
          <Kv k="Image">{short(a.image)}</Kv>
          {a.database && (
            <Kv k="Database">
              <span className="inline-flex items-center gap-3">
                {a.database}
                {adminerUrl && (
                  <Button asChild variant="outline" size="sm">
                    <a href={adminerUrl} target="_blank" rel="noreferrer">Open in Adminer ↗</a>
                  </Button>
                )}
              </span>
            </Kv>
          )}
        </CardContent></Card>
      )}

      <Releases org={org} app={app} prodImage={prodImage} />

      <QueryState isLoading={manifest.isLoading} error={imp && !manifest.data ? undefined : manifest.error}>
        {manifest.data && <ManifestEditor org={org} app={app} files={manifest.data} />}
      </QueryState>

      {a && <SecretsEditor org={org} app={app} classes={a.classes} />}

      {a && a.classes.length > 0 && <LogsPanel org={org} app={app} classes={a.classes} />}

      {appEvents.length > 0 && (
        <Card className="mt-4"><CardContent className="pt-6">
          <h2 className="mb-2 text-sm font-semibold">Recent deploys</h2>
          <Table>
            <TableHeader><TableRow><TableHead>time</TableHead><TableHead>node</TableHead><TableHead>action</TableHead><TableHead>result</TableHead><TableHead>commit</TableHead></TableRow></TableHeader>
            <TableBody className="font-mono text-xs">
              {appEvents.slice(0, 8).map((e, i) => (
                <TableRow key={i}><TableCell>{e.at}</TableCell><TableCell>{e.node}</TableCell><TableCell>{e.action}</TableCell>
                  <TableCell className={e.result.startsWith('FAILED') ? 'text-destructive' : ''}>{e.result}</TableCell><TableCell>{e.commit.slice(0, 12)}</TableCell></TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent></Card>
      )}
    </>
  )
}

function Releases({ org, app, prodImage }: { org: string; app: string; prodImage?: string }) {
  const q = useReleases(org, app)
  const m = useApiMutation({ invalidate: [['deploys', org], ['releases', org, app], ['events']] })
  const releases = q.data ?? []
  if (q.isLoading || q.error) return null

  return (
    <Card className="mb-4"><CardContent className="pt-6">
      <div className="mb-2 flex items-center justify-between gap-2">
        <h2 className="text-sm font-semibold">Releases</h2>
        <Button variant="outline" size="sm" disabled={m.isPending}
          title="Recover any vX.Y.Z publishes the registry_package webhook missed"
          onClick={() => m.mutate(() => send(urls.releaseBackfill(org, app)))}>
          Backfill from registry
        </Button>
      </div>
      {releases.length === 0 && (
        <p className="text-sm text-muted-foreground">
          No releases yet. Tag <code className="font-mono">vX.Y.Z</code> in the app repo to publish one, or Backfill from the registry.
        </p>
      )}
      <div className="flex flex-col gap-2">
        {releases.map((r) => {
          const onProd = !!prodImage && r.app_image === prodImage
          return (
            <div key={r.version} className="flex items-center gap-3 rounded-lg border px-4 py-2.5">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2 font-medium">
                  {r.version}
                  {onProd && <StatusBadge tone="success" dot>on production</StatusBadge>}
                </div>
                <div className="truncate font-mono text-xs text-muted-foreground">
                  {short(r.app_image)} · {r.commit.slice(0, 7)} · {r.published_at}
                </div>
              </div>
              {!onProd && (
                <ConfirmButton size="sm" title={`Promote ${app} ${r.version} to production?`}
                  description="Writes the release into production.yaml; an admin still merges the render PR."
                  confirmText="Promote"
                  onConfirm={() => m.mutate(() => send(urls.releasePromote(org, app, r.version)))}>
                  Promote → production
                </ConfirmButton>
              )}
            </div>
          )
        })}
      </div>
    </CardContent></Card>
  )
}

// ── rename an app (repo + manifests in one commit, then deploy) ───────────────
function RenameControl({ org, app, stateful }: { org: string; app: string; stateful: boolean }) {
  const [name, setName] = useState('')
  const navigate = useNavigate()
  const m = useApiMutation({
    invalidate: [['apps', org], ['projects'], ['deploys', org], ['events']],
    onDone: () => navigate({ to: '/projects/$org/apps/$app', params: { org, app: name } }),
  })
  const valid = /^[a-z0-9-]+$/.test(name) && name !== app
  return (
    <div className="flex items-center gap-2">
      <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="new-name" className="h-8 w-40" aria-label="new app name" />
      <ConfirmButton variant="outline" size="sm" disabled={!valid || m.isPending}
        title={`Rename ${app} → ${name}?`}
        description={stateful
          ? 'This app has a managed database — data-preserving rename for stateful apps is not enabled yet, so this will be refused.'
          : 'Renames the source repo and moves the app’s manifests in one ops commit, then re-renders. Non-production deploys immediately; production merges its render PR.'}
        confirmText="Rename"
        onConfirm={() => m.mutate(() => send(urls.appRename(org, app), { json: { new: name } }))}>
        Rename
      </ConfirmButton>
    </div>
  )
}

function RestartControl({ org, app, classes, run, busy }: {
  org: string; app: string; classes: string[]; run: (fn: () => Promise<string>) => void; busy: boolean
}) {
  const [cls, setCls] = useState(classes[0]!)
  return (
    <div className="flex items-center gap-2">
      <Select value={cls} onValueChange={setCls}>
        <SelectTrigger size="sm" className="w-auto"><SelectValue /></SelectTrigger>
        <SelectContent>{classes.map((c) => <SelectItem key={c} value={c}>{c}</SelectItem>)}</SelectContent>
      </Select>
      <Button variant="outline" size="sm" disabled={busy} onClick={() => run(() => send(urls.restart(org, cls, app)))}>Restart</Button>
    </div>
  )
}

// ── live container logs (streamed over the node Docker API) ───────────────────
function LogsPanel({ org, app, classes }: { org: string; app: string; classes: string[] }) {
  const opts = classes.length ? classes : ['production']
  const [cls, setCls] = useState(opts.includes('production') ? 'production' : opts[0]!)
  const q = useAppLogs(org, cls, app, true)
  return (
    <Card className="mb-4"><CardContent className="flex flex-col gap-3 pt-6">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h2 className="text-sm font-semibold">Logs</h2>
        <Select value={cls} onValueChange={setCls}>
          <SelectTrigger size="sm" className="w-36"><SelectValue /></SelectTrigger>
          <SelectContent>{opts.map((c) => <SelectItem key={c} value={c}>{c}</SelectItem>)}</SelectContent>
        </Select>
        <span className="text-xs text-muted-foreground">last 300 lines · live (5s)</span>
      </div>
      <QueryState isLoading={q.isLoading} error={q.error}>
        <pre className="max-h-96 overflow-auto rounded-md border bg-muted/40 p-3 font-mono text-[11px] leading-relaxed whitespace-pre-wrap">
          {q.data?.trim() ? q.data : 'No logs.'}
        </pre>
      </QueryState>
    </CardContent></Card>
  )
}

// ── secret values: fields/bulk editor, values decrypted for display ───────────
type Row = { key: string; value: string }

function SecretsEditor({ org, app, classes }: { org: string; app: string; classes: string[] }) {
  const opts = classes.length ? classes : ['production']
  const [cls, setCls] = useState(opts.includes('production') ? 'production' : opts[0]!)
  const [mode, setMode] = useState<'fields' | 'bulk'>('fields')
  const [rows, setRows] = useState<Row[]>([{ key: '', value: '' }])
  const [bulk, setBulk] = useState('')
  const q = useAppSecrets(org, cls, app)

  // Seed the editor from the decrypted current values whenever they load or the
  // class changes — so you edit what's actually set, not a blank slate.
  useEffect(() => {
    if (!q.data) return
    const entries = Object.entries(q.data)
    setRows(entries.length ? entries.map(([key, value]) => ({ key, value })) : [{ key: '', value: '' }])
    setBulk(entries.map(([k, v]) => `${k}=${v}`).join('\n'))
  }, [q.data, cls])

  const m = useApiMutation({
    invalidate: [['deploys', org], ['manifest', org, app], ['secrets', org, cls, app], ['events']],
  })

  // Both modes serialize to a dotenv blob for the set endpoint (full replace).
  const env = mode === 'bulk'
    ? bulk.trim()
    : rows.filter((r) => r.key.trim()).map((r) => `${r.key.trim()}=${r.value}`).join('\n')

  const setRow = (i: number, patch: Partial<Row>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)))

  return (
    <Card className="mb-4"><CardContent className="flex flex-col gap-3 pt-6">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h2 className="text-sm font-semibold">Secrets</h2>
        <Select value={cls} onValueChange={setCls}>
          <SelectTrigger size="sm" className="w-36"><SelectValue /></SelectTrigger>
          <SelectContent>{opts.map((c) => <SelectItem key={c} value={c}>{c}</SelectItem>)}</SelectContent>
        </Select>
        <div className="flex-1" />
        <div className="inline-flex rounded-md border p-0.5">
          <Button size="sm" variant={mode === 'fields' ? 'secondary' : 'ghost'} className="h-7 px-2.5" onClick={() => setMode('fields')}>Fields</Button>
          <Button size="sm" variant={mode === 'bulk' ? 'secondary' : 'ghost'} className="h-7 px-2.5" onClick={() => setMode('bulk')}>Bulk</Button>
        </div>
      </div>

      <QueryState isLoading={q.isLoading} error={q.error}>
        {mode === 'fields' ? (
          <div className="flex flex-col gap-2">
            {rows.map((r, i) => (
              <div key={i} className="flex items-center gap-2">
                <Input value={r.key} placeholder="SECRET_NAME" className="w-64 font-mono text-xs" onChange={(e) => setRow(i, { key: e.target.value })} />
                <Input value={r.value} placeholder="value" className="flex-1 font-mono text-xs" onChange={(e) => setRow(i, { value: e.target.value })} />
                <Button size="sm" variant="ghost" className="text-destructive" onClick={() => setRows((rs) => { const next = rs.filter((_, j) => j !== i); return next.length ? next : [{ key: '', value: '' }] })}>×</Button>
              </div>
            ))}
            <div><Button size="sm" variant="outline" onClick={() => setRows((rs) => [...rs, { key: '', value: '' }])}>+ Add secret</Button></div>
          </div>
        ) : (
          <Textarea value={bulk} onChange={(e) => setBulk(e.target.value)} className="min-h-28 font-mono text-xs"
            placeholder={'DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/…\nAPI_KEY=…'} />
        )}
      </QueryState>

      <span className="text-xs text-muted-foreground">
        Values shown are decrypted from the current SOPS file (VPN-only). Saving <em>replaces</em> the whole set for
        this class; they're re-encrypted to the project key and delivered as tmpfs files at <code className="font-mono">/run/secrets/&lt;NAME&gt;</code>, never env vars.
        Production writes open a render PR you review before it deploys.
      </span>
      <div>
        <ConfirmButton size="sm" disabled={!env || m.isPending}
          title={`Set ${cls} secrets for ${app}?`}
          description={cls === 'production'
            ? 'Encrypts + commits the values; a render PR will gate the deploy.'
            : 'Encrypts + commits the values; auto-deploys on render.'}
          confirmText="Encrypt & save"
          onConfirm={() => m.mutate(() => send(urls.appSecrets(org, app), { json: { class: cls, env } }))}>
          Save secrets
        </ConfirmButton>
      </div>
    </CardContent></Card>
  )
}

// ── manifest editor: file tabs + Form/YAML ────────────────────────────────────
function ManifestEditor({ org, app, files }: { org: string; app: string; files: Record<string, ManifestFile> }) {
  const [file, setFile] = useState('base.yaml')
  const [mode, setMode] = useState<'form' | 'yaml'>('form')
  const [draft, setDraft] = useState<ManifestDraft>(() => fromData(files[file]?.data))
  const [yaml, setYaml] = useState(() => files[file]?.yaml ?? '')

  useEffect(() => {
    setDraft(fromData(files[file]?.data))
    setYaml(files[file]?.yaml ?? '')
  }, [file, files])

  const save = useApiMutation({ invalidate: [['manifest', org, app], ['apps', org], ['deploys', org], ['events']] })
  const onSave = () => {
    if (mode === 'form') save.mutate(() => send(urls.manifestFile(org, app, file), { method: 'PUT', json: toManifest(draft, file, app) }))
    else save.mutate(() => send(urls.manifestFile(org, app, file), { method: 'PUT', body: yaml }))
  }

  return (
    <Card>
      <div className="flex flex-wrap items-center gap-1 border-b px-3 py-2">
        {FILES.map((f) => (
          <button key={f} onClick={() => setFile(f)}
            className={`rounded-md px-2.5 py-1.5 text-sm font-medium ${f === file ? 'bg-accent text-accent-foreground' : 'text-muted-foreground hover:text-foreground'}`}>
            {f}{!files[f] && <span className="text-muted-foreground/60"> (new)</span>}
          </button>
        ))}
        <div className="flex-1" />
        <div className="flex gap-1 rounded-md bg-muted p-0.5">
          <button onClick={() => setMode('form')} className={`rounded px-2.5 py-1 text-xs font-medium ${mode === 'form' ? 'bg-background shadow-sm' : 'text-muted-foreground'}`}>Form</button>
          <button onClick={() => setMode('yaml')} className={`rounded px-2.5 py-1 text-xs font-medium ${mode === 'yaml' ? 'bg-background shadow-sm' : 'text-muted-foreground'}`}>YAML</button>
        </div>
      </div>
      <CardContent className="flex flex-col gap-3.5 pt-5">
        {mode === 'form'
          ? <ManifestForm file={file} draft={draft} onChange={setDraft} />
          : <Textarea spellCheck={false} value={yaml} onChange={(e) => setYaml(e.target.value)} className="min-h-64 font-mono text-xs" />}
        <div className="flex items-center gap-3">
          <Button size="sm" disabled={save.isPending} onClick={onSave}>Save &amp; commit</Button>
          <span className="text-xs text-muted-foreground">Validated + committed to ops main; a render PR follows. production.yaml requires admin.</span>
        </div>
      </CardContent>
    </Card>
  )
}
