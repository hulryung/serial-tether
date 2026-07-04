import { defineConfig } from 'vitepress'

// Authoring note: don't soft-wrap a sentence so that a backtick-quoted
// `<placeholder>` starts a source line — markdown-it's HTML-block rule
// treats any line starting with `<tag>` as raw HTML (even inside a code
// span) and the Vue compile of that page fails with "Element is missing
// end tag". The build catches it loudly; just rewrap the sentence.

export default defineConfig({
  title: 'Serial Tether',
  description:
    'A daemon and CLI that lets AI agents and humans share a single serial device. Race-free RPC, raw-mode shell, UDS or TCP, cross-platform.',
  base: '/serial-tether/',
  lastUpdated: true,

  head: [
    // og:image must return 200 directly. github.com/.../raw/... redirects to
    // raw.githubusercontent.com and Twitterbot occasionally drops the
    // redirect, producing an empty card. Point straight at the CDN.
    ['meta', { property: 'og:title', content: 'Serial Tether — share any serial port with humans and AI' }],
    [
      'meta',
      {
        property: 'og:description',
        content:
          'One serial port. One daemon. Many clients. A small Rust daemon that lets a human, an AI agent, and a logger all attach to the same /dev/ttyUSB0 simultaneously.',
      },
    ],
    ['meta', { property: 'og:image', content: 'https://raw.githubusercontent.com/hulryung/serial-tether/main/assets/og.png' }],
    ['meta', { property: 'og:image:width', content: '1200' }],
    ['meta', { property: 'og:image:height', content: '630' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:url', content: 'https://hulryung.github.io/serial-tether/' }],
    ['meta', { name: 'twitter:card', content: 'summary_large_image' }],
    ['meta', { name: 'twitter:title', content: 'Serial Tether — share any serial port with humans and AI' }],
    [
      'meta',
      {
        name: 'twitter:description',
        content:
          'One serial port. One daemon. Many clients. A small Rust daemon that lets a human, an AI agent, and a logger all attach to the same /dev/ttyUSB0 simultaneously.',
      },
    ],
    ['meta', { name: 'twitter:image', content: 'https://raw.githubusercontent.com/hulryung/serial-tether/main/assets/og.png' }],
  ],

  // ../README.md (CLI_REFERENCE.md, TROUBLESHOOTING.md) and ../examples/
  // (OVERVIEW.md) are referenced from a couple of docs pages but live
  // outside srcDir (docs/); they render fine on GitHub but VitePress can't
  // resolve them as site routes, so they're exempted from dead-link
  // checking rather than rewritten (the .md files are canonical and out of
  // scope for this port). VitePress reports these normalized (extension
  // stripped, directory links resolved to their index) — matched as such.
  ignoreDeadLinks: [/^\.\/\.\.\/README$/, /^\.\/\.\.\/examples\/index$/],

  // The asciinema player + its stylesheet are only needed on the home page
  // (that's the only page with demo embeds) — inject them there instead of
  // via the global `head` array so the other ~10 doc pages don't pay for it.
  async transformHead(context) {
    if (context.page !== 'index.md') return
    return [
      ['link', { rel: 'stylesheet', href: 'https://cdn.jsdelivr.net/npm/asciinema-player@3.7.1/dist/bundle/asciinema-player.css' }],
      ['script', { src: 'https://cdn.jsdelivr.net/npm/asciinema-player@3.7.1/dist/bundle/asciinema-player.min.js', defer: '' }],
    ]
  },

  themeConfig: {
    nav: [
      { text: 'Guide', link: '/GETTING_STARTED' },
      { text: 'Cookbook', link: '/COOKBOOK' },
      { text: 'Reference', link: '/CLI_REFERENCE' },
      { text: 'crates.io', link: 'https://crates.io/crates/serial-tether' },
    ],

    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting Started', link: '/GETTING_STARTED' },
          { text: 'Cookbook', link: '/COOKBOOK' },
          { text: 'Troubleshooting', link: '/TROUBLESHOOTING' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'CLI Reference', link: '/CLI_REFERENCE' },
          { text: 'Architecture', link: '/OVERVIEW' },
          { text: 'Wire Protocol', link: '/PROTOCOL' },
          { text: 'exec on non-POSIX shells', link: '/EXEC_NONPOSIX_SHELLS' },
        ],
      },
      {
        text: 'AI Agents',
        items: [
          { text: 'Agent Cookbook', link: '/AGENT_USAGE' },
          { text: 'Onboarding Guide', link: '/AI_AGENT_GUIDE' },
        ],
      },
    ],

    outline: { level: [2, 3] },

    search: { provider: 'local' },

    socialLinks: [{ icon: 'github', link: 'https://github.com/hulryung/serial-tether' }],

    footer: {
      message:
        'Released under <a href="https://github.com/hulryung/serial-tether/blob/main/LICENSE-MIT">MIT</a> OR ' +
        '<a href="https://github.com/hulryung/serial-tether/blob/main/LICENSE-APACHE">Apache-2.0</a>.',
      copyright:
        'Crates: <a href="https://crates.io/crates/serial-tether">serial-tether</a> · ' +
        '<a href="https://crates.io/crates/tether-protocol">tether-protocol</a>',
    },
  },
})
