// Control plane page (#14) — the platform-admin surface for updating MajNet's
// own services (bot · reconciler · dashboard). Updates go through git: publishing
// commits a new pin to platform/version.yaml; master-1's majnet-update timer
// applies it. Nothing here executes on the host.
//
// The rollout recreates the control-plane containers (a few seconds of control-
// plane downtime; deployed apps keep running). The progress here is driven by a
// real signal — the bot reports its running build at /api/control-plane, so
// `converged=false` means the running build doesn't match the pinned one yet.
// This page is a client-side SPA, so it survives the dashboard's own restart and
// keeps polling across the blip (see useControlPlane's keepPreviousData).
import { useEffect, useRef, useState } from 'react'
import {
  ArrowUp, Cpu, GitCommit, History, Info, Loader2, RefreshCw, RotateCw, ServerCog, TriangleAlert,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { useControlPlane, useNodes, send, urls, type CpPin, type CpCommit, type ControlPlaneStatus } from './api'
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
const BUNDLE_COMMIT = import.meta.env.VITE_BUILD_COMMIT ?? ''

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

// ── rollout progress ──────────────────────────────────────────────────────────
// Honest progress: a time-based bar that eases toward ~90% over the expected
// rollout window and only completes when the real `converged` signal flips true.
const ROLLOUT_ESTIMATE_MS = 75_000

function RolloutProgress({ cp, node, stale, reconnecting }: {
  cp: ControlPlaneStatus; node: string; stale: boolean; reconnecting: boolean
}) {
  const startRef = useRef<number>(Date.now())
  const [, tick] = useState(0)
  // Re-render on a timer so the bar advances between polls.
  useEffect(() => {
    const id = setInterval(() => tick((n) => n + 1), 500)
    return () => clearInterval(id)
  }, [])

  const elapsed = Date.now() - startRef.current
  const pct = Math.min(90, Math.round((elapsed / ROLLOUT_ESTIMATE_MS) * 90))
  const running = cp.running.commit ? shortRef(cp.running.commit) : 'unknown'

  return (
    <Section>
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <StatusBadge tone="accent"><Loader2 className="size-3.5 animate-spin" /> Rolling out</StatusBadge>
        <span className="text-[13px] text-muted-foreground">
          · <span className="font-mono">{node}</span> is converging to <span className="font-mono text-foreground">{shortRef(cp.current.ref)}</span>
        </span>
      </div>

      <div className="mt-3.5 h-2 overflow-hidden rounded-full bg-muted">
        <div
          className="h-full rounded-full bg-primary transition-[width] duration-500 ease-out"
          style={{ width: `${pct}%` }}
        />
      </div>

      <div className="mt-3 flex flex-col gap-2">
        <Step done label="Pin committed" detail={`version.yaml → ${shortRef(cp.current.ref)}`} />
        <Step active label="Recreating control-plane containers" detail="bot · reconciler · dashboard (a few seconds of control-plane downtime; apps keep running)" />
        <Step label="Converged" detail={`running build ${running} → ${shortRef(cp.current.ref)}`} />
      </div>

      {reconnecting && (
        <div className="mt-3 flex items-center gap-2 text-[12.5px] text-muted-foreground">
          <Loader2 className="size-3.5 animate-spin" /> Reconnecting to the control plane…
        </div>
      )}
      {stale && (
        <div className="mt-3 flex items-start gap-2 rounded-md bg-warning/10 px-3 py-2 text-[12.5px] text-foreground/80">
          <RotateCw className="mt-0.5 size-3.5 shrink-0 text-warning" />
          <div>This page is running an older bundle. Once the rollout finishes, reload to pick up the new dashboard.</div>
        </div>
      )}
    </Section>
  )
}

function Step({ done, active, label, detail }: { done?: boolean; active?: boolean; label: string; detail?: string }) {
  const dot = done ? 'bg-success text-primary-foreground' : active ? 'border-2 border-primary text-primary' : 'border-2 border-border text-muted-foreground'
  return (
    <div className="flex items-start gap-2.5">
      <span className={`mt-0.5 grid size-4 shrink-0 place-items-center rounded-full text-[9px] ${dot}`}>
        {done ? '✓' : active ? <Loader2 className="size-2.5 animate-spin" /> : ''}
      </span>
      <div className="min-w-0">
        <div className={`text-[13px] ${active || done ? 'font-medium' : 'text-muted-foreground'}`}>{label}</div>
        {detail && <div className="text-[12px] text-muted-foreground">{detail}</div>}
      </div>
    </div>
  )
}

export function ControlPlane() {
  const q = useControlPlane()
  const nodes = useNodes()
  const mainNode = nodes.data?.find((n) => n.role === 'main')?.name ?? 'master-1'

  // A short optimistic window right after publishing, before the bot has
  // restarted onto the new pin and can report converged=false itself.
  const [justPublished, setJustPublished] = useState<number | null>(null)
  useEffect(() => {
    if (justPublished == null) return
    const id = setTimeout(() => setJustPublished(null), 20_000)
    return () => clearTimeout(id)
  }, [justPublished])

  const publish = useApiMutation({
    invalidate: [['control-plane'], ['events'], ['botEvents']],
    onDone: () => setJustPublished(Date.now()),
  })

  const cp = q.data
  const src = cp?.source
  const commitUrl = (ref: string) => (src ? `https://github.com/${src.org}/${src.repo}/commit/${ref}` : '#')

  // Rolling out when the bot reports the running build != pinned, or in the
  // optimistic post-publish window. The already-loaded SPA survives the
  // dashboard restart; q keeps its last data across the blip.
  const rolling = cp != null && (cp.converged === false || (justPublished != null && cp.converged !== true))
  const reconnecting = q.isError && cp != null
  // The loaded bundle is stale if it doesn't match the pinned build.
  const stale = !!BUNDLE_COMMIT && cp != null && !cp.current.ref.startsWith(BUNDLE_COMMIT.slice(0, 7))

  const headStatus = !cp ? null
    : rolling ? <StatusBadge tone="accent"><Loader2 className="size-3.5 animate-spin" /> Rolling out</StatusBadge>
    : cp.up_to_date ? <StatusBadge tone="success" dot>Up to date</StatusBadge>
    : cp.latest ? <StatusBadge tone="warn">Update available</StatusBadge>
    : cp.latest_building ? <StatusBadge tone="muted"><Loader2 className="size-3.5 animate-spin" /> Publishing…</StatusBadge>
    : <StatusBadge tone="muted">Couldn’t check</StatusBadge>

  return (
    <>
      <PageHead title="Control plane">{headStatus}</PageHead>
      {/* Once we have data, keep showing it even if a refetch errors mid-rollout. */}
      <QueryState isLoading={q.isLoading} error={cp ? null : q.error}>
        {cp && (
          <div className="mx-auto flex max-w-3xl flex-col gap-4">
            {/* How updates work */}
            <div className="flex items-start gap-2.5 rounded-lg bg-accent px-3.5 py-3 text-[13px] leading-relaxed text-accent-foreground">
              <Info className="mt-0.5 size-4 shrink-0" />
              <div>
                Updates go through git, like everything else. Publishing commits the new pin to{' '}
                <span className="font-mono">platform/version.yaml</span>; <span className="font-mono">{mainNode}</span>’s updater
                picks up the change (within ~1 min) and recreates the control-plane containers. Nothing executes on the host from here.
              </div>
            </div>

            {/* Live rollout progress — the real signal */}
            {rolling && <RolloutProgress cp={cp} node={mainNode} stale={stale} reconnecting={reconnecting} />}

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
                {cp.running.version && (
                  <span className="ml-auto text-[12.5px] text-muted-foreground">
                    build <span className="font-mono text-foreground">{cp.running.version}</span>
                    {cp.running.build_time ? ` · ${relTime(cp.running.build_time)}` : ''}
                  </span>
                )}
              </div>
              <Sep />
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <CompRow name="control-plane" hint="bot · reconciler" digest={digestLabel(cp.current.image)} />
                <CompRow name="dashboard" digest={digestLabel(cp.current.dashboard)} />
              </div>
              <Sep />
              <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
                <Stat label="Node"><span className="font-mono">{mainNode}</span></Stat>
                <Stat label="Root org"><span className="font-mono">{src?.org ?? '—'}</span></Stat>
                <Stat label="Running build">
                  <span className="font-mono">{cp.running.commit ? shortRef(cp.running.commit) : '—'}</span>
                </Stat>
              </div>
            </Section>

            {/* Up to date */}
            {!rolling && cp.up_to_date && (
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
            {/* Latest build still publishing (CI hasn't pushed sha-<HEAD> yet) */}
            {!rolling && !cp.latest && cp.latest_building && (
              <Section>
                <div className="flex items-center gap-3 text-[13px]">
                  <Loader2 className="size-4 shrink-0 animate-spin text-muted-foreground" />
                  <span>The latest build is still publishing — CI is pushing the images. Check back in a moment.</span>
                  <Button variant="ghost" size="sm" className="ml-auto" onClick={() => q.refetch()} disabled={q.isFetching}>
                    <RefreshCw className={`size-4 ${q.isFetching ? 'animate-spin' : ''}`} /> Check now
                  </Button>
                </div>
              </Section>
            )}

            {!rolling && !cp.latest && !cp.latest_building && cp.check_error && (
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
            {!rolling && cp.latest && !cp.up_to_date && (
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
                    description="This commits a new pin to platform/version.yaml. master-1 recreates the control-plane containers (bot, reconciler, dashboard) — a few seconds of control-plane downtime; your deployed apps keep running. This page tracks the rollout live."
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
    <div className="flex min-w-0 flex-col gap-1.5 rounded-md border px-3 py-2">
      <span className="flex items-center gap-1.5 text-[13px] font-medium"><GitCommit className="size-3.5 shrink-0 text-muted-foreground" />{name}</span>
      <div className="flex min-w-0 flex-col gap-0.5 font-mono text-xs">
        <span className="truncate text-muted-foreground line-through decoration-muted-foreground/40">{from}</span>
        <span className="flex min-w-0 items-center gap-1"><span className="shrink-0 text-muted-foreground">→</span><span className="truncate text-foreground">{to}</span></span>
      </div>
    </div>
  )
}
