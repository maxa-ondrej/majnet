// Typed client for the bot + reconciler WG-internal APIs, proxied by nginx at
// /api/bot and /api/recon. Every type mirrors the Rust serde output.
import { useQuery } from '@tanstack/react-query'

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
export interface Event {
  at: string; commit: string; project: string; node: string; action: string; result: string
}
export interface DeployFile {
  filename: string; status: string; additions: number; deletions: number; patch: string | null
}
export interface DeployPr {
  number: number; title: string; class: string; base: string; created_at: string; files: DeployFile[]
}
export interface ManifestFile { yaml: string; data: unknown }
export interface Member { user: string; role: string }
export interface StoredRelease {
  app: string; version: string; commit: string; app_image: string
  migration_image: string | null; migration_command: string[] | null; published_at: string
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

async function getText(url: string): Promise<string> {
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
  nodes: `${BOT}/nodes`,
  events: (limit = 300) => `${RECON}/events?limit=${limit}`,
  deploys: (org: string) => `${BOT}/deploys/${encodeURIComponent(org)}`,
  deployMerge: (org: string, n: number) => `${BOT}/deploys/${encodeURIComponent(org)}/${n}/merge`,
  deployClose: (org: string, n: number) => `${BOT}/deploys/${encodeURIComponent(org)}/${n}/close`,
  manifest: (org: string, app: string) => `${BOT}/manifest/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  manifestFile: (org: string, app: string, file: string) =>
    `${BOT}/manifest/${encodeURIComponent(org)}/${encodeURIComponent(app)}/${file}`,
  members: (org: string) => `${BOT}/members/${encodeURIComponent(org)}`,
  releases: (org: string, app: string) => `${BOT}/releases/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  version: `${BOT}/platform/version`,
  setupEnroll: '/api/setup/enroll.json',
  promote: (org: string, app: string) => `${BOT}/promote/${encodeURIComponent(org)}/${encodeURIComponent(app)}`,
  rollback: (org: string) => `${BOT}/rollback/${encodeURIComponent(org)}`,
  restart: (org: string, cls: string, app: string) =>
    `${RECON}/restart/${encodeURIComponent(org)}/${encodeURIComponent(cls)}/${encodeURIComponent(app)}`,
}

// ── query hooks ──────────────────────────────────────────────────────────────
export const useWhoami = () => useQuery({ queryKey: ['whoami'], queryFn: () => getJSON<WhoAmI>(urls.whoami) })
export const useProjects = () =>
  useQuery({ queryKey: ['projects'], queryFn: () => getJSON<ProjectSummary[]>(urls.projects) })
export const useApps = (org: string) =>
  useQuery({ queryKey: ['apps', org], queryFn: () => getJSON<AppSummary[]>(urls.apps(org)) })
export const useNodes = () =>
  useQuery({ queryKey: ['nodes'], queryFn: () => getJSON<PlatformNode[]>(urls.nodes) })
export const useEvents = (limit = 300) =>
  useQuery({ queryKey: ['events', limit], queryFn: () => getJSON<Event[]>(urls.events(limit)), refetchInterval: 15000 })
export const useDeploys = (org: string) =>
  useQuery({ queryKey: ['deploys', org], queryFn: () => getJSON<DeployPr[]>(urls.deploys(org)) })
export const useManifest = (org: string, app: string) =>
  useQuery({ queryKey: ['manifest', org, app], queryFn: () => getJSON<Record<string, ManifestFile>>(urls.manifest(org, app)) })
export const useMembers = (org: string) =>
  useQuery({ queryKey: ['members', org], queryFn: () => getJSON<Member[]>(urls.members(org)) })
export const useVersion = () =>
  useQuery({ queryKey: ['version'], queryFn: () => getText(urls.version) })
export const useReleases = (org: string, app: string) =>
  useQuery({ queryKey: ['releases', org, app], queryFn: () => getJSON<StoredRelease[]>(urls.releases(org, app)) })
