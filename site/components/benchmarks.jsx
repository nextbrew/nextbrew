import data from '../data/benchmarks.json'
import { text } from './text'

export const hasBenchmarks = data.results.length > 0

const seconds = s => (s == null ? '—' : s < 1 ? `${Math.round(s * 1000)}ms` : `${s.toFixed(1)}s`)

// Totals over the formulae every client installed, one group per phase.
export function Bars({ lang }) {
  const t = text[lang].bench
  return ['cold', 'warm'].map(phase => {
    const totals = data.total[phase]
    const max = Math.max(...Object.values(totals))
    return (
      <div key={phase} className="nb-bars">
        <div className="nb-muted">{t[phase]} · {t.total(data.total.formulae)}</div>
        {data.tools.map(({ id }) => (
          <div key={id} className="nb-bar"><span>{id}</span><i className={id === 'nextbrew' ? 'nb-bar-nb' : 'nb-bar-brew'} style={{ width: `${(totals[id] / max) * 100}%` }} /><span>{seconds(totals[id])}</span></div>
        ))}
      </div>
    )
  })
}

function PhaseTable({ lang, phase }) {
  const t = text[lang].bench
  const totals = data.total[phase]
  return (
    <>
      <h2>{t[phase]}</h2>
      <table>
        <thead><tr><th>{t.formula}</th><th>{t.deps}</th>{data.tools.map(({ id }) => <th key={id}>{id}</th>)}</tr></thead>
        <tbody>
          <tr><td><strong>{t.total(data.total.formulae)}</strong></td><td />{data.tools.map(({ id }) => <td key={id}><strong>{seconds(totals[id])}</strong></td>)}</tr>
          {data.results.map(r => (
            <tr key={r.formula}><td>{r.formula}</td><td>{r.deps}</td>{data.tools.map(({ id }) => <td key={id}>{seconds(r[phase][id])}</td>)}</tr>
          ))}
        </tbody>
      </table>
    </>
  )
}

export function BenchmarkTable({ lang }) {
  const t = text[lang].bench
  if (!hasBenchmarks) return <p>{t.pending}</p>
  return (
    <>
      <p>{t.measured}: {data.measured} · {data.machine} · {t.rounds(data.rounds)}</p>
      <p>{data.tools.map(({ id, version }) => `${id} ${version}`).join(' · ')}</p>
      <PhaseTable lang={lang} phase="cold" />
      <PhaseTable lang={lang} phase="warm" />
    </>
  )
}
