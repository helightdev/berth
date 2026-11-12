import { llm } from '@/lib/llms';

export const revalidate = false;

// Every page as Markdown in one file.
export async function GET() {
  return new Response(await llm.full(), {
    headers: { 'content-type': 'text/markdown; charset=utf-8' },
  });
}
