import { notFound } from 'next/navigation';
import { markdownSlug, pageMarkdown } from '@/lib/llms';
import { source } from '@/lib/source';

export const revalidate = false;

// Each page as a Markdown file: /md/using/commands.md.
export async function GET(_: Request, { params }: RouteContext<'/md/[...slug]'>) {
  const wanted = (await params).slug.join('/');
  const page = source.getPages().find((candidate) => markdownSlug(candidate).join('/') === wanted);
  if (!page) notFound();
  return new Response(await pageMarkdown(page), {
    headers: { 'content-type': 'text/markdown; charset=utf-8' },
  });
}

export function generateStaticParams() {
  return source.getPages().map((page) => ({ slug: markdownSlug(page) }));
}
