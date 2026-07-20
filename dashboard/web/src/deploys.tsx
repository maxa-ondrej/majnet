import { Link, useParams } from '@tanstack/react-router'
import { send, urls, useDeploys, type DeployFile } from './api'
import { useApiMutation } from './mutations'
import { ConfirmButton, Empty, QueryState } from './ui'
import { Crumbs, PageHead } from './views'
import { Badge } from '@/components/ui/badge'
import { Card } from '@/components/ui/card'

function DiffBlock({ patch }: { patch: string }) {
  return (
    <pre className="mt-2 overflow-x-auto rounded-md border bg-muted p-3 font-mono text-[11px] leading-relaxed">
      {patch.split('\n').map((line, i) => {
        const c = line[0] === '+' ? 'text-success' : line[0] === '-' ? 'text-destructive'
          : line.startsWith('@@') ? 'text-primary' : 'text-muted-foreground'
        return <div key={i} className={c}>{line || ' '}</div>
      })}
    </pre>
  )
}

export function FileDiff({ f }: { f: DeployFile }) {
  return (
    <details className="group">
      <summary className="cursor-pointer select-none font-mono text-xs">
        {f.filename} <span className="text-success">+{f.additions}</span> <span className="text-destructive">−{f.deletions}</span>{' '}
        <Badge variant="secondary">{f.status}</Badge>
      </summary>
      {f.patch && <DiffBlock patch={f.patch} />}
    </details>
  )
}

export function Deploys() {
  const { org } = useParams({ from: '/projects/$org/deploys' })
  const q = useDeploys(org)
  const m = useApiMutation({ invalidate: [['deploys', org], ['events']] })

  return (
    <>
      <Crumbs><Link to="/projects">Projects</Link> / <Link to="/projects/$org" params={{ org }}>{org}</Link> / Deployments</Crumbs>
      <PageHead title="Deployments" sub={`pending render PRs on ${org}/ops`} />
      <QueryState isLoading={q.isLoading} error={q.error}>
        {q.data?.length === 0 && (
          <Empty>No pending deployment requests. Production changes appear here as render PRs awaiting review; stable auto-deploys.</Empty>
        )}
        <div className="flex flex-col gap-4">
          {q.data?.map((d) => (
            <Card key={d.number} className="gap-0 py-0">
              <div className="flex flex-wrap items-center gap-2.5 border-b px-4 py-3">
                <Badge variant="secondary" className="bg-accent text-primary">{d.class}</Badge>
                <h2 className="font-semibold">#{d.number} · {d.title}</h2>
                {d.mergeable !== true && <Badge variant="outline" className="text-warning">reconciling…</Badge>}
                <div className="flex-1" />
                <ConfirmButton size="sm" disabled={d.mergeable !== true}
                  title={`Merge PR #${d.number}?`} description={`Deploy env/${d.class}.`} confirmText="Merge & deploy"
                  onConfirm={() => m.mutate(() => send(urls.deployMerge(org, d.number)))}>{d.class === 'production' ? 'Approve & deploy' : 'Merge & deploy'}</ConfirmButton>
                <ConfirmButton variant="outline" size="sm" className="text-destructive" title={`Close PR #${d.number}?`} description="Reject this change without deploying."
                  confirmText="Close" onConfirm={() => m.mutate(() => send(urls.deployClose(org, d.number)))}>Close</ConfirmButton>
              </div>
              <div className="flex flex-col gap-2 px-4 py-4">
                {d.files.length === 0 && <span className="text-xs text-muted-foreground">No file changes.</span>}
                {d.files.map((f) => <FileDiff key={f.filename} f={f} />)}
              </div>
            </Card>
          ))}
        </div>
      </QueryState>
    </>
  )
}
