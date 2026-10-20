// Which repository files become which pages under /docs/, in which section, and what each page says about itself.
//
// The table is the one place that says so; build.ts renders from it, links.ts (the copy of mopm's) resolves a
// relative link between docs through it, and test/docs-table.test.ts holds it to the files: every source exists,
// every file under docs/ is listed (or on the explicit exclude list below) and no slug or source is listed twice.
//
// Status is the table's, not the document's, so a reader sees it before reading: a document is tagged by what its
// own text says about what is built. The rule is the one the pages of the site already use:
//   works    the page describes what exists in the code today (and says so for anything that does not)
//   partial  built in part: the document marks some parts as built and others as planned
//   design   a design: written, reviewed or accepted, and not (or not yet) the code
//   planned  everything in it is planned
// Rendering puts the status beside the title and on the /docs/ index (tools/docs.ts).

export type Section = 'guides' | 'design' | 'project';
export type Status = 'works' | 'partial' | 'design' | 'planned';

export interface DocEntry {
  /** The repo path of the Markdown file. */
  source: string;
  /** The page is served at /docs/<slug>/. */
  slug: string;
  section: Section;
  status: Status;
}

export const SECTIONS: readonly { id: Section; title: string; blurb: string }[] = [
  { id: 'guides', title: 'Guides and reference', blurb: 'How to run kbf and what its interfaces are: host setup, the operator API, platform properties, authentication, artifacts.' },
  { id: 'design', title: 'Design', blurb: 'How kbf is built and why: the design documents, decision records and spikes. Each is tagged with how much of it is built.' },
  { id: 'project', title: 'Project', blurb: 'The repository README, the architecture overview, how to contribute and how to report a vulnerability.' },
];

export const STATUSES: Readonly<Record<Status, { label: string; tag: string }>> = {
  works: { label: 'Works today', tag: 'tag-ok' },
  partial: { label: 'Built in part', tag: 'tag-muted' },
  design: { label: 'Design', tag: 'tag-planned' },
  planned: { label: 'Planned', tag: 'tag-planned' },
};

const g = (source: string, slug: string, status: Status): DocEntry => ({ source, slug, section: 'guides', status });
const d = (source: string, slug: string, status: Status): DocEntry => ({ source, slug, section: 'design', status });
const p = (source: string, slug: string, status: Status): DocEntry => ({ source, slug, section: 'project', status });

export const DOCS: readonly DocEntry[] = [
  g('docs/deploy/kbf-installer-setup.md', 'deploy/kbf-installer-setup', 'partial'),
  g('docs/deploy/kbf-installer-migrate.md', 'deploy/kbf-installer-migrate', 'partial'),
  g('docs/deploy/kbf-modes.md', 'deploy/kbf-modes', 'works'),
  g('docs/deploy/storage-node.md', 'deploy/storage-node', 'works'),
  g('docs/deploy/linux-build-host.md', 'deploy/linux-build-host', 'works'),
  g('docs/deploy/host-releases.md', 'deploy/host-releases', 'works'),
  g('docs/deploy/tls-renewal.md', 'deploy/tls-renewal', 'works'),
  g('docs/api.md', 'api', 'works'),
  g('docs/artifacts.md', 'artifacts', 'partial'),
  g('docs/platform-properties.md', 'platform-properties', 'works'),
  g('docs/reapi-auth.md', 'reapi-auth', 'works'),

  d('docs/design/architecture-v2.md', 'design/architecture-v2', 'design'),
  d('docs/design/cluster.md', 'design/cluster', 'design'),
  d('docs/design/capabilities.md', 'design/capabilities', 'partial'),
  d('docs/design/scheduler.md', 'design/scheduler', 'partial'),
  d('docs/design/storage.md', 'design/storage', 'partial'),
  d('docs/design/write-path.md', 'design/write-path', 'partial'),
  d('docs/design/write-tier-scenarios.md', 'design/write-tier-scenarios', 'partial'),
  d('docs/design/daemon.md', 'design/daemon', 'partial'),
  d('docs/design/worker-protocol.md', 'design/worker-protocol', 'works'),
  d('docs/design/worker-cache.md', 'design/worker-cache', 'partial'),
  d('docs/design/deployment-topology.md', 'design/deployment-topology', 'partial'),
  d('docs/design/failure-classes.md', 'design/failure-classes', 'partial'),
  d('docs/design/simulation.md', 'design/simulation', 'partial'),
  d('docs/design/metadata-snapshots.md', 'design/metadata-snapshots', 'design'),
  d('docs/design/rebalance-handoff.md', 'design/rebalance-handoff', 'design'),
  d('docs/design/object-store-registration.md', 'design/object-store-registration', 'design'),
  d('docs/design/invocations.md', 'design/invocations', 'design'),
  d('docs/design/invocations-api.md', 'design/invocations-api', 'planned'),
  d('docs/design/operator-verbs.md', 'design/operator-verbs', 'works'),
  d('docs/design/ui-data.md', 'design/ui-data', 'design'),
  d('docs/design/distribution.md', 'design/distribution', 'design'),
  d('docs/design/kbf-installer.md', 'design/kbf-installer', 'partial'),
  d('docs/design/pull-updates.md', 'design/pull-updates', 'partial'),
  d('docs/design/split-mode-updates.md', 'design/split-mode-updates', 'partial'),
  d('docs/design/fleet-updates.md', 'design/fleet-updates', 'planned'),
  d('docs/design/fleet-updates-security.md', 'design/fleet-updates-security', 'planned'),
  d('docs/design/aio-handover.md', 'design/aio-handover', 'design'),
  d('docs/design/frontend-handover.md', 'design/frontend-handover', 'partial'),
  d('docs/design/frontend-proxy.md', 'design/frontend-proxy', 'design'),
  d('docs/design/frontend-proxy-u0.md', 'design/frontend-proxy-u0', 'works'),
  d('docs/design/kbf-proxy-api.md', 'design/kbf-proxy-api', 'design'),
  d('docs/design/update-ui.md', 'design/update-ui', 'design'),
  d('docs/design/kbf-cicd.md', 'design/kbf-cicd', 'design'),
  d('docs/design/gamma-deploy.md', 'design/gamma-deploy', 'works'),
  d('docs/design/macos-end-state.md', 'design/macos-end-state', 'planned'),
  d('docs/design/macos-vms.md', 'design/macos-vms', 'planned'),
  d('docs/design/macos-vm-guests.md', 'design/macos-vm-guests', 'planned'),
  d('docs/design/mac-node-provisioning.md', 'design/mac-node-provisioning', 'design'),
  d('docs/design/ios-devices.md', 'design/ios-devices', 'planned'),
  d('docs/design/mdm-backend.md', 'design/mdm-backend', 'planned'),
  d('docs/adr/0001-consensus.md', 'design/adr-0001-consensus', 'design'),
  d('docs/spikes/hosted-runners.md', 'design/spike-hosted-runners', 'works'),
  d('docs/spikes/macos-p0.md', 'design/spike-macos-p0', 'design'),

  p('README.md', 'readme', 'works'),
  // /docs/architecture/ is the site's own page (content/architecture.html); the repository's document is its long form.
  p('ARCHITECTURE.md', 'architecture/detail', 'partial'),
  p('CONTRIBUTING.md', 'contributing', 'works'),
  p('SECURITY.md', 'security', 'works'),
];

/** Files under docs/ that are deliberately not pages. Empty: every document under docs/ is published. */
export const EXCLUDE: readonly string[] = [];

/** The route a doc is served at. */
export const docRoute = (e: { slug: string }): string => `/docs/${e.slug}/`;
