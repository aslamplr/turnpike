/// Vite's client types, which declare the ambient modules for the asset imports
/// this app relies on — `import "./app.css"` above all. Without this file the
/// build succeeds (Vite resolves CSS itself) but `svelte-check` reports the
/// import as unresolvable.
/// <reference types="vite/client" />
