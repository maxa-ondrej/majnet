// Typed client for the bot + reconciler WG-internal APIs, proxied by nginx at
// /api/bot and /api/recon. Every type mirrors the Rust serde output.
import { useQuery, keepPreviousData } from '@tanstack/react-query'

const BOT = '/api/bot/api'
const RECON = '/api/recon/api'

// ── wire types ───────────────────────────────────────────────────────────────
export interface WhoAmI { login: string | null; admin: boolean }
export interface ProjectSummary { name: string; org: string; onboarded: boolean; apps: number }
export interface AppSummary {
  name: string; image: string; classes: string[]
  host: string | null; domains: string[]; database: string | null
}
export interface PlatformNode {
  name: string; role: string; wireguard_ip: string
  public_endpoint: string; wireguard_pubkey: string
}
export interface CpPin { ref: string; image: string | null; dashboard: string | null }
export interface CpCommit { sha: string; message: string; author: string; date: string }
export interface CpHistoryEntry { sha: string; message: string; author: string; date: string; current: boolean }
export interface TerminalSession {
  id: number; actor: string; node: string; mode: string; target: string
  started_at: string; ended_at: string | null; bytes: number | null
}
export interface CpRunning { version: string | null; commit: string | null; build_time: string | null }
export interface ControlPlaneStatus {
  current: CpPin
  latest: CpPin | null
  up_to_date: boolean
  commits: CpCommit[]
  history: CpHistoryEntry[]
  source: { org: string; repo: string; compare_url: string | null }
  running: CpRunning
  /** running build matches the pinned ref; null when the running build is unknown */
  converged: boolean | null
  check_error: string | null
  /** newest commit's images aren't published yet (CI still building) */
  latest_building: boolean
}
export interface ContainerMetric {
  name: string; image: string; state: string
  cpu_pct: number; mem_used: number; mem_limit: number
}
export interface NodeMetrics {
  name: string; role: string; reachable: boolean; error: string | null
  cpus: number; host_cpu_pct: number; mem_total: number; mem_used: number; disk_images: number
  containers: number; containers_running: number
  server_version: string; os: string; kernel: string
  apps: ContainerMetric[]
}
export interface Event {
  at: string; commit: string; project: string; node: string; action: string; result: string
  /** Coarse activity type set at write time: 'deploy' | 'remove' | 'config'. */
  kind?: string
}
export interface DeployFile {
  filename: string; status: string; additions: number; deletions: number; patch: string | null
}
export interface DeployPr {
  number: number; title: string; class: string; base: string; created_at: string; mergeable: boolean | null; files: DeployFile[]
}
export interface ManifestFile { yaml: string; data: unknown }
export interface Member { user: string; role: string }
export interface RegistryStatus { configured: boolean }
export interface ImportStatus {
  app: string; status: 'running' | 'failed'; step: string; detail: string; updated_at: string
}
/** Canonical import step order + labels (mirrors migrate.rs). */
export const IMPORT_STEPS: { key: string; label: string }[] = [
  { key: 'snapshot', label: 'Fetching source repo' },
  { key: 'repo', label: 'Creating app repo' },
  { key: 'commit', label: 'Importing code + CI' },
  { key: 'configure', label: 'Scaffolding manifest' },
  { key: 'secrets', label: 'Importing secrets' },
]
export interface StoredRelease {
  app: string; version: string; commit: string; app_image: string; published_at: string
}
/** Build metadata an app reported at its `/info` endpoint, recorded per env at
 *  deploy time. `info` is whatever JSON the app returned (or null). */
export interface AppInfo {
  class: string; commit: string; info: Record<string, unknown> | null; error: string | null; at: string
}

/** Parse a reconciler event timestamp (SQLite `datetime('now')`, UTC). */
export const parseAt = (at: string): number => Date.parse(at.replace(' ', 'T') + 'Z')

// ── fetch helpers ────────────────────────────────────────────────────────────
export async function getJSON<T>(url: string): Promise<T> {
  const r = await fetch(url)
  const text = await r.text()
  if (!r.ok) throw new Error(text || `${r.status} ${r.statusText}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

/** POST/PUT that returns the server's plain-text message; throws on !ok. */
export async function send(
  url: string,
  opts: { method?: string; json?: unknown; body?: string } = {},
): Promise<string> {
  const init: RequestInit = { method: opts.method ?? 'POST', headers: {} }
  if (opts.json !== undefined) {
    init.headers = { 'content-type': 'application/json' }
    init.body = JSON.stringify(opts.json)
  } else if (opts.body !== undefined) {
    init.body = opts.body
  }
  const r = await fetch(url, init)
  const text = await r.text()
  if (!r.ok) throw new Error(text || `${r.status} ${r.statusText}`)
  return text
}

export async function getText(url: string): Promise<string> {
  const r = await fetch(url)
  const text = await r.text()
  if (!r.ok) throw new Error(text || `${r.status} ${r.statusText}`)
  return text.trim()
}

export interface EnrollResult { ok: boolean; log: string }

/** Enroll a worker node via the setup service (JSON). Returns the log either
 *  way — the request only throws on transport/proxy errors, not enroll failure. */
export async function enrollNode(role: string, ssh_host: string): Promise<EnrollResult> {
  const r = await fetch(urls.setupEnroll, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ role, ssh_host }),
  })
  const text = await r.text()
  if (!r.ok) throw new Error(text || `${r.status} ${r.statusText}`)
  return JSON.parse(text) as EnrollResult
}

// ── query keys + endpoint URLs ───────────────────────────────────────────────
export const urls = {
  whoami: `${BOT}/whoami`,
  projects: `${BOT}/projects`,
  apps: (org: string) => `${BOT}/apps/${encodeURIComponent(org)}`,
  imports: (org: string) => `${BOT}/imports/${encodeURIComponent(org)}`,
  importRetry: (org: string, app: string) => `${BOT}/imports/${encodeURIComponent(org)}/${encodeURIComponent(app)}/retry`,
  nodes: `${BOT}/nodes`,
  metrics: `${RECON}/metrics`,
  metricsHistory: (range: number, node?: string) =>
    `${RECON}/metrics/history?range=${range}${node ? `&node=${encodeURIComponent(node)}` : ''}`,
  alertSettings: `${RECON}/settings/alerts`,
  alertTest: `${RECON}/settings/alerts/test`,
  appLogs: (org: string, cls: string, app: string, tail = 300) =>
    `${RECON}/logs/${encodeURIComponent(org)}/${encodeURIComponent(cls)}/${encodeURIComponent(app)}?tail=${tail}`,
  appInfo: (org: string, app: string) =>
    `${RECON}/info/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  events: (limit = 300) => `${RECON}/events?limit=${limit}`,
  botEvents: `${BOT}/events`,
  deploys: (org: string) => `${BOT}/deploys/${encodeURIComponent(org)}`,
  deployMerge: (org: string, n: number) => `${BOT}/deploys/${encodeURIComponent(org)}/${n}/merge`,
  deployClose: (org: string, n: number) => `${BOT}/deploys/${encodeURIComponent(org)}/${n}/close`,
  manifest: (org: string, app: string) => `${BOT}/manifest/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  manifestFile: (org: string, app: string, file: string) =>
    `${BOT}/manifest/${encodeURIComponent(org)}/${encodeURIComponent(app)}/${file}`,
  members: (org: string) => `${BOT}/members/${encodeURIComponent(org)}`,
  appSecrets: (org: string, app: string) => `${BOT}/secrets/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  appSecretValues: (org: string, cls: string, app: string) =>
    `${RECON}/secrets/${encodeURIComponent(org)}/${encodeURIComponent(cls)}/${encodeURIComponent(app)}`,
  releases: (org: string, app: string) => `${BOT}/releases/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  releaseCut: (org: string, app: string, bump: string) =>
    `${BOT}/releases/${encodeURIComponent(org)}/${encodeURIComponent(app)}/cut?bump=${bump}`,
  releasePromote: (org: string, app: string, version: string) =>
    `${BOT}/releases/${encodeURIComponent(org)}/${encodeURIComponent(app)}/promote/${encodeURIComponent(version)}`,
  releaseBackfill: (org: string, app: string) =>
    `${BOT}/releases/${encodeURIComponent(org)}/${encodeURIComponent(app)}/backfill`,
  version: `${BOT}/platform/version`,
  registry: `${BOT}/platform/registry`,
  dashboardLayout: `${BOT}/platform/dashboard-layout`,
  tailscale: `${BOT}/platform/tailscale`,
  tailscaleVerify: `${BOT}/platform/tailscale/verify`,
  setupEnroll: '/api/setup/enroll.json',
  promote: (org: string, app: string) => `${BOT}/promote/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  rollback: (org: string) => `${BOT}/rollback/${encodeURIComponent(org)}`,
  restart: (org: string, cls: string, app: string) =>
    `${RECON}/restart/${encodeURIComponent(org)}/${encodeURIComponent(cls)}/${encodeURIComponent(app)}`,
  appRename: (org: string, app: string) =>
    `${BOT}/apps/${encodeURIComponent(org)}/${encodeURIComponent(app)}/rename`,
  projectRename: (org: string) => `${BOT}/projects/${encodeURIComponent(org)}/rename`,
  appArchive: (org: string, app: string) =>
    `${BOT}/apps/${encodeURIComponent(org)}/${encodeURIComponent(app)}/archive`,
  appDelete: (org: string, app: string) =>
    `${BOT}/apps/${encodeURIComponent(org)}/${encodeURIComponent(app)}/delete`,
  archivedApps: (org: string) => `${BOT}/archived/${encodeURIComponent(org)}`,
  templateSync: (org: string) => `${BOT}/template-sync/${encodeURIComponent(org)}`,
  projectArchive: (org: string) => `${BOT}/projects/${encodeURIComponent(org)}/archive`,
  projectDelete: (org: string) => `${BOT}/projects/${encodeURIComponent(org)}/delete`,
  controlPlane: `${BOT}/control-plane`,
  controlPlanePin: `${BOT}/control-plane/pin`,
  terminalSessions: `${RECON}/terminal/sessions`,
  terminalTranscript: (id: number) => `${RECON}/terminal/transcript/${id}`,
}

// ── query hooks ──────────────────────────────────────────────────────────────
export const useWhoami = () => useQuery({ queryKey: ['whoami'], queryFn: () => getJSON<WhoAmI>(urls.whoami) })
export const useProjects = () =>
  useQuery({ queryKey: ['projects'], queryFn: () => getJSON<ProjectSummary[]>(urls.projects) })
export const useApps = (org: string) =>
  useQuery({ queryKey: ['apps', org], queryFn: () => getJSON<AppSummary[]>(urls.apps(org)) })
export const useImports = (org: string) =>
  useQuery({
    queryKey: ['imports', org],
    queryFn: () => getJSON<ImportStatus[]>(urls.imports(org)),
    // Poll while anything is still importing; back off once it's all settled.
    refetchInterval: (q) => (q.state.data?.some((i) => i.status === 'running') ? 2500 : false),
  })
export interface MetricPoint { ts: number; node: string; cpu_pct: number; mem_used: number; mem_total: number; containers_running: number }
export const useMetricsHistory = (range: number, enabled = true) =>
  useQuery({
    queryKey: ['metrics-history', range],
    queryFn: () => getJSON<MetricPoint[]>(urls.metricsHistory(range)),
    enabled,
    refetchInterval: 60_000,
  })

export interface ContainerPoint { ts: number; container: string; cpu_pct: number; mem_used: number; mem_limit: number }
export const useContainerHistory = (range: number, container: string, enabled = true) =>
  useQuery({
    queryKey: ['container-history', range, container],
    queryFn: () => getJSON<ContainerPoint[]>(`${RECON}/metrics/container-history?range=${range}&container=${encodeURIComponent(container)}`),
    enabled,
    refetchInterval: 60_000,
  })

export const useNodeMetrics = () =>
  useQuery({ queryKey: ['metrics'], queryFn: () => getJSON<NodeMetrics[]>(urls.metrics), refetchInterval: 10000 })
export const useAppLogs = (org: string, cls: string, app: string, enabled: boolean) =>
  useQuery({ queryKey: ['logs', org, cls, app], queryFn: () => getText(urls.appLogs(org, cls, app)), enabled, refetchInterval: 5000 })
export const useNodes = () =>
  useQuery({ queryKey: ['nodes'], queryFn: () => getJSON<PlatformNode[]>(urls.nodes) })
// WebSocket URL for the reconciler terminal (ADR 0016), same origin as the
// dashboard (tailscale serve injects identity; nginx upgrades /api/recon/api/terminal).
export function terminalWsUrl(params: Record<string, string | undefined>): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  const qs = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) if (v) qs.set(k, v)
  return `${proto}://${location.host}/api/recon/api/terminal?${qs.toString()}`
}

export const useTerminalSessions = () =>
  useQuery({ queryKey: ['terminal-sessions'], queryFn: () => getJSON<TerminalSession[]>(urls.terminalSessions), refetchInterval: 20000 })
export const useControlPlane = () =>
  useQuery({
    queryKey: ['control-plane'],
    queryFn: () => getJSON<ControlPlaneStatus>(urls.controlPlane),
    // Poll fast while a rollout is in flight (running != pinned) so the progress
    // reflects reality; ease off once converged. Keep the last good data across
    // the brief bot/dashboard blip mid-rollout so the progress bar never blanks.
    refetchInterval: (q) => (q.state.data?.converged === false ? 3000 : 20000),
    placeholderData: keepPreviousData,
    retry: true,
    retryDelay: 2000,
  })
export const useAppInfo = (org: string, app: string) =>
  useQuery({ queryKey: ['info', org, app], queryFn: () => getJSON<AppInfo[]>(urls.appInfo(org, app)), refetchInterval: 30000 })
export const useEvents = (limit = 300) =>
  useQuery({ queryKey: ['events', limit], queryFn: () => getJSON<Event[]>(urls.events(limit)), refetchInterval: 15000 })
export const useBotEvents = () =>
  useQuery({ queryKey: ['botEvents'], queryFn: () => getJSON<Event[]>(urls.botEvents), refetchInterval: 15000 })
export const useDeploys = (org: string) =>
  useQuery({
    queryKey: ['deploys', org],
    queryFn: () => getJSON<DeployPr[]>(urls.deploys(org)),
    // Poll so a "reconciling" PR flips to mergeable (and the Merge button
    // enables) without a manual refresh.
    refetchInterval: (q) => (q.state.data?.some((d) => d.mergeable !== true) ? 5000 : false),
  })
export const useManifest = (org: string, app: string) =>
  useQuery({ queryKey: ['manifest', org, app], queryFn: () => getJSON<Record<string, ManifestFile>>(urls.manifest(org, app)) })
export const useMembers = (org: string) =>
  useQuery({ queryKey: ['members', org], queryFn: () => getJSON<Member[]>(urls.members(org)) })
export interface AlertSettings { webhook_set: boolean; cpu_pct: number; mem_pct: number }
export const useAlertSettings = () =>
  useQuery({ queryKey: ['alert-settings'], queryFn: () => getJSON<AlertSettings>(urls.alertSettings) })
export const useAppSecrets = (org: string, cls: string, app: string) =>
  useQuery({ queryKey: ['secrets', org, cls, app], queryFn: () => getJSON<Record<string, string>>(urls.appSecretValues(org, cls, app)) })
export const useVersion = () =>
  useQuery({ queryKey: ['version'], queryFn: () => getText(urls.version) })
export const useRegistry = () =>
  useQuery({ queryKey: ['registry'], queryFn: () => getJSON<RegistryStatus>(urls.registry) })

export interface TailscaleStatus { configured: boolean; mode: 'oauth' | 'token' | 'none'; tailnet: string | null; manage_acl: boolean }
export interface TailscaleVerify { tailnet: string; devices: number; you: string | null }
export const useTailscale = () =>
  useQuery({ queryKey: ['tailscale'], queryFn: () => getJSON<TailscaleStatus>(urls.tailscale) })

// Per-user overview layout (react-grid-layout blob + hidden widgets). null = not customized.
export interface DashboardLayout { layouts?: Record<string, unknown[]>; hidden?: string[] }
export const useDashboardLayout = () =>
  useQuery({ queryKey: ['dashboard-layout'], queryFn: () => getJSON<DashboardLayout | null>(urls.dashboardLayout) })
export const saveDashboardLayout = (layout: DashboardLayout) =>
  send(urls.dashboardLayout, { method: 'PUT', json: { layout } })
export const useReleases = (org: string, app: string) =>
  useQuery({ queryKey: ['releases', org, app], queryFn: () => getJSON<StoredRelease[]>(urls.releases(org, app)) })
export const useArchivedApps = (org: string) =>
  useQuery({ queryKey: ['archived', org], queryFn: () => getJSON<string[]>(urls.archivedApps(org)) })
