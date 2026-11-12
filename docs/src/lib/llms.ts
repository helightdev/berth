import { llms } from 'fumadocs-core/source';
import type { InferPageType } from 'fumadocs-core/source';
import { source } from '@/lib/source';

type DocPage = InferPageType<typeof source>;

const base = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

/** Where a page is served as Markdown: /md/using/commands.md, or /md/index.md for the first page. */
export function markdownSlug(page: DocPage): string[] {
  const slugs = page.slugs.length ? page.slugs : ['index'];
  return [...slugs.slice(0, -1), `${slugs[slugs.length - 1]}.md`];
}

export function markdownUrl(page: DocPage): string {
  return `${base}/md/${markdownSlug(page).join('/')}`;
}

/** A page as plain Markdown, for programs that read the docs instead of people. */
export async function pageMarkdown(page: DocPage): Promise<string> {
  const text = await page.data.getText('processed');
  const about = page.data.description ? `\n\n${page.data.description}` : '';
  return `# ${page.data.title}${about}\n\nSource: ${base}${page.url}\n\n${text.trim()}\n`;
}

// llms.txt lists the Markdown pages, so a reader that follows a link gets Markdown.
export const llm = llms(source, {
  renderPage: pageMarkdown,
  renderUrl: (item) => {
    const page = source.getPages().find((candidate) => candidate.url === item.url);
    return page ? markdownUrl(page) : item.url;
  },
});
