/// <reference types="vite/client" />

interface ImportMetaEnv {
  // CI-baked build metadata (dashboard/Dockerfile). Absent in `npm run dev`.
  readonly VITE_BUILD_VERSION?: string
  readonly VITE_BUILD_COMMIT?: string
  readonly VITE_BUILD_TIME?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}
