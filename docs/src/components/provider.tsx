'use client';
import SearchDialog from '@/components/search';
import { RootProvider } from 'fumadocs-ui/provider/next';
import { type ReactNode } from 'react';

// The site is dark only, in the colours of berth's terminal UI: no system
// theme, no switch and no hotkey.
export function Provider({ children }: { children: ReactNode }) {
  return (
    <RootProvider
      search={{ SearchDialog }}
      theme={{ forcedTheme: 'dark', defaultTheme: 'dark', enableSystem: false, hotKey: false }}
    >
      {children}
    </RootProvider>
  );
}
