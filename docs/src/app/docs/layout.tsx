import { DocsLayout } from 'fumadocs-ui/layouts/spacious';
import type { ReactNode } from 'react';
import { baseOptions } from '@/lib/layout.shared';
import { source } from '@/lib/source';

// The spacious layout: the sidebar on the page background, each page in an
// inset panel beside it. The guide and the two API references are root
// folders, offered as tabs at the top of the sidebar.
export default function Layout({ children }: { children: ReactNode }) {
  return (
    <DocsLayout {...baseOptions()} tree={source.getPageTree()}>
      {children}
    </DocsLayout>
  );
}
