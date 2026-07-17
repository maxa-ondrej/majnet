import { useState } from 'react'
import { Network, ServerCog } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  enrollNode, send, urls,
  useAlertSettings, useNodes, useRegistry, useTailscale, useVersion, useWhoami,
  type EnrollResult, type PlatformNode, type TailscaleVerify,
} from './api'
import { PageHead } from './views'
import { QueryState, StatusBadge } from './ui'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Separator } from '@/components/ui/separator'

const ZONE: Record<string, string> = { main: 'control plane', prod: 'public', private: 'internal' }
// Enrollable worker roles (main enrolls itself at install).
const WORKER_ROLES = ['prod', 'private'] as const

// Every editable setting on the page, tracked centrally so one save bar can
// commit them all. Secret fields are write-only (baseline = ''); the rest load
// their baseline from the server.
type Field = 'ghcr_token' | 'ts_client_id' | 'ts_client_secret' | 'tailnet' | 'ts_manage_acl' | 'webhook' | 'cpu_pct' | 'mem_pct'
const GROUPS = {
  registry: ['ghcr_token'],
  tailscale: ['ts_client_id', 'ts_client_secret', 'tailnet', 'ts_manage_acl'],
  alerts: ['webhook', 'cpu_pct', 'mem_pct'],
} satisfies Record<string, Field[]>

export interface Form {
  val: (f: Field) => string
  dirty: (f: Field) => boolean
  set: (f: Field) => (e: React.ChangeEvent<HTMLInputElement>) => void
  setVal: (f: Field, v: string) => void
  groupDirty: (fields: readonly Field[]) => boolean
}

export function Settings() {
  const nodes = useNodes()
  const version = useVersion()
  const { data: me } = useWhoami()
  const reg = useRegistry()
  const ts = useTailscale()
  const alerts = useAlertSettings()
  const qc = useQueryClient()

  const base: Record<Field, string> = {
    ghcr_token: '', ts_client_id: '', ts_client_secret: '', webhook: '',
    tailnet: ts.data?.tailnet ?? '',
    ts_manage_acl: ts.data?.manage_acl ? '1' : '0',
    cpu_pct: alerts.data?.cpu_pct != null ? String(alerts.data.cpu_pct) : '',
    mem_pct: alerts.data?.mem_pct != null ? String(alerts.data.mem_pct) : '',
  }
  const [changes, setChanges] = useState<Partial<Record<Field, string>>>({})
  const [saving, setSaving] = useState(false)

  const val = (f: Field) => changes[f] ?? base[f]
  const dirty = (f: Field) => f in changes && changes[f] !== base[f]
  const set = (f: Field) => (e: React.ChangeEvent<HTMLInputElement>) =>
    setChanges((c) => ({ ...c, [f]: e.target.value }))
  const setVal = (f: Field, v: string) => setChanges((c) => ({ ...c, [f]: v }))
  const groupDirty = (fields: readonly Field[]) => fields.some(dirty)
  const form: Form = { val, dirty, set, setVal, groupDirty }

  const dirtyCount = (Object.keys(base) as Field[]).filter(dirty).length

  const discardAll = () => setChanges({})
  const saveAll = async () => {
    setSaving(true)
    const done: Field[] = []
    const errs: string[] = []
    const run = async (label: string, fields: Field[], body: Record<string, unknown>, url: string, key: string) => {
      if (!fields.some(dirty)) return
      try {
        await send(url, { json: body })
        done.push(...fields.filter(dirty))
        qc.invalidateQueries({ queryKey: [key] })
      } catch (e) {
        errs.push(`${label}: ${String(e)}`)
      }
    }

    await run('Container registry', ['ghcr_token'], { token: val('ghcr_token').trim() }, urls.registry, 'registry')

    const tsBody: Record<string, unknown> = {}
    if (dirty('ts_client_id')) tsBody.client_id = val('ts_client_id').trim()
    if (dirty('ts_client_secret')) tsBody.client_secret = val('ts_client_secret').trim()
    if (dirty('tailnet')) tsBody.tailnet = val('tailnet').trim()
    if (dirty('ts_manage_acl')) tsBody.manage_acl = val('ts_manage_acl') === '1'
    await run('Tailnet identity', GROUPS.tailscale, tsBody, urls.tailscale, 'tailscale')

    const alBody: Record<string, unknown> = {}
    if (dirty('webhook')) alBody.webhook = val('webhook').trim()
    // Only send a threshold if it parses to a finite number — an emptied field
    // reverts to the server value on save rather than posting 0%.
    const num = (f: Field) => (dirty(f) && val(f).trim() !== '' && Number.isFinite(Number(val(f))) ? Number(val(f)) : undefined)
    if (num('cpu_pct') !== undefined) alBody.cpu_pct = num('cpu_pct')
    if (num('mem_pct') !== undefined) alBody.mem_pct = num('mem_pct')
    await run('Alerts', GROUPS.alerts, alBody, urls.alertSettings, 'alert-settings')

    setChanges((c) => {
      const next = { ...c }
      done.forEach((f) => delete next[f])
      return next
    })
    setSaving(false)
    errs.forEach((m) => toast.error(m))
    if (done.length && !errs.length) toast.success('Settings saved')
    else if (done.length) toast.success(`Saved ${done.length} change${done.length > 1 ? 's' : ''}`)
  }

  const enrolled = new Map((nodes.data ?? []).filter((n) => n.wireguard_pubkey).map((n) => [n.role, n]))
  const pending = WORKER_ROLES.filter((r) => !enrolled.has(r))

  return (
    <>
      <PageHead title="Settings" />

      <Card className="mb-4">
        <CardHeader><CardTitle className="text-base">Platform</CardTitle></CardHeader>
        <CardContent className="flex flex-col gap-2.5 text-sm">
          <Row k="Control-plane pin">{version.isLoading ? '…' : <code className="font-mono text-xs">{version.data?.slice(0, 12) ?? '—'}</code>}</Row>
          <Row k="Signed in as"><span className="inline-flex items-center gap-2">{me?.login ?? 'infra'} {me?.admin && <StatusBadge tone="success">admin</StatusBadge>}</span></Row>
          <Row k="Config source"><span className="text-muted-foreground">All platform config lives in <code className="font-mono text-xs">majksa-platform/platform</code> (git). Edits commit there; the reconciler converges.</span></Row>
        </CardContent>
      </Card>

      {me?.admin && <RegistrySection form={form} configured={reg.data?.configured} loading={reg.isLoading} />}
      {me?.admin && <TailscaleSection form={form} tailnetChangesPending={groupDirty(GROUPS.tailscale)} />}
      {me?.admin && <AlertsSection form={form} webhookSet={alerts.data?.webhook_set} loading={alerts.isLoading} />}

      <Card>
        <CardHeader><CardTitle className="text-base">Nodes</CardTitle></CardHeader>
        <CardContent className="flex flex-col gap-3">
          <QueryState isLoading={nodes.isLoading} error={nodes.error}>
            <div className="flex flex-col gap-2">
              {(nodes.data ?? []).map((n) => <NodeRow key={n.role} n={n} />)}
            </div>
            {pending.length > 0 && <Separator className="my-1" />}
            {pending.map((role) => <Onboard key={role} role={role} />)}
            {pending.length === 0 && <p className="text-xs text-muted-foreground">All worker nodes are enrolled.</p>}
          </QueryState>
        </CardContent>
      </Card>

      <SaveBar count={dirtyCount} saving={saving} onSave={saveAll} onDiscard={discardAll} />
    </>
  )
}

// Slim header marker shown when a card has unsaved edits.
function EditedMark({ on }: { on: boolean }) {
  if (!on) return null
  return <span className="ml-auto inline-flex items-center gap-1.5 text-xs font-medium text-warning"><span className="size-1.5 rounded-full bg-warning" />edited</span>
}

// Field bound to the central form; highlights when changed. (Prop is `ctl` not
// `form` to avoid clashing with the intrinsic `form` input attribute.)
function Fld({ ctl, field, label, hint, ...rest }: { ctl: Form; field: Field; label: string; hint?: string } & React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <div>
      <Label className="text-xs text-muted-foreground">{label}{hint && <span className="ml-1.5 font-normal opacity-80">{hint}</span>}</Label>
      <Input {...rest} value={ctl.val(field)} onChange={ctl.set(field)} className={`mt-1 ${ctl.dirty(field) ? 'border-warning/70' : ''}`} />
    </div>
  )
}

// GHCR pull token — lets nodes pull private app images (ADR 0012).
function RegistrySection({ form, configured, loading }: { form: Form; configured?: boolean; loading: boolean }) {
  return (
    <Card className="mb-4">
      <CardHeader><CardTitle className="flex items-center text-base">Container registry (GHCR)<EditedMark on={form.groupDirty(GROUPS.registry)} /></CardTitle></CardHeader>
      <CardContent className="flex flex-col gap-3 text-sm">
        <Row k="Pull token">
          {loading ? '…' : configured ? <StatusBadge tone="success">configured</StatusBadge> : <span className="text-muted-foreground">not set</span>}
        </Row>
        <p className="text-xs text-muted-foreground">
          A classic PAT with <code className="font-mono">read:packages</code> so nodes can pull private app images. GitHub App tokens aren’t accepted by GHCR (ADR 0012). Stored by the bot; overrides the install-time value.
        </p>
        <Fld ctl={form} field="ghcr_token" type="password" label="Access token" hint="(leave blank to keep the current one)" placeholder="ghp_… (read:packages)" />
      </CardContent>
    </Card>
  )
}

// Tailnet identity — resolves who you are on the public dashboard address so the
// Terminal can attribute sessions to a named admin (bot-owned credential).
function TailscaleSection({ form, tailnetChangesPending }: { form: Form; tailnetChangesPending: boolean }) {
  const ts = useTailscale()
  const [verifying, setVerifying] = useState(false)
  const [result, setResult] = useState<TailscaleVerify | null>(null)
  const verify = async () => {
    setVerifying(true)
    setResult(null)
    try {
      const resp = await fetch(urls.tailscaleVerify, { method: 'POST' })
      const text = await resp.text()
      if (!resp.ok) throw new Error(text || `${resp.status} ${resp.statusText}`)
      setResult(JSON.parse(text) as TailscaleVerify)
    } catch (e) {
      toast.error(`Verify failed — ${String(e)}`)
    } finally {
      setVerifying(false)
    }
  }
  const configured = ts.data?.configured
  return (
    <Card className="mb-4">
      <CardHeader><CardTitle className="flex items-center gap-2 text-base"><Network className="size-4 opacity-80" />Tailnet identity<EditedMark on={form.groupDirty(GROUPS.tailscale)} /></CardTitle></CardHeader>
      <CardContent className="flex flex-col gap-3 text-sm">
        <Row k="API access">
          {ts.isLoading ? '…' : configured
            ? <StatusBadge tone="success" dot>configured{ts.data?.mode === 'oauth' ? ' · auto-renews' : ''}</StatusBadge>
            : <span className="text-muted-foreground">not set</span>}
        </Row>
        <p className="text-xs text-muted-foreground">
          Lets MajNet recognize you when you open the dashboard at its public address (<code className="font-mono">dash.majksa.net</code>). Required for the <strong>Terminal</strong>, which attributes and audits every session to a named admin — without it, terminal sessions there are refused. The bot resolves your tailnet address to a login via the Tailscale API; the credential never leaves the control plane.
        </p>
        <Fld ctl={form} field="ts_client_id" label="OAuth client ID" placeholder="k123abc…" />
        <Fld ctl={form} field="ts_client_secret" type="password" label="OAuth client secret" hint="(leave blank to keep)" placeholder="tskey-client-…" />
        <Fld ctl={form} field="tailnet" label="Tailnet" hint="(blank = your default tailnet)" placeholder="tail09a9c1.ts.net" />
        <p className="text-xs text-muted-foreground">
          Create an <strong>OAuth client</strong> (not an access token) with the <code className="font-mono">devices:read</code> scope in the Tailscale admin console. The secret is long-lived — the bot mints short-lived tokens from it, so it never needs manual renewal.
        </p>

        <label className="flex items-start gap-2.5 rounded-lg border p-3">
          <input type="checkbox" className="mt-0.5 size-4 accent-primary"
            checked={form.val('ts_manage_acl') === '1'}
            onChange={(e) => form.setVal('ts_manage_acl', e.target.checked ? '1' : '0')} />
          <span className="text-xs">
            <span className="font-medium text-foreground">Let MajNet manage the tailnet ACL</span>
            <span className="mt-0.5 block text-muted-foreground">
              Off by default. When on, MajNet <strong>overwrites your entire Tailscale access policy</strong> with one generated from <code className="font-mono">people.yaml</code> — a tag-based policy that assumes tagged nodes. Leave off if you manage the ACL yourself; an untagged tailnet would lock everyone out. Also needs <code className="font-mono">policy_file:write</code> on the OAuth client.
            </span>
          </span>
        </label>
        {result && (
          <div className="rounded-lg border border-success/30 bg-success/10 px-3 py-2.5 text-[13px]">
            {result.you
              ? <>Resolved you as <strong>{result.you}</strong> · {result.devices} tailnet device{result.devices === 1 ? '' : 's'} visible.</>
              : <>Credential works — {result.devices} device{result.devices === 1 ? '' : 's'} visible on <code className="font-mono">{result.tailnet}</code>. Your own address wasn’t matched (open the dashboard over the tailnet to attribute yourself).</>}
          </div>
        )}
        <div className="flex items-center gap-2">
          <Button variant="outline" disabled={verifying || !configured || tailnetChangesPending} onClick={verify}>
            {verifying ? 'Verifying…' : 'Verify identity'}
          </Button>
          {tailnetChangesPending && <span className="text-xs text-muted-foreground">Save your changes first, then verify.</span>}
        </div>
      </CardContent>
    </Card>
  )
}

function AlertsSection({ form, webhookSet, loading }: { form: Form; webhookSet?: boolean; loading: boolean }) {
  const test = async () => {
    try { toast.success(await send(urls.alertTest)) } catch (e) { toast.error(String(e)) }
  }
  return (
    <Card className="mb-4">
      <CardHeader><CardTitle className="flex items-center text-base">Alerts (Discord)<EditedMark on={form.groupDirty(GROUPS.alerts)} /></CardTitle></CardHeader>
      <CardContent className="flex flex-col gap-3 text-sm">
        <Row k="Webhook">
          {loading ? '…' : webhookSet ? <StatusBadge tone="success" dot>configured</StatusBadge> : <span className="text-muted-foreground">not set</span>}
        </Row>
        <p className="text-xs text-muted-foreground">
          The reconciler checks node/host metrics + site health every minute and posts up/down transitions to this Discord webhook.
        </p>
        <Fld ctl={form} field="webhook" type="password" label="Webhook URL" hint="(leave blank to keep)" placeholder="https://discord.com/api/webhooks/…" />
        <div className="grid grid-cols-2 gap-2">
          <Fld ctl={form} field="cpu_pct" type="number" label="CPU alert %" placeholder="90" />
          <Fld ctl={form} field="mem_pct" type="number" label="Memory alert %" placeholder="90" />
        </div>
        <div><Button variant="outline" disabled={!webhookSet} onClick={test}>Send test</Button></div>
      </CardContent>
    </Card>
  )
}

// GitHub-style floating bar: appears when any setting is dirty; saves all at once.
function SaveBar({ count, saving, onSave, onDiscard }: { count: number; saving: boolean; onSave: () => void; onDiscard: () => void }) {
  const shown = count > 0
  return (
    <div
      className={`pointer-events-none fixed inset-x-0 bottom-6 z-40 flex justify-center px-4 transition-all duration-300 md:left-60 ${shown ? 'translate-y-0 opacity-100' : 'pointer-events-none translate-y-24 opacity-0'}`}
      aria-hidden={!shown}
    >
      <div className="pointer-events-auto flex min-w-0 items-center gap-4 rounded-xl border bg-card px-4 py-2.5 shadow-lg shadow-black/20 sm:min-w-[440px]">
        <span className="flex items-center gap-2 text-sm font-medium">
          <span className="size-2 rounded-full bg-warning motion-safe:animate-pulse" />
          Unsaved changes
          <span className="font-normal text-muted-foreground">· {count} field{count === 1 ? '' : 's'}</span>
        </span>
        <div className="ml-auto flex gap-2">
          <Button variant="ghost" size="sm" disabled={saving} onClick={onDiscard}>Discard</Button>
          <Button size="sm" disabled={saving} onClick={onSave}>{saving ? 'Saving…' : 'Save changes'}</Button>
        </div>
      </div>
    </div>
  )
}

function Row({ k, children }: { k: string; children: React.ReactNode }) {
  return <div className="flex gap-2.5"><span className="min-w-36 text-muted-foreground">{k}</span><span>{children}</span></div>
}

function NodeRow({ n }: { n: PlatformNode }) {
  const enrolled = !!n.wireguard_pubkey
  const ep = [n.wireguard_ip, n.public_endpoint].filter(Boolean).join(' · ')
  return (
    <div className={`flex items-center gap-3 rounded-lg border px-4 py-3 ${enrolled ? '' : 'opacity-60'}`}>
      <div className="flex-1"><div className="font-semibold">{n.name} <span className="text-xs font-normal text-muted-foreground">{ZONE[n.role] ?? n.role}</span></div>
        <div className="mt-0.5 font-mono text-xs text-muted-foreground">{ep || '—'}</div></div>
      {enrolled ? <StatusBadge tone="success" dot>enrolled</StatusBadge> : <StatusBadge tone="muted">pending</StatusBadge>}
    </div>
  )
}

function Onboard({ role }: { role: string }) {
  const [host, setHost] = useState('')
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<EnrollResult | null>(null)
  const qc = useQueryClient()

  const run = async () => {
    setBusy(true)
    try {
      const res = await enrollNode(role, host.trim())
      setResult(res)
      if (res.ok) {
        toast.success(`${role} node enrolled`)
        setHost('')
        qc.invalidateQueries({ queryKey: ['nodes'] })
      } else {
        toast.error(`${role} enrollment failed`)
      }
    } catch (e) {
      setResult({ ok: false, log: (e as Error).message })
      toast.error((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="rounded-lg border border-dashed p-3.5">
      <div className="mb-2 flex items-center gap-2 text-sm font-medium"><ServerCog className="size-4" /> Onboard the <code className="font-mono">{role}</code> node</div>
      <p className="mb-3 text-xs text-muted-foreground">
        Provision a fresh Debian server, authorize the platform enrollment key on <code>root</code> (or the <code>majnet</code> user on a re-run),
        then enter its IP/host. The setup service SSHes in, runs bootstrap, brings up WireGuard, and registers it in <code>nodes.yaml</code>. Takes a few minutes.
      </p>
      <div className="flex flex-wrap items-end gap-2">
        <div className="flex flex-1 flex-col gap-1.5"><Label className="text-xs">SSH host</Label>
          <Input value={host} onChange={(e) => setHost(e.target.value)} placeholder="203.0.113.9 or node.example.com" /></div>
        <Button disabled={busy || !host.trim()} onClick={run}>{busy ? 'Enrolling…' : 'Enroll'}</Button>
      </div>
      <Dialog open={!!result} onOpenChange={(o) => !o && setResult(null)}>
        <DialogContent className="max-w-2xl">
          <DialogHeader><DialogTitle>{result?.ok ? `${role} node enrolled` : `${role} enrollment failed`}</DialogTitle></DialogHeader>
          <pre className="max-h-[28rem] overflow-auto whitespace-pre-wrap rounded-md border bg-muted p-3 font-mono text-xs">{result?.log}</pre>
        </DialogContent>
      </Dialog>
    </div>
  )
}
