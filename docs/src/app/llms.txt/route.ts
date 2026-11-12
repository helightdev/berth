import { llm } from '@/lib/llms';

export const revalidate = false;

// The index of the docs for language models: every page with a link to its Markdown.
export function GET() {
  return new Response(llm.index(), { headers: { 'content-type': 'text/markdown; charset=utf-8' } });
}
