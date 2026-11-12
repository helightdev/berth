import Link from 'next/link';
import { CodeBlock, Pre } from 'fumadocs-ui/components/codeblock';
import { Blocks, Cloud, Container, KeyRound, MonitorPlay, Plug, Server, SquareTerminal, Usb } from 'lucide-react';
import type { ReactNode } from 'react';
import { asset, github } from '@/lib/layout.shared';

const install = 'curl -fsSL https://raw.githubusercontent.com/helightdev/berth/main/install.sh | sh';

const features: { icon: ReactNode; title: string; text: string; href: string }[] = [
  {
    icon: <MonitorPlay />,
    title: 'A desktop you can watch',
    text: 'XFCE on a persistent virtual display, in your browser through noVNC, with T3 Code beside it.',
    href: '/docs/use/access',
  },
  {
    icon: <SquareTerminal />,
    title: 'One terminal UI for every host',
    text: 'Status, a shell, updates and reboots per host, and the same tasks as commands for scripts.',
    href: '/docs/use/overview',
  },
  {
    icon: <KeyRound />,
    title: 'Sign-ins and secrets',
    text: 'Sign the agents in from your own browser, and hand over tokens and registry logins.',
    href: '/docs/use/sign-ins',
  },
  {
    icon: <Blocks />,
    title: 'Plugins',
    text: 'Optional software, tasks and services as npm packages: Unity, backups, or your own.',
    href: '/docs/extend/plugins',
  },
  {
    icon: <Plug />,
    title: 'APIs on both sides',
    text: 'berthd on each host and berth serve on your machine, described as OpenAPI and AsyncAPI.',
    href: '/docs/extend/serve',
  },
  {
    icon: <Server />,
    title: 'One role for every install',
    text: 'The same Ansible role sets up a laptop, a server, a cloud machine or a container.',
    href: '/docs/start/concepts',
  },
];

const targets: { icon: ReactNode; title: string; href: string }[] = [
  { icon: <Usb />, title: 'Laptop from USB', href: '/docs/deploy/usb' },
  { icon: <Server />, title: 'Server over SSH', href: '/docs/deploy/ssh' },
  { icon: <Cloud />, title: 'Cloud from a seed', href: '/docs/deploy/cloud' },
  { icon: <Container />, title: 'Container', href: '/docs/deploy/container' },
];

export default function Home() {
  return (
    <main className="flex flex-1 flex-col">
      <section className="mx-auto flex w-full max-w-5xl flex-col items-center gap-6 px-4 pt-16 pb-10 text-center md:pt-24">
        <img src={asset('/logo.svg')} alt="" width={64} height={64} />
        <h1 className="text-4xl font-semibold tracking-tight md:text-5xl">Machines for coding agents</h1>
        <p className="max-w-2xl text-lg text-fd-muted-foreground">
          berth turns a spare laptop, a server or a container into an Ubuntu desktop for coding agents, and looks after
          it from your terminal: status, sign-ins, updates and secrets.
        </p>
        <div className="w-full max-w-2xl text-start">
          <CodeBlock title="Install on Linux x86_64" className="my-0">
            <Pre>
              <code className="block px-4">{install}</code>
            </Pre>
          </CodeBlock>
        </div>
        <div className="flex flex-wrap justify-center gap-3 text-sm font-medium">
          <Link
            href="/docs/start/quick-start"
            className="rounded-lg bg-fd-primary px-5 py-2.5 text-fd-primary-foreground transition-opacity hover:opacity-90"
          >
            Quick start
          </Link>
          <Link href="/docs" className="rounded-lg border px-5 py-2.5 transition-colors hover:bg-fd-accent">
            Read the docs
          </Link>
          <a href={github} className="rounded-lg border px-5 py-2.5 transition-colors hover:bg-fd-accent">
            GitHub
          </a>
        </div>
      </section>

      <section className="mx-auto w-full max-w-5xl px-4">
        <img
          src={asset('/screens/overview.svg')}
          alt="berth's terminal UI showing a host's overview with its tasks"
          className="w-full rounded-xl border shadow-2xl"
        />
      </section>

      <section className="mx-auto w-full max-w-5xl px-4 pt-16">
        <h2 className="mb-2 text-center text-2xl font-semibold">Install it your way</h2>
        <p className="mb-6 text-center text-fd-muted-foreground">
          Every way runs the same role, so every host ends up the same.
        </p>
        <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
          {targets.map((target) => (
            <Link
              key={target.href}
              href={target.href}
              className="flex items-center gap-3 rounded-xl border bg-fd-card p-4 text-sm font-medium transition-colors hover:bg-fd-accent [&_svg]:size-5 [&_svg]:shrink-0 [&_svg]:text-fd-primary"
            >
              {target.icon}
              {target.title}
            </Link>
          ))}
        </div>
      </section>

      <section className="mx-auto grid w-full max-w-5xl gap-4 px-4 py-16 sm:grid-cols-2 lg:grid-cols-3">
        {features.map((feature) => (
          <Link
            key={feature.title}
            href={feature.href}
            className="flex flex-col gap-2 rounded-xl border bg-fd-card p-5 transition-colors hover:bg-fd-accent"
          >
            <span className="text-fd-primary [&_svg]:size-5">{feature.icon}</span>
            <span className="font-medium">{feature.title}</span>
            <span className="text-sm text-fd-muted-foreground">{feature.text}</span>
          </Link>
        ))}
      </section>
    </main>
  );
}
