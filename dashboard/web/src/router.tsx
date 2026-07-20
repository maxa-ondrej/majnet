import { createRootRoute, createRoute, createRouter } from '@tanstack/react-router'
import { Shell } from './shell'
import { Activity, Nodes, ProjectDetail, Projects } from './views'
import { Overview } from './overview'
import { NewApp, NewProject, NewService } from './forms'
import { AppDetail } from './appDetail'
import { Members } from './members'
import { Deploys } from './deploys'
import { AllReleases, AllDeploys } from './fleet'
import { Settings } from './settings'
import { ControlPlane } from './controlPlane'
import { Terminal } from './terminal'

const rootRoute = createRootRoute({ component: Shell })

// Literal paths (not via a helper) so TanStack Router infers the typed route map.
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: Overview })
const projectsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects', component: Projects })
const newProjectRoute = createRoute({ getParentRoute: () => rootRoute, path: '/new-project', component: NewProject })
const activityRoute = createRoute({ getParentRoute: () => rootRoute, path: '/activity', component: Activity })
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/settings', component: Settings })
const nodesRoute = createRoute({ getParentRoute: () => rootRoute, path: '/nodes', component: Nodes })
const releasesRoute = createRoute({ getParentRoute: () => rootRoute, path: '/releases', component: AllReleases })
const allDeploysRoute = createRoute({ getParentRoute: () => rootRoute, path: '/deploys', component: AllDeploys })
const controlPlaneRoute = createRoute({ getParentRoute: () => rootRoute, path: '/control-plane', component: ControlPlane })
interface TermSearch { mode?: string; node?: string; project?: string; app?: string; class?: string }
const terminalRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/terminal',
  component: Terminal,
  validateSearch: (s: Record<string, unknown>): TermSearch => ({
    mode: s.mode as string | undefined,
    node: s.node as string | undefined,
    project: s.project as string | undefined,
    app: s.app as string | undefined,
    class: s.class as string | undefined,
  }),
})
const projectRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org', component: ProjectDetail })
const newAppRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/new-app', component: NewApp })
const newServiceRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/new-service', component: NewService })
const membersRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/members', component: Members })
const deploysRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/deploys', component: Deploys })
const appRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/apps/$app', component: AppDetail })

const routeTree = rootRoute.addChildren([
  indexRoute, projectsRoute, newProjectRoute, activityRoute, settingsRoute, nodesRoute, controlPlaneRoute, terminalRoute,
  releasesRoute, allDeploysRoute,
  projectRoute, newAppRoute, newServiceRoute, membersRoute, deploysRoute, appRoute,
])

export const router = createRouter({ routeTree, defaultPreload: 'intent' })

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}
