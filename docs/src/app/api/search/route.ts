import { createFromSource } from 'fumadocs-core/search/server';
import { source } from '@/lib/source';

export const revalidate = false;

// One static index of the guides; the search dialog downloads it.
export const { staticGET: GET } = createFromSource(source, { language: 'english' });
