import data from '../data/benchmarks.json'
import { useMDXComponents } from '../mdx-components'
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

// Theme components, so the tables look like the ones written in Markdown.
const { h2: H2, table: Table, tr: Tr, th: Th, td: Td } = useMDXComponents({})

const fastest = times => Math.min(...Object.values(times).filter(s => s != null))

// A time, in bold when it is the fastest of its row.
function Time({ times, id }) {
  const s = times[id]
  const best = s != null && s === fastest(times)
  const shown = seconds(s)
  return <Td className="nb-num">{best ? <strong>{shown}</strong> : shown}</Td>
}

function PhaseTable({ lang, phase }) {
  const t = text[lang].bench
  const totals = data.total[phase]
  return (
    <>
      <H2 id={phase}>{t[phase]}</H2>
      <Table>
        <thead><Tr><Th>{t.formula}</Th><Th className="nb-num">{t.deps}</Th>{data.tools.map(({ id }) => <Th key={id} className="nb-num">{id}</Th>)}</Tr></thead>
        <tbody>
          <Tr><Td><strong>{t.total(data.total.formulae)}</strong></Td><Td />{data.tools.map(({ id }) => <Time key={id} times={totals} id={id} />)}</Tr>
          {data.results.map(r => (
            <Tr key={r.formula}><Td>{r.formula}</Td><Td className="nb-num">{r.deps}</Td>{data.tools.map(({ id }) => <Time key={id} times={r[phase]} id={id} />)}</Tr>
          ))}
        </tbody>
      </Table>
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
      <div className="nb-landing nb-bench-bars"><Bars lang={lang} /></div>
      <p className="nb-note">{t.fastest}</p>
      <PhaseTable lang={lang} phase="cold" />
      <PhaseTable lang={lang} phase="warm" />
    </>
  )
}
