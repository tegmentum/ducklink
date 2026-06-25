// @ts-check

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

/**
 * Explicit sidebar for the ducklink docs.
 *
 * @type {import('@docusaurus/plugin-content-docs').SidebarsConfig}
 */
const sidebars = {
  docsSidebar: [
    'intro',
    {
      type: 'category',
      label: 'Architecture',
      link: {type: 'doc', id: 'architecture/index'},
      items: [
        'architecture/capability-surface',
        'architecture/lean-core',
        'architecture/composition',
        'architecture/type-contract',
      ],
    },
    {
      type: 'category',
      label: 'Capabilities reference',
      link: {type: 'doc', id: 'capabilities/index'},
      items: [
        'capabilities/storage-pushdown',
        'capabilities/catalog-files-casts',
      ],
    },
    'catalog',
    {
      type: 'category',
      label: 'Guides',
      link: {type: 'doc', id: 'guides/index'},
      items: [
        'guides/writing-a-component',
        'guides/building',
        'guides/embedding-tracking',
        'guides/prefixes',
        'guides/serve',
        'guides/deployment',
      ],
    },
    {
      type: 'category',
      label: 'Reference',
      link: {type: 'doc', id: 'reference/index'},
      items: [
        'reference/official-extensions',
        'reference/community-extensions',
        'reference/iceberg',
        'reference/extension-roadmap',
      ],
    },
  ],
};

export default sidebars;
