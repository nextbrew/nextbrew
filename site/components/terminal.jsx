'use client'
import { useEffect, useState } from 'react'

// ponytail: illustrative output, not a live run; keep in step with nb's real install output.
const LINES = [
  ['prompt', '$ nb install ffmpeg'],
  ['', 'Resolving dependencies…'],
  ['', 'Downloading  ████████████████'],
  ['', 'Linking      ████████████████'],
  ['ok', '✓ Installed ffmpeg'],
]

export function Terminal() {
  const [shown, setShown] = useState(LINES.length)
  useEffect(() => {
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return
    setShown(0)
    let n = 0
    const id = setInterval(() => { n += 1; setShown(n); if (n >= LINES.length) clearInterval(id) }, 600)
    return () => clearInterval(id)
  }, [])
  return (
    <div className="nb-term" role="img" aria-label="nb install ffmpeg">
      <div className="nb-term-bar"><i /><i /><i /></div>
      <pre aria-hidden="true">{LINES.slice(0, shown).map(([kind, line]) => <div key={line} className={kind && `nb-${kind}`}>{line}</div>)}</pre>
    </div>
  )
}
