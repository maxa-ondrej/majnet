import { useState, type ReactNode } from 'react'
import { Loader2 } from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
  AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger,
} from '@/components/ui/alert-dialog'
import { cn } from '@/lib/utils'
import type { Event } from './api'

export const short = (img: string | null | undefined) =>
  String(img ?? '').replace(/(@sha256:[0-9a-f]{8})[0-9a-f]+/, '$1…')

/// A hostname/URL rendered as a link that opens the site in a new tab. A bare
/// host (`app.example.com`) gets an `https://` scheme; anything already a URL is
/// used as-is. Shows a `↗` affordance.
export function ExtLink({ to, children, className }: { to: string; children?: ReactNode; className?: string }) {
  const href = /^https?:\/\//.test(to) ? to : `https://${to}`
  return (
    <a href={href} target="_blank" rel="noreferrer"
      className={cn('text-primary underline-offset-2 hover:underline', className)}>
      {children ?? to}<span aria-hidden className="ml-0.5 opacity-60">↗</span>
    </a>
  )
}

export const latestEventFor = (events: Event[] | undefined, project: string, app: string) =>
  (events ?? []).find((e) => e.project === project && e.action.trim().split(/\s+/).pop() === app)

// ── status badge ─────────────────────────────────────────────────────────────
const TONES = {
  success: 'border-transparent bg-success/15 text-success',
  warn: 'border-transparent bg-warning/15 text-warning',
  danger: 'border-transparent bg-destructive/15 text-destructive',
  muted: 'border-transparent bg-muted text-muted-foreground',
  accent: 'border-transparent bg-accent text-accent-foreground',
} as const

export function StatusBadge({ tone, dot, title, children }: {
  tone: keyof typeof TONES; dot?: boolean; title?: string; children: ReactNode
}) {
  return (
    <Badge variant="outline" title={title} className={cn('gap-1.5 font-medium', TONES[tone])}>
      {dot && <span className="size-1.5 rounded-full bg-current" />}
      {children}
    </Badge>
  )
}

export function DeployStatus({ ev }: { ev: Event | undefined }) {
  if (!ev) return <StatusBadge tone="muted">no deploys</StatusBadge>
  const r = ev.result || ''
  const act = ev.action.trim().split(/\s+/)[0] ?? ''
  const title = `${ev.action} → ${r}  ·  ${ev.at}  ·  ${ev.commit.slice(0, 12)}`
  if (r.startsWith('FAILED')) return <StatusBadge tone="danger" title={title}>failed</StatusBadge>
  if (act === 'gc') return <StatusBadge tone="muted" title={title}>removed</StatusBadge>
  if (r.startsWith('deployed')) return <StatusBadge tone="success" dot title={title}>deployed</StatusBadge>
  if (r === 'in sync') return <StatusBadge tone="success" dot title={title}>healthy</StatusBadge>
  return <StatusBadge tone="muted" title={title}>{(r || act).slice(0, 20)}</StatusBadge>
}

// ── query state ──────────────────────────────────────────────────────────────
export function QueryState({ isLoading, error, children }: {
  isLoading: boolean; error: unknown; children: ReactNode
}) {
  if (isLoading) return (
    <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground">
      <Loader2 className="size-4 animate-spin" /> Loading…
    </div>
  )
  if (error) return <div className="py-8 text-sm text-destructive">Failed to load: {String((error as Error).message)}</div>
  return <>{children}</>
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="py-8 text-sm text-muted-foreground">{children}</div>
}

// ── confirm dialog button ────────────────────────────────────────────────────
export function ConfirmButton({
  title, description, confirmText = 'Confirm', onConfirm, children, ...btn
}: React.ComponentProps<typeof Button> & {
  title: string; description?: string; confirmText?: string; onConfirm: () => void
}) {
  const [open, setOpen] = useState(false)
  return (
    <AlertDialog open={open} onOpenChange={setOpen}>
      <AlertDialogTrigger asChild><Button {...btn}>{children}</Button></AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          {description && <AlertDialogDescription>{description}</AlertDialogDescription>}
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction onClick={onConfirm}>{confirmText}</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
