// After `next build`: write llms.txt, llms-full.txt and a .md copy of every
// English docs page into out/, for AI agents (spec §5).
import { mkdir, readdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const ORIGIN = 'https://nextbrew.dev'

export function toMarkdown(src) {
  const m = src.match(/^---\n([\s\S]*?)\n---\n/)
  const front = Object.fromEntries(
    (m?.[1] ?? '').split('\n').map(l => l.match(/^(\w+):\s*(.*)$/)).filter(Boolean).map(([, k, v]) => [k, v]),
  )
  let fence = false
  const body = src.slice(m ? m[0].length : 0)
    .split('\n').filter(l => {
      if (l.startsWith('```')) fence = !fence
      return fence || !/^(import|export) /.test(l)
    }).join('\n')
    .replace(/^\n+/, '')
  return { title: front.title ?? '', description: front.description ?? '', body }
}

export function llmsIndex(pages, origin) {
  const lines = pages.map(p => `- [${p.title}](${origin}${p.route}.md): ${p.description}`)
  return `# nextbrew\n\n> A fast, Homebrew-compatible package manager for macOS and Linux, written in Rust.\n\n## Docs\n\n${lines.join('\n')}\n`
}

async function mdxFiles(dir) {
  const entries = await readdir(dir, { withFileTypes: true, recursive: true })
  return entries.filter(e => e.isFile() && e.name.endsWith('.mdx')).map(e => join(e.parentPath, e.name)).sort()
}

async function main() {
  const site = join(dirname(fileURLToPath(import.meta.url)), '..')
  const docs = join(site, 'content/en/docs')
  const out = join(site, 'out')
  const pages = []
  for (const file of await mdxFiles(docs)) {
    const rel = relative(docs, file).replace(/\.mdx$/, '').replace(/(^|\/)index$/, '')
    const route = `/en/docs${rel ? `/${rel}` : ''}`
    const page = { route, ...toMarkdown(await readFile(file, 'utf8')) }
    pages.push(page)
    await mkdir(dirname(join(out, `${route}.md`)), { recursive: true })
    await writeFile(join(out, `${route}.md`), page.body)
  }
  await writeFile(join(out, 'llms.txt'), llmsIndex(pages, ORIGIN))
  await writeFile(join(out, 'llms-full.txt'), pages.map(p => p.body).join('\n\n'))
  console.log(`llms: ${pages.length} pages`)
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main()
