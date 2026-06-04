import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const docsBase = '/duckagent';
const docsRoutePrefixes = new Set([
  'avatar',
  'benchmark',
  'capabilities',
  'development',
  'gateway',
  'reference',
  'sandbox',
  'start',
]);

function addDocsBasePath(value) {
  if (typeof value !== 'string') {
    return value;
  }
  if (value === '/') {
    return `${docsBase}/`;
  }
  if (!value.startsWith('/') || value.startsWith('//') || value.startsWith(`${docsBase}/`)) {
    return value;
  }

  const firstSegment = value.slice(1).split(/[/?#]/, 1)[0];
  if (!docsRoutePrefixes.has(firstSegment)) {
    return value;
  }

  return `${docsBase}${value}`;
}

function rewriteElementProperty(properties, name) {
  if (typeof properties?.[name] === 'string') {
    properties[name] = addDocsBasePath(properties[name]);
  }
}

function visitHtmlTree(node) {
  if (!node || typeof node !== 'object') {
    return;
  }

  if (node.type === 'element') {
    rewriteElementProperty(node.properties, 'href');
    rewriteElementProperty(node.properties, 'src');
  }

  if (Array.isArray(node.children)) {
    for (const child of node.children) {
      visitHtmlTree(child);
    }
  }
}

function rehypeProjectBaseLinks() {
  return visitHtmlTree;
}

export default defineConfig({
  site: 'https://selfonomy.github.io',
  base: docsBase,
  markdown: {
    rehypePlugins: [rehypeProjectBaseLinks],
  },
  integrations: [
    starlight({
      title: 'DuckAgent',
      description: 'Rust-native AI agent runtime, TUI, gateway, memory, skills, and sandbox documentation.',
      logo: {
        src: './public/favicon.png',
      },
      favicon: '/favicon.png',
      social: [
        {
          icon: 'github',
          label: 'GitHub',
          href: 'https://github.com/selfonomy/duckagent',
        },
      ],
      defaultLocale: 'root',
      locales: {
        root: {
          label: 'English',
          lang: 'en-US',
        },
      },
      pagefind: true,
      customCss: ['./src/styles/starlight.css'],
      components: {
        Header: './src/components/Header.astro',
        Sidebar: './src/components/Sidebar.astro',
        Footer: './src/components/Footer.astro',
      },
      sidebar: [
        {
          label: 'Start',
          items: [
            { label: 'Overview', slug: 'start' },
            { label: 'Install', slug: 'start/install' },
            { label: 'Getting Started', slug: 'start/getting-started' },
            { label: 'TUI or Service?', slug: 'start/tui-or-service' },
            { label: 'Configuration Basics', slug: 'start/configuration' },
            { label: 'Where Files Live', slug: 'start/files' },
          ],
        },
        {
          label: 'Avatar & Identity',
          items: [
            { label: 'Overview', slug: 'avatar' },
            { label: 'Profiles', slug: 'avatar/profiles' },
            { label: 'SOUL.md', slug: 'avatar/soul' },
            { label: 'USER.md', slug: 'avatar/user' },
            { label: 'Avatar Files', slug: 'avatar/avatar-files' },
            { label: 'SillyTavern Cards', slug: 'avatar/sillytavern-cards' },
            { label: 'AGENTS.md Instructions', slug: 'avatar/agents-md' },
          ],
        },
        {
          label: 'Capabilities',
          items: [
            { label: 'Overview', slug: 'capabilities' },
            { label: 'Built-in Capabilities', slug: 'capabilities/builtin' },
            { label: 'Filesystem Tools', slug: 'capabilities/filesystem' },
            { label: 'Process & Shell', slug: 'capabilities/process-shell' },
            { label: 'Web Search & Extract', slug: 'capabilities/web-search-extract' },
            { label: 'Memory', slug: 'capabilities/memory' },
            { label: 'Scheduled Tasks', slug: 'capabilities/cron' },
            { label: 'Skills', slug: 'capabilities/skills' },
            { label: 'MCP', slug: 'capabilities/mcp' },
          ],
        },
        {
          label: 'Gateway',
          items: [
            { label: 'Overview', slug: 'gateway' },
            { label: 'Service Start / Stop / Log', slug: 'gateway/service' },
            { label: 'Channels', slug: 'gateway/channels' },
            { label: 'Configure Channels', slug: 'gateway/configure-channels' },
            { label: 'Access And Approvals', slug: 'gateway/access-approvals' },
            { label: 'Session Routing', slug: 'gateway/session-routing' },
            { label: 'Media & Attachments', slug: 'gateway/media' },
            { label: 'Troubleshooting', slug: 'gateway/troubleshooting' },
          ],
        },
        {
          label: 'Sandbox',
          items: [
            { label: 'Overview', slug: 'sandbox' },
            { label: 'Presets', slug: 'sandbox/presets' },
            { label: 'Filesystem Rules', slug: 'sandbox/filesystem' },
            { label: 'Network Rules', slug: 'sandbox/network' },
            { label: 'Environment & Secrets', slug: 'sandbox/environment-secrets' },
            { label: 'Tool & Shell Permissions', slug: 'sandbox/tool-shell-permissions' },
            { label: 'Windows Setup', slug: 'sandbox/windows' },
            { label: 'Sandbox Config Reference', slug: 'sandbox/config-reference' },
          ],
        },
        {
          label: 'Benchmark',
          items: [
            { label: 'Overview', slug: 'benchmark' },
            { label: 'Current Context Policy', slug: 'benchmark/context-projection' },
            { label: 'Cache-Friendly Prompt', slug: 'benchmark/cache-friendly-prompt' },
            { label: 'Results', slug: 'benchmark/results' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'Overview', slug: 'reference' },
            { label: 'CLI Reference', slug: 'reference/cli' },
            { label: 'Config Reference', slug: 'reference/config' },
            { label: 'Gateway Config Reference', slug: 'reference/gateway-config' },
            { label: 'Channel Matrix', slug: 'reference/channel-matrix' },
            { label: 'TUI Reference', slug: 'reference/tui' },
            { label: 'Session Rewind', slug: 'reference/session-rewind' },
            { label: 'Feature Coverage', slug: 'reference/feature-coverage' },
          ],
        },
        {
          label: 'Development',
          items: [
            { label: 'Docs Site', slug: 'development/docs-site' },
            { label: 'Localization', slug: 'development/localization' },
          ],
        },
      ],
    }),
  ],
});
