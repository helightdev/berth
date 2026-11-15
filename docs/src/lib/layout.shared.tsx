import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

export const github = 'https://github.com/helightdev/berth';

/** Files in public/, with the folder the site is served from (DOCS_BASE_PATH). */
export const asset = (path: string) => `${process.env.NEXT_PUBLIC_BASE_PATH ?? ''}${path}`;

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <img src={asset('/logo.svg')} alt="" width={22} height={22} />
          <span className="font-semibold">berth</span>
        </>
      ),
    },
    githubUrl: github,
    // Dark only.
    themeSwitch: { enabled: false },
  };
}
