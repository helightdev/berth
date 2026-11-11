import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

export const github = 'https://github.com/helightdev/berth';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <img src="/logo.svg" alt="" width={20} height={20} />
          <b>berth</b>
        </>
      ),
    },
    githubUrl: github,
    links: [{ text: 'Docs', url: '/docs', active: 'nested-url' }],
  };
}
