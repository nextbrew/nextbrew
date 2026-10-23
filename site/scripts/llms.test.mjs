import assert from 'node:assert/strict'
import { test } from 'node:test'
import { llmsIndex, toMarkdown } from './llms.mjs'

test('toMarkdown drops frontmatter, imports and exports', () => {
  const src = `---\ntitle: Install\ndescription: How to install.\n---\nimport { X } from 'y'\nexport const a = 1\n\n# Install\n\nText.\n`
  assert.deepEqual(toMarkdown(src), { title: 'Install', description: 'How to install.', body: '# Install\n\nText.\n' })
})

test('llmsIndex lists pages with their .md links', () => {
  const out = llmsIndex([{ route: '/en/docs/installation', title: 'Installation', description: 'Install it.' }], 'https://nextbrew.dev')
  assert.match(out, /^# nextbrew\n/)
  assert.match(out, /- \[Installation\]\(https:\/\/nextbrew\.dev\/en\/docs\/installation\.md\): Install it\./)
})

test('toMarkdown keeps import/export lines inside code fences', () => {
  const src = '---\ntitle: Env\ndescription: Vars.\n---\nexport const a = 1\n\n```sh\nexport FOO=1\n```\n'
  assert.equal(toMarkdown(src).body, '```sh\nexport FOO=1\n```\n')
})
