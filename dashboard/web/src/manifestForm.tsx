import { X, Lock, Unlock } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Checkbox } from '@/components/ui/checkbox'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import type { ReactNode } from 'react'

// The manifest draft: optional sections carry an `on` flag so class overlays
// stay sparse (only enabled/non-empty fields are emitted).
export interface ManifestDraft {
  image: string
  /** Split image pin (ADR: image = bare repo in base, digest/tag per class). */
  digest: string
  tag: string
  ingress: { on: boolean; host: string; port: string; domains: string[] }
  health: { on: boolean; path: string; port: string; retries: string }
  database: { on: boolean; engine: string }
  env: [string, string][]
  // Opaque passthrough — the manifest form does NOT edit secret values (that's the
  // Secrets tab). Carried verbatim (legacy name list OR inline `majnet:` map, ADR
  // 0024) so a manifest edit never drops secrets.
  secrets: unknown
  migration: { on: boolean; command: string[] }
  volumes: [string, string][]
  replicas: string
  resources: { on: boolean; memory: string; cpus: string }
  /** Opt into OpenTelemetry (ADR 0023). */
  otel: boolean
  /** Container ports published on the node's WireGuard IP (cross-node reach). */
  wgPorts: string[]
}

type Rec = Record<string, unknown>
const asRec = (v: unknown): Rec => (v && typeof v === 'object' ? (v as Rec) : {})
const str = (v: unknown, d = '') => (v == null ? d : String(v))

// Top-level manifest keys the form knows how to edit. Anything else in an overlay
// (otel, wg_ports, network aliases, depends_on, …) is preserved verbatim on save.
export const MANAGED_KEYS = ['name', 'image', 'digest', 'tag', 'ingress', 'health', 'database', 'env', 'secrets', 'migration', 'volumes', 'replicas', 'resources', 'otel', 'wg_ports'] as const

export function fromData(data: unknown): ManifestDraft {
  const d = asRec(data)
  const ing = asRec(d.ingress), hl = asRec(d.health), db = asRec(d.database), mig = asRec(d.migration), env = asRec(d.env), res = asRec(d.resources)
  return {
    image: str(d.image),
    digest: str(d.digest),
    tag: str(d.tag),
    ingress: { on: !!d.ingress, host: str(ing.host), port: str(ing.port), domains: Array.isArray(ing.domains) ? ing.domains.map(String) : [] },
    health: { on: !!d.health, path: str(hl.path, '/healthz'), port: str(hl.port), retries: str(hl.retries, '5') },
    database: { on: !!d.database, engine: str(db.engine, 'postgres') },
    env: Object.entries(env)
      .map(([k, v]) => [k, String(v)] as [string, string])
      .sort((a, b) => a[0].localeCompare(b[0])),
    secrets: d.secrets,
    migration: { on: !!d.migration, command: Array.isArray(mig.command) ? mig.command.map(String) : [] },
    volumes: Array.isArray(d.volumes)
      ? d.volumes.map((v) => { const r = asRec(v); return [str(r.name), str(r.path)] as [string, string] })
      : [],
    replicas: str(d.replicas, '1'),
    resources: { on: !!d.resources, memory: str(res.memory), cpus: str(res.cpus) },
    otel: !!d.otel,
    wgPorts: Array.isArray(d.wg_ports) ? d.wg_ports.map(String) : [],
  }
}

// ── per-key serializers (shared by toManifest + toOverlay) ─────────────────────
const emitIngress = (d: ManifestDraft): Rec => {
  // A host is a production custom domain (ADR 0013); non-production classes get an
  // auto-assigned host at render, so omit it when blank.
  const ing: Rec = { port: Number(d.ingress.port) }
  const host = d.ingress.host.trim()
  if (host) ing.host = host
  const domains = host ? d.ingress.domains.map((s) => s.trim()).filter(Boolean) : []
  if (domains.length) ing.domains = domains
  return ing
}
const emitHealth = (d: ManifestDraft): Rec => ({ path: d.health.path.trim(), port: Number(d.health.port), retries: Number(d.health.retries) })
const emitEnv = (d: ManifestDraft): Rec => Object.fromEntries(d.env.map(([k, v]) => [k.trim(), v]).filter(([k]) => k))
const emitVolumes = (d: ManifestDraft) => d.volumes.map(([name, path]) => ({ name: name.trim(), path: path.trim() })).filter((v) => v.name && v.path)
const emitMigration = (d: ManifestDraft) => d.migration.command.map((s) => s.trim()).filter(Boolean)
const emitResources = (d: ManifestDraft): Rec => {
  const res: Rec = {}
  const mem = d.resources.memory.trim()
  if (mem) res.memory = mem
  const cpus = d.resources.cpus.trim()
  if (cpus) res.cpus = cpus
  return res
}
const emitWgPorts = (d: ManifestDraft) => d.wgPorts.map((p) => Number(p)).filter((p) => Number.isInteger(p) && p > 0 && p <= 65535)
const hasSecrets = (sec: unknown) => (Array.isArray(sec) ? sec.length > 0 : !!sec && typeof sec === 'object' && Object.keys(sec).length > 0)

export function toManifest(d: ManifestDraft, file: string, app: string, raw?: unknown): Rec {
  const out: Rec = {}
  // Preserve any key the form doesn't manage (future schema fields) verbatim.
  const managed = new Set<string>(MANAGED_KEYS)
  for (const [k, v] of Object.entries(asRec(raw))) if (!managed.has(k)) out[k] = v
  if (file === 'base.yaml') out.name = app // identity = directory
  if (d.image.trim()) out.image = d.image.trim()
  // Split pin (env-specific): the bare-repo `image` lives in base, the digest/tag
  // in each class overlay. Preserve verbatim so a form edit never drops the pin.
  if (d.digest.trim()) out.digest = d.digest.trim()
  if (d.tag.trim()) out.tag = d.tag.trim()
  if (d.ingress.on) out.ingress = emitIngress(d)
  if (d.health.on) out.health = emitHealth(d)
  if (d.database.on) out.database = { engine: d.database.engine }
  const env = emitEnv(d)
  if (Object.keys(env).length) out.env = env
  // Preserve secrets verbatim (list or inline map); the Secrets tab owns edits.
  if (hasSecrets(d.secrets)) out.secrets = d.secrets
  if (d.migration.on) {
    const command = emitMigration(d)
    if (command.length) out.migration = { command }
  }
  const volumes = emitVolumes(d)
  if (volumes.length) out.volumes = volumes
  const replicas = Number(d.replicas)
  if (Number.isFinite(replicas) && replicas > 1) out.replicas = replicas
  if (d.resources.on) {
    const res = emitResources(d)
    if (Object.keys(res).length) out.resources = res
  }
  if (d.otel) out.otel = true
  const wg = emitWgPorts(d)
  if (wg.length) out.wg_ports = wg
  return out
}

/** Serialize a *sparse* class overlay: only the top-level keys in `overridden`
 *  (env is per-key — every var in `d.env` is an override), plus verbatim
 *  passthrough of any unmanaged raw keys so a form save never drops otel/wg_ports/
 *  aliases. Unlike toManifest, an overridden value is emitted even at its default
 *  (e.g. replicas:1) — an overlay override is always explicit. */
export function toOverlay(d: ManifestDraft, overridden: Set<string>, raw: unknown): Rec {
  const out: Rec = {}
  const managed = new Set<string>(MANAGED_KEYS)
  for (const [k, v] of Object.entries(asRec(raw))) if (!managed.has(k)) out[k] = v // preserve unmanaged keys
  const env = emitEnv(d)
  if (Object.keys(env).length) out.env = env // per-key overrides
  if (hasSecrets(d.secrets)) out.secrets = d.secrets // class secrets, preserved verbatim
  const has = (k: string) => overridden.has(k)
  if (has('image') && d.image.trim()) out.image = d.image.trim()
  if (has('digest') && d.digest.trim()) out.digest = d.digest.trim()
  if (has('tag') && d.tag.trim()) out.tag = d.tag.trim()
  if (has('ingress')) out.ingress = emitIngress(d)
  if (has('health')) out.health = emitHealth(d)
  if (has('database')) out.database = { engine: d.database.engine }
  if (has('migration')) { const c = emitMigration(d); if (c.length) out.migration = { command: c } }
  if (has('volumes')) { const v = emitVolumes(d); if (v.length) out.volumes = v }
  if (has('replicas')) { const r = Number(d.replicas); if (Number.isFinite(r) && r >= 1) out.replicas = r }
  if (has('resources')) { const res = emitResources(d); if (Object.keys(res).length) out.resources = res }
  if (has('otel')) out.otel = d.otel // explicit false overrides an inherited true
  if (has('wg_ports')) { const wg = emitWgPorts(d); if (wg.length) out.wg_ports = wg }
  return out
}

/** Which managed top-level keys a raw overlay actually defines (env + secrets are
 *  handled by their own editors, name never appears in an overlay). */
export function overriddenKeys(raw: unknown): Set<string> {
  const skip = new Set(['env', 'secrets', 'name'])
  const managed = new Set<string>(MANAGED_KEYS)
  return new Set(Object.keys(asRec(raw)).filter((k) => managed.has(k) && !skip.has(k)))
}

// ── editors ──────────────────────────────────────────────────────────────────
function Section({ label, on, onToggle, children }: { label: string; on: boolean; onToggle: (v: boolean) => void; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-2.5 rounded-lg border p-3">
      <label className="flex items-center gap-2 text-sm font-medium">
        <Checkbox checked={on} onCheckedChange={(v) => onToggle(!!v)} /> {label}
      </label>
      {on && <div className="flex flex-col gap-2.5">{children}</div>}
    </div>
  )
}

function ListEditor({ values, onChange, placeholder }: { values: string[]; onChange: (v: string[]) => void; placeholder?: string }) {
  const set = (i: number, v: string) => onChange(values.map((x, j) => (j === i ? v : x)))
  return (
    <div className="flex flex-col gap-1.5">
      {values.map((v, i) => (
        <div key={i} className="flex gap-1.5">
          <Input value={v} placeholder={placeholder} onChange={(e) => set(i, e.target.value)} />
          <Button type="button" variant="ghost" size="icon" onClick={() => onChange(values.filter((_, j) => j !== i))}><X className="size-4" /></Button>
        </div>
      ))}
      <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => onChange([...values, ''])}>+ add</Button>
    </div>
  )
}

function KvEditor({ pairs, onChange, kPlaceholder = 'KEY', vPlaceholder = 'value' }: { pairs: [string, string][]; onChange: (v: [string, string][]) => void; kPlaceholder?: string; vPlaceholder?: string }) {
  const set = (i: number, k: string, val: string) => onChange(pairs.map((p, j) => (j === i ? [k, val] : p)))
  return (
    <div className="flex flex-col gap-1.5">
      {pairs.map(([k, v], i) => (
        <div key={i} className="grid grid-cols-[1fr_1fr_auto] gap-1.5">
          <Input value={k} placeholder={kPlaceholder} onChange={(e) => set(i, e.target.value, v)} />
          <Input value={v} placeholder={vPlaceholder} onChange={(e) => set(i, k, e.target.value)} />
          <Button type="button" variant="ghost" size="icon" onClick={() => onChange(pairs.filter((_, j) => j !== i))}><X className="size-4" /></Button>
        </div>
      ))}
      <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => onChange([...pairs, ['', '']])}>+ add</Button>
    </div>
  )
}

function Fld({ label, children }: { label: string; children: ReactNode }) {
  return <div className="flex flex-col gap-1.5"><Label className="text-xs">{label}</Label>{children}</div>
}

const ENGINES = ['postgres', 'mariadb', 'valkey', 'mongodb']

// ── shared section bodies (used by both the base form and the overlay form) ────
function IngressBody({ v, onChange }: { v: ManifestDraft['ingress']; onChange: (v: ManifestDraft['ingress']) => void }) {
  return (
    <>
      <div className="grid gap-2.5 sm:grid-cols-2">
        <Fld label="Production domain — optional"><Input value={v.host} placeholder="app.example.com" onChange={(e) => onChange({ ...v, host: e.target.value })} /></Fld>
        <Fld label="Container port"><Input type="number" value={v.port} onChange={(e) => onChange({ ...v, port: e.target.value })} /></Fld>
      </div>
      <span className="text-xs text-muted-foreground">Stable/testing/preview get an auto host <code className="font-mono">{'{app}.{project}.<base-domain>'}</code>; a domain here is a production custom domain (Cloudflare + edge).</span>
      <Fld label="Additional production domains"><ListEditor values={v.domains} placeholder="www.example.com" onChange={(domains) => onChange({ ...v, domains })} /></Fld>
    </>
  )
}
function HealthBody({ v, onChange }: { v: ManifestDraft['health']; onChange: (v: ManifestDraft['health']) => void }) {
  return (
    <div className="grid grid-cols-[2fr_1fr_1fr] gap-2.5">
      <Fld label="Path"><Input placeholder="/healthz" value={v.path} onChange={(e) => onChange({ ...v, path: e.target.value })} /></Fld>
      <Fld label="Port"><Input type="number" value={v.port} onChange={(e) => onChange({ ...v, port: e.target.value })} /></Fld>
      <Fld label="Retries"><Input type="number" value={v.retries} onChange={(e) => onChange({ ...v, retries: e.target.value })} /></Fld>
    </div>
  )
}
function DatabaseBody({ v, onChange }: { v: ManifestDraft['database']; onChange: (v: ManifestDraft['database']) => void }) {
  return (
    <Fld label="Engine">
      <Select value={v.engine} onValueChange={(engine) => onChange({ ...v, engine })}>
        <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
        <SelectContent>{ENGINES.map((en) => <SelectItem key={en} value={en}>{en}</SelectItem>)}</SelectContent>
      </Select>
    </Fld>
  )
}
function ResourcesBody({ v, onChange }: { v: ManifestDraft['resources']; onChange: (v: ManifestDraft['resources']) => void }) {
  return (
    <>
      <div className="grid gap-2.5 sm:grid-cols-2">
        <Fld label="Memory limit"><Input placeholder="512m" value={v.memory} onChange={(e) => onChange({ ...v, memory: e.target.value })} /></Fld>
        <Fld label="CPU limit (cores)"><Input placeholder="0.5" value={v.cpus} onChange={(e) => onChange({ ...v, cpus: e.target.value })} /></Fld>
      </div>
      <span className="text-xs text-muted-foreground">Hard caps on the container. Memory takes b/k/m/g (e.g. <code className="font-mono">512m</code>, <code className="font-mono">2g</code>); CPU is a core count (e.g. <code className="font-mono">0.5</code>). Blank = unlimited.</span>
    </>
  )
}
function MigrationBody({ v, onChange }: { v: ManifestDraft['migration']; onChange: (v: ManifestDraft['migration']) => void }) {
  return <Fld label="Command"><ListEditor values={v.command} placeholder="arg" onChange={(command) => onChange({ ...v, command })} /></Fld>
}

export function ManifestForm({ file, draft, onChange }: { file: string; draft: ManifestDraft; onChange: (d: ManifestDraft) => void }) {
  const set = <K extends keyof ManifestDraft>(k: K, v: ManifestDraft[K]) => onChange({ ...draft, [k]: v })
  return (
    <div className="flex flex-col gap-3.5">
      <Fld label={file === 'base.yaml' ? 'Image repository' : 'Image'}>
        <Input value={draft.image} placeholder="ghcr.io/org/app" onChange={(e) => set('image', e.target.value)} />
        <span className="text-xs text-muted-foreground">{file === 'base.yaml'
          ? 'Env-unspecific repository (or a full digest-pinned ref). Each class overlay supplies its own digest below.'
          : 'Optional per-class repository override — usually blank (inherits base); this env’s pin goes in Digest.'}</span>
      </Fld>
      <Fld label="Digest — optional (env-specific pins live in each overlay)"><Input value={draft.digest} placeholder="sha256:…" onChange={(e) => set('digest', e.target.value)} /></Fld>

      <Section label="Ingress" on={draft.ingress.on} onToggle={(on) => set('ingress', { ...draft.ingress, on })}>
        <IngressBody v={draft.ingress} onChange={(v) => set('ingress', v)} />
      </Section>

      <Fld label="Replicas">
        <Input type="number" min="1" value={draft.replicas} onChange={(e) => set('replicas', e.target.value)} />
        <span className="text-xs text-muted-foreground">Container replicas, load-balanced by the edge. Must be 1 for apps with a persistent volume (single-writer).</span>
      </Fld>

      <Section label="Resource limits" on={draft.resources.on} onToggle={(on) => set('resources', { ...draft.resources, on })}>
        <ResourcesBody v={draft.resources} onChange={(v) => set('resources', v)} />
      </Section>

      <Section label="Health check" on={draft.health.on} onToggle={(on) => set('health', { ...draft.health, on })}>
        <HealthBody v={draft.health} onChange={(v) => set('health', v)} />
      </Section>

      <Section label="Database" on={draft.database.on} onToggle={(on) => set('database', { ...draft.database, on })}>
        <DatabaseBody v={draft.database} onChange={(v) => set('database', v)} />
      </Section>

      <Fld label="Environment variables"><KvEditor pairs={draft.env} onChange={(env) => set('env', env)} /></Fld>
      {/* Secrets are edited in the dedicated Secrets section below (encrypted per
          key via the bot); this form preserves them untouched on save. */}

      <Fld label="Volumes">
        <KvEditor pairs={draft.volumes} kPlaceholder="name" vPlaceholder="/app/data" onChange={(volumes) => set('volumes', volumes)} />
        <span className="text-xs text-muted-foreground">Persistent named volumes (name → container mount path); survive redeploys, never auto-deleted.</span>
      </Fld>

      <Section label="Migration" on={draft.migration.on} onToggle={(on) => set('migration', { ...draft.migration, on })}>
        <MigrationBody v={draft.migration} onChange={(v) => set('migration', v)} />
      </Section>

      <div className="flex flex-col gap-2.5 rounded-lg border p-3">
        <label className="flex items-center gap-2 text-sm font-medium">
          <Checkbox checked={draft.otel} onCheckedChange={(v) => set('otel', !!v)} /> OpenTelemetry
        </label>
        <span className="text-xs text-muted-foreground">Inject the OTLP endpoint + resource attributes so the app emits traces &amp; logs (ADR 0023). Inert until a collector is configured.</span>
      </div>

      <Fld label="WireGuard published ports">
        <ListEditor values={draft.wgPorts} placeholder="4317" onChange={(wgPorts) => set('wgPorts', wgPorts)} />
        <span className="text-xs text-muted-foreground">Container ports bound to the node’s WireGuard IP for cross-node reach (e.g. an OTLP collector). Requires replicas 1. Empty = none.</span>
      </Fld>
    </div>
  )
}

// ── overlay (env override) form: one merged surface, per-field inheritance ─────
/** A field row that is either inherited from base (read-only preview + Override) or
 *  overridden for this env (the editor + Revert). */
function OverrideRow({ label, active, preview, onOverride, onRevert, children }: {
  label: string; active: boolean; preview: string; onOverride: () => void; onRevert: () => void; children: ReactNode
}) {
  return (
    <div className={`flex flex-col gap-2.5 rounded-lg border p-3 ${active ? 'border-primary/40 bg-primary/[0.03]' : 'bg-muted/30'}`}>
      <div className="flex items-center justify-between gap-2">
        <span className="flex items-center gap-1.5 text-sm font-medium">
          {active
            ? <Unlock className="size-3.5 text-primary" />
            : <Lock className="size-3.5 text-muted-foreground" />}
          {label}
        </span>
        {active
          ? <Button type="button" variant="ghost" size="sm" className="h-7 text-xs text-muted-foreground" onClick={onRevert}>Revert to base</Button>
          : <Button type="button" variant="outline" size="sm" className="h-7 text-xs" onClick={onOverride}>Override</Button>}
      </div>
      {active
        ? children
        : <div className="truncate font-mono text-xs text-muted-foreground">{preview || <span className="italic">inherits base</span>}</div>}
    </div>
  )
}

const previewSection = (on: boolean, summary: string) => (on ? summary : 'disabled')

export function OverlayForm({ cls, base, draft, overridden, onChange }: {
  cls: string
  base: ManifestDraft
  draft: ManifestDraft
  overridden: Set<string>
  onChange: (draft: ManifestDraft, overridden: Set<string>) => void
}) {
  const setField = <K extends keyof ManifestDraft>(k: K, v: ManifestDraft[K]) => onChange({ ...draft, [k]: v }, overridden)
  const override = (k: string, patch: Partial<ManifestDraft>) => onChange({ ...draft, ...patch }, new Set(overridden).add(k))
  const revert = (k: string) => { const n = new Set(overridden); n.delete(k); onChange(draft, n) }
  const ov = (k: string) => overridden.has(k)

  // env: every var in the overlay draft is an override; the rest are inherited.
  const baseEnv = Object.fromEntries(base.env)
  const overlayKeys = new Set(draft.env.map(([k]) => k))
  const inheritedEnv = Object.keys(baseEnv).filter((k) => !overlayKeys.has(k)).sort()
  const setEnv = (env: [string, string][]) => setField('env', env)

  return (
    <div className="flex flex-col gap-3">
      <p className="text-xs text-muted-foreground">
        Editing the <span className="font-medium text-foreground">{cls}</span> overlay. Locked fields inherit
        <span className="font-mono"> base.yaml</span>; override any field to pin it for {cls} only. Shared defaults are edited under
        <span className="font-medium text-foreground"> Base defaults</span>.
      </p>

      <OverrideRow label="Image digest — this env’s pin" active={ov('digest')} preview={base.digest}
        onOverride={() => override('digest', { digest: base.digest })} onRevert={() => revert('digest')}>
        <Input value={draft.digest} placeholder="sha256:…" onChange={(e) => setField('digest', e.target.value)} />
        <span className="text-xs text-muted-foreground">The image pin for {cls} (digest preferred over tag, §5).</span>
      </OverrideRow>

      <OverrideRow label="Image repository" active={ov('image')} preview={base.image}
        onOverride={() => override('image', { image: base.image })} onRevert={() => revert('image')}>
        <Input value={draft.image} placeholder="ghcr.io/org/app" onChange={(e) => setField('image', e.target.value)} />
        <span className="text-xs text-muted-foreground">Usually inherited — override only for a per-env repository.</span>
      </OverrideRow>

      <OverrideRow label="Ingress" active={ov('ingress')}
        preview={previewSection(base.ingress.on, `${base.ingress.host || 'auto host'} :${base.ingress.port}`)}
        onOverride={() => override('ingress', { ingress: { ...base.ingress, on: true } })} onRevert={() => revert('ingress')}>
        <IngressBody v={draft.ingress} onChange={(v) => setField('ingress', { ...v, on: true })} />
      </OverrideRow>

      <OverrideRow label="Replicas" active={ov('replicas')} preview={base.replicas}
        onOverride={() => override('replicas', { replicas: base.replicas })} onRevert={() => revert('replicas')}>
        <Input type="number" min="1" value={draft.replicas} onChange={(e) => setField('replicas', e.target.value)} />
      </OverrideRow>

      <OverrideRow label="Resource limits" active={ov('resources')}
        preview={previewSection(base.resources.on, `${base.resources.memory || '∞'} / ${base.resources.cpus || '∞'} cpu`)}
        onOverride={() => override('resources', { resources: { ...base.resources, on: true } })} onRevert={() => revert('resources')}>
        <ResourcesBody v={draft.resources} onChange={(v) => setField('resources', { ...v, on: true })} />
      </OverrideRow>

      <OverrideRow label="Health check" active={ov('health')}
        preview={previewSection(base.health.on, `${base.health.path} :${base.health.port}`)}
        onOverride={() => override('health', { health: { ...base.health, on: true } })} onRevert={() => revert('health')}>
        <HealthBody v={draft.health} onChange={(v) => setField('health', { ...v, on: true })} />
      </OverrideRow>

      <OverrideRow label="Database" active={ov('database')}
        preview={previewSection(base.database.on, base.database.engine)}
        onOverride={() => override('database', { database: { ...base.database, on: true } })} onRevert={() => revert('database')}>
        <DatabaseBody v={draft.database} onChange={(v) => setField('database', { ...v, on: true })} />
      </OverrideRow>

      {/* Environment variables — per-key inheritance (maps merge recursively). */}
      <div className="flex flex-col gap-2 rounded-lg border p-3">
        <span className="text-sm font-medium">Environment variables</span>
        <span className="text-xs text-muted-foreground">Base vars apply to every env; add or override individual vars for {cls}.</span>
        {draft.env.length > 0 && (
          <div className="flex flex-col gap-1.5">
            {draft.env.map(([k, v], i) => (
              <div key={i} className="grid grid-cols-[1fr_1fr_auto] gap-1.5">
                <Input value={k} placeholder="KEY" onChange={(e) => setEnv(draft.env.map((p, j) => (j === i ? [e.target.value, p[1]] : p)))} />
                <Input value={v} placeholder="value" onChange={(e) => setEnv(draft.env.map((p, j) => (j === i ? [p[0], e.target.value] : p)))} />
                <Button type="button" variant="ghost" size="icon" title="Revert to base" onClick={() => setEnv(draft.env.filter((_, j) => j !== i))}><X className="size-4" /></Button>
              </div>
            ))}
          </div>
        )}
        <Button type="button" variant="outline" size="sm" className="self-start" onClick={() => setEnv([...draft.env, ['', '']])}>+ override / add var</Button>
        {inheritedEnv.length > 0 && (
          <div className="mt-1 flex flex-col gap-1 border-t pt-2">
            <span className="text-xs text-muted-foreground">Inherited from base</span>
            {inheritedEnv.map((k) => (
              <div key={k} className="flex items-center gap-2">
                <span className="w-44 truncate font-mono text-xs">{k}</span>
                <span className="flex-1 truncate font-mono text-xs text-muted-foreground">{baseEnv[k]}</span>
                <Button type="button" variant="ghost" size="sm" className="h-6 text-xs" onClick={() => setEnv([...draft.env, [k, baseEnv[k] ?? '']])}>Override</Button>
              </div>
            ))}
          </div>
        )}
      </div>

      <OverrideRow label="Volumes" active={ov('volumes')}
        preview={base.volumes.map(([n, p]) => `${n}→${p}`).join(', ')}
        onOverride={() => override('volumes', { volumes: base.volumes.length ? base.volumes : [['', '']] })} onRevert={() => revert('volumes')}>
        <KvEditor pairs={draft.volumes} kPlaceholder="name" vPlaceholder="/app/data" onChange={(volumes) => setField('volumes', volumes)} />
      </OverrideRow>

      <OverrideRow label="Migration" active={ov('migration')}
        preview={previewSection(base.migration.on, base.migration.command.join(' '))}
        onOverride={() => override('migration', { migration: { ...base.migration, on: true } })} onRevert={() => revert('migration')}>
        <MigrationBody v={draft.migration} onChange={(v) => setField('migration', { ...v, on: true })} />
      </OverrideRow>

      <OverrideRow label="OpenTelemetry" active={ov('otel')} preview={base.otel ? 'on' : 'off'}
        onOverride={() => override('otel', { otel: base.otel })} onRevert={() => revert('otel')}>
        <label className="flex items-center gap-2 text-sm"><Checkbox checked={draft.otel} onCheckedChange={(v) => setField('otel', !!v)} /> Emit traces &amp; logs for {cls}</label>
      </OverrideRow>

      <OverrideRow label="WireGuard published ports" active={ov('wg_ports')} preview={base.wgPorts.join(', ')}
        onOverride={() => override('wg_ports', { wgPorts: base.wgPorts.length ? base.wgPorts : [''] })} onRevert={() => revert('wg_ports')}>
        <ListEditor values={draft.wgPorts} placeholder="4317" onChange={(wgPorts) => setField('wgPorts', wgPorts)} />
      </OverrideRow>
    </div>
  )
}
