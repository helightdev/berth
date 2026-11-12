import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Accordion, Accordions } from 'fumadocs-ui/components/accordion';
import { File, Files, Folder } from 'fumadocs-ui/components/files';
import { ImageZoom } from 'fumadocs-ui/components/image-zoom';
import { Step, Steps } from 'fumadocs-ui/components/steps';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import { TypeTable } from 'fumadocs-ui/components/type-table';
import type { MDXComponents } from 'mdx/types';
import { asset } from '@/lib/layout.shared';

/** A terminal screenshot from public/screens, made by scripts/screens.py. */
export function Screen({ name, alt, caption }: { name: string; alt: string; caption?: string }) {
  return (
    <figure className="not-prose my-6">
      <ImageZoom
        src={asset(`/screens/${name}.svg`)}
        alt={alt}
        width={1000}
        height={600}
        className="h-auto w-full rounded-xl border bg-[#0d111c] shadow-lg"
      />
      {caption && <figcaption className="mt-2 text-center text-sm text-fd-muted-foreground">{caption}</figcaption>}
    </figure>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    Accordion,
    Accordions,
    File,
    Files,
    Folder,
    Screen,
    Step,
    Steps,
    Tab,
    Tabs,
    TypeTable,
    ...components,
  } satisfies MDXComponents;
}
