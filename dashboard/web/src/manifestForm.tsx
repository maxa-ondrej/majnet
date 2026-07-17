import { X } from 'lucide-react'
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
  ingress: { on: boolean; host: string; port: string; domains: string[] }
  health: { on: boolean; path: string; port: string; retries: string }
  database: { on: boolean; engine: string }
  env: [string, string][]
  secrets: string[]
  migration: { on: boolean; command: string[] }
  volumes: [string, string][]
  replicas: string
  resources: { on: boolean; memory: string; cpus: string }
}

type Rec = Record<string, unknown>
const asRec = (v: unknown): Rec => (v && typeof v === 'object' ? (v as Rec) : {})
const str = (v: unknown, d = '') => (v == null ? d : String(v))

export function fromData(data: unknown): ManifestDraft {
  const d = asRec(data)
  const ing = asRec(d.ingress), hl = asRec(d.health), db = asRec(d.database), mig = asRec(d.migration), env = asRec(d.env), res = asRec(d.resources)
  return {
    image: str(d.image),
    ingress: { on: !!d.ingress, host: str(ing.host), port: str(ing.port), domains: Array.isArray(ing.domains) ? ing.domains.map(String) : [] },
    health: { on: !!d.health, path: str(hl.path, '/healthz'), port: str(hl.port), retries: str(hl.retries, '5') },
    database: { on: !!d.database, engine: str(db.engine, 'postgres') },
    env: Object.entries(env)
      .map(([k, v]) => [k, String(v)] as [string, string])
      .sort((a, b) => a[0].localeCompare(b[0])),
    secrets: Array.isArray(d.secrets) ? d.secrets.map(String) : [],
    migration: { on: !!d.migration, command: Array.isArray(mig.command) ? mig.command.map(String) : [] },
    volumes: Array.isArray(d.volumes)
      ? d.volumes.map((v) => { const r = asRec(v); return [str(r.name), str(r.path)] as [string, string] })
      : [],
    replicas: str(d.replicas, '1'),
    resources: { on: !!d.resources, memory: str(res.memory), cpus: str(res.cpus) },
  }
}

export function toManifest(d: ManifestDraft, file: string, app: string): Rec {
  const out: Rec = {}
  if (file === 'base.yaml') out.name = app // identity = directory
  if (d.image.trim()) out.image = d.image.trim()
  if (d.ingress.on) {
    // A host is a production custom domain (ADR 0013); non-production classes
    // get an auto-assigned host at render, so omit it when blank.
    const ing: Rec = { port: Number(d.ingress.port) }
    const host = d.ingress.host.trim()
    if (host) ing.host = host
    const domains = host ? d.ingress.domains.map((s) => s.trim()).filter(Boolean) : []
    if (domains.length) ing.domains = domains
    out.ingress = ing
  }
  if (d.health.on) out.health = { path: d.health.path.trim(), port: Number(d.health.port), retries: Number(d.health.retries) }
  if (d.database.on) out.database = { engine: d.database.engine }
  const env = Object.fromEntries(d.env.map(([k, v]) => [k.trim(), v]).filter(([k]) => k))
  if (Object.keys(env).length) out.env = env
  const secrets = d.secrets.map((s) => s.trim()).filter(Boolean)
  if (secrets.length) out.secrets = secrets
  if (d.migration.on) {
    const command = d.migration.command.map((s) => s.trim()).filter(Boolean)
    if (command.length) out.migration = { command }
  }
  const volumes = d.volumes
    .map(([name, path]) => ({ name: name.trim(), path: path.trim() }))
    .filter((v) => v.name && v.path)
  if (volumes.length) out.volumes = volumes
  const replicas = Number(d.replicas)
  if (Number.isFinite(replicas) && replicas > 1) out.replicas = replicas
  if (d.resources.on) {
    const res: Rec = {}
    const mem = d.resources.memory.trim()
    if (mem) res.memory = mem
    const cpus = d.resources.cpus.trim()
    if (cpus) res.cpus = cpus
    if (Object.keys(res).length) out.resources = res
  }
  return out
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

export function ManifestForm({ file, draft, onChange }: { file: string; draft: ManifestDraft; onChange: (d: ManifestDraft) => void }) {
  const set = <K extends keyof ManifestDraft>(k: K, v: ManifestDraft[K]) => onChange({ ...draft, [k]: v })
  return (
    <div className="flex flex-col gap-3.5">
      <Fld label="Image">
        <Input value={draft.image} placeholder="ghcr.io/org/app@sha256:…" onChange={(e) => set('image', e.target.value)} />
        <span className="text-xs text-muted-foreground">{file === 'base.yaml' ? 'Digest-pinned (required in base.yaml); tags are rejected.' : 'Optional per-class image override (production digests come from Promote).'}</span>
      </Fld>

      <Section label="Ingress" on={draft.ingress.on} onToggle={(on) => set('ingress', { ...draft.ingress, on })}>
        <div className="grid gap-2.5 sm:grid-cols-2">
          <Fld label="Production domain — optional"><Input value={draft.ingress.host} placeholder="app.example.com" onChange={(e) => set('ingress', { ...draft.ingress, host: e.target.value })} /></Fld>
          <Fld label="Container port"><Input type="number" value={draft.ingress.port} onChange={(e) => set('ingress', { ...draft.ingress, port: e.target.value })} /></Fld>
        </div>
        <span className="text-xs text-muted-foreground">Stable/testing/preview get an auto host <code className="font-mono">{'{app}.{project}.<base-domain>'}</code>; a domain here is a production custom domain (Cloudflare + edge).</span>
        <Fld label="Additional production domains"><ListEditor values={draft.ingress.domains} placeholder="www.example.com" onChange={(domains) => set('ingress', { ...draft.ingress, domains })} /></Fld>
      </Section>

      <Fld label="Replicas">
        <Input type="number" min="1" value={draft.replicas} onChange={(e) => set('replicas', e.target.value)} />
        <span className="text-xs text-muted-foreground">Container replicas, load-balanced by the edge. Must be 1 for apps with a persistent volume (single-writer).</span>
      </Fld>

      <Section label="Resource limits" on={draft.resources.on} onToggle={(on) => set('resources', { ...draft.resources, on })}>
        <div className="grid gap-2.5 sm:grid-cols-2">
          <Fld label="Memory limit"><Input placeholder="512m" value={draft.resources.memory} onChange={(e) => set('resources', { ...draft.resources, memory: e.target.value })} /></Fld>
          <Fld label="CPU limit (cores)"><Input placeholder="0.5" value={draft.resources.cpus} onChange={(e) => set('resources', { ...draft.resources, cpus: e.target.value })} /></Fld>
        </div>
        <span className="text-xs text-muted-foreground">Hard caps on the container. Memory takes b/k/m/g (e.g. <code className="font-mono">512m</code>, <code className="font-mono">2g</code>); CPU is a core count (e.g. <code className="font-mono">0.5</code>). Blank = unlimited.</span>
      </Section>

      <Section label="Health check" on={draft.health.on} onToggle={(on) => set('health', { ...draft.health, on })}>
        <div className="grid grid-cols-[2fr_1fr_1fr] gap-2.5">
          <Fld label="Path"><Input placeholder="/healthz" value={draft.health.path} onChange={(e) => set('health', { ...draft.health, path: e.target.value })} /></Fld>
          <Fld label="Port"><Input type="number" value={draft.health.port} onChange={(e) => set('health', { ...draft.health, port: e.target.value })} /></Fld>
          <Fld label="Retries"><Input type="number" value={draft.health.retries} onChange={(e) => set('health', { ...draft.health, retries: e.target.value })} /></Fld>
        </div>
      </Section>

      <Section label="Database" on={draft.database.on} onToggle={(on) => set('database', { ...draft.database, on })}>
        <Fld label="Engine">
          <Select value={draft.database.engine} onValueChange={(engine) => set('database', { ...draft.database, engine })}>
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>{ENGINES.map((en) => <SelectItem key={en} value={en}>{en}</SelectItem>)}</SelectContent>
          </Select>
        </Fld>
      </Section>

      <Fld label="Environment variables"><KvEditor pairs={draft.env} onChange={(env) => set('env', env)} /></Fld>
      <Fld label="Secrets">
        <ListEditor values={draft.secrets} placeholder="SECRET_NAME" onChange={(secrets) => set('secrets', secrets)} />
        <span className="text-xs text-muted-foreground">Names of secret env vars; the values live SOPS-encrypted and are edited outside the UI.</span>
      </Fld>

      <Fld label="Volumes">
        <KvEditor pairs={draft.volumes} kPlaceholder="name" vPlaceholder="/app/data" onChange={(volumes) => set('volumes', volumes)} />
        <span className="text-xs text-muted-foreground">Persistent named volumes (name → container mount path); survive redeploys, never auto-deleted.</span>
      </Fld>

      <Section label="Migration" on={draft.migration.on} onToggle={(on) => set('migration', { ...draft.migration, on })}>
        <Fld label="Command"><ListEditor values={draft.migration.command} placeholder="arg" onChange={(command) => set('migration', { ...draft.migration, command })} /></Fld>
      </Section>
    </div>
  )
}
