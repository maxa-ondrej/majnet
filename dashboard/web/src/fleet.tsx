// Fleet-wide full-page views for Releases (candidates) and Deployments — the
// "View all →" targets of the top-bar popovers. The popovers are for a glance;
// these show the complete current set across every project with room for detail.
import { Link } from '@tanstack/react-router'
import { useQueries } from '@tanstack/react-query'
import { getJSON, parseAt, send, urls, useProjects, useReleaseDrafts, type DeployPr } from './api'
import { useApiMutation } from './mutations'
import { ConfirmButton, Empty, QueryState } from './ui'
import { PageHead } from './views'
import { FileDiff } from './deploys'
import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'

const relAge = (at: string): string => {
  if (!at) return ''
  const s = Math.max(0, Math.round((Date.now() - parseAt(at)) / 1000))
  if (s < 60) return `${s}s ago`
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.round(m / 60)
  return h < 24 ? `${h}h ago` : `${Math.round(h / 24)}d ago`
}

const bumpChip = (b: string) =>
  b === 'major' ? 'bg-destructive/10 text-destructive'
  : b === 'minor' ? 'bg-primary/10 text-primary'
  : 'bg-secondary text-secondary-foreground'

// ── Releases: every app with a pending release candidate (draft) ──────────────
export function AllReleases() {
  const drafts = useReleaseDrafts()
  const projects = useProjects()
  const nameOf = (org: string) => projects.data?.find((p) => p.org === org)?.name ?? org
  const rows = (drafts.data ?? [])
    .slice()
    .sort((a, b) => b.updated_at.localeCompare(a.updated_at))

  return (
    <>
      <PageHead title="Release candidates" sub="apps with unreleased changes across all projects" />
      <QueryState isLoading={drafts.isLoading} error={drafts.error}>
        {rows.length === 0 && (
          <Empty>Nothing to release — every app is up to date. Candidates appear here when an app has commits since its last release.</Empty>
        )}
        {rows.length > 0 && (
          <Card className="py-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Project</TableHead>
                  <TableHead>App</TableHead>
                  <TableHead>Next</TableHead>
                  <TableHead>Bump</TableHead>
                  <TableHead className="text-right">Commits</TableHead>
                  <TableHead className="text-right">Updated</TableHead>
                  <TableHead></TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((c) => {
                  const leaf = c.app.startsWith(`${c.repo}-`) ? c.app.slice(c.repo.length + 1) : c.app
                  return (
                    <TableRow key={`${c.org}-${c.app}`}>
                      <TableCell className="font-medium">{nameOf(c.org)}</TableCell>
                      <TableCell className="font-mono text-xs">{leaf}</TableCell>
                      <TableCell className="font-mono">{c.version}</TableCell>
                      <TableCell>
                        <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${bumpChip(c.bump)}`}>{c.bump}</span>
                      </TableCell>
                      <TableCell className="text-right tabular-nums">{c.commit_count}</TableCell>
                      <TableCell className="text-right font-mono text-[11px] text-muted-foreground">{relAge(c.updated_at)}</TableCell>
                      <TableCell className="text-right">
                        <Link to="/projects/$org/apps/$app" params={{ org: c.org, app: c.app }} className="text-xs text-primary hover:underline">Review →</Link>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </Card>
        )}
      </QueryState>
    </>
  )
}

// ── Deployments: every pending render PR across all projects ──────────────────
export function AllDeploys() {
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
  const loading = results.some((r) => r.isLoading)
  const error = results.find((r) => r.error)?.error ?? null
  const m = useApiMutation({ invalidate: [['deploys'], ['events']] })

  return (
    <>
      <PageHead title="Deployments" sub="pending render PRs awaiting review across all projects" />
      <QueryState isLoading={loading} error={error as Error | null}>
        {pending.length === 0 && (
          <Empty>No pending deployment requests. Production changes appear here as render PRs awaiting review; stable auto-deploys.</Empty>
        )}
        <div className="flex flex-col gap-4">
          {pending.map(({ p, pr }) => (
            <Card key={`${p.org}-${pr.number}`} className="gap-0 py-0">
              <div className="flex flex-wrap items-center gap-2.5 border-b px-4 py-3">
                <Badge variant="secondary" className="bg-accent text-primary">{pr.class}</Badge>
                <Link to="/projects/$org" params={{ org: p.org }} className="font-medium hover:underline">{p.name}</Link>
                <h2 className="font-semibold">#{pr.number} · {pr.title}</h2>
                {pr.mergeable !== true && <Badge variant="outline" className="text-warning">reconciling…</Badge>}
                <div className="flex-1" />
                <ConfirmButton size="sm" disabled={pr.mergeable !== true}
                  title={`Merge PR #${pr.number}?`} description={`Deploy ${p.name} env/${pr.class}.`}
                  confirmText="Merge & deploy" onConfirm={() => m.mutate(() => send(urls.deployMerge(p.org, pr.number)))}>
                  {pr.class === 'production' ? 'Approve & deploy' : 'Merge & deploy'}
                </ConfirmButton>
                <ConfirmButton variant="outline" size="sm" className="text-destructive"
                  title={`Close PR #${pr.number}?`} description="Reject this change without deploying."
                  confirmText="Close" onConfirm={() => m.mutate(() => send(urls.deployClose(p.org, pr.number)))}>Close</ConfirmButton>
              </div>
              <div className="flex flex-col gap-2 px-4 py-4">
                {pr.files.length === 0 && <span className="text-xs text-muted-foreground">No file changes.</span>}
                {pr.files.map((f) => <FileDiff key={f.filename} f={f} />)}
              </div>
            </Card>
          ))}
        </div>
      </QueryState>
    </>
  )
}
