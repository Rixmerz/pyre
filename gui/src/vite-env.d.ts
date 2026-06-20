/// <reference types="vite/client" />

// Raw text imports (e.g. inline SVG): `import logo from "./logo.svg?raw"`.
declare module "*?raw" {
  const content: string;
  export default content;
}

// URL imports (asset path string): `import iconUrl from "./icon.png?url"`.
declare module "*?url" {
  const url: string;
  export default url;
}
