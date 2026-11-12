import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();
// Set when the site is served from a folder, such as /berth on github.io.
const basePath = process.env.DOCS_BASE_PATH || undefined;

/** @type {import('next').NextConfig} */
const config = {
  // A static site: `npm run build` writes it to out/, for GitHub Pages or any file host.
  output: 'export',
  trailingSlash: true,
  reactStrictMode: true,
  basePath,
  // No image server on a static host; the screenshots are SVG anyway.
  images: { unoptimized: true },
  // Pages that name files in public/ add the folder themselves.
  env: { NEXT_PUBLIC_BASE_PATH: basePath ?? '' },
};

export default withMDX(config);
