// Control plane page (#14) — the platform-admin surface for updating MajNet's
// own services (bot · reconciler · dashboard). Updates go through git: publishing
// commits a new pin to platform/version.yaml; master-1's majnet-update timer
// applies it. Nothing here executes on the host.
import { useEffect, useState } from 'react'
import {
  ArrowUp, Cpu, GitCommit, History, Info, RefreshCw, ServerCog, TriangleAlert,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useControlPlane, useNodes, send, urls, type CpPin, type CpCommit } from './api'
import { useApiMutation } from './mutations'
import { PageHead } from './views'
import { StatusBadge, QueryState, ConfirmButton } from './ui'

// The image's digest (short) or, if still tag-pinned, the tag.
function digestLabel(img: string | null): string {
  if (!img) return '—'
  const at = img.indexOf('@sha256:')
  if (at >= 0) return `sha256:${img.slice(at + 8, at + 8 + 12)}…`
  return img.slice(img.lastIndexOf(':') + 1) || img
}
const shortRef = (r: string) => r.slice(0, 7)

function relTime(at: string): string {
  const t = Date.parse(at)
  if (Number.isNaN(t)) return at
  const s = Math.round((Date.now() - t) / 1000)
  if (s < 60) return 'just now'
  const m = Math.round(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.round(m / 60)
  if (h < 24) return `${h}h ago`
  return `${Math.round(h / 24)}d ago`
}

function Sep() {
  return <div className="my-4 h-px bg-border" />
}
function Section({ children }: { children: React.ReactNode }) {
  return <div className="rounded-lg border bg-card px-4 py-4 sm:px-5">{children}</div>
}
function Stat({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="mt-0.5 text-sm font-medium">{children}</div>
    </div>
  )
}

export function ControlPlane() {
  const q = useControlPlane()
  const nodes = useNodes()
  const mainNode = nodes.data?.find((n) => n.role === 'main')?.name

  // A brief "rolling out" banner after a successful publish — the host converges
  // out of band (~1 min), so the pin flips to up-to-date before the rollout ends.
  const [rollingOut, setRollingOut] = useState<number | null>(null)
  useEffect(() => {
    if (rollingOut == null) return
    const id = setTimeout(() => setRollingOut(null), 90_000)
    return () => clearTimeout(id)
  }, [rollingOut])

  const publish = useApiMutation({
    invalidate: [['control-plane'], ['events'], ['botEvents']],
    onDone: () => setRollingOut(Date.now()),
  })

  const cp = q.data
  const src = cp?.source
  const commitUrl = (ref: string) => (src ? `https://github.com/${src.org}/${src.repo}/commit/${ref}` : '#')

  const headStatus = !cp ? null
    : cp.up_to_date ? <StatusBadge tone="success" dot>Up to date</StatusBadge>
    : cp.latest ? <StatusBadge tone="warn">Update available</StatusBadge>
    : <StatusBadge tone="muted">Couldn’t check</StatusBadge>

  return (
    <>
      <PageHead title="Control plane">{headStatus}</PageHead>
      <QueryState isLoading={q.isLoading} error={q.error}>
        {cp && (
          <div className="mx-auto flex max-w-3xl flex-col gap-4">
            {/* How updates work */}
            <div className="flex items-start gap-2.5 rounded-lg bg-accent px-3.5 py-3 text-[13px] leading-relaxed text-accent-foreground">
              <Info className="mt-0.5 size-4 shrink-0" />
              <div>
                Updates go through git, like everything else. Publishing commits the new pin to{' '}
                <span className="font-mono">platform/version.yaml</span>; <span className="font-mono">{mainNode ?? 'master-1'}</span>’s
                updater picks up the change and runs the blue-green rollout. Nothing executes on the host from here.
              </div>
            </div>

            {rollingOut != null && (
              <div className="flex items-start gap-2.5 rounded-lg border border-primary/30 bg-primary/10 px-3.5 py-3 text-[13px] leading-relaxed">
                <ServerCog className="mt-0.5 size-4 shrink-0 text-primary" />
                <div>
                  Rollout in progress on <span className="font-mono">{mainNode ?? 'master-1'}</span> — it converges within
                  ~1 minute. The dashboard restarts last, so this page may briefly disconnect and reconnect.
                </div>
              </div>
            )}

            {/* Running now */}
            <Section>
              <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                <ServerCog className="size-4 text-muted-foreground" />
                <h2 className="text-sm font-semibold">Running now</h2>
                <span className="text-[13px] text-muted-foreground">
                  · pinned at{' '}
                  <a href={commitUrl(cp.current.ref)} target="_blank" rel="noreferrer" className="font-mono text-primary hover:underline">
                    {shortRef(cp.current.ref)}
                  </a>
                </span>
              </div>
              <Sep />
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <CompRow name="control-plane" hint="bot · reconciler" digest={digestLabel(cp.current.image)} />
                <CompRow name="dashboard" digest={digestLabel(cp.current.dashboard)} />
              </div>
              <Sep />
              <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
                <Stat label="Node"><span className="font-mono">{mainNode ?? '—'}</span></Stat>
                <Stat label="Root org"><span className="font-mono">{src?.org ?? '—'}</span></Stat>
                <Stat label="Source"><span className="font-mono">{src ? `${src.org}/${src.repo}` : '—'}</span></Stat>
              </div>
            </Section>

            {/* Up to date */}
            {cp.up_to_date && (
              <Section>
                <div className="flex flex-wrap items-center gap-3">
                  <StatusBadge tone="success" dot>Up to date</StatusBadge>
                  <span className="text-[13px] text-muted-foreground">Matches the latest build on the source’s main.</span>
                  <Button variant="ghost" size="sm" className="ml-auto" onClick={() => q.refetch()} disabled={q.isFetching}>
                    <RefreshCw className={`size-4 ${q.isFetching ? 'animate-spin' : ''}`} /> Check now
                  </Button>
                </div>
              </Section>
            )}

            {/* Couldn't check */}
            {!cp.latest && cp.check_error && (
              <Section>
                <div className="flex items-start gap-2.5 text-[13px]">
                  <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warning" />
                  <div>
                    <div className="font-medium">Couldn’t check for updates</div>
                    <div className="mt-0.5 break-words font-mono text-xs text-muted-foreground">{cp.check_error}</div>
                  </div>
                  <Button variant="ghost" size="sm" className="ml-auto" onClick={() => q.refetch()} disabled={q.isFetching}>
                    <RefreshCw className={`size-4 ${q.isFetching ? 'animate-spin' : ''}`} /> Retry
                  </Button>
                </div>
              </Section>
            )}

            {/* Update available */}
            {cp.latest && !cp.up_to_date && (
              <Section>
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <StatusBadge tone="warn"><ArrowUp className="size-3.5" /> Update available</StatusBadge>
                  <span className="text-[13px] text-muted-foreground">
                    · new pin <span className="font-mono text-foreground">{shortRef(cp.latest.ref)}</span>
                  </span>
                </div>
                <Sep />
                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                  <DiffRow name="control-plane" from={digestLabel(cp.current.image)} to={digestLabel(cp.latest.image)} />
                  <DiffRow name="dashboard" from={digestLabel(cp.current.dashboard)} to={digestLabel(cp.latest.dashboard)} />
                </div>

                {cp.commits.length > 0 && (
                  <>
                    <Sep />
                    <div className="mb-1.5 text-[13px] text-muted-foreground">
                      {cp.commits.length} commit{cp.commits.length === 1 ? '' : 's'} since the running pin
                    </div>
                    <div className="flex flex-col">
                      {cp.commits.map((c: CpCommit) => (
                        <div key={c.sha} className="flex items-baseline gap-2.5 border-t py-1.5 text-[13px] first:border-t-0">
                          <a href={commitUrl(c.sha)} target="_blank" rel="noreferrer" className="shrink-0 font-mono text-xs text-primary hover:underline">{c.sha}</a>
                          <span className="min-w-0 truncate">{c.message}</span>
                        </div>
                      ))}
                    </div>
                  </>
                )}

                <Sep />
                <div className="flex flex-wrap items-center gap-3">
                  <ConfirmButton
                    title={`Update control plane to ${shortRef(cp.latest.ref)}?`}
                    description="This commits a new pin to platform/version.yaml. master-1 will pull the images and roll out bot, reconciler, and dashboard behind health gates. The dashboard restarts last, so this page may briefly disconnect."
                    confirmText="Update"
                    onConfirm={() => publish.mutate(() => send(urls.controlPlanePin, { method: 'PUT', json: pinBody(cp.latest!) }))}
                    disabled={publish.isPending}
                  >
                    <ArrowUp className="size-4" /> Update to {shortRef(cp.latest.ref)}
                  </ConfirmButton>
                  {src?.compare_url && (
                    <a href={src.compare_url} target="_blank" rel="noreferrer" className="text-[13px] text-primary hover:underline">
                      View diff ↗
                    </a>
                  )}
                  <span className="ml-auto text-xs text-muted-foreground">Platform-admin · commits version.yaml</span>
                </div>
                <div className="mt-3 flex items-start gap-2.5 rounded-md bg-warning/10 px-3 py-2.5 text-[12.5px] leading-relaxed text-warning">
                  <TriangleAlert className="mt-0.5 size-4 shrink-0" />
                  <div className="text-foreground/80">
                    Roll-forward only fixes are safe. If a build misbehaves, use <b>Roll back</b> below to return to a known-good pin.
                  </div>
                </div>
              </Section>
            )}

            {/* Pin history */}
            {cp.history.length > 0 && (
              <Section>
                <div className="flex items-center gap-2">
                  <History className="size-4 text-muted-foreground" />
                  <h2 className="text-sm font-semibold">Pin history</h2>
                  <span className="text-[13px] text-muted-foreground">· recent commits to version.yaml</span>
                </div>
                <Sep />
                <div className="flex flex-col">
                  {cp.history.map((h) => (
                    <div key={h.sha} className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t py-2.5 first:border-t-0">
                      <span className="font-mono text-[13px] text-primary">{shortRef(h.sha)}</span>
                      {h.current && <StatusBadge tone="success" dot>current</StatusBadge>}
                      <span className="min-w-0 flex-1 truncate text-[13px] text-muted-foreground" title={h.message}>{h.message}</span>
                      <span className="text-xs text-muted-foreground" title={h.date}>{relTime(h.date)} · {h.author}</span>
                      {!h.current && (
                        <ConfirmButton
                          variant="outline"
                          size="xs"
                          title={`Roll back to ${shortRef(h.sha)}?`}
                          description="This re-commits the pin from that commit to version.yaml. master-1 will roll the control plane back to it."
                          confirmText="Roll back"
                          onConfirm={() => publish.mutate(() => send(urls.controlPlanePin, { method: 'PUT', json: { from_commit: h.sha } }))}
                          disabled={publish.isPending}
                        >
                          Roll back
                        </ConfirmButton>
                      )}
                    </div>
                  ))}
                </div>
              </Section>
            )}
          </div>
        )}
      </QueryState>
    </>
  )
}

function pinBody(p: CpPin) {
  return { ref: p.ref, image: p.image, dashboard: p.dashboard }
}

function CompRow({ name, hint, digest }: { name: string; hint?: string; digest: string }) {
  return (
    <div className="flex items-center justify-between gap-3 rounded-md border px-3 py-2">
      <div className="flex items-center gap-1.5">
        <Cpu className="size-3.5 text-muted-foreground" />
        <span className="text-[13px] font-medium">{name}</span>
        {hint && <span className="text-[11px] text-muted-foreground">· {hint}</span>}
      </div>
      <span className="font-mono text-xs text-muted-foreground">{digest}</span>
    </div>
  )
}

function DiffRow({ name, from, to }: { name: string; from: string; to: string }) {
  return (
    <div className="flex items-center justify-between gap-2 rounded-md border px-3 py-2">
      <span className="flex items-center gap-1.5 text-[13px] font-medium"><GitCommit className="size-3.5 text-muted-foreground" />{name}</span>
      <span className="flex items-center gap-1.5 font-mono text-xs">
        <span className="text-muted-foreground line-through decoration-muted-foreground/40">{from}</span>
        <span className="text-muted-foreground">→</span>
        <span className="text-foreground">{to}</span>
      </span>
    </div>
  )
}
