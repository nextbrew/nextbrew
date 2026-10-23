import { Bars, hasBenchmarks } from './benchmarks'
import { CopyCommand } from './copy-command'
import { Terminal } from './terminal'
import { text } from './text'

const INSTALL = 'curl -fsSL https://nextbrew.dev/install | bash'

export function Landing({ lang }) {
  const t = text[lang].landing
  return (
    <div className="nb-landing">
      <section className="nb-hero">
        <div>
          <h1>{t.title}</h1>
          <p>{t.subtitle}</p>
          <div className="nb-actions">
            <a className="nb-btn nb-btn-primary" href={`/${lang}/docs/installation`}>{t.start}</a>
            <a className="nb-btn" href={`/${lang}/docs`}>{t.why}</a>
          </div>
          <CopyCommand command={INSTALL} />
        </div>
        <Terminal />
      </section>
      {hasBenchmarks && (
        <section className="nb-speed">
          <div><h2>{t.speedTitle}</h2><p>{t.speedBody}</p><a href={`/${lang}/benchmarks`}>{t.allBenchmarks}</a></div>
          <div><Bars /></div>
        </section>
      )}
      <section className="nb-features">
        {t.features.map(([title, body]) => <div key={title}><h3>{title}</h3><p>{body}</p></div>)}
      </section>
      <section className="nb-cta"><h2>{t.cta}</h2><CopyCommand command={INSTALL} /></section>
    </div>
  )
}
