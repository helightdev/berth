import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
  EditOnGitHub,
  MarkdownCopyButton,
  PageLastUpdate,
  ViewOptionsPopover,
} from 'fumadocs-ui/layouts/spacious/page';
import { createRelativeLink } from 'fumadocs-ui/mdx';
import { getMDXComponents } from '@/components/mdx';
import { github } from '@/lib/layout.shared';
import { markdownUrl } from '@/lib/llms';
import { source } from '@/lib/source';

export default async function Page(props: PageProps<'/docs/[[...slug]]'>) {
  const page = source.getPage((await props.params).slug);
  if (!page) notFound();
  const MDX = page.data.body;
  // The operation pages are generated and have no source file in the repository.
  const generated = /^api\/(host|serve)\//.test(page.path);
  const { lastModified } = page.data;
  return (
    <DocsPage
      toc={page.data.toc}
      tableOfContent={{ style: 'clerk' }}
      breadcrumb={{ includeRoot: false }}
    >
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription className="mb-0">{page.data.description}</DocsDescription>
      {/* The page as Markdown, for pasting into or pointing an assistant at. */}
      <div className="flex flex-row flex-wrap items-center gap-2 border-b pb-6">
        <MarkdownCopyButton markdownUrl={markdownUrl(page)} />
        <ViewOptionsPopover
          markdownUrl={markdownUrl(page)}
          githubUrl={generated ? undefined : `${github}/blob/main/docs/content/docs/${page.path}`}
        />
      </div>
      <DocsBody>
        {/* Links between the guides are written as file names. */}
        <MDX
          components={getMDXComponents({
            a: createRelativeLink(source, page),
          })}
        />
      </DocsBody>
      {!generated && (
        <div className="flex flex-row flex-wrap items-center justify-between gap-4 border-t pt-4">
          <EditOnGitHub href={`${github}/blob/main/docs/content/docs/${page.path}`} />
          {lastModified && <PageLastUpdate date={new Date(lastModified)} />}
        </div>
      )}
    </DocsPage>
  );
}

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: PageProps<'/docs/[[...slug]]'>): Promise<Metadata> {
  const page = source.getPage((await props.params).slug);
  if (!page) notFound();
  return { title: page.data.title, description: page.data.description };
}
