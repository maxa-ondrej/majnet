import { useSyncExternalStore } from 'react'

// A global, persisted "current environment" — shown in the top bar on every page
// and shared across navigation (localStorage-backed, so it survives reloads and
// page changes). Env-aware views (app detail, config, observability) read it via
// useEnv(); the top-bar selector writes it via setEnv().
export const ENV_CLASSES = ['production', 'stable', 'testing', 'ephemeral'] as const
export type EnvClass = (typeof ENV_CLASSES)[number]

const KEY = 'majnet-env'
const isEnv = (v: string | null): v is EnvClass => !!v && (ENV_CLASSES as readonly string[]).includes(v)

let current: EnvClass = (() => {
  try { const s = localStorage.getItem(KEY); return isEnv(s) ? s : 'production' } catch { return 'production' }
})()
const listeners = new Set<() => void>()

export function setEnv(e: EnvClass) {
  if (e === current) return
  current = e
  try { localStorage.setItem(KEY, e) } catch { /* private mode — in-memory only */ }
  listeners.forEach((l) => l())
}

export function useEnv(): EnvClass {
  return useSyncExternalStore(
    (cb) => { listeners.add(cb); return () => { listeners.delete(cb) } },
    () => current,
    () => current,
  )
}
