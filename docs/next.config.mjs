import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  // A static site: `npm run build` writes it to out/, for GitHub Pages or any file host.
  output: 'export',
  trailingSlash: true,
  reactStrictMode: true,
  // Set when the site is served from a folder, such as /berth on github.io.
  basePath: process.env.DOCS_BASE_PATH || undefined,
};

export default withMDX(config);
