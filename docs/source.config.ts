import { defineConfig, defineDocs } from 'fumadocs-mdx/config';

// The guides are the Markdown files in content/docs.
export const docs = defineDocs({ dir: 'content/docs' });

export default defineConfig({
  mdxOptions: {
    rehypeCodeOptions: {
      themes: { light: 'github-light', dark: 'github-dark' },
      defaultColor: false,
    },
  },
});
