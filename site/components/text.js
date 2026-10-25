// Strings shown outside MDX content, per locale.
export const text = {
  en: {
    footer: {
      docs: 'Docs', project: 'Project', language: 'Language',
      installation: 'Installation', quickStart: 'Quick Start', migrating: 'Migrating from Homebrew',
      releases: 'Releases', benchmarks: 'Benchmarks',
      credits: 'MIT or Apache 2.0.',
    },
    landing: {
      title: 'Homebrew, but fast.',
      subtitle: 'Same formulae. Same bottles. Built for speed. A single Rust binary for macOS and Linux.',
      start: 'Get Started', why: 'Why nextbrew?',
      speedTitle: 'Installs in seconds.',
      speedBody: 'The same packages as brew, downloaded and unpacked in parallel.',
      allBenchmarks: 'See all benchmarks →',
      features: [
        ['Homebrew compatible', 'Same formulae, same bottles, same commands and exit codes.'],
        ['Runs alongside Homebrew', 'Uses its own prefix (/opt/nextbrew on macOS) and never touches your brew setup. Try it risk-free.'],
        ['Never skips silently', 'Anything unsupported fails by name. Skipped post-install steps are reported.'],
        ['Verified', 'Every bottle is checked against its SHA-256. Releases ship with build provenance.'],
        ['macOS and Linux', 'Apple Silicon, Intel, and Linux on x86_64 and arm64.'],
        ['Agent-ready', 'nbx runs any formula without linking it into your prefix, and the docs ship llms.txt and an Agent Skill.'],
      ],
      cta: 'Ready to brew faster?', copy: 'Copy',
    },
    bench: {
      formula: 'Formula', deps: 'Dependencies', measured: 'Measured', pending: 'Benchmarks have not been measured yet.',
      cold: 'Cold', warm: 'Warm', total: n => `Total (${n} formulae)`, rounds: n => (n === 1 ? '1 round' : `median of ${n} rounds`),
      fastest: 'The fastest time in each row is in bold.',
    },
  },
  ja: {
    footer: {
      docs: 'ドキュメント', project: 'プロジェクト', language: '言語',
      installation: 'インストール', quickStart: 'クイックスタート', migrating: 'Homebrew からの移行',
      releases: 'リリース', benchmarks: 'ベンチマーク',
      credits: 'MIT または Apache 2.0。',
    },
    landing: {
      title: 'Homebrew, but fast.',
      subtitle: '同じ formula、同じ bottle のまま、速さにこだわりました。macOS と Linux に対応した、Rust 製の単一バイナリです。',
      start: 'はじめる', why: 'nextbrew を選ぶ理由',
      speedTitle: '数秒でインストール。',
      speedBody: 'brew と同じパッケージを、並列にダウンロードして展開します。',
      allBenchmarks: 'すべてのベンチマーク →',
      features: [
        ['Homebrew 互換', '同じ formula、同じ bottle、同じコマンドと終了コード。'],
        ['Homebrew と共存', '専用の場所（macOS では /opt/nextbrew）に入り、brew の環境には触れません。気軽に試せます。'],
        ['黙ってスキップしない', '対応していない処理は名前を出して失敗します。スキップした post-install も報告します。'],
        ['検証つき', 'bottle はすべて SHA-256 で検証。リリースにはビルド元の証明が付きます。'],
        ['macOS と Linux', 'Apple Silicon、Intel、Linux の x86_64 と arm64。'],
        ['エージェント対応', 'nbx ならリンクせずにすぐ実行。ドキュメントは llms.txt と Agent Skill を用意。'],
      ],
      cta: 'さあ、速く brew しよう。', copy: 'コピー',
    },
    bench: {
      formula: 'Formula', deps: '依存', measured: '計測日', pending: 'ベンチマークはまだ計測していません。',
      cold: 'Cold', warm: 'Warm', total: n => `合計（${n} formula）`, rounds: n => (n === 1 ? '1 回' : `${n} 回の中央値`),
      fastest: '各行でいちばん速い時間を太字にしています。',
    },
  },
}
