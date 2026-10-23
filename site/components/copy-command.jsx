'use client'
import { useState } from 'react'

export function CopyCommand({ command, label }) {
  const [copied, setCopied] = useState(false)
  return (
    <button type="button" className="nb-cmd" title={label}
      onClick={() => navigator.clipboard.writeText(command).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500) }, () => {})}>
      <span><span className="nb-muted">$</span> {command}</span>
      <span className="nb-muted" aria-live="polite">{copied ? '✓' : '⧉'}</span>
    </button>
  )
}
