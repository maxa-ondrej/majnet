import { createRootRoute, createRoute, createRouter } from '@tanstack/react-router'
import { Shell } from './shell'
import { Activity, Nodes, ProjectDetail, Projects } from './views'
import { NewApp, NewProject } from './forms'
import { AppDetail } from './appDetail'
import { Members } from './members'
import { Deploys } from './deploys'
import { Settings } from './settings'
import { ControlPlane } from './controlPlane'

const rootRoute = createRootRoute({ component: Shell })

// Literal paths (not via a helper) so TanStack Router infers the typed route map.
const indexRoute = createRoute({ getParentRoute: () => rootRoute, path: '/', component: Projects })
const newProjectRoute = createRoute({ getParentRoute: () => rootRoute, path: '/new-project', component: NewProject })
const activityRoute = createRoute({ getParentRoute: () => rootRoute, path: '/activity', component: Activity })
const settingsRoute = createRoute({ getParentRoute: () => rootRoute, path: '/settings', component: Settings })
const nodesRoute = createRoute({ getParentRoute: () => rootRoute, path: '/nodes', component: Nodes })
const controlPlaneRoute = createRoute({ getParentRoute: () => rootRoute, path: '/control-plane', component: ControlPlane })
const projectRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org', component: ProjectDetail })
const newAppRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/new-app', component: NewApp })
const membersRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/members', component: Members })
const deploysRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/deploys', component: Deploys })
const appRoute = createRoute({ getParentRoute: () => rootRoute, path: '/projects/$org/apps/$app', component: AppDetail })

const routeTree = rootRoute.addChildren([
  indexRoute, newProjectRoute, activityRoute, settingsRoute, nodesRoute, controlPlaneRoute,
  projectRoute, newAppRoute, membersRoute, deploysRoute, appRoute,
])

export const router = createRouter({ routeTree, defaultPreload: 'intent' })

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router
  }
}
