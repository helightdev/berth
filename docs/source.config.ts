import { defineConfig, defineDocs } from 'fumadocs-mdx/config';
import lastModified from 'fumadocs-mdx/plugins/last-modified';

// The guide is content/docs/(guide); the API reference beside it is generated.
export const docs = defineDocs({
  dir: 'content/docs',
  // Each page also as plain Markdown, for llms.txt and the .md routes.
  docs: { postprocess: { includeProcessedMarkdown: true } },
});

export default defineConfig({
  // "Last updated" on each page, from the git history.
  plugins: [lastModified()],
  mdxOptions: {
    rehypeCodeOptions: {
      themes: { light: 'github-light', dark: 'github-dark' },
      defaultColor: false,
    },
  },
});
