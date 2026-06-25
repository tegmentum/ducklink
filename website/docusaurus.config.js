// @ts-check
// `@type` JSDoc annotations allow editor autocompletion and type checking
// (when paired with `@ts-check`).
// See: https://docusaurus.io/docs/api/docusaurus-config

import {themes as prismThemes} from 'prism-react-renderer';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

const GITHUB_REPO = 'https://github.com/tegmentum/ducklink';

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'ducklink',
  tagline: 'DuckDB compiled to WebAssembly with a catalog of composable extension components',
  favicon: 'img/favicon.ico',

  // Future flags, see https://docusaurus.io/docs/api/docusaurus-config#future
  future: {
    v4: true, // Improve compatibility with the upcoming Docusaurus v4
  },

  // Set the production url of your site here
  url: 'https://tegmentum.github.io',
  // Set the /<baseUrl>/ pathname under which your site is served
  // For GitHub pages deployment, it is often '/<projectName>/'
  baseUrl: '/ducklink/',

  // GitHub pages deployment config.
  organizationName: 'tegmentum',
  projectName: 'ducklink',
  deploymentBranch: 'gh-pages',
  // GitHub Pages serves project sites without normalizing trailing slashes;
  // false avoids redirect/404 quirks on the hosted site.
  trailingSlash: false,

  // A stray cross-link shouldn't fail the build.
  onBrokenLinks: 'warn',

  markdown: {
    hooks: {
      onBrokenMarkdownLinks: 'warn',
    },
  },

  // Even if you don't use internationalization, you can use this field to set
  // useful metadata like html lang.
  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          sidebarPath: './sidebars.js',
          editUrl: `${GITHUB_REPO}/tree/main/website/`,
        },
        // Blog is unused.
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      colorMode: {
        respectPrefersColorScheme: true,
      },
      navbar: {
        title: 'ducklink',
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docsSidebar',
            position: 'left',
            label: 'Docs',
          },
          {
            href: GITHUB_REPO,
            label: 'GitHub',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Docs',
            items: [
              {label: 'Introduction', to: '/docs/intro'},
              {label: 'Architecture', to: '/docs/architecture/capability-surface'},
              {label: 'Extension catalog', to: '/docs/catalog'},
            ],
          },
          {
            title: 'More',
            items: [
              {label: 'GitHub', href: GITHUB_REPO},
            ],
          },
        ],
        copyright: `Copyright © ${new Date().getFullYear()} Tegmentum. Built with Docusaurus.`,
      },
      prism: {
        theme: prismThemes.github,
        darkTheme: prismThemes.dracula,
        additionalLanguages: ['bash', 'toml', 'sql', 'json', 'rust'],
      },
    }),
};

export default config;
