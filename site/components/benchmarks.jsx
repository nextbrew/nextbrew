import data from '../data/benchmarks.json'
import { text } from './text'

export const hasBenchmarks = data.results.length > 0

export function Bars() {
  return data.results.map(r => {
    const max = Math.max(r.nextbrew, r.brew)
    return (
      <div key={r.formula} className="nb-bars">
        <div className="nb-muted">{r.formula}</div>
        {[['nextbrew', r.nextbrew, 'nb-bar-nb'], ['brew', r.brew, 'nb-bar-brew']].map(([who, s, cls]) => (
          <div key={who} className="nb-bar"><span>{who}</span><i className={cls} style={{ width: `${(s / max) * 100}%` }} /><span>{s.toFixed(1)}s</span></div>
        ))}
      </div>
    )
  })
}

export function BenchmarkTable({ lang }) {
  const t = text[lang].bench
  if (!hasBenchmarks) return <p>{t.pending}</p>
  return (
    <>
      <p>{t.measured}: {data.measured} · {data.machine}</p>
      <table>
        <thead><tr><th>{t.formula}</th><th>{t.deps}</th><th>nextbrew</th><th>brew</th></tr></thead>
        <tbody>{data.results.map(r => (
          <tr key={r.formula}><td>{r.formula}</td><td>{r.deps}</td><td>{r.nextbrew.toFixed(1)}s</td><td>{r.brew.toFixed(1)}s</td></tr>
        ))}</tbody>
      </table>
    </>
  )
}
