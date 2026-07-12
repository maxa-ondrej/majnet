import { useState } from 'react'
import { Link, useNavigate, useParams } from '@tanstack/react-router'
import { toast } from 'sonner'
import { Info } from 'lucide-react'
import { send, urls } from './api'
import { useApiMutation } from './mutations'
import { Crumbs, PageHead } from './views'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Textarea } from '@/components/ui/textarea'
import { Checkbox } from '@/components/ui/checkbox'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'

function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <Label>{label}</Label>
      {children}
      {hint && <span className="text-xs text-muted-foreground">{hint}</span>}
    </div>
  )
}

// ── New project ──────────────────────────────────────────────────────────────
export function NewProject() {
  const nav = useNavigate()
  const [name, setName] = useState('')
  const [org, setOrg] = useState('')
  const m = useApiMutation({ invalidate: [['projects']], onDone: () => nav({ to: '/' }) })
  return (
    <>
      <Crumbs><Link to="/">Projects</Link> / New</Crumbs>
      <PageHead title="New project" />
      <Card><CardContent className="flex flex-col gap-4 pt-6">
        <div className="flex gap-2.5 rounded-lg border bg-muted/40 p-3 text-sm text-muted-foreground">
          <Info className="mt-0.5 size-4 shrink-0" />
          <div>Create the GitHub org yourself (GitHub has no org-creation API), then register it here. Discovery needs the org listed <b>and</b> the App installed.</div>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Project name" hint="Lowercase slug, used in the dashboard."><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="blog" /></Field>
          <Field label="GitHub org" hint="The org this project's repos live in."><Input value={org} onChange={(e) => setOrg(e.target.value)} placeholder="majksa-projects" /></Field>
        </div>
        <Field label="1 · Install the App on the org">
          <pre className="overflow-x-auto rounded-md border bg-muted p-3 font-mono text-xs">https://github.com/apps/majnet-platform/installations/new</pre>
        </Field>
        <div className="flex items-center gap-3">
          <Button disabled={m.isPending} onClick={() => {
            if (!name.trim() || !org.trim()) return toast.error('name and org are required')
            m.mutate(() => send(urls.projects, { json: { name: name.trim(), org: org.trim() } }))
          }}>Register project</Button>
          <span className="text-xs text-muted-foreground">Commits to projects.yaml; the ops repo is created on the next org sync.</span>
        </div>
      </CardContent></Card>
    </>
  )
}

// ── New app ──────────────────────────────────────────────────────────────────
const CLASSES = ['production', 'stable', 'testing', 'ephemeral'] as const
const TEMPLATES = ['web-app', 'rust-service'] as const
export function NewApp() {
  const { org } = useParams({ from: '/projects/$org/new-app' })
  const nav = useNavigate()
  const [name, setName] = useState('')
  const [image, setImage] = useState('')
  const [host, setHost] = useState('')
  const [port, setPort] = useState('8080')
  const [domains, setDomains] = useState('')
  const [database, setDatabase] = useState('none')
  const [template, setTemplate] = useState<string>('web-app')
  const [classes, setClasses] = useState<string[]>(['production'])
  const [importing, setImporting] = useState(false)
  const [importRepo, setImportRepo] = useState('')
  const [importToken, setImportToken] = useState('')
  const m = useApiMutation({ invalidate: [['apps', org]], onDone: () => nav({ to: '/projects/$org', params: { org } }) })
  const toggle = (c: string) => setClasses((cs) => (cs.includes(c) ? cs.filter((x) => x !== c) : [...cs, c]))

  return (
    <>
      <Crumbs><Link to="/">Projects</Link> / <Link to="/projects/$org" params={{ org }}>{org}</Link> / New app</Crumbs>
      <PageHead title="New app" />
      <Card><CardContent className="flex flex-col gap-4 pt-6">
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="App name" hint="Lowercase; its manifest directory."><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="blog" /></Field>
          <Field label="Image" hint="Digest-pinned; tags are rejected."><Input value={image} onChange={(e) => setImage(e.target.value)} placeholder="ghcr.io/org/app@sha256:…" /></Field>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Primary domain — optional" hint="Cloudflare + cert handled automatically for production."><Input value={host} onChange={(e) => setHost(e.target.value)} placeholder="blog.majksa.cz" /></Field>
          <Field label="Container port"><Input type="number" value={port} onChange={(e) => setPort(e.target.value)} /></Field>
        </div>
        <Field label="Additional domains — optional, one per line"><Textarea value={domains} onChange={(e) => setDomains(e.target.value)} className="min-h-16" placeholder="www.majksa.cz" /></Field>
        <Field label="Classes" hint="Which environments this app deploys to. Production goes through the reviewed render PR.">
          <div className="flex flex-wrap gap-2">
            {CLASSES.map((c) => (
              <label key={c} className="flex cursor-pointer items-center gap-2 rounded-md border px-3 py-1.5 text-sm has-[:checked]:border-primary has-[:checked]:bg-accent has-[:checked]:text-accent-foreground">
                <Checkbox checked={classes.includes(c)} onCheckedChange={() => toggle(c)} /> {c}
              </label>
            ))}
          </div>
        </Field>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Source-repo template" hint={importing ? 'Selects which MajNet CI workflows to inject into the imported repo.' : "Scaffolds the app's GitHub repo (CI wired for the delivery pipeline)."}>
            <Select value={template} onValueChange={setTemplate}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                {TEMPLATES.map((t) => <SelectItem key={t} value={t}>{t}</SelectItem>)}
              </SelectContent>
            </Select>
          </Field>
          <Field label="Database — optional">
            <Select value={database} onValueChange={setDatabase}>
              <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
              <SelectContent>
                {['none', 'postgres', 'mariadb', 'valkey', 'mongodb'].map((e) => <SelectItem key={e} value={e}>{e}</SelectItem>)}
              </SelectContent>
            </Select>
          </Field>
        </div>
        <div className="rounded-lg border p-3">
          <label className="flex cursor-pointer items-center gap-2 text-sm font-medium">
            <Checkbox checked={importing} onCheckedChange={(v) => setImporting(!!v)} />
            Import an existing app (seed the source repo from an old GitHub repo + inject MajNet CI)
          </label>
          {importing && (
            <div className="mt-3 grid gap-3 sm:grid-cols-2">
              <Field label="Old repo URL" hint="The existing GitHub repo to import (history preserved).">
                <Input value={importRepo} onChange={(e) => setImportRepo(e.target.value)} placeholder="https://github.com/old-org/blog" />
              </Field>
              <Field label="Read token — optional" hint="A GitHub PAT if the source repo is private. Held in memory for the import; never stored.">
                <Input type="password" value={importToken} onChange={(e) => setImportToken(e.target.value)} placeholder="ghp_…" />
              </Field>
            </div>
          )}
        </div>
        <div className="flex items-center gap-3">
          <Button disabled={m.isPending} onClick={() => {
            if (!name.trim() || !image.trim()) return toast.error('name and image are required')
            if (!classes.length) return toast.error('select at least one class')
            if (importing && !importRepo.trim()) return toast.error('enter the old repo URL to import')
            m.mutate(() => send(urls.apps(org), {
              json: {
                name: name.trim(), image: image.trim(), host: host.trim(), port: Number(port),
                domains: domains.split('\n').map((s) => s.trim()).filter(Boolean),
                classes, database: database === 'none' ? null : database, template,
                ...(importing ? { import: { repo: importRepo.trim(), token: importToken.trim() || null } } : {}),
              },
            }))
          }}>{importing ? 'Import app' : 'Create app'}</Button>
          <span className="text-xs text-muted-foreground">
            {importing
              ? 'Imports the repo + injects CI (runs in the background), then writes base.yaml + overlays and declares the app in project.yaml.'
              : 'Writes base.yaml + overlays and declares the app in project.yaml; the source repo is scaffolded on the next org sync.'}
          </span>
        </div>
      </CardContent></Card>
    </>
  )
}
