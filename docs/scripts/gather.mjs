// Generates the API reference before every build, from the committed operation
// documents in docs/openapi. The guide, and the overview and portal pages of
// the API, are written by hand. Then puts the page of berth serve under /web.
await import('./reference.mjs');
await import('./web.mjs');
