import Link from 'next/link';
import { github } from '@/lib/layout.shared';

export default function Home() {
  return (
    <main className="mx-auto flex w-full max-w-3xl flex-1 flex-col justify-center gap-6 px-4 py-16">
      <img src="/logo.svg" alt="" width={56} height={56} />
      <h1 className="text-4xl font-semibold tracking-tight">berth</h1>
      <p className="text-lg text-fd-muted-foreground">
        Sets up Ubuntu machines for coding agents and looks after them: a spare laptop from a USB stick, a rented server
        over SSH, a cloud machine from a seed, or a container. Each gets a desktop you can watch, the agents you pick,
        and a page in a terminal UI for status, sign-ins, updates and secrets.
      </p>
      <pre className="overflow-x-auto rounded-lg border bg-fd-card p-4 text-sm">
        <code>{'git clone https://github.com/helightdev/berth && cd berth\nmake install\nberth'}</code>
      </pre>
      <div className="flex gap-3 text-sm font-medium">
        <Link href="/docs" className="rounded-md border px-4 py-2 text-fd-primary">
          Read the docs
        </Link>
        <a href={github} className="rounded-md border px-4 py-2">
          GitHub
        </a>
      </div>
    </main>
  );
}
